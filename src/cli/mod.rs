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
use crate::model::{
    MutationEvent, OpType, TaskState, DEPENDS_ON, REL_KEY, RESERVED_FIELD_KEYS, STATUS_KEY,
    TARGET_KEY, TASK_TYPE_KEY,
};
use crate::storage::{EventStore, FileStore};

mod commands;
use commands::{
    cmd_compact, cmd_config, cmd_create, cmd_delete, cmd_dep_group, cmd_init, cmd_list, cmd_repair,
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
        /// field, `id`, `deps` (any edge), a relationship type (`depends_on=x`)
        /// or inverse name (`subtask_of=epic`, `blocks=x`), or a computed
        /// column (`unblocks=0`). With none given, lists every task.
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
    /// Repair the store; `--migrate` brings the on-disk format up to date
    Repair {
        /// Migrate the event log and baseline to the current on-disk format
        #[arg(long)]
        migrate: bool,
    },
    /// Git event-log merge driver entrypoint (invoked by Git, not humans)
    #[command(name = "git-merge", hide = true)]
    GitMerge {
        ancestor: String,
        current: String,
        incoming: String,
        /// Original pathname (%P), repo-relative; locates the store (its
        /// parent dir), which walk-up discovery from the repo root cannot do
        /// for a store nested in a subdirectory.
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
            path,
        } => {
            // Read the conflict policy and marker location from the merged
            // file's own store (resolved via %P — see `merge_driver_store`),
            // falling back to defaults so a merge never fails merely for lack
            // of config.
            let store = merge_driver_store(&path);
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

        // Repair is the format fixer, so it bypasses the format gate (but still
        // needs a valid config — it reads the default blocker type to migrate to).
        Commands::Repair { migrate } => {
            let store = FileStore::discover()?;
            enforce_config(store.config())?;
            cmd_repair(&store, migrate)
        }

        // Everything else resolves the store once and validates its config and its
        // on-disk format before dispatching, so a bad config edit or a stale store
        // surfaces on the next command (the latter pointing at `ta repair`).
        store_command => {
            let store = FileStore::discover()?;
            enforce_config(store.config())?;
            enforce_format(&store)?;
            warn_scm_health(&store);
            dispatch_store_command(store_command, &store)
        }
    }
}

/// Print (never fail on) the SCM health warning before every store-backed
/// command: an unregistered merge driver in this clone, missing `.gitattributes`
/// entries, or an unsupported SCM — each pointing at its remedy. Warning-only,
/// unlike the enforce gates: the store itself is healthy, the clone's merge
/// protection is what's incomplete, and a warning per command nags exactly until
/// someone runs `ta init`.
fn warn_scm_health(store: &FileStore) {
    if let Some(warning) = store.repo_root().and_then(crate::git::health_warning) {
        eprintln!("warning: {warning}");
    }
}

/// The store owning the file a merge driver was invoked on. Git runs drivers
/// at the repo root and passes `%P`, the merged file's repo-relative path —
/// its parent IS the store dir, so resolving via `%P` finds a store NESTED in
/// a subdirectory, which walk-up discovery from the repo root cannot.
/// Discovery remains the fallback for unusual invocations (e.g. an empty `%P`
/// from an old driver registration).
fn merge_driver_store(merged_path: &str) -> Option<FileStore> {
    std::path::Path::new(merged_path)
        .parent()
        .filter(|d| d.is_dir())
        .and_then(|d| FileStore::at(d.to_path_buf()).ok())
        .or_else(|| FileStore::discover().ok())
}

/// Validate config on every store-backed command, so a bad config edit surfaces
/// on the very next `ta` invocation rather than silently at the next compaction.
fn enforce_config(cfg: &Config) -> Result<(), DynError> {
    cfg.validate()
}

/// Refuse a normal command if the store is in an older on-disk format, pointing
/// at `ta repair --migrate` rather than mis-reading or silently rewriting legacy
/// data. Detection only — `repair` bypasses this and does the migration.
fn enforce_format(store: &FileStore) -> Result<(), DynError> {
    let snap = crate::migrate::Snapshot {
        log: store.load_mutations()?,
        baseline: store.load_baseline()?,
    };
    if let Some(reason) = crate::migrate::pending(&snap, store.config()) {
        return Err(format!(
            "{reason}. The store is in an older on-disk format — run \
             `ta repair --migrate` to update it."
        )
        .into());
    }
    Ok(())
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
        | Commands::Repair { .. }
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
    Engine::materialize_state(baseline, mutations, &w.done_status)
}

/// Like [`replay`] but keeping the orphan report (see [`Engine::materialize_report`]).
pub(crate) fn replay_report(
    store: &impl EventStore,
    baseline: Vec<TaskState>,
    mutations: Vec<MutationEvent>,
) -> (HashMap<String, TaskState>, Vec<u64>) {
    let w = &store.config().workflow;
    Engine::materialize_report(baseline, mutations, &w.done_status)
}

/// Materialize from raw baseline + log slices, using `config`'s workflow names.
/// The variant the `append_checked` verifier closures use: they hold slices
/// (read under the store lock), not a store, so can't go through [`replay`].
/// RAW state: the status lives under the canonical [`STATUS_KEY`], not the
/// configured display name — which is what verifiers and event writers want.
pub(crate) fn materialize(
    config: &Config,
    baseline: &[TaskState],
    log: &[MutationEvent],
) -> HashMap<String, TaskState> {
    Engine::materialize_state(
        baseline.to_vec(),
        log.to_vec(),
        &config.workflow.done_status,
    )
}

/// Load and materialize the current task map from any store.
///
/// Replay also reports *orphaned* events — `Update`/`Append`/`AddEdge`/`RemoveEdge`/
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
    // Surface canonically-stored fields under their configured DISPLAY names
    // (the inverse of the write-side mapping in `canonicalize_fields`). Display
    // -only, like the timestamps above: columns, filters, sorting, and json
    // output all see the display name, while events/baseline keep the canonical
    // key — which is what makes the names freely renamable in config.
    for (display, canonical) in canonical_field_pairs(&store.config().workflow) {
        if display == canonical {
            continue;
        }
        for task in state.values_mut() {
            if let Some(value) = task.custom_fields.remove(canonical) {
                task.custom_fields.insert(display.clone(), value);
            }
        }
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
/// them (as a shown column, the sort key, or — via `extra_refs` — a filter
/// criterion's field) — so default, `--full`, and json output stay unchanged
/// unless asked. They are graph-derived and surfaced as ordinary fields, so
/// `cell_value`/`--sort`/`--columns`/filtering handle them with no
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
    extra_refs: &[String],
) {
    let refs = crate::format::referenced_columns(display, cfg);
    let wants = |name: &str| refs.iter().any(|c| c == name) || extra_refs.iter().any(|c| c == name);

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

/// A task's INVERSE relationship edges for display: for every OTHER task with an
/// edge pointing here, that edge's configured `inverse` name (an empty inverse
/// is one-way and not surfaced). Keyed by display name → sorted target ids. The
/// task's own forward edges are not included — the `deps` column carries them,
/// grouped by type. Used by `show`.
pub(crate) fn inverse_edges(
    state: &HashMap<String, TaskState>,
    id: &str,
    types: &BTreeMap<String, RelationshipDef>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut display: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (other_id, other) in state {
        if other_id == id {
            continue;
        }
        for (rel_type, targets) in &other.relationships {
            if !targets.iter().any(|t| t == id) {
                continue;
            }
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

/// Validate a batch of draft events against the current `state` and drop the
/// redundant ones, returning exactly the events worth appending.
///
/// Meant to run inside the store's write lock (via `EventStore::append_checked`),
/// so the verify-then-write is atomic and can't race a concurrent writer. Rules
/// (a rejection is a hard error — nothing in the batch is written):
/// - Setting a reserved/computed field name (the envelope keys, `id`/`deps`/`dep`,
///   the timestamp and graph columns, relationship names) is **rejected**.
/// - `Create` of an existing id — or any op whose target task is absent (incl.
///   `Delete`) — is **rejected**, as is an `AddEdge` to itself or to a missing
///   target.
/// - An `Update` keeps only the fields that actually change (a value already
///   equal, or a `null`-unset of an already-absent field, is dropped); an
///   `Update` left with no fields is dropped entirely.
/// - `AddEdge` of an existing edge and `RemoveEdge` of an absent one are dropped as
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
    // The would-be RESULTING fields of every task a surviving field-carrying
    // draft touches, simulated with the engine's own apply functions — the
    // schema gate validates WHOLE tasks (per the type-schemas decisions), not
    // just the drafts, so a write to a non-conforming task surfaces every
    // violation at once. `None` marks a task deleted within the batch.
    let mut preview: HashMap<String, Option<Map<String, Value>>> = HashMap::new();
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
                let mut fields = preview_entry(&mut preview, id, None);
                crate::engine::apply_set(&mut fields, draft.payload.clone());
                preview.insert(id.to_string(), Some(fields));
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
                    let mut fields = preview_entry(&mut preview, id, Some(task));
                    crate::engine::apply_set(&mut fields, payload.clone());
                    preview.insert(id.to_string(), Some(fields));
                    let mut event = draft.clone();
                    event.payload = payload;
                    out.push(event);
                }
            }
            OpType::Append => {
                require_existing(state, id)?;
                // Drafts are canonical by the time they reach the gate, so the
                // single-valued check looks for the storage keys (status and
                // the task-type discriminator), not the display names.
                if let Some(bad) = draft
                    .payload
                    .keys()
                    .find(|k| *k == STATUS_KEY || *k == TASK_TYPE_KEY)
                {
                    return Err(format!(
                        "can't append (`+=`) to `{bad}`: it holds a single value, not a log"
                    )
                    .into());
                }
                let task = state.get(id);
                let mut fields = preview_entry(&mut preview, id, task);
                crate::engine::apply_append(&mut fields, draft.payload.clone());
                preview.insert(id.to_string(), Some(fields));
                out.push(draft.clone()); // appends accumulate — never a no-op
            }
            OpType::AddEdge => {
                let task = require_existing(state, id)?;
                let target = draft.payload.get(TARGET_KEY).and_then(Value::as_str);
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
            OpType::RemoveEdge => {
                let task = require_existing(state, id)?;
                if dep_edge_exists(task, &draft.payload) {
                    out.push(draft.clone());
                }
            }
            OpType::Delete => {
                // Deleting a missing task is a typo, like any other mutation on it.
                require_existing(state, id)?;
                preview.insert(id.to_string(), None);
                out.push(draft.clone());
            }
        }
    }
    enforce_schemas(&preview, config)?;
    Ok(out)
}

/// Take a task's working field set out of the preview (falling back to its
/// current state, then to empty for a fresh create), for [`vet_events`] to
/// apply the next draft onto.
fn preview_entry(
    preview: &mut HashMap<String, Option<Map<String, Value>>>,
    id: &str,
    base: Option<&TaskState>,
) -> Map<String, Value> {
    preview
        .remove(id)
        .flatten()
        .or_else(|| base.map(|t| t.custom_fields.clone()))
        .unwrap_or_default()
}

/// The schema gate tail of [`vet_events`]: whole-task conformance for every
/// touched (and surviving) previewed task, with EVERY violation in one error so
/// a user or LLM can fix them all in a single follow-up. Inert while
/// `[task_types]` declares nothing (the schema-agnostic floor).
fn enforce_schemas(
    preview: &HashMap<String, Option<Map<String, Value>>>,
    config: &Config,
) -> Result<(), DynError> {
    if config.task_types.types.is_empty() {
        return Ok(());
    }
    for (id, fields) in preview {
        let Some(fields) = fields else { continue };
        let violations = schema_violations(fields, config);
        if !violations.is_empty() {
            return Err(format!(
                "task `{id}` does not conform to its task-type schema:\n  - {}",
                violations.join("\n  - ")
            )
            .into());
        }
    }
    Ok(())
}

/// The stored key a DECLARED schema field name refers to: declarations use
/// display names, storage is canonical — only the status field differs (the
/// discriminator can't be declared; `Config::validate` enforces both rules).
fn declared_field_key<'a>(name: &'a str, status_display: &str) -> &'a str {
    if name == status_display {
        STATUS_KEY
    } else {
        name
    }
}

/// Every way `fields` (a task's would-be stored fields) violates the declared
/// `[task_types]` schemas: a missing/non-string/unknown discriminator, a missing
/// required field, a value not matching its declared kind, or an undeclared
/// field on a `closed` type. Field names in declarations are DISPLAY names
/// (the status field may be declared under its configured name); stored keys
/// are canonical, so names resolve through the same pairs the write/read
/// boundaries use. Empty = conforming.
fn schema_violations(fields: &Map<String, Value>, config: &Config) -> Vec<String> {
    let w = &config.workflow;
    let types = &config.task_types.types;
    let declared_types = || types.keys().cloned().collect::<Vec<_>>().join(", ");
    let mut violations = Vec::new();

    // Resolve the task's type by the canonical key (drafts are canonical here).
    let Some(type_value) = fields.get(TASK_TYPE_KEY) else {
        violations.push(format!(
            "missing the `{}` field (declared task types: {})",
            w.type_field,
            declared_types()
        ));
        return violations;
    };
    let Some(type_name) = type_value.as_str() else {
        violations.push(format!(
            "`{}` must be a string naming a task type (one of: {})",
            w.type_field,
            declared_types()
        ));
        return violations;
    };
    let Some(def) = types.get(type_name) else {
        violations.push(format!(
            "unknown task type `{type_name}` (declared: {})",
            declared_types()
        ));
        return violations;
    };

    for (name, schema) in &def.fields {
        // validate() guarantees the kind parses; stay defensive anyway.
        let Ok(kind) = crate::config::FieldKind::parse(schema.kind_str()) else {
            continue;
        };
        match fields.get(declared_field_key(name, &w.status_field)) {
            None => {
                if schema.required() {
                    let hint = if schema.values().is_empty() {
                        String::new()
                    } else {
                        format!(" (one of: {})", schema.values().join(", "))
                    };
                    violations.push(format!("missing required field `{name}`{hint}"));
                }
            }
            Some(value) => {
                if !kind.matches_value(value, schema.values()) {
                    let hint = if schema.values().is_empty() {
                        String::new()
                    } else {
                        format!(" (one of: {})", schema.values().join(", "))
                    };
                    violations.push(format!(
                        "`{name}`: expected {}{hint}, got {value}",
                        schema.kind_str()
                    ));
                }
            }
        }
    }

    if def.closed {
        let allowed: BTreeSet<&str> = def
            .fields
            .keys()
            .map(|name| declared_field_key(name, &w.status_field))
            .chain([TASK_TYPE_KEY, STATUS_KEY])
            .collect();
        for key in fields.keys() {
            if !allowed.contains(key.as_str()) {
                violations.push(format!(
                    "undeclared field `{key}` (task type `{type_name}` is closed; declared \
                     fields: {})",
                    def.fields.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    violations
}

/// The full set of field names the write gate refuses: the static
/// [`RESERVED_FIELD_KEYS`] plus the config-dependent computed/injected names —
/// the configured timestamp columns and the relationship type names + inverses
/// (which `show` surfaces and `ta dep` edits). A user field with any of these
/// names would be silently shadowed at read time, so meaningless and invisible.
fn reserved_field_names(config: &Config) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = RESERVED_FIELD_KEYS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
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

/// Whether `task` already has the edge described by an `AddEdge`/`RemoveEdge`
/// payload (`dep` target, optional `type`; absent type = [`DEPENDS_ON`]).
fn dep_edge_exists(task: &TaskState, payload: &Map<String, Value>) -> bool {
    let Some(target) = payload.get(TARGET_KEY).and_then(Value::as_str) else {
        return false;
    };
    let rel_type = payload
        .get(REL_KEY)
        .and_then(Value::as_str)
        .unwrap_or(DEPENDS_ON);
    task.relationships
        .get(rel_type)
        .is_some_and(|targets| targets.iter().any(|d| d == target))
}

/// A parsed field list, split by operator: fields to **set** (`=`) and fields to
/// **append** to (`+=`).
pub(crate) type FieldOps = (Map<String, Value>, Map<String, Value>);

/// The `(display name, canonical storage key)` pairs of the config-renamable
/// fields: the workflow status and the task-type discriminator. Shared by the
/// write-side mapping ([`canonicalize_fields`]) and `state_of`'s read-side
/// rename, so the two boundaries can never disagree.
const fn canonical_field_pairs(
    workflow: &crate::config::WorkflowConfig,
) -> [(&String, &'static str); 2] {
    [
        (&workflow.status_field, STATUS_KEY),
        (&workflow.type_field, TASK_TYPE_KEY),
    ]
}

/// Map configured DISPLAY field names onto their canonical storage keys, before
/// vetting/appending — the write-side inverse of `state_of`'s display rename.
/// Writing the canonical spelling directly while a different display name is
/// configured is rejected: one name per concept per store, never two writable
/// spellings.
pub(crate) fn canonicalize_fields(
    fields: &mut Map<String, Value>,
    workflow: &crate::config::WorkflowConfig,
) -> Result<(), DynError> {
    for (display, canonical) in canonical_field_pairs(workflow) {
        if display == canonical {
            continue;
        }
        if fields.contains_key(canonical) {
            return Err(format!(
                "`{canonical}` is the canonical storage key of the configured `{display}` \
                 field; set `{display}=` instead"
            )
            .into());
        }
        if let Some(value) = fields.remove(display.as_str()) {
            fields.insert(canonical.to_string(), value);
        }
    }
    Ok(())
}

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
            return Err(format!(
                "`{key}` is a reserved or computed field and can't be set directly"
            )
            .into());
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
        // Both reservation reasons reject at parse time: envelope keys (`seq`)
        // and the static computed columns (`id`, `deps`, `unblocks`).
        for key in ["seq", "id", "deps", "unblocks"] {
            assert!(
                parse_field_ops(&[format!("{key}=1")]).is_err(),
                "reserved set: {key}"
            );
        }
        assert!(
            parse_field_ops(&["seq+=1".into()]).is_err(),
            "reserved append"
        );
        assert!(parse_field_ops(&["+=x".into()]).is_err(), "empty key");
        assert!(parse_field_ops(&["noeq".into()]).is_err(), "no operator");
    }

    #[test]
    fn schema_gate_validates_whole_tasks_and_lists_every_violation() {
        let config: Config = toml::from_str(
            r#"
[task_types.bug]
closed = true
[task_types.bug.fields]
points = "uint"
tags = "set<string>"
[task_types.bug.fields.severity]
type = "enum"
values = ["low", "high"]
required = true
[task_types.feature.fields.owner]
type = "string"
required = true
"#,
        )
        .unwrap();
        let create = |id: &str, fields: &[(&str, Value)]| {
            let payload: Map<String, Value> = fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect();
            MutationEvent::new(OpType::Create, id, payload)
        };
        let empty = HashMap::new();

        // Missing discriminator: rejected naming the display field and options.
        let err = vet_events(&[create("t", &[])], &empty, &config).unwrap_err();
        assert!(
            err.to_string().contains("missing the `type` field")
                && err.to_string().contains("bug, feature"),
            "{err}"
        );

        // EVERY violation in one error: missing required + wrong kind + closed.
        let err = vet_events(
            &[create(
                "t",
                &[
                    ("task_type", serde_json::json!("bug")),
                    ("points", serde_json::json!("abc")),
                    ("extra", serde_json::json!(1)),
                ],
            )],
            &empty,
            &config,
        )
        .unwrap_err()
        .to_string();
        for needle in [
            "missing required field `severity` (one of: low, high)",
            "`points`: expected uint",
            "undeclared field `extra`",
        ] {
            assert!(err.contains(needle), "`{needle}` in: {err}");
        }

        // A conforming create passes; set<string> rejects duplicates.
        let ok = create(
            "t",
            &[
                ("task_type", serde_json::json!("bug")),
                ("severity", serde_json::json!("low")),
                ("tags", serde_json::json!(["a", "b"])),
            ],
        );
        assert!(vet_events(&[ok], &empty, &config).is_ok());
        let dup = create(
            "t",
            &[
                ("task_type", serde_json::json!("bug")),
                ("severity", serde_json::json!("low")),
                ("tags", serde_json::json!(["a", "a"])),
            ],
        );
        let err = vet_events(&[dup], &empty, &config).unwrap_err();
        assert!(err.to_string().contains("expected set<string>"), "{err}");

        // No [task_types] declared: the gate is inert (schema-agnostic floor).
        assert!(vet_events(&[create("t", &[])], &empty, &Config::default()).is_ok());
    }

    #[test]
    fn schema_gate_revalidates_on_retype() {
        use crate::test_support::{state, task};
        let config: Config = toml::from_str(
            r#"
[task_types.bug.fields.severity]
type = "enum"
values = ["low", "high"]
required = true
[task_types.feature.fields.owner]
type = "string"
required = true
"#,
        )
        .unwrap();
        // Whole-task on update: retyping revalidates against the NEW type, and
        // one update can fix everything at once.
        let existing = state(&[task(
            "t",
            &[],
            &[
                ("task_type", serde_json::json!("bug")),
                ("severity", serde_json::json!("low")),
            ],
        )]);
        let retype = MutationEvent::new(
            OpType::Update,
            "t",
            std::iter::once(("task_type".to_string(), serde_json::json!("feature"))).collect(),
        );
        let err = vet_events(&[retype], &existing, &config).unwrap_err();
        assert!(
            err.to_string().contains("missing required field `owner`"),
            "{err}"
        );
        let retype_fixed = MutationEvent::new(
            OpType::Update,
            "t",
            [
                ("task_type".to_string(), serde_json::json!("feature")),
                ("owner".to_string(), serde_json::json!("bob")),
            ]
            .into_iter()
            .collect(),
        );
        assert!(vet_events(&[retype_fixed], &existing, &config).is_ok());
    }

    #[test]
    fn schema_gate_resolves_renamed_status_display_name() {
        use crate::test_support::{state, task};
        // The schema declares the status under its DISPLAY name `state`; the
        // stored key is canonical `status` — the gate must match them up.
        let config: Config = toml::from_str(
            r#"
[workflow]
status_field = "state"
[task_types.job.fields.state]
type = "enum"
values = ["todo", "done"]
required = true
"#,
        )
        .unwrap();
        let existing = state(&[task(
            "j",
            &[],
            &[
                ("task_type", serde_json::json!("job")),
                ("status", serde_json::json!("todo")), // canonical storage
            ],
        )]);
        let touch = MutationEvent::new(
            OpType::Update,
            "j",
            std::iter::once(("note".to_string(), serde_json::json!("x"))).collect(),
        );
        assert!(
            vet_events(&[touch], &existing, &config).is_ok(),
            "declared display name matches canonical storage"
        );
        // And a bad stored status is reported under the DECLARED name.
        let bad = MutationEvent::new(
            OpType::Update,
            "j",
            std::iter::once(("status".to_string(), serde_json::json!("nope"))).collect(),
        );
        let err = vet_events(&[bad], &existing, &config).unwrap_err();
        assert!(err.to_string().contains("`state`: expected enum"), "{err}");
    }

    #[test]
    fn canonicalize_maps_display_status_and_rejects_the_canonical_spelling() {
        use crate::config::WorkflowConfig;
        let renamed = WorkflowConfig {
            status_field: "state".to_string(),
            ..WorkflowConfig::default()
        };

        // The configured display name maps onto the canonical storage key.
        let mut fields = Map::new();
        fields.insert("state".to_string(), serde_json::json!("open"));
        canonicalize_fields(&mut fields, &renamed).unwrap();
        assert_eq!(fields.get(STATUS_KEY), Some(&serde_json::json!("open")));
        assert!(!fields.contains_key("state"), "display key consumed");

        // Writing the canonical spelling directly is rejected while a different
        // display name is configured — one writable name per concept.
        let mut direct = Map::new();
        direct.insert(STATUS_KEY.to_string(), serde_json::json!("x"));
        let err = canonicalize_fields(&mut direct, &renamed).unwrap_err();
        assert!(
            err.to_string().contains("state"),
            "points at display: {err}"
        );

        // Default name: canonical IS the display name; nothing to do.
        let mut plain = Map::new();
        plain.insert(STATUS_KEY.to_string(), serde_json::json!("open"));
        canonicalize_fields(&mut plain, &WorkflowConfig::default()).unwrap();
        assert_eq!(plain.get(STATUS_KEY), Some(&serde_json::json!("open")));
    }
}
