//! `ta` command-line surface: argument parsing, dispatch, and presentation.
//!
//! Command handlers depend on the [`EventStore`] abstraction rather than the
//! concrete [`FileStore`], so they can be exercised against any store.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value};

use crate::config::{CompactionConfig, Config, DisplayConfig, WorkflowConfig};
use crate::engine::Engine;
use crate::error::DynError;
use crate::git;
use crate::graph;
use crate::merge;
use crate::model::{MutationEvent, OpType, TaskState};
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
    Human,
    Json,
}

/// Display flags shared by `list`, `search`, and `ready`.
#[derive(Args, Clone)]
struct DisplayArgs {
    /// Render as an aligned table (human) or a JSON array (json)
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
    /// Show every field, not just the configured columns
    #[arg(long)]
    full: bool,
    /// Comma-separated columns to show, overriding config (e.g. --columns id,status)
    #[arg(long, value_delimiter = ',')]
    columns: Option<Vec<String>>,
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
    /// Search tasks by field value: `ta search <key> <val>`
    Search {
        key: String,
        val: String,
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
        Commands::Search { key, val, display } => {
            cmd_search(store, &key, &val, &display, &store.config().display)
        }
        Commands::Show { id, display } => cmd_show(store, &id, &display, &store.config().display),
        Commands::Ready { display } => {
            let workflow = store.config().workflow.clone();
            cmd_ready(store, &workflow, &display, &store.config().display)
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
        | Commands::Resolve { .. }
        | Commands::GitMerge { .. }
        | Commands::GitMergeBaseline { .. } => {
            unreachable!("non-store commands are handled before dispatch")
        }
    }
}

/// Load and materialize the current task map from any store.
///
/// Replay also reports *orphaned* events — `Update`/`AddDep`/`RemoveDep`/`Delete`
/// events whose target task no longer exists, which apply to nothing. They are a
/// silent symptom of a dropped `Create` (from the merge driver's removal-union, a
/// revert, or a manual edit), so every read command warns about them on STDERR
/// and points at `ta resolve`. The warning never blocks the read.
fn state_of(store: &impl EventStore) -> Result<HashMap<String, TaskState>, DynError> {
    let (state, orphans) =
        Engine::materialize_report(store.load_baseline()?, store.load_mutations()?);
    if !orphans.is_empty() {
        eprintln!(
            "taska: warning: {} orphaned event(s) in the log (no matching task) — \
             run `ta resolve` to clean them up.",
            orphans.len()
        );
    }
    Ok(state)
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
    let mut tasks: Vec<&TaskState> = state.values().collect();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    println!("{}", render(&tasks, display, cfg, "(no tasks)"));
    Ok(())
}

fn cmd_search(
    store: &impl EventStore,
    key: &str,
    val: &str,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    // Match the query against the same JSON coercion used on write.
    let needle =
        serde_json::from_str::<Value>(val).unwrap_or_else(|_| Value::String(val.to_string()));
    let mut hits = Engine::filter_tasks(&state, key, &needle);
    hits.sort_by(|a, b| a.id.cmp(&b.id));
    println!("{}", render(&hits, display, cfg, "(no matches)"));
    Ok(())
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
    // `--columns` overrides; we reuse the shared `render` plumbing for either.
    let columns = display
        .columns
        .as_ref()
        .map_or_else(|| full_columns(&tasks), Clone::clone);
    let output = match display.format {
        OutputFormat::Json => render_json(&tasks, &columns),
        OutputFormat::Human => render_human(&tasks, &columns, effective_max_width(display, cfg)),
    };
    println!("{output}");
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
    println!("{}", render(&tasks, display, cfg, "(nothing ready)"));
    Ok(())
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
    // so divergent branches can still be reconciled by event id.
    let (to_fold, to_keep) = mutations.split_at(split);
    let folded = Engine::materialize_state(baseline, to_fold.to_vec());
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

    let current = Engine::materialize_state(baseline.clone(), mutations.clone());
    let target = Engine::materialize_state(baseline.clone(), mutations[..keep].to_vec());

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
        let post = Engine::materialize_state(baseline, mutations[..truncate_to].to_vec());
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
    let (_, orphans) = Engine::materialize_report(baseline, mutations.clone());
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
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Render tasks per the display args. The selected columns (`--columns`/`--full`/
/// config) decide *which* fields appear; `--format` decides only how they print,
/// and both formats share the same field order.
fn render(tasks: &[&TaskState], display: &DisplayArgs, cfg: &DisplayConfig, empty: &str) -> String {
    let columns = resolve_columns(display, cfg, tasks);
    match display.format {
        OutputFormat::Json => render_json(tasks, &columns),
        OutputFormat::Human if tasks.is_empty() => empty.to_string(),
        OutputFormat::Human => render_human(tasks, &columns, effective_max_width(display, cfg)),
    }
}

/// The truncation width to apply: `--full` prints values untruncated (0 is the
/// "no limit" sentinel `truncate` already honors), so it reads the *complete*
/// view it asked for. Without `--full`, the configured `max_width` still governs
/// the default and `--columns` views.
const fn effective_max_width(display: &DisplayArgs, cfg: &DisplayConfig) -> usize {
    if display.full {
        0
    } else {
        cfg.max_width
    }
}

/// Decide the columns: `--full` (id + every field seen, sorted, + deps), else an
/// explicit `--columns`, else the configured default.
fn resolve_columns(
    display: &DisplayArgs,
    cfg: &DisplayConfig,
    tasks: &[&TaskState],
) -> Vec<String> {
    if display.full {
        full_columns(tasks)
    } else if let Some(cols) = &display.columns {
        cols.clone()
    } else {
        cfg.columns.clone()
    }
}

/// The "full" column set for a slice of tasks: `id` + every custom field seen
/// (deduplicated and sorted) + `deps`. Used by `--full` and by `show`'s default.
fn full_columns(tasks: &[&TaskState]) -> Vec<String> {
    let fields: std::collections::BTreeSet<&String> =
        tasks.iter().flat_map(|t| t.custom_fields.keys()).collect();
    let mut cols = vec!["id".to_string()];
    cols.extend(fields.into_iter().cloned());
    cols.push("deps".to_string());
    cols
}

fn render_human(tasks: &[&TaskState], columns: &[String], max_width: usize) -> String {
    let headers: Vec<String> = columns.iter().map(|c| c.to_uppercase()).collect();
    let rows: Vec<Vec<String>> = tasks
        .iter()
        .map(|t| {
            columns
                .iter()
                .map(|c| truncate(&human_cell(t, c), max_width))
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

fn render_json(tasks: &[&TaskState], columns: &[String]) -> String {
    if tasks.is_empty() {
        return "[]".to_string();
    }
    let objects: Vec<String> = tasks
        .iter()
        .map(|t| {
            let pairs: Vec<String> = columns
                .iter()
                .map(|c| {
                    let key = serde_json::to_string(c).unwrap_or_default();
                    format!("{key}:{}", json_cell(t, c))
                })
                .collect();
            format!("  {{{}}}", pairs.join(","))
        })
        .collect();
    format!("[\n{}\n]", objects.join(",\n"))
}

/// A field's value for the human table: bare string, or compact JSON otherwise.
fn human_cell(task: &TaskState, col: &str) -> String {
    match col {
        "id" => task.id.clone(),
        "deps" => task.depends_on.join(", "),
        _ => match task.custom_fields.get(col) {
            Some(Value::String(s)) => s.clone(),
            Some(v) => serde_json::to_string(v).unwrap_or_default(),
            None => String::new(),
        },
    }
}

/// A field's value as a JSON literal; a task missing the field yields `null`.
fn json_cell(task: &TaskState, col: &str) -> String {
    match col {
        "id" => serde_json::to_string(&task.id).unwrap_or_default(),
        "deps" => serde_json::to_string(&task.depends_on).unwrap_or_default(),
        _ => task.custom_fields.get(col).map_or_else(
            || "null".to_string(),
            |v| serde_json::to_string(v).unwrap_or_default(),
        ),
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
    }

    impl EventStore for InMemoryStore {
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

    #[test]
    fn show_full_columns_cover_every_field_and_unknown_errors() {
        let store = InMemoryStore::default();
        cmd_create(&store, "api", &["status=open".into(), "priority=3".into()]).unwrap();
        let state = state_of(&store).unwrap();
        let task = state.get("api").unwrap();

        // `show`'s default column set is id + the task's own fields (sorted) + deps,
        // so every field of the task is rendered.
        let cols = full_columns(&[task]);
        assert_eq!(cols, ["id", "priority", "status", "deps"], "full set: {cols:?}");
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
        }
    }

    fn display(format: OutputFormat, full: bool, columns: Option<&[&str]>) -> DisplayArgs {
        DisplayArgs {
            format,
            full,
            columns: columns.map(|c| c.iter().map(|s| (*s).to_string()).collect()),
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
    fn all_unions_fields_and_empty_json_is_brackets() {
        let a = task("a", &[], &[("x", serde_json::json!(1))]);
        let b = task("b", &[], &[("y", serde_json::json!(2))]);
        let d = display(OutputFormat::Json, true, None);
        let out = render(&[&a, &b], &d, &DisplayConfig::default(), "(none)");
        // --full unions fields: both x and y appear as keys.
        assert!(
            out.contains("\"x\"") && out.contains("\"y\""),
            "union: {out}"
        );
        // a missing field is null, not absent.
        assert!(out.contains("\"y\":null"), "missing field is null: {out}");

        let empty = render(&[], &d, &DisplayConfig::default(), "(none)");
        assert_eq!(empty, "[]", "empty json is []");
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
        assert_eq!(c[0].payload.get("status"), Some(&serde_json::json!("closed")));
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
                .any(|e| e.op == OpType::AddDep && e.payload.get("dep") == Some(&serde_json::json!("y"))),
            "adds y: {events:?}"
        );
        assert!(
            events.iter().any(
                |e| e.op == OpType::RemoveDep && e.payload.get("dep") == Some(&serde_json::json!("x"))
            ),
            "removes x: {events:?}"
        );
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
