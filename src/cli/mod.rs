//! `ta` command-line surface: argument parsing, dispatch, and shared plumbing.
//!
//! This module owns the clap definitions and `run()`/dispatch. Each subcommand's
//! handler lives in [`commands`]; the cross-cutting helpers handlers reach for —
//! materializing state ([`state_of`]/[`replay`]), parsing `key=value` fields
//! ([`parse_field_ops`]), and confirming destructive actions ([`confirm`]) — live
//! here so the handlers stay thin. The write gate and `[task_types]` schema law
//! (event vetting, conformance, coercion) live in the `schema` submodule.
//! Handlers depend on the [`EventStore`] abstraction rather than the concrete
//! [`FileStore`], so they can be exercised against any store.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

use crate::config::{Config, RelationshipDef};
use crate::engine::Engine;
use crate::error::DynError;
use crate::format::{DisplayArgs, OutputArgs};
use crate::merge;
use crate::model::{MutationEvent, TaskState, RESERVED_FIELD_KEYS, STATUS_KEY, TASK_TYPE_KEY};
use crate::storage::{EventStore, FileStore};

mod commands;
use commands::{
    cmd_compact, cmd_config, cmd_create, cmd_delete, cmd_dep_group, cmd_init, cmd_list, cmd_repair,
    cmd_resolve, cmd_show, cmd_status, cmd_undo, cmd_update, ConfigAction, DepAction,
};

mod schema;
pub(crate) use schema::{
    build_field_events, coerce_event_fields, coerce_value, declared_field_key,
    schema_conformance_report, schema_default_stamps, vet_events,
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
    /// Create a new task: `ta create <id> [field=value ...]`
    ///
    /// Errors if `<id>` already exists or a field name is reserved/computed
    /// (`id`, `deps`, the timestamp/graph columns, relationship type names).
    /// Fields are free-form until `[task_types]` declares schemas — then the
    /// task must conform to its type (every violation reported in one error).
    Create {
        id: String,
        /// Fields as `key=value` (parsed as JSON when possible). `key=@FILE` reads
        /// the value from a file, `key=@-` from stdin; `key=@@x` is a literal `@x`.
        fields: Vec<String>,
    },
    /// Update a task: `=` sets, `+=` accumulates, `-=` removes (e.g. `points+=2`)
    ///
    /// The task must exist; a write that changes nothing is dropped (nothing is
    /// logged), and `+=`/`-=` are rejected on the single-valued status and
    /// task-type fields.
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
    schema::warn_nonconforming(&state, store.config());
    // Then the default substitution: missing/invalid declared fields READ as
    // their declared default (after the warning, so the report reflects the
    // stored truth; display-only, like everything below).
    schema::substitute_schema_defaults(&mut state, store.config());
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
