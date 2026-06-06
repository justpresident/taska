//! `ta` command-line surface: argument parsing, dispatch, and shared plumbing.
//!
//! This module owns the clap definitions and `run()`/dispatch. Each subcommand's
//! handler lives in [`commands`]; the cross-cutting helpers handlers reach for —
//! materializing state ([`state_of`]/[`replay`]), parsing `key=value` fields
//! ([`parse_field_ops`]), and confirming destructive actions ([`confirm`]) — live
//! here so the handlers stay thin. Handlers depend on the [`EventStore`]
//! abstraction rather than the concrete [`FileStore`], so they can be exercised
//! against any store.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

use crate::config::{Config, RelationshipDef};
use crate::engine::Engine;
use crate::error::DynError;
use crate::format::{DisplayArgs, OutputArgs};
use crate::merge;
use crate::model::{MutationEvent, OpType, TaskState, DEPENDS_ON, DEP_KEY, DEP_TYPE_KEY};
use crate::storage::{EventStore, FileStore};

mod commands;
use commands::{
    cmd_compact, cmd_config, cmd_create, cmd_delete, cmd_dep_group, cmd_init, cmd_list,
    cmd_resolve, cmd_show, cmd_status, cmd_undo, cmd_update, ConfigAction, DepAction,
};

#[derive(Parser)]
#[command(name = "ta", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a taska repository environment
    Init,
    /// Create a new schema-agnostic task: `ta create <id> [field=value ...]`
    ///
    /// Errors if `<id>` already exists or a field name is reserved/computed
    /// (`id`, `deps`, the timestamp/graph columns, relationship type names).
    Create {
        id: String,
        /// Fields as `key=value` (parsed as JSON when possible). `key=@FILE` reads
        /// the value from a file, `key=@-` from stdin; `key=@@x` is a literal `@x`.
        fields: Vec<String>,
    },
    /// Update a task: `=` sets a field, `+=` appends (e.g. `status=done log+=note`)
    ///
    /// The task must exist; setting a field to its current value is a no-op
    /// (nothing is written), and `+=` is rejected on the single-valued status field.
    Update {
        id: String,
        /// `key=value` sets a field, `key+=value` appends (one entry per line;
        /// concurrent appends merge conflict-free). Values parse as JSON-or-string;
        /// `key=@FILE` / `key=@-` read from a file / stdin. At least one required.
        #[arg(required = true)]
        fields: Vec<String>,
    },
    /// Add/remove typed relationship edges: `ta dep add <task> <type>=<target> …`
    Dep {
        #[command(subcommand)]
        action: DepAction,
    },
    /// Delete a task: `ta delete <id>` (errors if it doesn't exist)
    Delete { id: String },
    /// List tasks, optionally filtered: `ta list status~open priority=3 --open`
    List {
        /// Filter criteria, all of which must match: `field=value` (exact),
        /// `field~regex`, `field!=value`, `field!~regex`. `field` may be a task
        /// field, `id`, or `deps`. With none given, lists every task.
        criteria: Vec<String>,
        /// Only tasks that are not done (status is not the configured done value)
        #[arg(long)]
        open: bool,
        /// Only tasks ready to work on: not done and every dependency done
        #[arg(long)]
        ready: bool,
        #[command(flatten)]
        display: DisplayArgs,
    },
    /// Show a single task in full by id: `ta show <id>`
    Show {
        id: String,
        #[command(flatten)]
        display: DisplayArgs,
    },
    /// Summary counts: total, per-status, blocked, ready, closed
    Status {
        #[command(flatten)]
        output: OutputArgs,
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
        Commands::Create { id, fields } => {
            let workflow = store.config().workflow.clone();
            cmd_create(store, &workflow, &id, &fields)
        }
        Commands::Update { id, fields } => cmd_update(store, &id, &fields),
        Commands::Dep { action } => {
            let types = store.config().relationships.types.clone();
            cmd_dep_group(store, action, &types)
        }
        Commands::Delete { id } => cmd_delete(store, &id),
        Commands::List {
            criteria,
            open,
            ready,
            display,
        } => {
            let workflow = store.config().workflow.clone();
            cmd_list(
                store,
                &criteria,
                open,
                ready,
                &workflow,
                &display,
                &store.config().display,
            )
        }
        Commands::Show { id, display } => cmd_show(store, &id, &display, &store.config().display),
        Commands::Status { output } => {
            let workflow = store.config().workflow.clone();
            cmd_status(store, &workflow, &output)
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
pub(crate) fn replay(
    store: &impl EventStore,
    baseline: Vec<TaskState>,
    mutations: Vec<MutationEvent>,
) -> HashMap<String, TaskState> {
    let w = &store.config().workflow;
    Engine::materialize_state(baseline, mutations, &w.status_field, &w.done_status)
}

/// Like [`replay`] but keeping the orphan report (see [`Engine::materialize_report`]).
pub(crate) fn replay_report(
    store: &impl EventStore,
    baseline: Vec<TaskState>,
    mutations: Vec<MutationEvent>,
) -> (HashMap<String, TaskState>, Vec<u64>) {
    let w = &store.config().workflow;
    Engine::materialize_report(baseline, mutations, &w.status_field, &w.done_status)
}

/// Materialize from raw baseline + log slices, using `config`'s workflow names.
/// The variant the `append_checked` verifier closures use: they hold slices
/// (read under the store lock), not a store, so can't go through [`replay`].
pub(crate) fn materialize(
    config: &Config,
    baseline: &[TaskState],
    log: &[MutationEvent],
) -> HashMap<String, TaskState> {
    let w = &config.workflow;
    Engine::materialize_state(
        baseline.to_vec(),
        log.to_vec(),
        &w.status_field,
        &w.done_status,
    )
}

/// Load and materialize the current task map from any store.
///
/// Replay also reports *orphaned* events — `Update`/`Append`/`AddDep`/`RemoveDep`/
/// `Delete` events whose target task no longer exists, which apply to nothing. They are a
/// silent symptom of a dropped `Create` (from the merge driver's removal-union, a
/// revert, or a manual edit), so every read command warns about them on STDERR
/// and points at `ta resolve`. The warning never blocks the read.
pub(crate) fn state_of(store: &impl EventStore) -> Result<HashMap<String, TaskState>, DynError> {
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
    // their configured names, so list/show/--sort treat them like any
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

/// Inject the computed columns onto `state`, but only when the display references
/// them (as a shown column or the sort key) — so default, `--full`, and json
/// output stay unchanged unless asked. They are graph-derived and surfaced as
/// ordinary fields, so `cell_value`/`--sort`/`--columns` handle them with no
/// special-casing. Used by `list` (including `--ready`):
///
/// - `unblocks`/`blocked_by` — transitive not-done dependents / prerequisites
///   over the blocker edges (numbers).
/// - `subtasks` — a parent's `done/total` direct-child completion (string).
pub(crate) fn inject_computed_columns(
    store: &impl EventStore,
    state: &mut HashMap<String, TaskState>,
    workflow: &crate::config::WorkflowConfig,
    display: &DisplayArgs,
    cfg: &crate::config::DisplayConfig,
) {
    let refs = crate::format::referenced_columns(display, cfg);
    let wants = |name: &str| refs.iter().any(|c| c == name);

    if wants("unblocks") || wants("blocked_by") {
        let blockers = store.config().relationships.blocker_types();
        let counts = crate::graph::reachability_counts(
            state,
            &blockers,
            &workflow.status_field,
            &workflow.done_status,
        );
        for (id, task) in state.iter_mut() {
            if let Some(&(unblocks, blocked_by)) = counts.get(id) {
                task.custom_fields
                    .insert("unblocks".to_string(), serde_json::json!(unblocks));
                task.custom_fields
                    .insert("blocked_by".to_string(), serde_json::json!(blocked_by));
            }
        }
    }

    if wants("subtasks") {
        let hierarchy = store.config().relationships.hierarchy_types();
        let progress = crate::graph::subtask_progress(
            state,
            &hierarchy,
            &workflow.status_field,
            &workflow.done_status,
        );
        for (id, task) in state.iter_mut() {
            if let Some(&(done, total)) = progress.get(id) {
                task.custom_fields.insert(
                    "subtasks".to_string(),
                    serde_json::json!(format!("{done}/{total}")),
                );
            }
        }
    }
}

/// A task's relationship edges for display: its forward edges (the `depends_on`
/// field + the typed map) plus inverse edges — for every OTHER task with an edge
/// pointing here, that edge's configured `inverse` name (an empty inverse is
/// one-way and not surfaced). Keyed by display name → sorted target ids. Used by
/// `show` to surface a task's relationships.
pub(crate) fn relationship_edges(
    state: &HashMap<String, TaskState>,
    id: &str,
    types: &BTreeMap<String, RelationshipDef>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut display: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(task) = state.get(id) {
        for (rel, targets) in &task.relationships {
            display
                .entry(rel.clone())
                .or_default()
                .extend(targets.iter().cloned());
        }
    }
    for (other_id, other) in state {
        if other_id == id {
            continue;
        }
        let mut hit_types: Vec<&str> = Vec::new();
        for (rel_type, targets) in &other.relationships {
            if targets.iter().any(|t| t == id) {
                hit_types.push(rel_type);
            }
        }
        for rel_type in hit_types {
            if let Some(def) = types.get(rel_type) {
                if !def.inverse.is_empty() {
                    display
                        .entry(def.inverse.clone())
                        .or_default()
                        .insert(other_id.clone());
                }
            }
        }
    }
    display
}

/// Event keys that are struct fields, not schema-agnostic task fields. Letting a
/// user field shadow one of these would either collide with the event envelope
/// or be silently swallowed by `_meta`, so we reject them up front.
const RESERVED_FIELD_KEYS: &[&str] = &["seq", "timestamp", "op", "task_id", "_meta"];

/// Validate a batch of draft events against the current `state` and drop the
/// redundant ones, returning exactly the events worth appending.
///
/// Meant to run inside the store's write lock (via `EventStore::append_checked`),
/// so the verify-then-write is atomic and can't race a concurrent writer. Rules
/// (a rejection is a hard error — nothing in the batch is written):
/// - Setting a reserved/computed field name (the envelope keys, `id`/`deps`/`dep`,
///   the timestamp and graph columns, relationship names) is **rejected**.
/// - `Create` of an existing id — or any op whose target task is absent (incl.
///   `Delete`) — is **rejected**, as is an `AddDep` to itself or to a missing
///   target.
/// - An `Update` keeps only the fields that actually change (a value already
///   equal, or a `null`-unset of an already-absent field, is dropped); an
///   `Update` left with no fields is dropped entirely.
/// - `AddDep` of an existing edge and `RemoveDep` of an absent one are dropped as
///   no-ops.
/// - `Append` (`+=`) never lands on a no-op, but is rejected on the single-valued
///   status field. This is where per-field type-schema checks will plug in later.
pub(crate) fn vet_events(
    drafts: &[MutationEvent],
    state: &HashMap<String, TaskState>,
    config: &Config,
) -> Result<Vec<MutationEvent>, DynError> {
    let reserved = reserved_field_names(config);
    let mut out = Vec::new();
    for draft in drafts {
        let id = draft.task_id.as_str();
        // A field whose value is computed/injected (id, deps, the timestamp and
        // graph columns, relationship names) can't be set directly — a user value
        // of the same name is silently shadowed. Applies to ops carrying fields.
        if matches!(draft.op, OpType::Create | OpType::Update | OpType::Append) {
            if let Some(bad) = draft.payload.keys().find(|k| reserved.contains(k.as_str())) {
                return Err(format!(
                    "`{bad}` is a reserved or computed field and can't be set directly"
                )
                .into());
            }
        }
        match draft.op {
            OpType::Create => {
                if state.contains_key(id) {
                    return Err(format!(
                        "task `{id}` already exists (use `ta update {id} …` to change it)"
                    )
                    .into());
                }
                out.push(draft.clone());
            }
            OpType::Update => {
                let task = require_existing(state, id)?;
                let mut payload = Map::new();
                for (key, value) in &draft.payload {
                    if changes_field(task, key, value) {
                        payload.insert(key.clone(), value.clone());
                    }
                }
                if !payload.is_empty() {
                    let mut event = draft.clone();
                    event.payload = payload;
                    out.push(event);
                }
            }
            OpType::Append => {
                require_existing(state, id)?;
                if let Some(bad) = draft
                    .payload
                    .keys()
                    .find(|k| *k == &config.workflow.status_field)
                {
                    return Err(format!(
                        "can't append (`+=`) to `{bad}`: it holds a single status value, not a log"
                    )
                    .into());
                }
                out.push(draft.clone()); // appends accumulate — never a no-op
            }
            OpType::AddDep => {
                let task = require_existing(state, id)?;
                let target = draft.payload.get(DEP_KEY).and_then(Value::as_str);
                if target == Some(id) {
                    return Err(format!("a task can't reference itself (`{id}`)").into());
                }
                if let Some(t) = target {
                    if !state.contains_key(t) {
                        return Err(format!("no task `{t}` to reference").into());
                    }
                }
                if !dep_edge_exists(task, &draft.payload) {
                    out.push(draft.clone());
                }
            }
            OpType::RemoveDep => {
                let task = require_existing(state, id)?;
                if dep_edge_exists(task, &draft.payload) {
                    out.push(draft.clone());
                }
            }
            OpType::Delete => {
                // Deleting a missing task is a typo, like any other mutation on it.
                require_existing(state, id)?;
                out.push(draft.clone());
            }
        }
    }
    Ok(out)
}

/// Field names that can't be set directly because their value is computed or
/// injected — a user field of the same name is silently shadowed (so meaningless
/// and invisible). The envelope keys plus: the structural columns `id`/`deps`
/// (and `dep`, which reads like a dependency — use `ta dep add`), the computed
/// graph columns, the configured timestamp columns, and the relationship type
/// names + inverses (which `show` surfaces and `ta dep` edits).
fn reserved_field_names(config: &Config) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = RESERVED_FIELD_KEYS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for s in ["id", "deps", "dep", "unblocks", "blocked_by", "subtasks"] {
        names.insert(s.to_string());
    }
    for name in [
        &config.timestamps.create_time,
        &config.timestamps.update_time,
        &config.timestamps.close_time,
    ] {
        if !name.is_empty() {
            names.insert(name.clone());
        }
    }
    for (name, def) in &config.relationships.types {
        names.insert(name.clone());
        if !def.inverse.is_empty() {
            names.insert(def.inverse.clone());
        }
    }
    names
}

/// The task `id` in `state`, or an error if it doesn't exist — so a mutation
/// against a typo'd/absent task is rejected at write time rather than becoming a
/// silent orphan. (Replay still tolerates orphans from merges/reverts.)
fn require_existing<'a>(
    state: &'a HashMap<String, TaskState>,
    id: &str,
) -> Result<&'a TaskState, DynError> {
    state
        .get(id)
        .ok_or_else(|| format!("no task `{id}`").into())
}

/// Whether setting `key` = `value` would change `task`. A `null` value is the
/// unset convention: it changes only a field that is currently present.
fn changes_field(task: &TaskState, key: &str, value: &Value) -> bool {
    if value.is_null() {
        task.custom_fields.contains_key(key)
    } else {
        task.custom_fields.get(key) != Some(value)
    }
}

/// Whether `task` already has the edge described by an `AddDep`/`RemoveDep`
/// payload (`dep` target, optional `type`; absent type = [`DEPENDS_ON`]).
fn dep_edge_exists(task: &TaskState, payload: &Map<String, Value>) -> bool {
    let Some(target) = payload.get(DEP_KEY).and_then(Value::as_str) else {
        return false;
    };
    let rel_type = payload
        .get(DEP_TYPE_KEY)
        .and_then(Value::as_str)
        .unwrap_or(DEPENDS_ON);
    task.relationships
        .get(rel_type)
        .is_some_and(|targets| targets.iter().any(|d| d == target))
}

/// A parsed field list, split by operator: fields to **set** (`=`) and fields to
/// **append** to (`+=`).
pub(crate) type FieldOps = (Map<String, Value>, Map<String, Value>);

/// Parse `key=value` / `key+=value` tokens into two payload maps: fields to
/// **set** (`=`) and fields to **append** to (`+=`). One `update` can mix both,
/// which the caller emits as an `Update` event plus an `Append` event.
///
/// Values follow the same rules either way: parsed as JSON, falling back to a
/// plain string (so `status=open` stays a string, `priority=3` becomes a number);
/// a value of `@PATH` is read from that file and `@-` from stdin (verbatim, one
/// trailing newline trimmed) — the way to pass long or shell-hostile text without
/// fighting argv quoting; `@@text` escapes to the literal `@text`.
pub(crate) fn parse_field_ops(fields: &[String]) -> Result<FieldOps, DynError> {
    let (mut set, mut append) = (Map::new(), Map::new());
    for raw in fields {
        let (key_part, val) = raw
            .split_once('=')
            .ok_or_else(|| format!("invalid field `{raw}` (expected key=value or key+=value)"))?;
        // `key+=value` appends; a trailing `+` on the key is the operator.
        let (key, is_append) = key_part
            .strip_suffix('+')
            .map_or((key_part, false), |k| (k, true));
        if key.is_empty() {
            return Err(format!("invalid field `{raw}`: empty field name").into());
        }
        if RESERVED_FIELD_KEYS.contains(&key) {
            return Err(format!("field name `{key}` is reserved and can't be used").into());
        }
        let value = field_value(key, val)?;
        if is_append {
            append.insert(key.to_string(), value);
        } else {
            set.insert(key.to_string(), value);
        }
    }
    Ok((set, append))
}

/// Resolve one `key=value` value: `@file`/`@-` (file or stdin, verbatim string),
/// `@@x` (literal `@x`), or a JSON-or-string bare value.
fn field_value(key: &str, val: &str) -> Result<Value, DynError> {
    let Some(src) = val.strip_prefix('@') else {
        return Ok(serde_json::from_str::<Value>(val).unwrap_or_else(|_| Value::String(val.into())));
    };
    if let Some(literal) = src.strip_prefix('@') {
        // `@@foo` is the escape hatch for a literal value that starts with `@`.
        return Ok(Value::String(format!("@{literal}")));
    }
    let mut content = if src == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(src)
            .map_err(|e| format!("cannot read `{src}` for field `{key}`: {e}"))?
    };
    // Trim a single trailing newline (`\n` or `\r\n`) — files almost always have
    // one and it's rarely wanted in a field value.
    if content.ends_with('\n') {
        content.pop();
        if content.ends_with('\r') {
            content.pop();
        }
    }
    Ok(Value::String(content))
}

/// Ask the user to confirm a destructive action. `force` (from `--force`) skips
/// the prompt. The prompt goes to stderr so stdout stays clean for piping; reads
/// a `y/N` line from stdin and defaults to no.
pub(crate) fn confirm(prompt: &str, force: bool) -> Result<bool, DynError> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

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

    #[test]
    fn field_value_coerces_bare_and_strings_fallback() {
        assert_eq!(field_value("k", "open").unwrap(), serde_json::json!("open"));
        assert_eq!(field_value("k", "3").unwrap(), serde_json::json!(3));
    }

    #[test]
    fn field_value_double_at_escapes_to_literal_at() {
        // `@@bob` is a literal `@bob`, not a file read.
        assert_eq!(
            field_value("owner", "@@bob").unwrap(),
            serde_json::json!("@bob")
        );
    }

    #[test]
    fn field_value_at_path_reads_a_file_verbatim() {
        // The whole point: a value full of quotes, backticks, and newlines that
        // would be a nightmare to pass on argv.
        let body = "Notes: \"quoted\", `backticked`,\nsecond line.\n";
        let path = std::env::temp_dir().join("taska-field-value-unit-test.md");
        std::fs::write(&path, body).unwrap();
        let v = field_value("notes", &format!("@{}", path.display())).unwrap();
        // Verbatim, with the single trailing newline trimmed and no JSON coercion.
        assert_eq!(
            v,
            serde_json::json!("Notes: \"quoted\", `backticked`,\nsecond line.")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn field_value_missing_file_errors() {
        assert!(field_value("notes", "@/no/such/taska/file").is_err());
    }

    #[test]
    fn parse_field_ops_splits_set_and_append() {
        let (set, append) = parse_field_ops(&[
            "status=open".into(),
            "log+=first".into(),
            "priority=3".into(),
        ])
        .unwrap();
        assert_eq!(set["status"], serde_json::json!("open"));
        assert_eq!(set["priority"], serde_json::json!(3));
        assert_eq!(append["log"], serde_json::json!("first"));
        assert!(
            !set.contains_key("log") && !append.contains_key("status"),
            "each token lands in exactly one map"
        );
    }

    #[test]
    fn parse_field_ops_rejects_reserved_empty_and_opless() {
        assert!(parse_field_ops(&["seq=1".into()]).is_err(), "reserved set");
        assert!(
            parse_field_ops(&["seq+=1".into()]).is_err(),
            "reserved append"
        );
        assert!(parse_field_ops(&["+=x".into()]).is_err(), "empty key");
        assert!(parse_field_ops(&["noeq".into()]).is_err(), "no operator");
    }
}
