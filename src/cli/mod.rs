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
    /// Update a task: `=` sets, `+=` accumulates, `-=` removes (e.g. `points+=2`)
    ///
    /// The task must exist; a write that changes nothing is dropped (nothing is
    /// logged), and `+=`/`-=` are rejected on the single-valued status field.
    Update {
        id: String,
        /// `key=value` sets a field; `key+=value` appends text (string fields),
        /// adds (declared numeric fields), or inserts elements (declared set<…>
        /// fields); `key-=value` subtracts / removes elements (declared
        /// numeric/set only). Accumulates merge conflict-free across branches.
        /// Values parse as JSON-or-string; `key=@FILE` / `key=@-` read from a
        /// file / stdin. At least one required.
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
    /// Repair the store: on-disk format migrations and schema data fixes
    ///
    /// Repair is the one command allowed to REWRITE existing log/baseline
    /// records (everything else is append-only). It never prompts: review the
    /// result with `git diff .taska`, revert with `git restore .taska` before
    /// committing. Every fix is deterministic for a given store + config, so
    /// two clones running the same repair produce identical bytes. Anything
    /// ambiguous is reported with a suggested command — repair never guesses,
    /// and never writes data the schemas would reject.
    ///
    /// Typical uses:
    ///   ta repair --migrate                # after upgrading taska: update the on-disk format
    ///   ta repair --schema                 # after declaring `[task_types]`: apply lossless data fixes
    ///   ta repair --schema --set-type-if-none bug   # ...and type every untyped task as `bug`
    ///   ta repair --rename severity=sev    # move a misnamed column under its declared name
    ///   ta repair --rename type=category   # adopt a de-facto type column as the task type
    ///
    /// Flags combine. Order applied: --migrate, then --rename, then
    /// --set-type-if-none, then the lossless schema fixes (which also run for
    /// --rename/--set-type-if-none alone, so moved or freshly typed values
    /// coerce in the same pass). Whatever still violates a schema afterwards
    /// is listed with the `ta update` one-liner that fixes it.
    #[command(verbatim_doc_comment)]
    Repair {
        /// Migrate the event log and baseline to the current on-disk format.
        ///
        /// Runs every pending format migration in order (stores several
        /// versions behind catch up in one go); a stale store is detected on
        /// every command and pointed here. Idempotent.
        #[arg(long)]
        migrate: bool,
        /// Apply every LOSSLESS data fix toward the `[task_types]` schemas.
        ///
        /// Rewrites each offending value where it is stored: numeric strings
        /// to numbers ("3" -> 3), bare scalars to singleton arrays/sets,
        /// "true"/"false" strings to booleans, numbers to strings for string
        /// fields, and common date formats (YYYY-MM-DD, with optional
        /// HH:MM:SS) to RFC 3339. Never types an untyped task (see
        /// --set-type-if-none) and never guesses: ambiguous values are listed
        /// for a human/agent to fix per task. Idempotent.
        #[arg(long)]
        schema: bool,
        /// Set this declared task type on every task that has NONE.
        ///
        /// An explicit migration choice — never inferred, even when only one
        /// type is declared (you may be migrating gradually or keeping some
        /// tasks untyped; see also the `workflow.untyped_tasks` config ladder
        /// allow -> warn -> deny). Rejected if TYPE isn't declared in
        /// `[task_types]`. The schema fixes run afterwards, so freshly typed
        /// tasks coerce in the same pass.
        #[arg(long, value_name = "TYPE")]
        set_type_if_none: Option<String>,
        /// Move a field to a new name everywhere, assignment-style: NEW=OLD.
        ///
        /// `--rename severity=sev` moves `sev`'s values under `severity`
        /// across all events and the baseline, then the lossless fixes coerce
        /// them toward the destination's declared kind. One pair per
        /// invocation. The task-type field is a legal destination
        /// (`--rename type=category`) for adopting an existing discriminator
        /// column: only records whose value names a DECLARED type convert;
        /// the rest keep the old column and are reported. Records already
        /// carrying the destination keep it (values are never merged). The
        /// status field and reserved/computed names are not valid
        /// destinations.
        #[arg(long, value_name = "NEW=OLD")]
        rename: Option<String>,
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

        // Repair is the format/data fixer, so it bypasses the format gate (but
        // still needs a valid config — migrations and schema fixes read it).
        Commands::Repair {
            migrate,
            schema,
            rename,
            set_type_if_none,
        } => {
            let store = FileStore::discover()?;
            enforce_config(store.config())?;
            cmd_repair(
                &store,
                migrate,
                schema,
                rename.as_deref(),
                set_type_if_none.as_deref(),
            )
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
    // Read-tolerance: schemas are write-time law only, so non-conforming tasks
    // (grandfathered by a schema change, merged in, restored by undo) always
    // materialize — but every read command says so ONCE, before the display
    // renames below would skew the check. Silenceable via config.
    warn_nonconforming(&state, store.config());
    // Then the default substitution: missing/invalid declared fields READ as
    // their declared default (after the warning, so the report reflects the
    // stored truth; display-only, like everything below).
    substitute_schema_defaults(&mut state, store.config());
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

/// The grandfathered-data report: every task whose RAW stored fields violate
/// its `[task_types]` schema, each as one `task `id`: first violation (+N
/// more)` line. Empty while schemas are off. Shared by `state_of`'s one-line
/// warning and `ta config validate`'s detailed listing — reads stay tolerant
/// (this is a report, never an error), while any WRITE to such a task must
/// bring it into conformance (the whole-task gate).
pub(crate) fn schema_conformance_report(
    state: &HashMap<String, TaskState>,
    config: &Config,
) -> Vec<String> {
    if config.task_types.types.is_empty() {
        return Vec::new();
    }
    let mut report: Vec<String> = state
        .values()
        .filter_map(|task| {
            // Under `untyped_tasks = "allow"`, a typeless task is sanctioned —
            // not reported anywhere. `warn` and `deny` both report it.
            if !task.custom_fields.contains_key(TASK_TYPE_KEY)
                && config.workflow.untyped_tasks == crate::config::UntypedTasks::Allow
            {
                return None;
            }
            let violations = schema_violations(&task.custom_fields, config);
            let first = violations.first()?;
            let more = match violations.len() {
                1 => String::new(),
                n => format!(" (+{} more)", n - 1),
            };
            Some(format!("task `{}`: {first}{more}", task.id))
        })
        .collect();
    report.sort();
    report
}

/// Print (never fail on) the ONE-line non-conformance warning for a read
/// command, pointing at the detail surface. Gated by
/// `[workflow] warn_nonconforming` and active only while `[task_types]`
/// declares schemas. Runs on RAW state — before the display renames and
/// timestamp injection that would skew the check.
fn warn_nonconforming(state: &HashMap<String, TaskState>, config: &Config) {
    if !config.workflow.warn_nonconforming {
        return;
    }
    let report = schema_conformance_report(state, config);
    if let Some(example) = report.first() {
        eprintln!(
            "taska: warning: {} task(s) do not conform to their task-type schema (e.g. \
             {example}) — `ta config validate` lists them, `ta repair --schema` applies the \
             lossless fixes; writes to such a task must bring it into conformance. Silence \
             with `workflow.warn_nonconforming = false`.",
            report.len()
        );
    }
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
        if matches!(
            draft.op,
            OpType::Create | OpType::Update | OpType::Append | OpType::Add | OpType::Remove
        ) {
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
            OpType::Add | OpType::Remove => {
                let task = require_existing(state, id)?;
                if vet_accumulate(draft, task, &mut preview)? {
                    out.push(draft.clone());
                }
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

/// The `Add`/`Remove` arm of [`vet_events`]: reject accumulating into a
/// single-valued field, apply onto the preview with the engine's own
/// semantics, and report whether anything changed — an accumulate that changes
/// nothing (inserting a present set element, removing an absent one, adding 0)
/// is dropped rather than logged.
fn vet_accumulate(
    draft: &MutationEvent,
    task: &TaskState,
    preview: &mut HashMap<String, Option<Map<String, Value>>>,
) -> Result<bool, DynError> {
    if let Some(bad) = draft
        .payload
        .keys()
        .find(|k| *k == STATUS_KEY || *k == TASK_TYPE_KEY)
    {
        return Err(
            format!("can't accumulate (`+=`/`-=`) into `{bad}`: it holds a single value").into(),
        );
    }
    let id = draft.task_id.as_str();
    let mut fields = preview_entry(preview, id, Some(task));
    let before = fields.clone();
    crate::engine::apply_accumulate(
        &mut fields,
        draft.payload.clone(),
        matches!(draft.op, OpType::Add),
    );
    let changed = fields != before;
    preview.insert(id.to_string(), Some(fields));
    Ok(changed)
}

/// The declared defaults a write should stamp: every field of the effective
/// task type (the payload's discriminator wins over the current one) that has
/// a `default`, is absent from the current task, and is not being set, unset,
/// or accumulated by this very write. Used by `create` (stamp into the
/// payload) and `update` (heal the task on any write), so a task with
/// defaulted required fields conforms without spelling them out.
pub(crate) fn schema_default_stamps(
    current: Option<&Map<String, Value>>,
    payload: &Map<String, Value>,
    touched: &BTreeSet<String>,
    config: &Config,
) -> Map<String, Value> {
    let mut stamps = Map::new();
    let Some(def) = payload
        .get(TASK_TYPE_KEY)
        .or_else(|| current.and_then(|fields| fields.get(TASK_TYPE_KEY)))
        .and_then(Value::as_str)
        .and_then(|name| config.task_types.types.get(name))
    else {
        return stamps;
    };
    for (name, schema) in &def.fields {
        let Some(default) = schema.default_value() else {
            continue;
        };
        let key = declared_field_key(name, &config.workflow.status_field);
        let absent = !payload.contains_key(key)
            && !touched.contains(key)
            && !current.is_some_and(|fields| fields.contains_key(key));
        if absent {
            stamps.insert(key.to_string(), default.clone());
        }
    }
    stamps
}

/// Read-side default substitution (display-only, like the timestamp
/// injection): a declared field that is MISSING or whose stored value is
/// invalid (wrong kind or constraint-violating) reads as its declared
/// `default`. The stored log/baseline are untouched — the non-conformance
/// warning and `ta repair --schema` remain the signals to actually fix the
/// data. Runs on RAW state, before the display renames.
fn substitute_schema_defaults(state: &mut HashMap<String, TaskState>, config: &Config) {
    if config.task_types.types.is_empty() {
        return;
    }
    for task in state.values_mut() {
        let Some(def) = task
            .custom_fields
            .get(TASK_TYPE_KEY)
            .and_then(Value::as_str)
            .and_then(|name| config.task_types.types.get(name))
        else {
            continue;
        };
        let mut substitutions: Vec<(String, Value)> = Vec::new();
        for (name, schema) in &def.fields {
            let Some(default) = schema.default_value() else {
                continue;
            };
            let Ok(kind) = crate::config::FieldKind::parse(schema.kind_str()) else {
                continue;
            };
            let key = declared_field_key(name, &config.workflow.status_field);
            let invalid = task.custom_fields.get(key).is_none_or(|value| {
                !kind.matches_value(value, schema.values())
                    || !schema.constraint_violations(value).is_empty()
            });
            if invalid {
                substitutions.push((key.to_string(), default.clone()));
            }
        }
        for (key, value) in substitutions {
            task.custom_fields.insert(key, value);
        }
    }
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
        // The untyped-tasks policy: under `allow`/`warn`, a task with NO type
        // is outside the schemas — writes proceed unvalidated (the migration
        // ladder's lax rungs). Only `deny` makes the type mandatory here.
        if !fields.contains_key(TASK_TYPE_KEY)
            && config.workflow.untyped_tasks != crate::config::UntypedTasks::Deny
        {
            continue;
        }
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
                if kind.matches_value(value, schema.values()) {
                    // Kind-correct values still face the declared constraints
                    // (min/max, pattern, length and item bounds).
                    for constraint in schema.constraint_violations(value) {
                        violations.push(format!("`{name}`: {value} {constraint}"));
                    }
                } else {
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

/// Schema-aware value coercion for `Create`/`Update` payloads, run under the
/// store lock (where the task's type is known) just before [`vet_events`].
///
/// Best-effort lifting toward each DECLARED field's kind — the write gate
/// stays the enforcer with its messages:
/// - declared string: a guessed scalar reverts to its verbatim token (`raw`)
///   or stringifies, so `version=3.10` stores `"3.10"`, not the number 3.1;
/// - declared int/uint/float: a (quoted) numeric string parses to a number;
/// - declared bool: the strings "true"/"false" parse;
/// - declared array<T>/set<T>: a bare scalar lifts to a singleton, elements
///   coerce per T, and a set canonicalizes to its STORED form — deduped and
///   sorted (`cmp_json` order) — so concurrent inserts converge bytewise and
///   re-adding an element is a no-op, like relationship edges.
///
/// Undeclared fields, unknown/missing types, and `Append` payloads keep the
/// JSON-or-string guess (the schema-agnostic floor; `+=` semantics arrive with
/// schema-numeric-append). The payload's own (re)typed discriminator wins over
/// the task's current type, so retype + fields coerce against the new schema.
pub(crate) fn coerce_event_fields(
    events: &mut [MutationEvent],
    raw: &Map<String, Value>,
    state: &HashMap<String, TaskState>,
    config: &Config,
) {
    if config.task_types.types.is_empty() {
        return;
    }
    for event in events.iter_mut() {
        if !matches!(event.op, OpType::Create | OpType::Update) {
            continue;
        }
        let type_name = event
            .payload
            .get(TASK_TYPE_KEY)
            .or_else(|| {
                state
                    .get(&event.task_id)
                    .and_then(|t| t.custom_fields.get(TASK_TYPE_KEY))
            })
            .and_then(Value::as_str);
        let Some(def) = type_name.and_then(|n| config.task_types.types.get(n)) else {
            continue;
        };
        for (name, schema) in &def.fields {
            let key = declared_field_key(name, &config.workflow.status_field);
            let Some(value) = event.payload.get(key) else {
                continue;
            };
            if value.is_null() {
                continue; // the unset convention is never coerced
            }
            let Ok(kind) = crate::config::FieldKind::parse(schema.kind_str()) else {
                continue; // validate() reports the bad declaration
            };
            if let Some(coerced) = coerce_value(value, &kind, raw.get(key)) {
                event.payload.insert(key.to_string(), coerced);
            }
        }
    }
}

/// One value's lift toward `kind` (see [`coerce_event_fields`]); `None` = leave
/// it for the gate to judge as-is.
fn coerce_value(
    value: &Value,
    kind: &crate::config::FieldKind,
    raw: Option<&Value>,
) -> Option<Value> {
    use crate::config::FieldKind as K;
    match kind {
        K::String => match value {
            Value::Number(_) | Value::Bool(_) => Some(
                raw.cloned()
                    .unwrap_or_else(|| Value::String(value.to_string())),
            ),
            _ => None,
        },
        K::Int | K::Uint | K::Float => value
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
            .filter(Value::is_number),
        K::Bool => match value.as_str() {
            Some("true") => Some(Value::Bool(true)),
            Some("false") => Some(Value::Bool(false)),
            _ => None,
        },
        K::Datetime | K::Enum | K::Any => None,
        K::Array(element) => Some(coerce_sequence(value, element, raw, false)),
        K::Set(element) => Some(coerce_sequence(value, element, raw, true)),
    }
}

/// Coerce toward `array<element>`/`set<element>`: lift a bare scalar to a
/// singleton (its `raw` token still applies), coerce each element, and give a
/// set its canonical stored form — sorted (`cmp_json`) and deduped (by compact
/// JSON) — the bytewise form concurrent writers converge on.
fn coerce_sequence(
    value: &Value,
    element: &crate::config::FieldKind,
    raw: Option<&Value>,
    canonical_set: bool,
) -> Value {
    let mut items: Vec<Value> = match value {
        Value::Array(items) => items
            .iter()
            .map(|item| coerce_value(item, element, None).unwrap_or_else(|| item.clone()))
            .collect(),
        scalar => vec![coerce_value(scalar, element, raw).unwrap_or_else(|| scalar.clone())],
    };
    if canonical_set {
        items.sort_by(crate::model::cmp_json);
        items.dedup_by(|a, b| a == b);
    }
    Value::Array(items)
}

/// Build the events for one `update`'s field operations, run under the store
/// lock (the accumulate dispatch needs the task's type from live state). The
/// `Update` (set) event comes first so `field=reset field+=more` applies the
/// reset before accumulating, independent of token order; the schema-aware
/// coercion then shapes the set values.
pub(crate) fn build_field_events(
    id: &str,
    ops: &FieldOps,
    state: &HashMap<String, TaskState>,
    config: &Config,
) -> Result<Vec<MutationEvent>, DynError> {
    let (text, add, remove) = dispatch_accumulate(id, ops, state, config)?;
    // Heal-on-write: any write to a task whose declared, DEFAULTED fields are
    // still absent stamps them in the same Update — so `required` + `default`
    // never blocks a write, and the task converges toward conformance.
    let mut set = ops.set.clone();
    let touched: BTreeSet<String> = text
        .keys()
        .chain(add.keys())
        .chain(remove.keys())
        .cloned()
        .collect();
    let current = state.get(id).map(|task| &task.custom_fields);
    for (key, value) in schema_default_stamps(current, &set, &touched, config) {
        set.insert(key, value);
    }
    let mut events = Vec::new();
    for (payload, op) in [
        (set, OpType::Update),
        (text, OpType::Append),
        (add, OpType::Add),
        (remove, OpType::Remove),
    ] {
        if !payload.is_empty() {
            events.push(MutationEvent::new(op, id, payload));
        }
    }
    coerce_event_fields(&mut events, &ops.raw, state, config);
    Ok(events)
}

/// [`dispatch_accumulate`]'s result: the `(Append, Add, Remove)` payloads.
type AccumulatePayloads = (Map<String, Value>, Map<String, Value>, Map<String, Value>);

/// Split the `+=`/`-=` maps into per-op payloads by each field's DECLARED kind
/// — the keyboard vocabulary stays `{=, +=, -=}` while the event vocabulary
/// dispatches:
/// - `+=`: strings, `any`, undeclared fields, and unknown/missing types keep
///   the text `Append` (the schema-agnostic floor); int/uint/float and
///   `set<T>` become `Add` (set operands lift to element arrays, so replay's
///   set path is unambiguous); bool/enum/datetime/`array<T>` reject.
/// - `-=`: int/uint/float and `set<T>` become `Remove`; anything else rejects
///   (`-=` has no meaning without subtraction or removal semantics).
///
/// The applicable schema: the set payload's (re)typed discriminator wins over
/// the task's current type, mirroring [`coerce_event_fields`].
fn dispatch_accumulate(
    id: &str,
    ops: &FieldOps,
    state: &HashMap<String, TaskState>,
    config: &Config,
) -> Result<AccumulatePayloads, DynError> {
    use crate::config::FieldKind;
    let type_name = ops
        .set
        .get(TASK_TYPE_KEY)
        .or_else(|| {
            state
                .get(id)
                .and_then(|t| t.custom_fields.get(TASK_TYPE_KEY))
        })
        .and_then(Value::as_str);
    let def = type_name.and_then(|n| config.task_types.types.get(n));
    let declared_kind = |field: &str| -> Option<(&str, FieldKind)> {
        def?.fields.iter().find_map(|(name, schema)| {
            (declared_field_key(name, &config.workflow.status_field) == field)
                .then(|| FieldKind::parse(schema.kind_str()).ok())
                .flatten()
                .map(|kind| (schema.kind_str(), kind))
        })
    };

    let (mut text, mut add, mut remove) = (Map::new(), Map::new(), Map::new());
    for (key, operand) in &ops.append {
        match declared_kind(key) {
            Some((_, kind @ (FieldKind::Int | FieldKind::Uint | FieldKind::Float))) => {
                let operand = coerce_value(operand, &kind, None).unwrap_or_else(|| operand.clone());
                add.insert(key.clone(), operand);
            }
            Some((_, FieldKind::Set(element))) => {
                add.insert(key.clone(), coerce_sequence(operand, &element, None, true));
            }
            Some((kind_str, FieldKind::Bool | FieldKind::Enum | FieldKind::Datetime)) => {
                return Err(format!(
                    "`+=` is not defined for `{key}` (declared {kind_str}); set it with `{key}=`"
                )
                .into());
            }
            Some((kind_str, FieldKind::Array(_))) => {
                return Err(format!(
                    "`+=` is not defined for `{key}` (declared {kind_str}, which allows \
                     duplicates and keeps order); set the whole value with `{key}=[…]`, or \
                     declare it set<…> for element inserts"
                )
                .into());
            }
            // Strings, `any`, undeclared fields, unknown/missing type: text.
            _ => {
                text.insert(key.clone(), operand.clone());
            }
        }
    }
    for (key, operand) in &ops.subtract {
        match declared_kind(key) {
            Some((_, kind @ (FieldKind::Int | FieldKind::Uint | FieldKind::Float))) => {
                let operand = coerce_value(operand, &kind, None).unwrap_or_else(|| operand.clone());
                remove.insert(key.clone(), operand);
            }
            Some((_, FieldKind::Set(element))) => {
                remove.insert(key.clone(), coerce_sequence(operand, &element, None, true));
            }
            _ => {
                return Err(format!(
                    "`-=` needs a field declared as a number or set<…> (`{key}` isn't)"
                )
                .into());
            }
        }
    }
    Ok((text, add, remove))
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
pub(crate) struct FieldOps {
    /// Fields to **set** (`=`), values JSON-guessed.
    pub(crate) set: Map<String, Value>,
    /// Fields to **accumulate** into (`+=`), values JSON-guessed. Dispatched by
    /// declared kind at write time: text append for strings/undeclared, `Add`
    /// for numeric and set fields.
    pub(crate) append: Map<String, Value>,
    /// Fields to **remove** from (`-=`): numeric subtract or set-element
    /// removal — requires a declared numeric/set field.
    pub(crate) subtract: Map<String, Value>,
    /// The verbatim inline token text per SET key (always `Value::String`).
    /// Schema-aware coercion uses it to recover exact input the JSON guess
    /// mangles — `version=3.10` guesses the number 3.1, but a declared string
    /// field wants "3.10". `@file`/`@-` values are already verbatim strings and
    /// have no entry. A `Map<String, Value>` (not strings) so the same
    /// [`canonicalize_fields`] keeps its keys aligned with `set`.
    pub(crate) raw: Map<String, Value>,
}

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
    let mut ops = FieldOps {
        set: Map::new(),
        append: Map::new(),
        subtract: Map::new(),
        raw: Map::new(),
    };
    for token in fields {
        let (key_part, val) = token.split_once('=').ok_or_else(|| {
            format!("invalid field `{token}` (expected key=value, key+=value, or key-=value)")
        })?;
        // `key+=value` accumulates, `key-=value` removes; the trailing `+`/`-`
        // on the key is the operator.
        let (key, operator) = key_part.strip_suffix('+').map_or_else(
            || {
                key_part
                    .strip_suffix('-')
                    .map_or((key_part, '='), |k| (k, '-'))
            },
            |k| (k, '+'),
        );
        if key.is_empty() {
            return Err(format!("invalid field `{token}`: empty field name").into());
        }
        if RESERVED_FIELD_KEYS.contains(&key) {
            return Err(format!(
                "`{key}` is a reserved or computed field and can't be set directly"
            )
            .into());
        }
        let value = field_value(key, val)?;
        match operator {
            '+' => ops.append.insert(key.to_string(), value),
            '-' => ops.subtract.insert(key.to_string(), value),
            _ => {
                // The verbatim token is kept for SET values only — it backs the
                // declared-string coercion, which never applies to operands.
                if !val.starts_with('@') {
                    ops.raw
                        .insert(key.to_string(), Value::String(val.to_string()));
                }
                ops.set.insert(key.to_string(), value)
            }
        };
    }
    Ok(ops)
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
    fn parse_field_ops_splits_operators_and_keeps_raw_tokens() {
        let FieldOps {
            set,
            append,
            subtract,
            raw,
        } = parse_field_ops(&[
            "status=open".into(),
            "log+=first".into(),
            "points-=2".into(),
            "priority=3".into(),
            "version=3.10".into(),
        ])
        .unwrap();
        assert_eq!(set["status"], serde_json::json!("open"));
        assert_eq!(set["priority"], serde_json::json!(3));
        assert_eq!(append["log"], serde_json::json!("first"));
        assert_eq!(subtract["points"], serde_json::json!(2));
        assert!(
            !set.contains_key("log")
                && !append.contains_key("status")
                && !set.contains_key("points"),
            "each token lands in exactly one map"
        );
        // The guess loses "3.10" (-> 3.1); the raw token preserves it for
        // declared-string coercion.
        assert_eq!(set["version"], serde_json::json!(3.1));
        assert_eq!(raw["version"], serde_json::json!("3.10"));
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
    fn schema_coercion_lifts_declared_values() {
        let config: Config = toml::from_str(
            r#"
[task_types.bug.fields]
version = "string"
points = "uint"
flag = "bool"
tags = "set<string>"
nums = "array<int>"
"#,
        )
        .unwrap();
        let raw: Map<String, Value> =
            std::iter::once(("version".to_string(), serde_json::json!("3.10"))).collect();
        let payload: Map<String, Value> = [
            ("task_type", serde_json::json!("bug")),
            ("version", serde_json::json!(3.1)),
            ("points", serde_json::json!("5")),
            ("flag", serde_json::json!("true")),
            ("tags", serde_json::json!(["b", "a", "b"])),
            ("nums", serde_json::json!(7)),
            ("free", serde_json::json!("kept")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut events = vec![MutationEvent::new(OpType::Create, "t", payload)];
        coerce_event_fields(&mut events, &raw, &HashMap::new(), &config);
        let p = &events[0].payload;
        assert_eq!(
            p["version"],
            serde_json::json!("3.10"),
            "raw token wins for a declared string (the guess said 3.1)"
        );
        assert_eq!(p["points"], serde_json::json!(5), "numeric string parses");
        assert_eq!(p["flag"], serde_json::json!(true));
        assert_eq!(
            p["tags"],
            serde_json::json!(["a", "b"]),
            "set canonical form: sorted + deduped"
        );
        assert_eq!(
            p["nums"],
            serde_json::json!([7]),
            "bare scalar lifts to a singleton"
        );
        assert_eq!(
            p["free"],
            serde_json::json!("kept"),
            "undeclared fields keep the guess"
        );
        // The coerced create then passes the gate.
        assert!(vet_events(&events, &HashMap::new(), &config).is_ok());

        // Without [task_types] nothing is touched (the schema-agnostic floor).
        let before = events[0].payload.clone();
        let mut untouched = events.clone();
        coerce_event_fields(&mut untouched, &raw, &HashMap::new(), &Config::default());
        assert_eq!(untouched[0].payload, before);
    }

    #[test]
    fn accumulate_dispatch_follows_declared_kinds() {
        use crate::test_support::{state, task};
        let config: Config = toml::from_str(
            r#"
[task_types.bug.fields]
points = "uint"
tags = "set<string>"
notes = "string"
flag = "bool"
"#,
        )
        .unwrap();
        let existing = state(&[task(
            "t",
            &[],
            &[
                ("task_type", serde_json::json!("bug")),
                ("points", serde_json::json!(3)),
                ("tags", serde_json::json!(["a"])),
            ],
        )]);
        let ops = |append: &[(&str, Value)], subtract: &[(&str, Value)]| FieldOps {
            set: Map::new(),
            append: append
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            subtract: subtract
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            raw: Map::new(),
        };

        // `+=` dispatch: numeric -> Add (string operand parses), set -> Add
        // (scalar lifts to an element array), string/undeclared -> Append.
        let events = build_field_events(
            "t",
            &ops(
                &[
                    ("points", serde_json::json!("2")),
                    ("tags", serde_json::json!("b")),
                    ("notes", serde_json::json!("x")),
                    ("free", serde_json::json!("y")),
                ],
                &[("points", serde_json::json!(1))],
            ),
            &existing,
            &config,
        )
        .unwrap();
        let by_op = |op: OpType| {
            events
                .iter()
                .find(|e| e.op == op)
                .map(|e| e.payload.clone())
                .unwrap_or_default()
        };
        let add = by_op(OpType::Add);
        assert_eq!(add["points"], serde_json::json!(2), "operand parsed");
        assert_eq!(add["tags"], serde_json::json!(["b"]), "scalar lifted");
        let append = by_op(OpType::Append);
        assert!(
            append.contains_key("notes") && append.contains_key("free"),
            "strings and undeclared stay text appends"
        );
        assert_eq!(by_op(OpType::Remove)["points"], serde_json::json!(1));

        // Rejections: `+=` on bool, `-=` without a numeric/set declaration.
        let err = build_field_events(
            "t",
            &ops(&[("flag", serde_json::json!("true"))], &[]),
            &existing,
            &config,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not defined"), "{err}");
        let err = build_field_events(
            "t",
            &ops(&[], &[("free", serde_json::json!(1))]),
            &existing,
            &config,
        )
        .unwrap_err();
        assert!(err.to_string().contains("declared as a number"), "{err}");

        // No schema at all: `+=` is plain text append (the floor).
        let events = build_field_events(
            "t",
            &ops(&[("points", serde_json::json!(2))], &[]),
            &existing,
            &Config::default(),
        )
        .unwrap();
        assert_eq!(events[0].op, OpType::Append, "floor keeps text append");
    }

    #[test]
    fn accumulate_no_ops_drop_and_results_validate() {
        use crate::test_support::{state, task};
        let config: Config =
            toml::from_str("[task_types.bug.fields]\npoints = \"uint\"\ntags = \"set<string>\"\n")
                .unwrap();
        let existing = state(&[task(
            "t",
            &[],
            &[
                ("task_type", serde_json::json!("bug")),
                ("points", serde_json::json!(3)),
                ("tags", serde_json::json!(["a"])),
            ],
        )]);
        let ops = |append: &[(&str, Value)], subtract: &[(&str, Value)]| FieldOps {
            set: Map::new(),
            append: append
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            subtract: subtract
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            raw: Map::new(),
        };

        // No-op accumulates are dropped by the gate: inserting a present set
        // element and adding 0 write nothing.
        let noop = build_field_events(
            "t",
            &ops(
                &[
                    ("tags", serde_json::json!("a")),
                    ("points", serde_json::json!(0)),
                ],
                &[],
            ),
            &existing,
            &config,
        )
        .unwrap();
        assert!(
            vet_events(&noop, &existing, &config).unwrap().is_empty(),
            "no-op accumulates never reach the log"
        );

        // A uint underflow is rejected by whole-task validation of the
        // previewed RESULT.
        let underflow = build_field_events(
            "t",
            &ops(&[], &[("points", serde_json::json!(5))]),
            &existing,
            &config,
        )
        .unwrap();
        let err = vet_events(&underflow, &existing, &config).unwrap_err();
        assert!(
            err.to_string().contains("expected uint"),
            "underflow caught by the result check: {err}"
        );
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
