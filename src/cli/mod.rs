//! `ta` command-line surface: argument parsing, dispatch, and shared plumbing.
//!
//! This module owns the clap definitions and `run()`/dispatch. Each subcommand's
//! handler lives in [`commands`]; the cross-cutting helpers handlers reach for -
//! raw materialization ([`replay`]), parsing `key=value` fields
//! ([`parse_field_ops`]), and confirming destructive actions ([`confirm`]) - live
//! here so the handlers stay thin. The data work itself is the frontend-agnostic
//! [`crate::action`] layer (display state, warnings, every command's typed
//! outcome). The write gate and `[task_types]` schema law
//! (event vetting, conformance, coercion) are NOT here - they live in the
//! crate-level [`crate::schema`] module, the frontend-agnostic domain layer
//! every frontend funnels writes through; this module only adds the CLI's
//! presentation (e.g. printing the non-conformance warning). Handlers depend
//! on the [`EventStore`] abstraction rather than the concrete [`FileStore`],
//! so they can be exercised against any store.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::engine::Engine;
use crate::error::DynError;
use crate::format::{DisplayArgs, OutputArgs};
use crate::merge;
use crate::model::{MutationEvent, TaskState, RESERVED_FIELD_KEYS};
use crate::storage::{EventStore, FileStore};

mod commands;
pub(crate) mod complete;
use clap_complete::engine::{ArgValueCandidates, ArgValueCompleter};
use commands::{
    cmd_compact, cmd_completions, cmd_config, cmd_create, cmd_delete, cmd_dep_group, cmd_edit,
    cmd_init, cmd_list, cmd_prime, cmd_repair, cmd_resolve, cmd_self_update, cmd_show, cmd_status,
    cmd_undo, cmd_update, cmd_watch, parse_duration, ConfigAction, DepAction, InstallScope,
};

use crate::schema::FieldOps;

#[derive(Parser)]
#[command(name = "ta", version, about)]
struct Cli {
    /// Run as if `ta` were started in <DIR> (like `git -C`). Store discovery,
    /// relative `@FILE` paths, and `init`'s repo-root search all resolve from
    /// there - e.g. `ta -C ../main list` drives a worktree's main checkout store.
    #[arg(short = 'C', long = "directory", value_name = "DIR", global = true)]
    directory: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a taska repository environment
    Init {
        /// Create the store and `.gitattributes` but don't git-commit them
        #[arg(long)]
        no_commit: bool,
    },
    /// Create a new task: `ta create <id> [field=value ...]`
    ///
    /// Errors if `<id>` already exists or a field name is reserved/computed
    /// (`id`, `deps`, the timestamp/graph columns, relationship type names).
    /// Fields are free-form until `[task_types]` declares schemas - then the
    /// task must conform to its type (every violation reported in one error).
    /// A field name no task uses yet is rejected (with a did-you-mean) unless
    /// `--new-field` is passed, so a typo can't silently create a phantom column.
    Create {
        id: String,
        /// Fields as `key=value` (parsed as JSON when possible). `key=@FILE` reads
        /// the value from a file, `key=@-` from stdin; `key=@@x` is a literal `@x`.
        fields: Vec<String>,
        /// Allow field names the store has never seen (otherwise a never-before-used
        /// name is rejected as a likely typo). The first task on an empty store is
        /// exempt - it seeds the vocabulary.
        #[arg(long)]
        new_field: bool,
    },
    /// Update a task: `=` sets, `+=` accumulates, `-=` removes (e.g. `points+=2`)
    ///
    /// The task must exist; a write that changes nothing is dropped (nothing is
    /// logged), and `+=`/`-=` are rejected on the single-valued status and
    /// task-type fields.
    Update {
        #[arg(add = ArgValueCandidates::new(complete::task_ids))]
        id: String,
        /// `key=value` sets a field; `key+=value` appends text (string fields),
        /// adds (declared numeric fields), or inserts elements (declared set<...>
        /// fields); `key-=value` subtracts / removes elements (declared
        /// numeric/set only). Accumulates merge conflict-free across branches.
        /// Values parse as JSON-or-string; `key=@FILE` / `key=@-` read from a
        /// file / stdin. At least one required.
        #[arg(required = true)]
        fields: Vec<String>,
        /// Allow field names the store has never seen (otherwise a never-before-used
        /// name is rejected as a likely typo - did-you-mean included).
        #[arg(long)]
        new_field: bool,
    },
    /// Add/remove typed relationship edges: `ta dep add <task> <type>=<target> ...`
    Dep {
        #[command(subcommand)]
        action: DepAction,
    },
    /// Delete a task: `ta delete <id>` (errors if it doesn't exist)
    Delete {
        #[arg(add = ArgValueCandidates::new(complete::task_ids))]
        id: String,
    },
    /// List tasks, optionally filtered: `ta list status=~open priority=3 --open`
    List {
        /// Filter criteria, all of which must match: `field=value` (exact),
        /// `field=~regex` (perl/bash spelling), `field!=value`, `field!~regex`, or
        /// a comparison `field>value`/`>=`/`<`/`<=` (numbers compare numerically,
        /// strings/dates lexicographically; a cross-type compare never matches).
        /// Quote comparisons so the shell doesn't treat `>`/`<` as redirection:
        /// `ta list 'unblocks>0' 'priority>=4'`. `field` may be a task field,
        /// `id`, `deps` (any edge), a relationship type (`depends_on=x`) or
        /// inverse name (`subtask_of=epic`, `blocks=x`), or a computed column
        /// (`unblocks`/`blocked_by`/`subtasks`). A MULTI-VALUED field (a set/
        /// array field, `deps`, or a relationship type) matches if ANY element
        /// does - `tags=urgent` (member), `scores>=5` (some score >= 5) - while
        /// `!=`/`!~` hold when NONE does (so also when it's empty/absent). With
        /// none given, lists every task.
        #[arg(add = ArgValueCompleter::new(complete::criteria))]
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
    /// Watch for tasks matching a filter to change past a cursor, then print a diff
    ///
    /// Blocks until a task matching the (list-style) criteria is created, updated,
    /// or deleted with `seq` greater than `--since`, then prints a per-task diff of
    /// what changed and exits 0. On the first change it waits `--holdout` to batch a
    /// burst. If nothing matches before `--timeout`, prints `No updates yet` to
    /// stderr and exits 1. Seed `--since` from `ta status --current` (or the
    /// `[seq:N]` any mutation prints); computed columns aren't available as filters.
    Watch {
        /// Filter criteria, same grammar as `ta list` (`field=value`, `field=~regex`,
        /// `field!=value`, comparisons); with none given, every changed task matches.
        #[arg(add = ArgValueCompleter::new(complete::criteria))]
        criteria: Vec<String>,
        /// Only report tasks that are not done.
        #[arg(long)]
        open: bool,
        /// Only report tasks ready to work on: not done and every dependency done.
        #[arg(long)]
        ready: bool,
        /// Only report changes after this mutation seq (from `ta status --current`
        /// or the `[seq:N]` any mutation prints).
        #[arg(long)]
        since: u64,
        /// How long to block waiting for a change, e.g. `9m`, `1m30s`, `1h`.
        /// The default stays under common 10-minute foreground caps; for longer
        /// waits pass a bigger value and run it backgrounded.
        #[arg(long, value_parser = parse_duration, default_value = "9m")]
        timeout: Duration,
        /// After the first change, wait this long to batch a burst before printing.
        #[arg(long, value_parser = parse_duration, default_value = "10s")]
        holdout: Duration,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Show one or more tasks in full by id: `ta show <id>...` (duplicates ignored)
    Show {
        /// One or more task ids; duplicate ids are shown once.
        #[arg(required = true, num_args = 1.., add = ArgValueCandidates::new(complete::task_ids))]
        ids: Vec<String>,
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
        #[arg(add = ArgValueCandidates::new(complete::task_ids))]
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
        /// Print only the store's current high-water mutation `seq` - the cursor
        /// `ta watch --since` takes - instead of the counts. Seeds a watch loop:
        /// `SINCE=$(ta status --current)`.
        #[arg(long)]
        current: bool,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Print a config-tailored agent primer for this store (`--format json` for the raw facts)
    Prime {
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Set up (or `--install`) shell completion for `ta`
    ///
    /// Completion is DYNAMIC and store-aware: besides subcommands and flags it
    /// completes task ids, `list` filter fields, and column names, read live from
    /// the `.taska` store in the current directory - so it always matches your data
    /// and config. By default the registration script is printed; `--install`
    /// writes it into your shell's completion directory instead.
    #[command(
        after_help = "Easiest - let `ta` install it (asks user vs system, uses sudo for a system path):\n\n  ta completions bash --install\n  ta completions zsh  --install user\n  ta completions fish --install system\n\nOr source the output yourself from your shell's startup file:\n\n  echo 'source <(ta completions bash)' >> ~/.bashrc\n\nThen start a new shell. (`ta init` and the installer also set this up on a TTY, asking only user vs system.)"
    )]
    Completions {
        /// The shell to generate the completion script for
        shell: clap_complete::Shell,
        /// Install it into your shell instead of printing it. Optionally `user` or
        /// `system`; with no value you're asked where (and prompted for sudo if a
        /// system path needs root).
        #[allow(clippy::option_option)] // clap idiom: absent / `--install` / `--install x`
        #[arg(long, num_args = 0..=1, value_name = "user|system")]
        install: Option<Option<InstallScope>>,
    },
    /// Update `ta` itself to the latest released version
    ///
    /// Downloads this platform's prebuilt binary from the latest GitHub release
    /// and replaces the running executable in place (the one resolved via
    /// `current_exe`, so an update can't land on a copy you don't run). Platforms
    /// without a prebuilt binary are pointed at `cargo install taska`.
    SelfUpdate {
        /// Only report the current and latest versions; install nothing.
        #[arg(long)]
        check: bool,
        /// Reinstall even when already on the latest version.
        #[arg(long)]
        force: bool,
    },
    /// Undo event(s), walking back through real history: `ta undo [--seq S] [--count N] [--remove] [--force]`
    ///
    /// With no flags, undoes the most recent undoable event; run it again and it
    /// keeps walking older, skipping anything already undone (it never bounces on
    /// its own compensations). `--seq S` targets a specific event; `--count N`
    /// undoes N events from the start point, going older.
    Undo {
        /// Undo the specific event at this seq (default: the most recent undoable event)
        #[arg(long)]
        seq: Option<u64>,
        /// How many events to undo, walking back from the start point (default 1)
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
    /// ambiguous is reported with a suggested command - repair never guesses,
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
        /// An explicit migration choice - never inferred, even when only one
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

/// Whether `p` is a regular, executable file.
/// Whether `p` is a regular, executable file.
#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(std::fs::Metadata::is_file)
}

/// The distinct real `ta` binaries reachable via `path` (a `PATH` value), as
/// `(displayed, canonical)` pairs in PATH order - deduped by canonical target so a
/// symlink to an already-seen binary isn't double-counted.
fn shadowed_binaries(path: &std::ffi::OsStr) -> Vec<(PathBuf, PathBuf)> {
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    for dir in std::env::split_paths(path) {
        let candidate = dir.join("ta");
        if !is_executable_file(&candidate) {
            continue;
        }
        let target = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if found.iter().any(|(_, t)| *t == target) {
            continue;
        }
        found.push((candidate, target));
    }
    found
}

/// One `ta` on PATH: its displayed path, resolved version (`None` if unreadable),
/// and whether it's the binary currently executing.
struct ShadowEntry {
    display: PathBuf,
    version: Option<(u64, u64, u64)>,
    running: bool,
}

/// Parse `major.minor.patch` from clap's `--version` line (`ta 0.5.0`): the first
/// whitespace token starting with a digit, pre-release/build metadata dropped.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let tok = text
        .split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))?;
    let core = tok.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Run `<path> --version` to read a sibling's version. Sets `TASKA_VERSION_PROBE`
/// so the probed binary skips its OWN shadow check - no recursive probing.
fn probe_version(path: &Path) -> Option<(u64, u64, u64)> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .env("TASKA_VERSION_PROBE", "1")
        .output()
        .ok()?;
    if out.status.success() {
        parse_version(&String::from_utf8_lossy(&out.stdout))
    } else {
        None
    }
}

/// Single-quote a path for a copy-pasteable shell command.
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

fn fmt_ver((a, b, c): (u64, u64, u64)) -> String {
    format!("{a}.{b}.{c}")
}

/// The multi-line advisory: a `version  path` table marking the running and newest
/// copies, then a concrete `rm` that keeps only the newest. Pure (no I/O) - so the
/// formatting + keep/remove choice is unit-testable.
fn shadow_recommendation(entries: &[ShadowEntry]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "warning: {} `ta` binaries are on PATH (the first shadows the rest):",
        entries.len()
    );
    let newest = entries.iter().filter_map(|e| e.version).max();
    for e in entries {
        let v = e.version.map_or_else(|| "?".to_string(), fmt_ver);
        let mut tag = String::new();
        if e.running {
            tag.push_str(" <- running");
        }
        if e.version.is_some() && e.version == newest {
            tag.push_str(" (newest)");
        }
        let _ = writeln!(s, "  {v:<8} {}{tag}", e.display.display());
    }
    let Some(newest) = newest else {
        let _ = write!(
            s,
            "hint: couldn't read their versions - compare with `<path> --version` and remove the older copies."
        );
        return s;
    };
    // Keep one newest copy (prefer the running one); recommend removing the rest.
    let keep = entries
        .iter()
        .position(|e| e.running && e.version == Some(newest))
        .or_else(|| entries.iter().position(|e| e.version == Some(newest)));
    let Some(keep) = keep else { return s };
    let rm: Vec<String> = entries
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != keep)
        .map(|(_, e)| shell_quote(&e.display))
        .collect();
    if rm.is_empty() {
        return s;
    }
    if entries[keep].running {
        let _ = writeln!(
            s,
            "to keep only the newest (already running) and drop the rest, run:"
        );
    } else {
        let _ = writeln!(
            s,
            "the running `ta` is NOT the newest ({}); to keep only the newest, run:",
            fmt_ver(newest)
        );
    }
    let _ = write!(s, "  rm {}", rm.join(" "));
    s
}

/// Warn (to stderr) when more than one `ta` is on `PATH` - the first shadows the
/// rest, so e.g. `cargo install taska` can update a copy the user never runs. We
/// probe each sibling's `--version` (guarded against recursion) and recommend the
/// exact `rm` to keep only the newest. TTY-gated, so it never spams scripts, CI,
/// or the merge driver; purely advisory - it never deletes anything itself.
fn warn_shadowed_binaries() {
    // Skip when we're a sibling being probed (see `probe_version`) or non-interactive.
    if std::env::var_os("TASKA_VERSION_PROBE").is_some() || !std::io::stderr().is_terminal() {
        return;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return;
    };
    let found = shadowed_binaries(&path);
    if found.len() < 2 {
        return;
    }
    let running = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let entries: Vec<ShadowEntry> = found
        .into_iter()
        .map(|(display, canonical)| {
            let is_running = running.as_ref() == Some(&canonical);
            // Our own version is known; only the OTHERS need an exec probe.
            let version = if is_running {
                parse_version(env!("CARGO_PKG_VERSION"))
            } else {
                probe_version(&canonical)
            };
            ShadowEntry {
                display,
                version,
                running: is_running,
            }
        })
        .collect();
    eprintln!("{}", shadow_recommendation(&entries));
}

/// Honor `-C <DIR>` / `--directory <DIR>` from the RAW args, before clap parses,
/// so the COMPLETION path can use it too. A completion callback is served by
/// `complete()` and EXITS before the authoritative post-parse chdir in `run()` -
/// yet the store-aware completers discover from the cwd, so without this they'd
/// complete against the wrong store. Best-effort (a missing/invalid value is
/// ignored, degrading to cwd discovery); the real-command path re-applies `-C`
/// from the clap-parsed value and errors loudly on a bad dir. Handles every form
/// clap accepts: `-C x`, `-Cx`, `-C=x`, `--directory x`, `--directory=x`.
fn chdir_to_directory_flag() {
    use std::ffi::OsString;
    let mut dir: Option<OsString> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-C" | "--directory") => dir = args.next(),
            Some(s) if s.starts_with("--directory=") => {
                dir = Some(OsString::from(&s["--directory=".len()..]));
            }
            Some(s) if s.starts_with("-C") && s.len() > 2 => {
                let rest = &s[2..];
                dir = Some(OsString::from(rest.strip_prefix('=').unwrap_or(rest)));
            }
            _ => {}
        }
    }
    if let Some(dir) = dir.filter(|d| !d.is_empty()) {
        let _ = std::env::set_current_dir(&dir); // best-effort: bad dir -> cwd discovery
    }
}

/// Parse args and dispatch. `main` maps the result to an exit code.
pub fn run() -> Result<(), DynError> {
    // A completion callback (the shell sets `COMPLETE` - the var the registration
    // shim uses) is served by `complete()` below, which EXITS before the
    // post-parse `-C` chdir. Apply `-C` up front so completions target the same
    // store the real command would, not the cwd.
    if std::env::var_os("COMPLETE").is_some() {
        chdir_to_directory_flag();
    }
    // Dynamic completion: when the shell asks (COMPLETE env set), emit candidates
    // and exit BEFORE any other work or output. A normal run returns and proceeds.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    warn_shadowed_binaries();
    let cli = Cli::parse();
    // The authoritative `-C` for a real command (git's `-C` semantics): act as if
    // started in <DIR> before any store discovery or relative `@FILE` resolution.
    // Errors loudly on a bad dir, unlike the best-effort completion pass above.
    if let Some(dir) = cli.directory.as_deref() {
        std::env::set_current_dir(dir).map_err(|e| format!("-C {}: {e}", dir.display()))?;
    }
    match cli.command {
        // Commands that don't operate on an existing store.
        Commands::Init { no_commit } => cmd_init(no_commit),
        Commands::Completions { shell, install } => cmd_completions(shell, install),
        Commands::SelfUpdate { check, force } => cmd_self_update(check, force),
        Commands::GitMerge {
            ancestor,
            current,
            incoming,
            path,
        } => {
            // Read the conflict policy and marker location from the merged
            // file's own store (resolved via %P - see `merge_driver_store`),
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

        // Config viewing/editing must also bypass the validation gate - otherwise
        // a bad hand-edit (e.g. keep_events below the floor) would lock you out of
        // the very command that fixes it. `set` validates the *result* itself.
        Commands::Config { action } => cmd_config(&FileStore::discover()?, action),

        // Repair is the format/data fixer, so it bypasses the format gate (but
        // still needs a valid config - migrations and schema fixes read it).
        Commands::Repair {
            migrate,
            schema,
            rename,
            set_type_if_none,
        } => {
            let store = FileStore::discover()?;
            enforce_config(store.config())?;
            // v1 repair can't read a pre-1.0 store either - refuse rather than
            // load-and-rewrite it (which would drop the legacy edges).
            refuse_if_legacy(&store)?;
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

/// Heal-or-warn on SCM merge protection before every store-backed command.
/// `ensure_scm_health` silently re-registers this clone's merge drivers when
/// `.gitattributes` already declares them (the per-clone definitions a fresh
/// clone lacks); the residual warning printed here covers only what it can't fix
/// (missing `.gitattributes` entries, a failed registration, or an unsupported
/// SCM), each pointing at its remedy. Warning-only, unlike the enforce gates:
/// the store itself is healthy, it's the clone's merge protection that may be
/// incomplete.
fn warn_scm_health(store: &FileStore) {
    if let Some(warning) = store.repo_root().and_then(crate::git::ensure_scm_health) {
        eprintln!("warning: {warning}");
    }
}

/// The store owning the file a merge driver was invoked on. Git runs drivers
/// at the repo root and passes `%P`, the merged file's repo-relative path -
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
/// data. Detection only - `repair` bypasses this and does the migration.
fn enforce_format(store: &FileStore) -> Result<(), DynError> {
    refuse_if_legacy(store)?;
    let snap = crate::migrate::Snapshot {
        log: store.load_mutations()?,
        baseline: store.load_baseline()?,
    };
    if let Some(reason) = crate::migrate::pending(&snap, store.config()) {
        return Err(format!(
            "{reason}. The store is in an older on-disk format - run \
             `ta repair --migrate` to update it."
        )
        .into());
    }
    Ok(())
}

/// Refuse a store written in a PRE-1.0 on-disk format (the read shims are gone,
/// so reading it would silently drop its legacy edges), pointing at the last
/// 0.x's `ta repair --migrate`. Shared by the format gate and `repair` - unlike
/// the v1.0+ migrations, `repair` can't fix a pre-1.0 store either.
fn refuse_if_legacy(store: &FileStore) -> Result<(), DynError> {
    if let Some(reason) = store.detect_legacy_format()? {
        return Err(format!(
            "{reason}: this store is in a pre-1.0 on-disk format this version can't \
             read. Run `ta repair --migrate` with the last 0.x release (e.g. 0.5.x) \
             first, then upgrade."
        )
        .into());
    }
    Ok(())
}

/// Dispatch a command that operates on an already-resolved, already-validated
/// store. Handlers depend only on the `EventStore` abstraction.
fn dispatch_store_command(command: Commands, store: &FileStore) -> Result<(), DynError> {
    match command {
        Commands::Create {
            id,
            fields,
            new_field,
        } => cmd_create(store, &id, &fields, new_field),
        Commands::Update {
            id,
            fields,
            new_field,
        } => cmd_update(store, &id, &fields, new_field),
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
        Commands::Watch {
            criteria,
            open,
            ready,
            since,
            timeout,
            holdout,
            output,
        } => cmd_watch(
            store, &criteria, open, ready, since, timeout, holdout, &output,
        ),
        Commands::Show { ids, display } => cmd_show(store, &ids, &display, &store.config().display),
        Commands::Edit { id, json, toml: _ } => cmd_edit(store, &id, json),
        Commands::Status { current, output } => cmd_status(store, current, &output),
        Commands::Prime { output } => cmd_prime(store, &output),
        Commands::Undo {
            seq,
            count,
            force,
            remove,
        } => cmd_undo(store, seq, count, force, remove),
        Commands::Compact => {
            let cfg = store.config().compaction.clone();
            cmd_compact(store, &cfg, Utc::now())
        }
        // Resolved before dispatch in `run`.
        Commands::Init { .. }
        | Commands::Completions { .. }
        | Commands::SelfUpdate { .. }
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

/// Render a read's [`Warning`](crate::action::Warning)s into user-facing message
/// lines (one per warning; a nonconformance warning with an empty report yields
/// none) - the CLI's presentation of the data [`crate::action::read`] returns.
/// Printing is the caller's concern (every command sends them to stderr); this is
/// pure so it never blocks the read, and the nonconformance warning is already
/// gated (by config) in the action.
pub(crate) fn render_warnings(warnings: &[crate::action::Warning]) -> Vec<String> {
    use crate::action::Warning;
    let mut lines = Vec::new();
    for warning in warnings {
        match warning {
            Warning::Orphans(n) => lines.push(format!(
                "taska: warning: {n} orphaned event(s) in the log (no matching task) - \
                 run `ta resolve` to clean them up."
            )),
            Warning::NonConformance(report) => {
                if let Some(example) = report.first() {
                    lines.push(format!(
                        "taska: warning: {} task(s) do not conform to their task-type schema \
                         (e.g. {example}) - `ta config validate` lists them, `ta repair \
                         --schema` applies the lossless fixes; writes to such a task must bring \
                         it into conformance. Silence with `workflow.warn_nonconforming = false`.",
                        report.len()
                    ));
                }
            }
        }
    }
    lines
}

/// Parse `key=value` / `key+=value` tokens into two payload maps: fields to
/// **set** (`=`) and fields to **append** to (`+=`). One `update` can mix both,
/// which the caller emits as an `Update` event plus an `Append` event.
///
/// Values follow the same rules either way: parsed as JSON, falling back to a
/// plain string (so `status=open` stays a string, `priority=3` becomes a number);
/// a value of `@PATH` is read from that file and `@-` from stdin (verbatim, one
/// trailing newline trimmed) - the way to pass long or shell-hostile text without
/// fighting argv quoting; `@@text` escapes to the literal `@text`.
pub(crate) fn parse_field_ops(fields: &[String]) -> Result<FieldOps, DynError> {
    let mut ops = FieldOps {
        set: Map::new(),
        append: Vec::new(),
        subtract: Vec::new(),
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
            // Lists (not maps), in token order, so repeated `field+=`/`field-=`
            // on one field accumulate rather than overwrite.
            '+' => ops.append.push((key.to_string(), value)),
            '-' => ops.subtract.push((key.to_string(), value)),
            _ => {
                // The verbatim token is kept for SET values only - it backs the
                // declared-string coercion, which never applies to operands.
                if !val.starts_with('@') {
                    ops.raw
                        .insert(key.to_string(), Value::String(val.to_string()));
                }
                ops.set.insert(key.to_string(), value);
            }
        }
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
    // Trim a single trailing newline (`\n` or `\r\n`) - files almost always have
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
    use crate::model::{DEPS_KEY, ID_KEY, SEQ_KEY, STATUS_KEY, UNBLOCKS_KEY};
    use crate::test_support::names::*;

    #[test]
    #[cfg(unix)]
    fn shadowed_binaries_dedups_symlinks_and_skips_non_executables() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let base = std::env::temp_dir().join(format!("taska-shadow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mk = |name: &str, mode: u32| {
            let d = base.join(name);
            std::fs::create_dir_all(&d).unwrap();
            let f = d.join("ta");
            std::fs::write(&f, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
            (d, f)
        };
        let (da, fa) = mk("a", 0o755); // executable
        let (db, _) = mk("b", 0o755); // a DIFFERENT executable
        let (dc, fc) = mk("c", 0o755); // will become a symlink to a/ta
        std::fs::remove_file(&fc).unwrap();
        symlink(&fa, &fc).unwrap();
        let (dd, _) = mk("d", 0o644); // present but NOT executable

        let path = std::env::join_paths([&da, &db, &dc, &dd]).unwrap();
        let shown: Vec<_> = shadowed_binaries(&path)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        // a + b are distinct; c is a symlink to a (deduped); d is not executable.
        assert_eq!(shown, vec![da.join("ta"), db.join("ta")], "{shown:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_version_reads_clap_output() {
        assert_eq!(parse_version("ta 0.5.0"), Some((0, 5, 0)));
        assert_eq!(parse_version("ta 1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.6\n"), Some((0, 6, 0)));
        assert_eq!(parse_version("ta unknown"), None);
    }

    #[test]
    fn shadow_recommendation_flags_a_stale_running_copy() {
        let e = |p: &str, v: Option<(u64, u64, u64)>, r: bool| ShadowEntry {
            display: PathBuf::from(p),
            version: v,
            running: r,
        };
        // The running 0.5.0 shadows a newer 0.6.0 sibling.
        let out = shadow_recommendation(&[
            e("/a/ta", Some((0, 5, 0)), true),
            e("/b/ta", Some((0, 6, 0)), false),
        ]);
        assert!(out.contains("0.5.0") && out.contains("/a/ta") && out.contains("<- running"));
        assert!(out.contains("0.6.0") && out.contains("(newest)"));
        assert!(out.contains("NOT the newest"), "{out}");
        // Recommends removing ONLY the stale one, keeping the newest.
        assert!(
            out.contains("rm '/a/ta'") && !out.contains("'/b/ta'"),
            "{out}"
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
        let parsed = Cli::try_parse_from(["ta", "update", "api", &format!("{STATUS_KEY}=open")]);
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
            format!("{STATUS_FIELD}=open"),
            "log+=first".into(),
            "points-=2".into(),
            "priority=3".into(),
            "version=3.10".into(),
        ])
        .unwrap();
        assert_eq!(set[STATUS_FIELD], serde_json::json!("open"));
        assert_eq!(set["priority"], serde_json::json!(3));
        assert_eq!(
            append,
            vec![("log".to_string(), serde_json::json!("first"))]
        );
        assert_eq!(subtract, vec![("points".to_string(), serde_json::json!(2))]);
        assert!(
            !set.contains_key("log")
                && !append.iter().any(|(k, _)| k == STATUS_FIELD)
                && !set.contains_key("points"),
            "each token lands in exactly one bucket"
        );
        // The guess loses "3.10" (-> 3.1); the raw token preserves it for
        // declared-string coercion.
        assert_eq!(set["version"], serde_json::json!(3.1));
        assert_eq!(raw["version"], serde_json::json!("3.10"));
    }

    #[test]
    fn parse_field_ops_keeps_repeated_same_field_tokens_in_order() {
        // The bug `repeated-compound-assign-drops-values` was here: a map slot
        // per field collapsed `tags+=a tags+=b` to just `b`. Now `+=`/`-=` are
        // ordered lists, so both survive for the gate to accumulate.
        let FieldOps {
            set,
            append,
            subtract,
            ..
        } = parse_field_ops(&[
            "tags+=a".into(),
            "tags+=b".into(),
            "scores-=1".into(),
            "scores-=2".into(),
            // Repeated `=` stays last-wins (a map): you're choosing one value.
            "title=first".into(),
            "title=second".into(),
        ])
        .unwrap();
        assert_eq!(
            append,
            vec![
                ("tags".to_string(), serde_json::json!("a")),
                ("tags".to_string(), serde_json::json!("b")),
            ]
        );
        assert_eq!(
            subtract,
            vec![
                ("scores".to_string(), serde_json::json!(1)),
                ("scores".to_string(), serde_json::json!(2)),
            ]
        );
        assert_eq!(
            set["title"],
            serde_json::json!("second"),
            "`=` is last-wins"
        );
    }

    #[test]
    fn parse_field_ops_rejects_reserved_empty_and_opless() {
        // Both reservation reasons reject at parse time: envelope keys (`seq`)
        // and the static computed columns (`id`, `deps`, `unblocks`).
        for key in [SEQ_KEY, ID_KEY, DEPS_KEY, UNBLOCKS_KEY] {
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
