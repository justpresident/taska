//! `ta` command-line surface: argument parsing, dispatch, and shared plumbing.
//!
//! This module owns the clap definitions and `run()`/dispatch. Each subcommand's
//! handler lives in [`commands`]; the cross-cutting helpers handlers reach for —
//! raw materialization ([`replay`]), parsing `key=value` fields
//! ([`parse_field_ops`]), and confirming destructive actions ([`confirm`]) — live
//! here so the handlers stay thin. The data work itself is the frontend-agnostic
//! [`crate::action`] layer (display state, warnings, every command's typed
//! outcome). The write gate and `[task_types]` schema law
//! (event vetting, conformance, coercion) are NOT here — they live in the
//! crate-level [`crate::schema`] module, the frontend-agnostic domain layer
//! every frontend funnels writes through; this module only adds the CLI's
//! presentation (e.g. printing the non-conformance warning). Handlers depend
//! on the [`EventStore`] abstraction rather than the concrete [`FileStore`],
//! so they can be exercised against any store.

use std::collections::HashMap;

use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::engine::Engine;
use crate::error::DynError;
use crate::format::{DisplayArgs, OutputArgs};
use crate::merge;
use crate::model::{MutationEvent, TaskState, RESERVED_FIELD_KEYS};
use crate::storage::{EventStore, FileStore};

mod commands;
use commands::{
    cmd_compact, cmd_config, cmd_create, cmd_delete, cmd_dep_group, cmd_edit, cmd_init, cmd_list,
    cmd_repair, cmd_resolve, cmd_show, cmd_status, cmd_undo, cmd_update, ConfigAction, DepAction,
};

use crate::schema::FieldOps;

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
        /// `field~regex` (or `field=~regex`), `field!=value`, `field!~regex`, or a
        /// comparison `field>value`/`>=`/`<`/`<=` (numbers compare numerically,
        /// strings/dates lexicographically; a cross-type compare never matches).
        /// Quote comparisons so the shell doesn't treat `>`/`<` as redirection:
        /// `ta list 'unblocks>0' 'priority>=4'`. `field` may be a task field,
        /// `id`, `deps` (any edge), a relationship type (`depends_on=x`) or
        /// inverse name (`subtask_of=epic`, `blocks=x`), or a computed column
        /// (`unblocks`/`blocked_by`/`subtasks`). A MULTI-VALUED field (a set/
        /// array field, `deps`, or a relationship type) matches if ANY element
        /// does — `tags=urgent` (member), `scores>=5` (some score >= 5) — while
        /// `!=`/`!~` hold when NONE does (so also when it's empty/absent). With
        /// none given, lists every task.
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
    /// Edit a task's fields in `$EDITOR`: `ta edit <id>` (alias `ed`)
    ///
    /// Round-trips the task's current fields through `$VISUAL`/`$EDITOR` (else
    /// `vi`) as TOML (default) or JSON (`--json`). Save to apply the diff: a
    /// changed or added field is set, a deleted field is unset. On a syntax,
    /// naming, or schema error the message prints to stderr and you're offered to
    /// re-edit the same file. Relationships are managed with `ta dep`, not here.
    #[command(visible_alias = "ed")]
    Edit {
        id: String,
        /// Edit as pretty-printed JSON instead of TOML.
        #[arg(long, conflicts_with = "toml")]
        json: bool,
        /// Edit as TOML (the default).
        #[arg(long)]
        toml: bool,
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
        Commands::Create { id, fields } => cmd_create(store, &id, &fields),
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
        } => cmd_list(
            store,
            &criteria,
            open,
            ready,
            &display,
            &store.config().display,
        ),
        Commands::Show { id, display } => cmd_show(store, &id, &display, &store.config().display),
        Commands::Edit { id, json, toml: _ } => cmd_edit(store, &id, json),
        Commands::Status { output } => cmd_status(store, &output),
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

/// Render a read's [`Warning`](crate::action::Warning)s to stderr — the CLI's
/// presentation of the data [`crate::action::read`] returns. Never blocks the
/// read; the nonconformance warning is already gated (by config) in the action.
pub(crate) fn print_warnings(warnings: &[crate::action::Warning]) {
    use crate::action::Warning;
    for warning in warnings {
        match warning {
            Warning::Orphans(n) => eprintln!(
                "taska: warning: {n} orphaned event(s) in the log (no matching task) — \
                 run `ta resolve` to clean them up."
            ),
            Warning::NonConformance(report) => {
                if let Some(example) = report.first() {
                    eprintln!(
                        "taska: warning: {} task(s) do not conform to their task-type schema \
                         (e.g. {example}) — `ta config validate` lists them, `ta repair \
                         --schema` applies the lossless fixes; writes to such a task must bring \
                         it into conformance. Silence with `workflow.warn_nonconforming = false`.",
                        report.len()
                    );
                }
            }
        }
    }
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
}
