//! `ta` command-line surface: argument parsing, dispatch, and shared plumbing.
//!
//! This module owns the clap definitions and `run()`/dispatch. Each subcommand's
//! handler lives in [`commands`]; the cross-cutting helpers handlers reach for —
//! materializing state ([`state_of`]/[`replay`]), parsing `key=value` fields
//! ([`parse_fields`]), and confirming destructive actions ([`confirm`]) — live
//! here so the handlers stay thin. Handlers depend on the [`EventStore`]
//! abstraction rather than the concrete [`FileStore`], so they can be exercised
//! against any store.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::engine::Engine;
use crate::error::DynError;
use crate::format::{DisplayArgs, OutputFormat};
use crate::merge;
use crate::model::{MutationEvent, TaskState};
use crate::storage::{EventStore, FileStore};

mod commands;
use commands::{
    cmd_compact, cmd_config, cmd_create, cmd_delete, cmd_dep_group, cmd_init, cmd_list, cmd_ready,
    cmd_resolve, cmd_show, cmd_status, cmd_undo, cmd_update, ConfigAction, DepAction,
};

#[derive(Parser)]
#[command(name = "ta", version = "0.1.0", about = "Taska Event Log Engine")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a taska repository environment
    Init,
    /// Create a new schema-agnostic task: `ta create <id> [field=value ...]`
    Create {
        id: String,
        /// Fields as `key=value` (parsed as JSON when possible). `key=@FILE` reads
        /// the value from a file, `key=@-` from stdin; `key=@@x` is a literal `@x`.
        fields: Vec<String>,
    },
    /// Update a task: `=` sets a field, `+=` appends (e.g. `status=done log+=note`)
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
    /// Delete a task: `ta delete <id>`
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
            display,
        } => {
            let workflow = store.config().workflow.clone();
            cmd_list(
                store,
                &criteria,
                open,
                &workflow,
                &display,
                &store.config().display,
            )
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

/// Load and materialize the current task map from any store.
///
/// Replay also reports *orphaned* events — `Update`/`AddDep`/`RemoveDep`/`Delete`
/// events whose target task no longer exists, which apply to nothing. They are a
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
