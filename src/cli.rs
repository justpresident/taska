//! `ta` command-line surface: argument parsing, dispatch, and presentation.
//!
//! Command handlers depend on the [`EventStore`] abstraction rather than the
//! concrete [`FileStore`], so they can be exercised against any store.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value};

use crate::config::{CompactionConfig, Config, DisplayConfig, WorkflowConfig};
use crate::engine::Engine;
use crate::error::DynError;
use crate::git;
use crate::graph;
use crate::merge;
use crate::model::{is_done, MutationEvent, OpType, TaskState};
use crate::storage::{EventStore, FileStore};

#[derive(Parser)]
#[command(name = "ta", version = "0.1.0", about = "Taska Event Log Engine")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Output format for the listing commands. `--format` changes only *how* tasks
/// are rendered, never *which* fields show — that is `--columns`/`--full`/config.
#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    /// Aligned human table.
    Human,
    /// Pretty JSON array.
    Json,
    /// Newline-delimited JSON (one object per line).
    Jsonl,
}

/// Display flags shared by `list`, `search`, and `ready`.
#[derive(Args, Clone)]
struct DisplayArgs {
    /// Output format: human (aligned table), json (array), or jsonl (NDJSON)
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
    /// Show every field, not just the configured columns
    #[arg(long)]
    full: bool,
    /// Comma-separated columns to show, overriding config (e.g. --columns id,status)
    #[arg(long, value_delimiter = ',')]
    columns: Option<Vec<String>>,
    /// Sort rows by this column (id, deps, or any field), overriding config
    #[arg(long)]
    sort: Option<String>,
    /// Reverse the sort order (descending)
    #[arg(long)]
    reverse: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a taska repository environment
    Init,
    /// Create a new schema-agnostic task: `ta create <id> [field=value ...]`
    Create {
        id: String,
        /// Custom fields as `key=value` pairs (values parsed as JSON when possible)
        fields: Vec<String>,
    },
    /// Update fields on an existing task: `ta update <id> <field=value ...>`
    Update {
        id: String,
        /// Custom fields as `key=value` pairs; at least one is required
        #[arg(required = true)]
        fields: Vec<String>,
    },
    /// Bind a block constraint: `ta block <task_id> <depends_on>`
    Block { task_id: String, depends_on: String },
    /// Remove a block constraint: `ta unblock <task_id> <depends_on>`
    Unblock { task_id: String, depends_on: String },
    /// Delete a task: `ta delete <id>`
    Delete { id: String },
    /// List all tasks
    List {
        #[command(flatten)]
        display: DisplayArgs,
    },
    /// Search by AND-combined criteria: `ta search status~open priority=3`
    Search {
        /// One or more `field<op>value` criteria, all of which must match:
        /// `=` exact, `~` regex, `!=` not-equal, `!~` regex-no-match. `field`
        /// may be a task field, `id`, or `deps`.
        #[arg(required = true)]
        criteria: Vec<String>,
        #[command(flatten)]
        display: DisplayArgs,
    },
    /// Show a single task in full by id: `ta show <id>`
    Show {
        id: String,
        #[command(flatten)]
        display: DisplayArgs,
    },
    /// Show tasks ready to work on (deps satisfied, not done)
    Ready {
        #[command(flatten)]
        display: DisplayArgs,
    },
    /// Summary counts: total, per-status, blocked, ready, closed
    Status {
        /// Render as a human summary or a JSON object
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        format: OutputFormat,
    },
    /// Undo the last event(s): `ta undo [--count N] [--remove] [--force]`
    Undo {
        /// How many of the most recent events to undo (default 1)
        #[arg(long, default_value_t = 1)]
        count: usize,
        /// Apply without the confirmation prompt
        #[arg(long)]
        force: bool,
        /// Truncate committed events instead of appending compensating ones
        #[arg(long)]
        remove: bool,
    },
    /// Fold the mutation log into the baseline snapshot
    Compact,
    /// View or change config (`.taska/config.toml`) by dotted key
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Review and clear surfaced merge conflicts and orphaned events
    Resolve {
        /// Apply changes without the confirmation prompt
        #[arg(long)]
        force: bool,
    },
    /// Git event-log merge driver entrypoint (invoked by Git, not humans)
    #[command(name = "git-merge", hide = true)]
    GitMerge {
        ancestor: String,
        current: String,
        incoming: String,
        /// Original pathname (%P); accepted for Git compatibility, unused.
        #[arg(default_value = "")]
        path: String,
    },
    /// Git baseline merge driver entrypoint (invoked by Git, not humans)
    #[command(name = "git-merge-baseline", hide = true)]
    GitMergeBaseline {
        ancestor: String,
        current: String,
        incoming: String,
        /// Original pathname (%P); accepted for Git compatibility, unused.
        #[arg(default_value = "")]
        path: String,
    },
}

/// `ta config` subcommands: git-config-style get/set/list over dotted keys.
#[derive(Subcommand)]
enum ConfigAction {
    /// Print one effective value: `ta config get compaction.keep_events`
    Get { key: String },
    /// Set a value, validating the result: `ta config set merge.on_conflict ours`
    Set { key: String, value: String },
    /// Print every effective config value as `dotted.key = value`
    List,
}

/// Parse args and dispatch. `main` maps the result to an exit code.
pub fn run() -> Result<(), DynError> {
    let cli = Cli::parse();
    match cli.command {
        // Commands that don't operate on an existing store.
        Commands::Init => cmd_init(),
        Commands::GitMerge {
            ancestor,
            current,
            incoming,
            path: _,
        } => {
            // Git invokes the driver from the repo root; read the conflict policy
            // and marker location from the store if it's discoverable, else fall
            // back to defaults so a merge never fails merely for lack of config.
            let store = FileStore::discover().ok();
            let on_conflict = store
                .as_ref()
                .map(|s| s.config().merge.on_conflict)
                .unwrap_or_default();
            let marker = store
                .as_ref()
                .map(|s| s.base_dir.join("merge-conflict.json"));
            merge::execute_git_merge(
                &ancestor,
                &current,
                &incoming,
                on_conflict,
                marker.as_deref(),
            )
        }
        Commands::GitMergeBaseline {
            ancestor,
            current,
            incoming,
            path: _,
        } => merge::execute_git_merge_baseline(&ancestor, &current, &incoming),

        // Reviewing a surfaced conflict must work even if the config is currently
        // invalid, so it resolves the store without the validation gate.
        Commands::Resolve { force } => cmd_resolve(&FileStore::discover()?, force),

        // Config viewing/editing must also bypass the validation gate — otherwise
        // a bad hand-edit (e.g. keep_events below the floor) would lock you out of
        // the very command that fixes it. `set` validates the *result* itself.
        Commands::Config { action } => cmd_config(&FileStore::discover()?, action),

        // Everything else resolves the store once and validates its config
        // before dispatching, so a bad config edit surfaces on the next command.
        store_command => {
            let store = FileStore::discover()?;
            enforce_config(store.config())?;
            dispatch_store_command(store_command, &store)
        }
    }
}

/// Validate config on every store-backed command, so a bad config edit surfaces
/// on the very next `ta` invocation rather than silently at the next compaction.
fn enforce_config(cfg: &Config) -> Result<(), DynError> {
    cfg.validate()
}

/// Dispatch a command that operates on an already-resolved, already-validated
/// store. Handlers depend only on the `EventStore` abstraction.
fn dispatch_store_command(command: Commands, store: &FileStore) -> Result<(), DynError> {
    match command {
        Commands::Create { id, fields } => cmd_create(store, &id, &fields),
        Commands::Update { id, fields } => cmd_update(store, &id, &fields),
        Commands::Block {
            task_id,
            depends_on,
        } => cmd_dep(store, &task_id, &depends_on, OpType::AddDep),
        Commands::Unblock {
            task_id,
            depends_on,
        } => cmd_dep(store, &task_id, &depends_on, OpType::RemoveDep),
        Commands::Delete { id } => cmd_delete(store, &id),
        Commands::List { display } => cmd_list(store, &display, &store.config().display),
        Commands::Search { criteria, display } => {
            cmd_search(store, &criteria, &display, &store.config().display)
        }
        Commands::Show { id, display } => cmd_show(store, &id, &display, &store.config().display),
        Commands::Ready { display } => {
            let workflow = store.config().workflow.clone();
            cmd_ready(store, &workflow, &display, &store.config().display)
        }
        Commands::Status { format } => {
            let workflow = store.config().workflow.clone();
            cmd_status(store, &workflow, format)
        }
        Commands::Undo {
            count,
            force,
            remove,
        } => cmd_undo(store, count, force, remove),
        Commands::Compact => {
            let cfg = store.config().compaction.clone();
            cmd_compact(store, &cfg, Utc::now())
        }
        // Resolved before dispatch in `run`.
        Commands::Init
        | Commands::Config { .. }
        | Commands::Resolve { .. }
        | Commands::GitMerge { .. }
        | Commands::GitMergeBaseline { .. } => {
            unreachable!("non-store commands are handled before dispatch")
        }
    }
}

/// Materialize via the engine using the store's own workflow config (only the
/// `close_time` computation needs `status_field`/`done_status`), so callers don't
/// repeat those two arguments at every replay site.
fn replay(
    store: &impl EventStore,
    baseline: Vec<TaskState>,
    mutations: Vec<MutationEvent>,
) -> HashMap<String, TaskState> {
    let w = &store.config().workflow;
    Engine::materialize_state(baseline, mutations, &w.status_field, &w.done_status)
}

/// Like [`replay`] but keeping the orphan report (see [`Engine::materialize_report`]).
fn replay_report(
    store: &impl EventStore,
    baseline: Vec<TaskState>,
    mutations: Vec<MutationEvent>,
) -> (HashMap<String, TaskState>, Vec<u64>) {
    let w = &store.config().workflow;
    Engine::materialize_report(baseline, mutations, &w.status_field, &w.done_status)
}

/// Load and materialize the current task map from any store.
///
/// Replay also reports *orphaned* events — `Update`/`AddDep`/`RemoveDep`/`Delete`
/// events whose target task no longer exists, which apply to nothing. They are a
/// silent symptom of a dropped `Create` (from the merge driver's removal-union, a
/// revert, or a manual edit), so every read command warns about them on STDERR
/// and points at `ta resolve`. The warning never blocks the read.
fn state_of(store: &impl EventStore) -> Result<HashMap<String, TaskState>, DynError> {
    let (mut state, orphans) =
        replay_report(store, store.load_baseline()?, store.load_mutations()?);
    if !orphans.is_empty() {
        eprintln!(
            "taska: warning: {} orphaned event(s) in the log (no matching task) — \
             run `ta resolve` to clean them up.",
            orphans.len()
        );
    }
    // Surface the computed timestamps as ordinary (RFC 3339 string) fields under
    // their configured names, so list/search/show/--sort treat them like any
    // other column. This is display-only: the raw Option<DateTime> stays on
    // TaskState (and in the baseline); injection never reaches the stored log.
    let ts = &store.config().timestamps;
    for task in state.values_mut() {
        inject_time(&mut task.custom_fields, &ts.create_time, task.create_time);
        inject_time(&mut task.custom_fields, &ts.update_time, task.update_time);
        inject_time(&mut task.custom_fields, &ts.close_time, task.close_time);
    }
    Ok(state)
}

/// Insert a computed timestamp into a task's fields under `name` (RFC 3339), so
/// it renders/searches/sorts like a normal field. A blank `name` disables that
/// timestamp; a `None` value (e.g. `close_time` on an open task) injects
/// nothing, staying consistent with the omit-absent-fields rule.
fn inject_time(fields: &mut Map<String, Value>, name: &str, value: Option<DateTime<Utc>>) {
    if name.is_empty() {
        return;
    }
    if let Some(t) = value {
        fields.insert(name.to_string(), Value::String(t.to_rfc3339()));
    }
}

/// Event keys that are struct fields, not schema-agnostic task fields. Letting a
/// user field shadow one of these would either collide with the event envelope
/// or be silently swallowed by `_meta`, so we reject them up front.
const RESERVED_FIELD_KEYS: &[&str] = &["seq", "timestamp", "op", "task_id", "_meta"];

/// Parse `key=value` strings; values are parsed as JSON, falling back to a
/// plain string when that fails (so `status=open` stays a string).
fn parse_fields(fields: &[String]) -> Result<Map<String, Value>, DynError> {
    let mut map = Map::new();
    for raw in fields {
        let (key, val) = raw
            .split_once('=')
            .ok_or_else(|| format!("invalid field `{raw}` (expected key=value)"))?;
        if RESERVED_FIELD_KEYS.contains(&key) {
            return Err(format!("field name `{key}` is reserved and can't be used").into());
        }
        let value =
            serde_json::from_str::<Value>(val).unwrap_or_else(|_| Value::String(val.to_string()));
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

/// Idempotent: reuse an existing store if one is discoverable from the current
/// directory (e.g. a fresh clone), otherwise create one here. Either way, the
/// git merge driver is (re)registered, so re-running `ta init` is how a clone
/// installs the driver into its local config.
fn cmd_init() -> Result<(), DynError> {
    // Resolve the store directory: reuse an existing one (so re-running from
    // anywhere in the repo is idempotent), else create one in the current dir.
    let base_dir = if let Ok(existing) = FileStore::discover() {
        println!(
            "taska store already present at {}",
            existing.base_dir.display()
        );
        existing.base_dir
    } else {
        let dir = std::env::current_dir()?.join(".taska");
        println!("Initialized taska store at {}", dir.display());
        dir
    };

    // Provision honors the (possibly user-edited) config, creating any newly
    // configured log files — this is what makes re-running `ta init` the way to
    // apply a change to the `[store]` paths.
    let store = FileStore::provision(base_dir)?;
    let repo_root = store
        .repo_root()
        .ok_or("could not determine repository root from the .taska directory")?;
    git::setup(repo_root)?;
    Ok(())
}

fn cmd_create(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let payload = parse_fields(fields)?;
    store.append_events(&[MutationEvent::new(OpType::Create, id, payload)])?;
    println!("Created task `{id}`");
    Ok(())
}

fn cmd_update(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let payload = parse_fields(fields)?;
    store.append_events(&[MutationEvent::new(OpType::Update, id, payload)])?;
    println!("Updated task `{id}`");
    Ok(())
}

fn cmd_dep(
    store: &impl EventStore,
    task_id: &str,
    depends_on: &str,
    op: OpType,
) -> Result<(), DynError> {
    let mut payload = Map::new();
    payload.insert("dep".to_string(), Value::String(depends_on.to_string()));
    let is_add = matches!(op, OpType::AddDep);
    store.append_events(&[MutationEvent::new(op, task_id, payload)])?;
    if is_add {
        println!("`{task_id}` now depends on `{depends_on}`");
    } else {
        println!("`{task_id}` no longer depends on `{depends_on}`");
    }
    Ok(())
}

fn cmd_delete(store: &impl EventStore, id: &str) -> Result<(), DynError> {
    store.append_events(&[MutationEvent::new(OpType::Delete, id, Map::new())])?;
    println!("Deleted task `{id}`");
    Ok(())
}

fn cmd_list(
    store: &impl EventStore,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    print_tasks(state.values().collect(), display, cfg, "(no tasks)");
    Ok(())
}

fn cmd_search(
    store: &impl EventStore,
    criteria: &[String],
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    // Compile (and validate regexes) up front so a bad criterion errors before
    // we touch the store.
    let criteria = compile_criteria(criteria)?;
    let state = state_of(store)?;
    let hits: Vec<&TaskState> = state
        .values()
        .filter(|t| criteria.iter().all(|c| c.matches(t)))
        .collect();
    print_tasks(hits, display, cfg, "(no matches)");
    Ok(())
}

/// A search operator. `=`/`!=` compare the field's value against a JSON-coerced
/// query; `~`/`!~` match a regex against the field's string form.
#[derive(Clone, Copy)]
enum SearchOp {
    Eq,
    Ne,
    Re,
    NotRe,
}

/// One parsed, compiled search criterion: a field plus how to test it.
struct Criterion {
    field: String,
    matcher: Matcher,
}

enum Matcher {
    Eq(Value),
    Ne(Value),
    Re(regex::Regex),
    NotRe(regex::Regex),
}

impl Criterion {
    /// Whether `task` satisfies this criterion. A field offers zero or more
    /// candidate values (a custom field is absent→none; `deps` is one per edge);
    /// equality/regex pass if ANY candidate matches. The negated forms are the
    /// logical NOT, so they also hold when the field is absent.
    fn matches(&self, task: &TaskState) -> bool {
        let values = field_values(task, &self.field);
        match &self.matcher {
            Matcher::Eq(q) => values.iter().any(|v| v == q),
            Matcher::Ne(q) => !values.iter().any(|v| v == q),
            Matcher::Re(re) => values.iter().any(|v| re.is_match(&value_string(v))),
            Matcher::NotRe(re) => !values.iter().any(|v| re.is_match(&value_string(v))),
        }
    }
}

/// The JSON value(s) a field offers for matching: the `id`, each dependency
/// (`deps`), or a single custom field (empty when the task lacks it). Unifying
/// the three lets every operator treat built-ins and custom fields alike.
fn field_values(task: &TaskState, field: &str) -> Vec<Value> {
    match field {
        "id" => vec![Value::String(task.id.clone())],
        "deps" => task
            .depends_on
            .iter()
            .map(|d| Value::String(d.clone()))
            .collect(),
        _ => task.custom_fields.get(field).cloned().into_iter().collect(),
    }
}

/// A JSON value's string form for regex matching: the raw string for a JSON
/// string, else its compact JSON (so `priority~^3$` can match the number 3).
fn value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn compile_criteria(raw: &[String]) -> Result<Vec<Criterion>, DynError> {
    raw.iter().map(|r| compile_criterion(r)).collect()
}

fn compile_criterion(raw: &str) -> Result<Criterion, DynError> {
    let (field, op, value) = split_criterion(raw)?;
    let matcher = match op {
        SearchOp::Eq => Matcher::Eq(json_or_string(value)),
        SearchOp::Ne => Matcher::Ne(json_or_string(value)),
        SearchOp::Re => Matcher::Re(compile_regex(value)?),
        SearchOp::NotRe => Matcher::NotRe(compile_regex(value)?),
    };
    Ok(Criterion {
        field: field.to_string(),
        matcher,
    })
}

/// Split `field<op>value` at its FIRST operator, so an operator character inside
/// the value (e.g. a regex `~`) doesn't fool the parser. `!` is only an operator
/// when followed by `=` or `~`.
fn split_criterion(raw: &str) -> Result<(&str, SearchOp, &str), DynError> {
    let bytes = raw.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        let (op, len) = match c {
            b'=' => (SearchOp::Eq, 1),
            b'~' => (SearchOp::Re, 1),
            b'!' => match bytes.get(i + 1) {
                Some(b'=') => (SearchOp::Ne, 2),
                Some(b'~') => (SearchOp::NotRe, 2),
                _ => continue,
            },
            _ => continue,
        };
        if i == 0 {
            return Err(format!("invalid criterion `{raw}`: empty field name").into());
        }
        return Ok((&raw[..i], op, &raw[i + len..]));
    }
    Err(format!(
        "invalid criterion `{raw}`: expected field=value, field~regex, field!=value, or field!~regex"
    )
    .into())
}

/// Coerce a query string as JSON, falling back to a plain string — the same
/// coercion `create`/`update` apply, so `priority=3` matches the number 3.
fn json_or_string(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn compile_regex(pattern: &str) -> Result<regex::Regex, DynError> {
    regex::Regex::new(pattern).map_err(|e| format!("invalid regex `{pattern}`: {e}").into())
}

/// Show a single task by id, defaulting to ALL of its fields (unlike `list`,
/// which uses the configured columns). With no `--columns`, the columns are
/// `id` + that task's own custom-field keys (sorted) + `deps`; an explicit
/// `--columns` still restricts. `--full` doesn't change *which* columns appear
/// here (the default is already the complete set), but it still disables
/// truncation as it does everywhere. Renders via the same human/json path as
/// `list` (json is a one-element array, as in list).
fn cmd_show(
    store: &impl EventStore,
    id: &str,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let task = state.get(id).ok_or_else(|| format!("no task `{id}`"))?;
    let tasks = [task];
    // Default to the full task: every field of this one task. An explicit
    // `--columns` overrides; either way the shared `render_rows` dispatch prints.
    let columns = display
        .columns
        .clone()
        .unwrap_or_else(|| full_columns(&tasks, cfg));
    println!("{}", render_rows(&tasks, &columns, display, cfg));
    Ok(())
}

fn cmd_ready(
    store: &impl EventStore,
    workflow: &WorkflowConfig,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let ready = graph::ready_tasks(&state, &workflow.status_field, &workflow.done_status)?;
    let tasks: Vec<&TaskState> = ready.iter().filter_map(|id| state.get(id)).collect();
    // ready_tasks returns a topological order, but ready tasks never depend on
    // one another (their deps are all done, hence excluded), so re-sorting in
    // print_tasks is free of ordering hazards and gives a consistent order.
    print_tasks(tasks, display, cfg, "(nothing ready)");
    Ok(())
}

/// Aggregate counts for `ta status`.
///
/// Status values are user-defined, so the per-status buckets are *discovered*
/// from the data rather than hardcoded — `done_status` is simply the bucket that
/// also feeds the `closed` count. `blocked` and `ready` are COMPUTED from the
/// dependency graph, never read from a status value: `ready` reuses the same set
/// as `ta ready`, and among not-done tasks the two partition the set (a not-done
/// task is blocked iff an existing dependency isn't done, else ready).
struct StatusSummary {
    total: usize,
    by_status: BTreeMap<String, usize>,
    no_status: usize,
    ready: usize,
    blocked: usize,
    closed: usize,
}

fn status_summary(
    state: &HashMap<String, TaskState>,
    workflow: &WorkflowConfig,
) -> Result<StatusSummary, DynError> {
    let (field, done) = (&workflow.status_field, &workflow.done_status);
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut no_status = 0usize;
    for t in state.values() {
        match t.custom_fields.get(field) {
            Some(Value::String(s)) => *by_status.entry(s.clone()).or_default() += 1,
            // A non-string status still groups, keyed by its compact JSON form.
            Some(v) => {
                *by_status
                    .entry(serde_json::to_string(v).unwrap_or_default())
                    .or_default() += 1;
            }
            None => no_status += 1,
        }
    }
    let ready = graph::ready_tasks(state, field, done)?.len();
    let closed = state.values().filter(|t| is_done(t, field, done)).count();
    let blocked = state
        .values()
        .filter(|t| {
            !is_done(t, field, done)
                && t.depends_on
                    .iter()
                    .any(|d| state.get(d).is_some_and(|dep| !is_done(dep, field, done)))
        })
        .count();
    Ok(StatusSummary {
        total: state.len(),
        by_status,
        no_status,
        ready,
        blocked,
        closed,
    })
}

fn cmd_status(
    store: &impl EventStore,
    workflow: &WorkflowConfig,
    format: OutputFormat,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let summary = status_summary(&state, workflow)?;
    let out = match format {
        // The summary is a single object, so json and jsonl render identically.
        OutputFormat::Json | OutputFormat::Jsonl => render_status_json(&summary),
        OutputFormat::Human => render_status_human(&summary),
    };
    println!("{out}");
    Ok(())
}

/// Human summary: an aligned `Total`, a per-status block (sorted, with an
/// `(unset)` bucket last), then the computed `Ready`/`Blocked`/`Closed` lines.
fn render_status_human(s: &StatusSummary) -> String {
    // Per-status rows, indented; the no-status bucket sorts last under `(unset)`.
    let mut status_rows: Vec<(String, usize)> = s
        .by_status
        .iter()
        .map(|(k, v)| (format!("  {k}"), *v))
        .collect();
    if s.no_status > 0 {
        status_rows.push(("  (unset)".to_string(), s.no_status));
    }

    // Width over every numeric row so labels and counts line up in one table.
    let summary_rows = [
        ("Ready", s.ready),
        ("Blocked", s.blocked),
        ("Closed", s.closed),
    ];
    let label_w = status_rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .chain(std::iter::once("Total".len()))
        .chain(summary_rows.iter().map(|(l, _)| l.len()))
        .max()
        .unwrap_or(0);
    let count_w = status_rows
        .iter()
        .map(|(_, c)| *c)
        .chain(std::iter::once(s.total))
        .chain(summary_rows.iter().map(|(_, c)| *c))
        .map(|c| c.to_string().len())
        .max()
        .unwrap_or(1);
    let row = |label: &str, count: usize| format!("{label:<label_w$}  {count:>count_w$}");

    let mut lines = vec![
        row("Total", s.total),
        String::new(),
        "By status:".to_string(),
    ];
    lines.extend(status_rows.iter().map(|(label, count)| row(label, *count)));
    lines.push(String::new());
    lines.extend(summary_rows.iter().map(|(label, count)| row(label, *count)));
    lines.join("\n")
}

/// Machine-readable summary as a single compact JSON object, keys in a fixed
/// order so the output is stable for scripting.
fn render_status_json(s: &StatusSummary) -> String {
    let by_status: Vec<String> = s
        .by_status
        .iter()
        .map(|(k, v)| format!("{}:{v}", serde_json::to_string(k).unwrap_or_default()))
        .collect();
    format!(
        "{{\"total\":{},\"by_status\":{{{}}},\"no_status\":{},\"ready\":{},\"blocked\":{},\"closed\":{}}}",
        s.total,
        by_status.join(","),
        s.no_status,
        s.ready,
        s.blocked,
        s.closed
    )
}

fn cmd_compact(
    store: &impl EventStore,
    cfg: &CompactionConfig,
    now: DateTime<Utc>,
) -> Result<(), DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;

    let split = Engine::retention_split(&mutations, cfg.keep_events, cfg.keep_days, now);
    if split == 0 {
        println!(
            "Nothing to compact ({} event(s) in log, keep_events = {})",
            mutations.len(),
            cfg.keep_events
        );
        return Ok(());
    }

    // Fold the old prefix into the baseline; retain the recent suffix in the log
    // so divergent branches can still be reconciled by event id. `replay` uses the
    // store's workflow config so the folded baseline carries computed timestamps.
    let (to_fold, to_keep) = mutations.split_at(split);
    let folded = replay(store, baseline, to_fold.to_vec());
    let mut new_baseline: Vec<TaskState> = folded.into_values().collect();
    new_baseline.sort_by(|a, b| a.id.cmp(&b.id));

    store.compact(&new_baseline, to_keep)?;
    println!(
        "Compacted {} event(s) into baseline ({} task(s)); kept {} recent event(s)",
        split,
        new_baseline.len(),
        to_keep.len()
    );
    Ok(())
}

/// Dispatch `ta config get|set|list`.
fn cmd_config(store: &FileStore, action: ConfigAction) -> Result<(), DynError> {
    match action {
        ConfigAction::Get { key } => cmd_config_get(store.config(), &key),
        ConfigAction::List => cmd_config_list(store.config()),
        ConfigAction::Set { key, value } => cmd_config_set(store, &key, &value),
    }
}

/// Print one effective config value addressed by a dotted key. Reads the merged
/// config (file values over defaults), so a key absent from the file still
/// resolves to its default.
fn cmd_config_get(cfg: &Config, key: &str) -> Result<(), DynError> {
    let root = toml::Value::try_from(cfg)?;
    let mut cur = &root;
    for part in key.split('.') {
        cur = cur
            .get(part)
            .ok_or_else(|| format!("no config key `{key}`"))?;
    }
    println!("{}", show_config_value(cur));
    Ok(())
}

/// Print every effective config value as sorted `dotted.key = value` lines.
fn cmd_config_list(cfg: &Config) -> Result<(), DynError> {
    let root = toml::Value::try_from(cfg)?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    flatten_config("", &root, &mut pairs);
    pairs.sort();
    for (k, v) in pairs {
        println!("{k} = {v}");
    }
    Ok(())
}

/// Set one config value, preserving the file's comments, then validate the
/// result before writing — so an invalid edit (unknown key, bad type or enum,
/// `keep_events` below the floor) is rejected and the file left untouched.
fn cmd_config_set(store: &FileStore, key: &str, raw: &str) -> Result<(), DynError> {
    let path = store.base_dir.join("config.toml");
    // Edit the existing file, or seed from the documented template when absent,
    // so even a first `set` on a fresh store yields a fully-commented config.
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::config::default_toml(),
        Err(e) => return Err(e.into()),
    };
    let mut doc = existing.parse::<toml_edit::DocumentMut>()?;
    let value = parse_config_value(raw);
    let shown = value.to_string().trim().to_string();
    set_dotted(&mut doc, key, value)?;

    // Reject the change unless the whole document still deserializes to a valid
    // Config (catches bad types / unknown enum variants) AND passes validate()
    // (catches semantic limits like the keep_events floor).
    let candidate: Config = toml::from_str(&doc.to_string())?;
    candidate.validate()?;

    // Guard against typo'd keys: serde(default) silently drops an unknown field,
    // so the value must survive a load round-trip to confirm the key is real.
    let normalized = toml::Value::try_from(&candidate)?;
    let mut cur = &normalized;
    for part in key.split('.') {
        cur = cur.get(part).ok_or_else(|| {
            format!("unknown config key `{key}` (no such field; nothing was changed)")
        })?;
    }

    std::fs::write(&path, doc.to_string())?;
    println!("Set {key} = {shown}");
    Ok(())
}

/// Render a leaf config value git-config-style: bare for strings, TOML form
/// (numbers, bools, arrays) otherwise.
fn show_config_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Flatten a TOML tree into sorted-friendly `dotted.key`/value pairs, recursing
/// through tables so nested sub-tables (e.g. `display.column_max_width.*`) show.
fn flatten_config(prefix: &str, v: &toml::Value, out: &mut Vec<(String, String)>) {
    if let toml::Value::Table(table) = v {
        for (k, val) in table {
            let key = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            flatten_config(&key, val, out);
        }
    } else {
        out.push((prefix.to_string(), show_config_value(v)));
    }
}

/// Coerce a CLI string into a TOML value using TOML's own value grammar (so
/// `100` becomes an integer, `true` a bool, `["a","b"]` an array, `"x"` a
/// string). A bare word that isn't valid TOML — e.g. `open` — falls back to a
/// string, matching how `create`/`update` coerce field values.
fn parse_config_value(raw: &str) -> toml_edit::Value {
    format!("__x__ = {raw}")
        .parse::<toml_edit::DocumentMut>()
        .ok()
        .and_then(|doc| {
            doc.get("__x__")
                .and_then(toml_edit::Item::as_value)
                .cloned()
        })
        .unwrap_or_else(|| toml_edit::Value::from(raw.to_string()))
}

/// Set a dotted key in a `toml_edit` document, creating intermediate tables as
/// needed. Editing in place preserves the surrounding comments and formatting,
/// which is the whole reason `set` uses `toml_edit` rather than re-serializing.
fn set_dotted(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: toml_edit::Value,
) -> Result<(), DynError> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("invalid config key `{key}`").into());
    }
    let (last, parents) = parts
        .split_last()
        .ok_or_else(|| format!("invalid config key `{key}`"))?;
    let mut table = doc.as_table_mut();
    for &parent in parents {
        let item = table
            .entry(parent)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        table = item
            .as_table_mut()
            .ok_or_else(|| format!("config key `{parent}` is not a table"))?;
    }
    table[*last] = toml_edit::Item::Value(value);
    Ok(())
}

/// Undo the last `count` event(s) in the log.
///
/// Two paths, chosen by whether any undone event is already git-committed:
/// - All undone events are still local (uncommitted), or `--remove` is set:
///   truncate the log's tail. This is safe for local events because they were
///   never shared; `--remove` extends it to committed events at the cost of
///   rewriting shared history (it prints a loud warning).
/// - Some undone events are already committed (the default for that case): keep
///   committed history intact and instead *append* compensating events that
///   transform the current state back to the target (pre-undo prefix) state.
///   Staying append-only avoids cross-branch seq collisions on merge.
///
/// Only events still in the log can be undone; anything folded into the baseline
/// by compaction is out of reach.
fn cmd_undo(store: &FileStore, count: usize, force: bool, remove: bool) -> Result<(), DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    let n = mutations.len();
    if count == 0 || n == 0 {
        println!("Nothing to undo.");
        return Ok(());
    }

    let count = count.min(n);
    let keep = n - count;
    let undone = &mutations[keep..];

    let current = replay(store, baseline.clone(), mutations.clone());
    let target = replay(store, baseline.clone(), mutations[..keep].to_vec());

    // The tasks any undone event touched, sorted for stable output.
    let mut affected: Vec<String> = undone.iter().map(|e| e.task_id.clone()).collect();
    affected.sort();
    affected.dedup();

    // PREVIEW: name each undone event, then show each affected task's before/after.
    println!("Undoing {count} event(s):");
    for event in undone {
        println!("  seq {}: {:?} `{}`", event.seq, event.op, event.task_id);
    }
    for id in &affected {
        println!(
            "  - {id}: {} -> {}",
            describe(current.get(id)),
            describe(target.get(id))
        );
    }

    // How many of the log's events are already committed to git. If the file was
    // never committed (or there is no HEAD yet), nothing is committed.
    let committed_count = committed_mutation_count(store);
    let any_committed = keep < committed_count;

    if !confirm("Apply this undo?", force)? {
        println!("Aborted; nothing changed.");
        return Ok(());
    }

    if remove || !any_committed {
        // Truncate the tail. Safe for local events; for committed ones `--remove`
        // rewrites shared history, so warn loudly.
        store.replace_mutations(&mutations[..keep])?;
        if remove && any_committed {
            eprintln!(
                "DANGER: --remove deleted committed event(s), rewriting shared history. \
                 Other branches will see a removal on merge; only do this if you are sure \
                 the removed events were never pushed or pulled elsewhere."
            );
        }
    } else {
        // Default committed path: keep committed history, append compensating
        // events. Build them from the committed prefix's state toward the target.
        let truncate_to = committed_count;
        let post = replay(store, baseline, mutations[..truncate_to].to_vec());
        let comps = compensate(&post, &target, &affected);

        // Continue the seq sequence past the highest committed seq we keep.
        let next = mutations[..truncate_to]
            .iter()
            .map(|e| e.seq)
            .max()
            .map_or(1, |m| m + 1);
        let mut new_log = mutations[..truncate_to].to_vec();
        for (seq, mut comp) in (next..).zip(comps) {
            comp.seq = seq;
            new_log.push(comp);
        }
        store.replace_mutations(&new_log)?;
    }

    println!("Undone.");
    Ok(())
}

/// Count non-empty lines in the git-committed `mutations.jsonl` (`HEAD:` blob).
/// Returns 0 when the file is not committed yet or there is no `HEAD`, which the
/// caller treats as "nothing committed", so every event is safe to truncate.
fn committed_mutation_count(store: &FileStore) -> usize {
    let Some(repo_root) = store.repo_root() else {
        return 0;
    };
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", "HEAD:.taska/mutations.jsonl"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        _ => 0,
    }
}

/// Render a task's salient state for the before/after preview: `(absent)` for a
/// missing task, else the JSON of its custom fields plus `deps={:?}` when it has
/// any. Mirrors the field-centric framing of the other task views.
fn describe(task: Option<&TaskState>) -> String {
    task.map_or_else(
        || "(absent)".to_string(),
        |t| {
            let fields =
                serde_json::to_string(&t.custom_fields).unwrap_or_else(|_| "{}".to_string());
            if t.depends_on.is_empty() {
                fields
            } else {
                format!("{fields} deps={:?}", t.depends_on)
            }
        },
    )
}

/// Produce DRAFT events (seq 0; the caller assigns real seqs) that transform the
/// `from` state into the `to` state for the `affected` tasks, using the existing
/// op vocabulary plus the null-unset convention:
///
/// - in `from` but not `to` -> `Delete`.
/// - in `to` but not `from` -> `Create` carrying its fields, then one `AddDep`
///   per dependency.
/// - in both -> a single `Update` that sets each field whose value differs and
///   unsets (sets to null) each field present in `from` but gone in `to`; skipped
///   when that payload is empty. Then `AddDep`/`RemoveDep` to reconcile deps.
/// - in neither -> nothing.
fn compensate(
    from: &HashMap<String, TaskState>,
    to: &HashMap<String, TaskState>,
    affected: &[String],
) -> Vec<MutationEvent> {
    let mut events = Vec::new();
    for id in affected {
        match (from.get(id), to.get(id)) {
            (Some(_), None) => {
                events.push(MutationEvent::new(OpType::Delete, id.clone(), Map::new()));
            }
            (None, Some(t)) => {
                events.push(MutationEvent::new(
                    OpType::Create,
                    id.clone(),
                    t.custom_fields.clone(),
                ));
                for dep in &t.depends_on {
                    events.push(dep_event(OpType::AddDep, id, dep));
                }
            }
            (Some(f), Some(t)) => {
                let mut payload = Map::new();
                // Set every field that differs (present-and-changed or newly added).
                for (k, v) in &t.custom_fields {
                    if f.custom_fields.get(k) != Some(v) {
                        payload.insert(k.clone(), v.clone());
                    }
                }
                // Unset (null) every field that existed in `from` but not in `to`.
                for k in f.custom_fields.keys() {
                    if !t.custom_fields.contains_key(k) {
                        payload.insert(k.clone(), Value::Null);
                    }
                }
                if !payload.is_empty() {
                    events.push(MutationEvent::new(OpType::Update, id.clone(), payload));
                }
                for dep in &t.depends_on {
                    if !f.depends_on.contains(dep) {
                        events.push(dep_event(OpType::AddDep, id, dep));
                    }
                }
                for dep in &f.depends_on {
                    if !t.depends_on.contains(dep) {
                        events.push(dep_event(OpType::RemoveDep, id, dep));
                    }
                }
            }
            (None, None) => {}
        }
    }
    events
}

/// A dependency draft event with the `{ "dep": <id> }` payload shape that
/// `cmd_dep` and the engine's `AddDep`/`RemoveDep` replay expect.
fn dep_event(op: OpType, task_id: &str, dep: &str) -> MutationEvent {
    let mut payload = Map::new();
    payload.insert("dep".to_string(), Value::String(dep.to_string()));
    MutationEvent::new(op, task_id, payload)
}

/// Clean up after a merge or a divergent history: report and clear a surfaced
/// merge conflict, and drop any orphaned events the log has accumulated.
///
/// The deterministic merge is already written to the log by the driver, so the
/// conflict step only acknowledges the conflicts and removes the marker; per-field
/// resolution is future work. The orphan step prunes events that apply to nothing
/// (a dropped `Create` left their target missing). Dropping a no-op event is
/// state-neutral, so it needs no confirmation. With neither a marker nor an
/// orphan, there is nothing to do.
fn cmd_resolve(store: &FileStore, force: bool) -> Result<(), DynError> {
    let cleared_marker = resolve_merge_marker(store)?;
    let dropped_orphans = resolve_orphans(store, force)?;
    if !cleared_marker && dropped_orphans == 0 {
        println!("Nothing to resolve (no merge conflicts and no orphaned events).");
    }
    Ok(())
}

/// Report and clear a surfaced merge-conflict marker, if present. Returns whether
/// a marker was found and cleared.
fn resolve_merge_marker(store: &FileStore) -> Result<bool, DynError> {
    let marker = store.base_dir.join("merge-conflict.json");
    if !marker.exists() {
        return Ok(false);
    }

    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&marker)?)?;
    let conflicts = doc.get("conflicts").and_then(|c| c.as_array());
    match conflicts {
        Some(items) if !items.is_empty() => {
            println!(
                "{} field conflict(s) were merged tentatively (keeping ours):",
                items.len()
            );
            for item in items {
                let task = item.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
                let field = item.get("field").and_then(|v| v.as_str());
                let ours = item.get("ours").cloned().unwrap_or(Value::Null);
                let theirs = item.get("theirs").cloned().unwrap_or(Value::Null);
                let kept = item.get("kept").and_then(|v| v.as_str()).unwrap_or("ours");
                match field {
                    Some(f) => {
                        println!("  - `{task}`.{f}: ours={ours} / theirs={theirs} -> kept {kept}");
                    }
                    None => println!(
                        "  - `{task}` (whole task): ours={ours} / theirs={theirs} -> kept {kept}"
                    ),
                }
            }
            println!(
                "\nThe tentative merge is already written to the log. To accept it, `git add` \
                 the files and commit; to pick differently, edit the log or re-merge with a \
                 different `on_conflict` strategy."
            );
        }
        _ => println!("Merge marker present but lists no conflicts."),
    }

    std::fs::remove_file(&marker)?;
    Ok(true)
}

/// Prune orphaned events — those that apply to nothing during replay — from the
/// log, rewriting it without them. Returns how many were dropped. Because an
/// orphan is by definition a no-op, removing it can't change materialized state.
fn resolve_orphans(store: &FileStore, force: bool) -> Result<usize, DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    let (_, orphans) = replay_report(store, baseline, mutations.clone());
    if orphans.is_empty() {
        return Ok(0);
    }

    let drop: std::collections::HashSet<u64> = orphans.iter().copied().collect();
    // Verbose: name every event that would be dropped before touching the log.
    println!(
        "{} orphaned event(s) apply to no existing task and would be dropped:",
        orphans.len()
    );
    for event in mutations.iter().filter(|e| drop.contains(&e.seq)) {
        println!("  - seq {}: {:?} `{}`", event.seq, event.op, event.task_id);
    }
    if !confirm("Drop these orphaned events from the log?", force)? {
        println!("Aborted; the log is unchanged.");
        return Ok(0);
    }

    let kept: Vec<MutationEvent> = mutations
        .into_iter()
        .filter(|e| !drop.contains(&e.seq))
        .collect();
    store.replace_mutations(&kept)?;
    println!("Dropped {} orphaned event(s) from the log.", orphans.len());
    Ok(orphans.len())
}

/// Ask the user to confirm a destructive action. `force` (from `--force`) skips
/// the prompt. The prompt goes to stderr so stdout stays clean for piping; reads
/// a `y/N` line from stdin and defaults to no.
fn confirm(prompt: &str, force: bool) -> Result<bool, DynError> {
    if force {
        return Ok(true);
    }
    eprint!("{prompt} [y/N] ");
    std::io::Write::flush(&mut std::io::stderr())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Render tasks per the display args. The selected columns (`--columns`/`--full`/
/// config) decide *which* fields appear; `--format` decides only how they print,
/// and both formats share the same field order.
fn render(tasks: &[&TaskState], display: &DisplayArgs, cfg: &DisplayConfig, empty: &str) -> String {
    // Only the human table needs an explicit empty placeholder; json/jsonl render
    // their own empty forms (`[]` / no lines).
    if display.format == OutputFormat::Human && tasks.is_empty() {
        return empty.to_string();
    }
    let columns = resolve_columns(display, cfg, tasks);
    render_rows(tasks, &columns, display, cfg)
}

/// Sort a collected task set by the display args and print it, with `empty` as
/// the human placeholder for no rows. The shared tail of `list`/`search`/`ready`,
/// each of which differs only in how it gathers the tasks.
fn print_tasks(
    mut tasks: Vec<&TaskState>,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
    empty: &str,
) {
    sort_tasks(&mut tasks, display, cfg);
    println!("{}", render(&tasks, display, cfg, empty));
}

/// Dispatch the chosen `--format` over an already-resolved column set. Shared by
/// the multi-row `render` path and single-task `show`, so a new output format is
/// wired in exactly one place.
fn render_rows(
    tasks: &[&TaskState],
    columns: &[String],
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> String {
    match display.format {
        OutputFormat::Json => render_json(tasks, columns),
        OutputFormat::Jsonl => render_jsonl(tasks, columns),
        OutputFormat::Human => {
            render_human(tasks, columns, &truncation_caps(columns, display, cfg))
        }
    }
}

/// Sort `tasks` in place by the effective sort column (`--sort`, else the
/// configured default), ascending, with `id` as a stable tiebreaker; `--reverse`
/// flips the result. The column may be `id`, `deps`, or any field (including the
/// injected computed timestamps). Rows lacking the column sort last (ascending);
/// an empty or unknown column leaves only the `id` tiebreak, i.e. orders by id.
fn sort_tasks(tasks: &mut [&TaskState], display: &DisplayArgs, cfg: &DisplayConfig) {
    let column = display.sort.as_deref().unwrap_or(cfg.sort.as_str());
    tasks.sort_by(|a, b| {
        let ord = match (cell_value(a, column), cell_value(b, column)) {
            (Some(x), Some(y)) => cmp_json(&x, &y),
            (Some(_), None) => Ordering::Less, // a present value sorts before a missing one
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        ord.then_with(|| a.id.cmp(&b.id))
    });
    if display.reverse {
        tasks.reverse();
    }
}

/// The value of `column` for a task as a JSON `Value` — the single source of
/// truth shared by JSON output, human rendering, and sorting. `id` is the id
/// string, `deps` the array of dependency ids, and anything else a custom or
/// computed field. `None` only for a missing custom field (the built-ins always
/// resolve), which is how JSON omits absent fields and sorting orders them last.
fn cell_value(task: &TaskState, column: &str) -> Option<Value> {
    match column {
        "id" => Some(Value::String(task.id.clone())),
        "deps" => Some(Value::Array(
            task.depends_on.iter().cloned().map(Value::String).collect(),
        )),
        _ => task.custom_fields.get(column).cloned(),
    }
}

/// A total order over heterogeneous JSON scalars: numbers compare numerically,
/// strings/bools by their natural order, and any mismatch falls back to a stable
/// per-type rank then the value's string form — so a column holding mixed types
/// still sorts deterministically.
fn cmp_json(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => value_rank(a)
            .cmp(&value_rank(b))
            .then_with(|| a.to_string().cmp(&b.to_string())),
    }
}

/// Stable per-type ordinal so values of different JSON types compare consistently.
const fn value_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

/// The per-column truncation cap, one entry per column (0 = no limit, which
/// `truncate` already honors). `--full` prints everything untruncated, so every
/// column gets cap 0. Otherwise a column listed in `[display.column_max_width]`
/// uses its own width and the rest fall back to the global `max_width`.
fn truncation_caps(columns: &[String], display: &DisplayArgs, cfg: &DisplayConfig) -> Vec<usize> {
    columns
        .iter()
        .map(|c| {
            if display.full {
                0
            } else {
                cfg.column_max_width
                    .get(c)
                    .copied()
                    .unwrap_or(cfg.max_width)
            }
        })
        .collect()
}

/// Decide the columns: `--full` (the canonical full order), else an explicit
/// `--columns`, else the configured default.
fn resolve_columns(
    display: &DisplayArgs,
    cfg: &DisplayConfig,
    tasks: &[&TaskState],
) -> Vec<String> {
    if display.full {
        full_columns(tasks, cfg)
    } else if let Some(cols) = &display.columns {
        cols.clone()
    } else {
        cfg.columns.clone()
    }
}

/// The canonical column order for an all-fields view (`--full` and `show`'s
/// default): the configured `columns` that are actually present, in their exact
/// configured order — so `deps` keeps its slot — then every other present field
/// sorted alphabetically. The built-ins `id`/`deps` are always covered. A
/// configured column that no task in the view has is dropped, so a single-task
/// `show` and `--full` never pad with empty columns. Both human and JSON
/// rendering consume this same order, so their columns match.
fn full_columns(tasks: &[&TaskState], cfg: &DisplayConfig) -> Vec<String> {
    // Every field present across the view, plus the always-shown built-ins.
    let mut present: BTreeSet<&str> = BTreeSet::from(["id", "deps"]);
    for t in tasks {
        present.extend(t.custom_fields.keys().map(String::as_str));
    }
    // Configured columns that are present, in configured order...
    let mut cols: Vec<String> = cfg
        .columns
        .iter()
        .filter(|c| present.contains(c.as_str()))
        .cloned()
        .collect();
    // ...then the remaining present fields (incl. id/deps if unconfigured),
    // alphabetically (BTreeSet) for a deterministic tail.
    let listed: HashSet<&str> = cols.iter().map(String::as_str).collect();
    let tail: Vec<String> = present
        .into_iter()
        .filter(|f| !listed.contains(f))
        .map(String::from)
        .collect();
    drop(listed);
    cols.extend(tail);
    cols
}

/// Render the aligned human table. `caps[i]` is the truncation width for column
/// `i` (0 = no limit); the caller derives it from config/`--full` per column.
fn render_human(tasks: &[&TaskState], columns: &[String], caps: &[usize]) -> String {
    let headers: Vec<String> = columns.iter().map(|c| c.to_uppercase()).collect();
    let rows: Vec<Vec<String>> = tasks
        .iter()
        .map(|t| {
            columns
                .iter()
                .enumerate()
                .map(|(i, c)| truncate(&human_cell(t, c), caps[i]))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = (0..columns.len())
        .map(|i| {
            let header = headers[i].chars().count();
            let body = rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
            header.max(body)
        })
        .collect();
    let mut lines = vec![format_row(&headers, &widths)];
    lines.extend(rows.iter().map(|r| format_row(r, &widths)));
    lines.join("\n")
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    cells
        .iter()
        .zip(widths)
        .map(|(c, w)| format!("{c:<w$}"))
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

/// Pretty JSON array: one indented object per line, wrapped in `[ ]`.
fn render_json(tasks: &[&TaskState], columns: &[String]) -> String {
    if tasks.is_empty() {
        return "[]".to_string();
    }
    let objects: Vec<String> = tasks
        .iter()
        .map(|t| format!("  {}", json_object(t, columns)))
        .collect();
    format!("[\n{}\n]", objects.join(",\n"))
}

/// Newline-delimited JSON (NDJSON): one compact object per line, no array
/// wrapper — better for streaming, `grep`, and agents. Empty input yields no
/// lines (an empty string).
fn render_jsonl(tasks: &[&TaskState], columns: &[String]) -> String {
    tasks
        .iter()
        .map(|t| json_object(t, columns))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One task as a compact JSON object over `columns`, in order. A column the task
/// lacks is OMITTED rather than emitted as null; only the built-ins `id`/`deps`
/// always resolve (`deps` is `[]` when empty, which is data, not absence).
fn json_object(task: &TaskState, columns: &[String]) -> String {
    let pairs: Vec<String> = columns
        .iter()
        .filter_map(|c| {
            cell_value(task, c).map(|v| {
                let key = serde_json::to_string(c).unwrap_or_default();
                format!("{key}:{}", serde_json::to_string(&v).unwrap_or_default())
            })
        })
        .collect();
    format!("{{{}}}", pairs.join(","))
}

/// A column's value for the human table: a bare string, an array joined by
/// `", "` (so `deps` and any list field read the same), or compact JSON for
/// anything else. Empty for a column the task lacks.
fn human_cell(task: &TaskState, col: &str) -> String {
    cell_value(task, col)
        .as_ref()
        .map(human_display)
        .unwrap_or_default()
}

/// Render a single JSON value for the human table (see [`human_cell`]).
fn human_display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(human_display)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn truncate(s: &str, max_width: usize) -> String {
    if max_width == 0 || s.chars().count() <= max_width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_width.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    //! These tests are the dividend the `EventStore` trait pays out: command
    //! handlers run against an in-memory fake with no disk, locks, or git.
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct InMemoryStore {
        events: RefCell<Vec<MutationEvent>>,
        baseline: RefCell<Vec<TaskState>>,
        config: Config,
    }

    impl EventStore for InMemoryStore {
        fn config(&self) -> &Config {
            &self.config
        }
        fn load_baseline(&self) -> Result<Vec<TaskState>, DynError> {
            Ok(self.baseline.borrow().clone())
        }
        fn load_mutations(&self) -> Result<Vec<MutationEvent>, DynError> {
            Ok(self.events.borrow().clone())
        }
        fn append_events(&self, drafts: &[MutationEvent]) -> Result<(), DynError> {
            let mut events = self.events.borrow_mut();
            let start = events.iter().map(|e| e.seq).max().map_or(1, |m| m + 1);
            for (seq, draft) in (start..).zip(drafts) {
                let mut event = draft.clone();
                event.seq = seq;
                events.push(event);
            }
            Ok(())
        }
        fn compact(
            &self,
            baseline: &[TaskState],
            retained: &[MutationEvent],
        ) -> Result<(), DynError> {
            *self.baseline.borrow_mut() = baseline.to_vec();
            *self.events.borrow_mut() = retained.to_vec();
            Ok(())
        }
        fn replace_mutations(&self, events: &[MutationEvent]) -> Result<(), DynError> {
            *self.events.borrow_mut() = events.to_vec();
            Ok(())
        }
    }

    #[test]
    fn create_then_materialize() {
        let store = InMemoryStore::default();
        cmd_create(&store, "api", &["status=open".into(), "priority=3".into()]).unwrap();
        let state = state_of(&store).unwrap();
        assert_eq!(
            state["api"].custom_fields["status"],
            serde_json::json!("open")
        );
        // `priority=3` is coerced to a JSON number, not a string.
        assert_eq!(state["api"].custom_fields["priority"], serde_json::json!(3));
    }

    #[test]
    fn compact_folds_log_into_baseline() {
        let store = InMemoryStore::default();
        cmd_create(&store, "a", &[]).unwrap();
        cmd_create(&store, "b", &[]).unwrap();
        // keep_events = 0 still retains the most recent event (the log never
        // empties, so the seq watermark stays derivable); the rest folds.
        let cfg = CompactionConfig {
            keep_events: 0,
            keep_days: 0,
        };
        cmd_compact(&store, &cfg, Utc::now()).unwrap();
        assert_eq!(
            store.load_mutations().unwrap().len(),
            1,
            "one event retained"
        );
        assert_eq!(store.load_baseline().unwrap().len(), 1, "the rest folded");
        // Appends still work and overlay the baseline post-compaction.
        cmd_create(&store, "c", &[]).unwrap();
        assert_eq!(state_of(&store).unwrap().len(), 3);
    }

    #[test]
    fn compact_retains_recent_events() {
        let store = InMemoryStore::default();
        for id in ["a", "b", "c", "d", "e"] {
            cmd_create(&store, id, &[]).unwrap();
        }
        // Keep the 2 most recent, time window off.
        let cfg = CompactionConfig {
            keep_events: 2,
            keep_days: 0,
        };
        cmd_compact(&store, &cfg, Utc::now()).unwrap();
        assert_eq!(
            store.load_mutations().unwrap().len(),
            2,
            "kept 2 recent events"
        );
        assert_eq!(
            store.load_baseline().unwrap().len(),
            3,
            "folded 3 into baseline"
        );
        assert_eq!(
            state_of(&store).unwrap().len(),
            5,
            "all tasks still visible"
        );
    }

    #[test]
    fn invalid_field_is_rejected() {
        let store = InMemoryStore::default();
        let err = cmd_create(&store, "x", &["no_equals_sign".into()]);
        assert!(err.is_err());
    }

    /// An in-memory store with the computed-timestamp columns disabled, for
    /// tests asserting exact field/column sets that shouldn't see injected times.
    fn store_without_timestamps() -> InMemoryStore {
        let mut store = InMemoryStore::default();
        store.config.timestamps = crate::config::TimestampConfig {
            create_time: String::new(),
            update_time: String::new(),
            close_time: String::new(),
        };
        store
    }

    #[test]
    fn show_full_columns_cover_every_field_and_unknown_errors() {
        let store = store_without_timestamps();
        cmd_create(&store, "api", &["status=open".into(), "priority=3".into()]).unwrap();
        let state = state_of(&store).unwrap();
        let task = state.get("api").unwrap();

        // `show`'s default columns follow the canonical order, but only over
        // fields the task actually has: the configured columns that are present
        // (id, status, deps — `title` is dropped, this task has none), then any
        // remaining present field alphabetically (priority).
        let cols = full_columns(&[task], &DisplayConfig::default());
        assert_eq!(
            cols,
            ["id", "status", "deps", "priority"],
            "canonical present-only set: {cols:?}"
        );
        let json = render_json(&[task], &cols);
        assert!(json.contains(r#""status":"open""#), "show full: {json}");
        assert!(json.contains(r#""priority":3"#), "show full: {json}");

        // An existing task renders Ok; an unknown id is an error (non-zero exit).
        let d = display(OutputFormat::Human, false, None);
        assert!(cmd_show(&store, "api", &d, &DisplayConfig::default()).is_ok());
        assert!(
            cmd_show(&store, "nope", &d, &DisplayConfig::default()).is_err(),
            "unknown id must error"
        );
    }

    #[test]
    fn canonical_full_order_shared_by_human_and_json() {
        // Configured columns come first in their exact order; remaining fields
        // follow alphabetically. `deps` keeps its configured slot.
        let cfg = DisplayConfig {
            columns: vec!["id".into(), "status".into(), "deps".into()],
            max_width: 0,
            column_max_width: BTreeMap::new(),
            sort: String::new(),
        };
        let t = task(
            "api",
            &["db"],
            &[
                ("zeta", serde_json::json!(1)),
                ("status", serde_json::json!("open")),
                ("alpha", serde_json::json!(2)),
            ],
        );
        let cols = full_columns(&[&t], &cfg);
        assert_eq!(
            cols,
            ["id", "status", "deps", "alpha", "zeta"],
            "configured order then alphabetical extras: {cols:?}"
        );

        // The human header tokens are exactly the columns, in order.
        let full = display(OutputFormat::Human, true, None);
        let human = render_human(&[&t], &cols, &truncation_caps(&cols, &full, &cfg));
        let header: Vec<String> = human
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let expected: Vec<String> = cols.iter().map(|c| c.to_uppercase()).collect();
        assert_eq!(header, expected, "human header follows canonical order");

        // The JSON keys appear in the identical order.
        let json = render_json(&[&t], &cols);
        let mut last = 0;
        for c in &cols {
            let at = json.find(&format!("\"{c}\"")).unwrap();
            assert!(at >= last, "json key `{c}` out of canonical order: {json}");
            last = at;
        }
    }

    #[test]
    fn sort_tasks_orders_by_column_missing_last_and_reverse() {
        let pri3 = task("a", &[], &[("priority", serde_json::json!(3))]);
        let pri1 = task("b", &[], &[("priority", serde_json::json!(1))]);
        let pri2 = task("c", &[], &[("priority", serde_json::json!(2))]);
        let none = task("d", &[], &[]); // no priority -> sorts last (ascending)
        let cfg = DisplayConfig::default();
        let args = |sort: &str, reverse: bool| DisplayArgs {
            format: OutputFormat::Human,
            full: false,
            columns: None,
            sort: Some(sort.to_string()),
            reverse,
        };
        let ids =
            |tasks: &[&TaskState]| -> Vec<String> { tasks.iter().map(|t| t.id.clone()).collect() };

        // Numeric ascending, with the missing-value task last.
        let mut list = vec![&pri3, &pri1, &pri2, &none];
        sort_tasks(&mut list, &args("priority", false), &cfg);
        assert_eq!(ids(&list), ["b", "c", "a", "d"], "asc, missing last");

        // --reverse flips the whole order.
        let mut list = vec![&pri3, &pri1, &pri2, &none];
        sort_tasks(&mut list, &args("priority", true), &cfg);
        assert_eq!(ids(&list), ["d", "a", "c", "b"], "reversed");

        // An unknown column leaves only the id tiebreak (orders by id).
        let mut list = vec![&pri2, &pri3, &pri1];
        sort_tasks(&mut list, &args("nope", false), &cfg);
        assert_eq!(ids(&list), ["a", "b", "c"], "unknown column -> by id");
    }

    #[test]
    fn cell_value_unifies_columns_and_human_joins_arrays() {
        let t = task(
            "api",
            &["db", "web"],
            &[
                ("tags", serde_json::json!(["x", "y"])),
                ("priority", serde_json::json!(3)),
            ],
        );

        // cell_value is the single source of truth: id string, deps array,
        // custom passthrough, and None for a missing field.
        assert_eq!(cell_value(&t, "id"), Some(serde_json::json!("api")));
        assert_eq!(
            cell_value(&t, "deps"),
            Some(serde_json::json!(["db", "web"]))
        );
        assert_eq!(cell_value(&t, "priority"), Some(serde_json::json!(3)));
        assert_eq!(cell_value(&t, "missing"), None);

        // Human cells: bare string, arrays joined so deps and any list field read
        // the same way, numbers as their text, empty for a missing column.
        assert_eq!(human_cell(&t, "id"), "api");
        assert_eq!(human_cell(&t, "deps"), "db, web");
        assert_eq!(
            human_cell(&t, "tags"),
            "x, y",
            "custom arrays join like deps"
        );
        assert_eq!(human_cell(&t, "priority"), "3");
        assert_eq!(human_cell(&t, "missing"), "");
    }

    #[test]
    fn cmp_json_orders_numbers_strings_and_mixed_types() {
        use serde_json::json;
        assert_eq!(
            cmp_json(&json!(2), &json!(10)),
            Ordering::Less,
            "numeric, not lexical"
        );
        assert_eq!(cmp_json(&json!("a"), &json!("b")), Ordering::Less);
        // Mixed types fall back to a stable per-type rank (number < string).
        assert_eq!(cmp_json(&json!(1), &json!("1")), Ordering::Less);
    }

    #[test]
    fn update_without_fields_is_rejected_by_parser() {
        // `ta update <id>` with no field=value args must fail to parse rather
        // than appending a no-op empty Update event.
        let parsed = Cli::try_parse_from(["ta", "update", "api"]);
        assert!(
            parsed.is_err(),
            "update with no fields should be a parse error"
        );
    }

    #[test]
    fn update_with_a_field_parses() {
        let parsed = Cli::try_parse_from(["ta", "update", "api", "status=open"]);
        assert!(parsed.is_ok(), "update with a field should parse");
    }

    #[test]
    fn create_without_fields_still_parses() {
        // `ta create <id>` with no fields remains valid.
        let parsed = Cli::try_parse_from(["ta", "create", "api"]);
        assert!(parsed.is_ok(), "create with no fields should still parse");
    }

    fn task(id: &str, deps: &[&str], fields: &[(&str, Value)]) -> TaskState {
        TaskState {
            id: id.to_string(),
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            custom_fields: fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            create_time: None,
            update_time: None,
            close_time: None,
        }
    }

    fn display(format: OutputFormat, full: bool, columns: Option<&[&str]>) -> DisplayArgs {
        DisplayArgs {
            format,
            full,
            columns: columns.map(|c| c.iter().map(|s| (*s).to_string()).collect()),
            sort: None,
            reverse: false,
        }
    }

    #[test]
    fn human_has_header_and_unquoted_values() {
        let t = task("api", &["db"], &[("status", serde_json::json!("open"))]);
        let d = display(OutputFormat::Human, false, Some(&["id", "status", "deps"]));
        let out = render(&[&t], &d, &DisplayConfig::default(), "(none)");
        assert!(
            out.contains("ID") && out.contains("STATUS"),
            "header: {out}"
        );
        assert!(out.lines().any(|l| l.starts_with("api")), "row: {out}");
        // value is bare `open`, not JSON-quoted, and deps are comma-joined.
        assert!(
            out.contains("open") && !out.contains("\"open\""),
            "unquoted: {out}"
        );
        assert!(out.contains("db"), "deps: {out}");
    }

    #[test]
    fn json_is_array_in_column_order() {
        let item = task(
            "api",
            &[],
            &[
                ("status", serde_json::json!("open")),
                ("priority", serde_json::json!(3)),
            ],
        );
        let args = display(
            OutputFormat::Json,
            false,
            Some(&["id", "priority", "status"]),
        );
        let out = render(&[&item], &args, &DisplayConfig::default(), "(none)");
        assert!(out.trim_start().starts_with('['), "array: {out}");
        let id_at = out.find("\"id\"").unwrap();
        let pri_at = out.find("\"priority\"").unwrap();
        let status_at = out.find("\"status\"").unwrap();
        assert!(
            id_at < pri_at && pri_at < status_at,
            "keys follow column order: {out}"
        );
        assert!(
            out.contains("\"priority\":3"),
            "number stays a number: {out}"
        );
    }

    #[test]
    fn all_unions_fields_but_each_object_omits_absent_ones() {
        let a = task("a", &[], &[("x", serde_json::json!(1))]);
        let b = task("b", &[], &[("y", serde_json::json!(2))]);
        let d = display(OutputFormat::Json, true, None);
        let out = render(&[&a, &b], &d, &DisplayConfig::default(), "(none)");
        // --full unions the column set: both x and y appear across the array.
        assert!(
            out.contains("\"x\"") && out.contains("\"y\""),
            "union: {out}"
        );
        // But an absent field is OMITTED, never emitted as null — no nulls anywhere.
        assert!(
            !out.contains("null"),
            "absent fields omitted, not null: {out}"
        );

        let empty = render(&[], &d, &DisplayConfig::default(), "(none)");
        assert_eq!(empty, "[]", "empty json is []");
    }

    #[test]
    fn jsonl_is_one_object_per_line_omitting_absent_fields() {
        let a = task("a", &["d"], &[("x", serde_json::json!(1))]);
        let b = task("b", &[], &[("y", serde_json::json!(2))]);
        let d = display(OutputFormat::Jsonl, true, None);
        let out = render(&[&a, &b], &d, &DisplayConfig::default(), "(none)");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "one object per line: {out}");
        // Each line is a standalone object (no array brackets), absent keys gone.
        for line in &lines {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(v.is_object(), "each line is a JSON object: {line}");
        }
        assert!(
            lines[0].contains(r#""x":1"#) && !lines[0].contains("\"y\""),
            "a: {}",
            lines[0]
        );
        assert!(
            lines[1].contains(r#""y":2"#) && !lines[1].contains("\"x\""),
            "b: {}",
            lines[1]
        );
        // deps is a built-in: always present, [] when empty (data, not absence).
        assert!(lines[0].contains(r#""deps":["d"]"#) && lines[1].contains(r#""deps":[]"#));

        // Empty input yields no lines.
        assert_eq!(render(&[], &d, &DisplayConfig::default(), "(none)"), "");
    }

    #[test]
    fn truncate_caps_long_values() {
        assert_eq!(truncate("hello", 0), "hello");
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
    }

    #[test]
    fn full_disables_truncation_but_default_and_columns_still_truncate() {
        let long = "a value that is definitely longer than the configured max width";
        let t = task("api", &[], &[("notes", serde_json::json!(long))]);
        let cfg = DisplayConfig {
            columns: vec!["id".into(), "notes".into()],
            max_width: 20,
            column_max_width: BTreeMap::new(),
            sort: String::new(),
        };

        // --full: the full value survives, no ellipsis.
        let full = render(
            &[&t],
            &display(OutputFormat::Human, true, None),
            &cfg,
            "(none)",
        );
        assert!(full.contains(long), "--full prints untruncated: {full}");
        assert!(!full.contains('…'), "--full adds no ellipsis: {full}");

        // Default (config columns) still truncates per max_width.
        let default = render(
            &[&t],
            &display(OutputFormat::Human, false, None),
            &cfg,
            "(none)",
        );
        assert!(!default.contains(long), "default truncates: {default}");
        assert!(default.contains('…'), "default shows ellipsis: {default}");

        // An explicit --columns view also still truncates.
        let cols = render(
            &[&t],
            &display(OutputFormat::Human, false, Some(&["id", "notes"])),
            &cfg,
            "(none)",
        );
        assert!(cols.contains('…'), "--columns still truncates: {cols}");
    }

    #[test]
    fn per_column_max_width_overrides_the_global() {
        // `notes` gets a wide override (60); `summary` falls back to max_width (10).
        let long = "0123456789abcdefghij"; // 20 chars
        let t = task(
            "api",
            &[],
            &[
                ("notes", serde_json::json!(long)),
                ("summary", serde_json::json!(long)),
            ],
        );
        let cfg = DisplayConfig {
            columns: vec!["id".into(), "notes".into(), "summary".into()],
            max_width: 10,
            column_max_width: std::iter::once(("notes".to_string(), 60)).collect(),
            sort: String::new(),
        };
        let out = render(
            &[&t],
            &display(OutputFormat::Human, false, None),
            &cfg,
            "(none)",
        );
        // notes keeps all 20 chars (override 60 > 20, no ellipsis); summary is cut.
        assert!(out.contains(long), "notes column not truncated: {out}");
        assert!(
            out.contains('…'),
            "summary column truncated to max_width: {out}"
        );

        // --full ignores the per-column map entirely: both survive intact.
        let full = render(
            &[&t],
            &display(OutputFormat::Human, true, None),
            &cfg,
            "(none)",
        );
        assert!(!full.contains('…'), "--full disables truncation: {full}");
    }

    fn state(tasks: &[TaskState]) -> HashMap<String, TaskState> {
        tasks.iter().map(|t| (t.id.clone(), t.clone())).collect()
    }

    #[test]
    fn compensate_unsets_a_removed_field_with_null() {
        // `from` has the field, `to` does not: the compensating Update must set
        // the field to JSON null so the engine's unset convention drops it.
        let from = state(&[task("a", &[], &[("owner", serde_json::json!("bob"))])]);
        let to = state(&[task("a", &[], &[])]);
        let events = compensate(&from, &to, &["a".to_string()]);
        assert_eq!(events.len(), 1, "one Update: {events:?}");
        assert_eq!(events[0].op, OpType::Update);
        assert_eq!(
            events[0].payload.get("owner"),
            Some(&Value::Null),
            "removed field unset via null: {:?}",
            events[0].payload
        );
    }

    #[test]
    fn compensate_handles_create_delete_and_field_change() {
        // a: present in `from`, absent in `to` -> Delete.
        // b: absent in `from`, present in `to` with a dep -> Create + AddDep.
        // c: changed field value -> Update with just the changed key.
        let from = state(&[
            task("a", &[], &[("x", serde_json::json!(1))]),
            task("c", &[], &[("status", serde_json::json!("open"))]),
        ]);
        let to = state(&[
            task("b", &["dep1"], &[("y", serde_json::json!(2))]),
            task("c", &[], &[("status", serde_json::json!("closed"))]),
        ]);
        let affected = ["a".to_string(), "b".to_string(), "c".to_string()];
        let events = compensate(&from, &to, &affected);

        // a -> Delete
        let a: Vec<_> = events.iter().filter(|e| e.task_id == "a").collect();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].op, OpType::Delete);

        // b -> Create (carrying y) then AddDep dep1
        let b: Vec<_> = events.iter().filter(|e| e.task_id == "b").collect();
        assert_eq!(b.len(), 2, "create + adddep: {b:?}");
        assert_eq!(b[0].op, OpType::Create);
        assert_eq!(b[0].payload.get("y"), Some(&serde_json::json!(2)));
        assert_eq!(b[1].op, OpType::AddDep);
        assert_eq!(b[1].payload.get("dep"), Some(&serde_json::json!("dep1")));

        // c -> Update setting only the changed status
        let c: Vec<_> = events.iter().filter(|e| e.task_id == "c").collect();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].op, OpType::Update);
        assert_eq!(
            c[0].payload.get("status"),
            Some(&serde_json::json!("closed"))
        );
    }

    #[test]
    fn compensate_reconciles_dependencies() {
        // from depends on x; to depends on y -> RemoveDep x, AddDep y, no Update.
        let from = state(&[task("a", &["x"], &[])]);
        let to = state(&[task("a", &["y"], &[])]);
        let events = compensate(&from, &to, &["a".to_string()]);
        assert!(
            !events.iter().any(|e| e.op == OpType::Update),
            "no field changes -> no Update: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.op == OpType::AddDep
                    && e.payload.get("dep") == Some(&serde_json::json!("y"))),
            "adds y: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.op == OpType::RemoveDep
                && e.payload.get("dep") == Some(&serde_json::json!("x"))),
            "removes x: {events:?}"
        );
    }

    #[test]
    fn search_criteria_compile_and_match() {
        let t = task(
            "api",
            &["db"],
            &[
                ("status", serde_json::json!("open")),
                ("priority", serde_json::json!(3)),
            ],
        );
        let matches = |s: &str| compile_criterion(s).unwrap().matches(&t);

        // Exact (JSON-coerced: number 3, not "3"), regex, negation.
        assert!(matches("status=open"));
        assert!(!matches("status=closed"));
        assert!(matches("priority=3"), "number coercion");
        assert!(matches(r"status~^op"), "regex on string");
        assert!(matches(r"priority~^3$"), "regex on number's string form");
        assert!(matches("status!=closed"));
        assert!(!matches("status!~^op"));

        // Built-in id and deps fields.
        assert!(matches("id=api"));
        assert!(matches("deps=db"));
        assert!(!matches("deps=missing"));

        // A negated criterion also holds when the field is absent entirely.
        assert!(matches("owner!=bob"), "absent field passes !=");
        assert!(matches("owner!~x"), "absent field passes !~");
        assert!(!matches("owner=bob"), "absent field fails =");

        // Parse errors: no operator, empty field, bad regex.
        assert!(compile_criterion("nooperator").is_err());
        assert!(compile_criterion("=value").is_err());
        assert!(compile_criterion("title~[").is_err());

        // The first operator wins, so a regex value may contain operators.
        let (field, _, value) = split_criterion("title~a=b").unwrap();
        assert_eq!((field, value), ("title", "a=b"));
    }

    #[test]
    fn config_value_parsing_coerces_by_toml_grammar() {
        // Integers, bools and arrays parse to their TOML types; a bare word that
        // isn't valid TOML falls back to a string (like create/update coercion).
        assert!(parse_config_value("100").is_integer());
        assert!(parse_config_value("true").is_bool());
        assert!(parse_config_value(r#"["a","b"]"#).is_array());
        assert!(parse_config_value("open").is_str(), "bare word -> string");
        assert_eq!(parse_config_value("open").as_str(), Some("open"));
    }

    #[test]
    fn set_dotted_updates_in_place_and_creates_nested_tables() {
        let mut doc = "[compaction]\n# keep comment\nkeep_events = 1000\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        // Update an existing key: value changes, the comment survives.
        set_dotted(
            &mut doc,
            "compaction.keep_events",
            parse_config_value("200"),
        )
        .unwrap();
        let text = doc.to_string();
        assert!(text.contains("keep_events = 200"), "updated: {text}");
        assert!(text.contains("# keep comment"), "comment preserved: {text}");

        // A brand-new dotted path creates the intermediate table.
        set_dotted(
            &mut doc,
            "display.column_max_width.title",
            parse_config_value("80"),
        )
        .unwrap();
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.display.column_max_width.get("title"), Some(&80));

        // An empty key segment is rejected rather than producing a bogus table.
        assert!(set_dotted(&mut doc, "display..title", parse_config_value("1")).is_err());
    }

    #[test]
    fn config_flatten_and_show_render_git_style() {
        let cfg = Config::default();
        let root = toml::Value::try_from(&cfg).unwrap();
        let mut pairs = Vec::new();
        flatten_config("", &root, &mut pairs);
        pairs.sort();
        // Nested sub-tables flatten to dotted keys; strings render bare.
        assert!(pairs.contains(&("workflow.status_field".to_string(), "status".to_string())));
        assert!(pairs.contains(&("compaction.keep_events".to_string(), "1000".to_string())));
        assert!(pairs.contains(&(
            "display.column_max_width.title".to_string(),
            "80".to_string()
        )));
    }

    #[test]
    fn status_summary_counts_and_partitions_ready_blocked() {
        let workflow = WorkflowConfig::default(); // status / closed
        let tasks = vec![
            task("a", &[], &[("status", serde_json::json!("todo"))]), // ready (no deps)
            task("b", &["a"], &[("status", serde_json::json!("todo"))]), // blocked by a
            task("c", &[], &[("status", serde_json::json!("closed"))]), // done
            task("d", &["c"], &[("status", serde_json::json!("todo"))]), // ready (dep done)
            task("e", &[], &[]),                                      // no status -> ready
        ];
        let st = state(&tasks);
        let s = status_summary(&st, &workflow).unwrap();

        assert_eq!(s.total, 5);
        assert_eq!(s.by_status.get("todo"), Some(&3));
        assert_eq!(s.by_status.get("closed"), Some(&1));
        assert_eq!(s.no_status, 1);
        assert_eq!(s.closed, 1, "one done task");
        assert_eq!(s.ready, 3, "a, d, e");
        assert_eq!(s.blocked, 1, "b");
        // Among not-done tasks, ready and blocked partition the set.
        let not_done = s.total - s.closed;
        assert_eq!(s.ready + s.blocked, not_done);

        // Human output names the sections and a `(unset)` bucket for no-status.
        let human = render_status_human(&s);
        assert!(human.contains("Total"), "human: {human}");
        assert!(human.contains("By status:"), "human: {human}");
        assert!(human.contains("(unset)"), "no-status bucket shown: {human}");
        assert!(
            human.contains("Ready") && human.contains("Blocked"),
            "{human}"
        );

        // JSON output is a single valid object with the computed fields.
        let json = render_status_json(&s);
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total"], 5);
        assert_eq!(parsed["ready"], 3);
        assert_eq!(parsed["blocked"], 1);
        assert_eq!(parsed["closed"], 1);
        assert_eq!(parsed["no_status"], 1);
        assert_eq!(parsed["by_status"]["todo"], 3);
    }

    #[test]
    fn describe_renders_absent_fields_and_deps() {
        assert_eq!(describe(None), "(absent)");
        let t = task("a", &["d1"], &[("status", serde_json::json!("open"))]);
        let out = describe(Some(&t));
        assert!(out.contains(r#""status":"open""#), "fields: {out}");
        assert!(out.contains("deps="), "deps shown: {out}");

        let no_deps = task("b", &[], &[]);
        assert_eq!(describe(Some(&no_deps)), "{}", "empty fields, no deps");
    }
}
