//! Git integration: register the custom merge drivers for the event log.
//!
//! Separated from the CLI because wiring up Git is its own responsibility that
//! changes independently of argument parsing or command dispatch.
//!
//! Note the split: `.gitattributes` is committed and travels with the repo, but
//! the `merge.<driver>.driver` settings live in *local* git config, which is
//! per-clone and never committed. So a fresh clone of a repo that already has a
//! `.taska` store still needs the drivers registered locally — which is why
//! [`setup`] is idempotent and safe to re-run via `ta init`.

use std::io::Write;
use std::path::Path;

use crate::error::DynError;

const LOG_DRIVER: &str = "taska-merge-driver";
const BASELINE_DRIVER: &str = "taska-baseline-keep-ours";
const MUTATIONS_PATH: &str = ".taska/mutations.jsonl";
const BASELINE_PATH: &str = ".taska/baseline.jsonl";

/// Wire up the `.gitattributes` entries and local git merge drivers: a restack
/// driver for the event log, and a keep-ours driver for the compacted baseline.
///
/// Idempotent and safe to call at any time: attribute lines are only added if
/// absent, and `git config` simply re-asserts the same values. Best-effort — a
/// missing git repo warns rather than failing, so `ta init` still works in a
/// plain directory.
pub fn setup(repo_root: &Path) -> Result<(), DynError> {
    ensure_gitattribute(repo_root, MUTATIONS_PATH, LOG_DRIVER)?;
    ensure_gitattribute(repo_root, BASELINE_PATH, BASELINE_DRIVER)?;

    let log_ok = register_driver(
        repo_root,
        LOG_DRIVER,
        "Taska Auto-Resolution Log Consolidation Driver",
        "ta git-merge %O %A %B %P",
    );
    let baseline_ok = register_driver(
        repo_root,
        BASELINE_DRIVER,
        "Taska Baseline Keep-Ours Driver",
        "ta git-merge-baseline %O %A %B %P",
    );

    if log_ok && baseline_ok {
        println!("Configured git merge drivers for the taska event log");
    } else {
        eprintln!("warning: could not configure git merge drivers (is this a git repo?)");
    }
    Ok(())
}

/// Append a `<path> merge=<driver>` attribute line if it isn't already present.
fn ensure_gitattribute(repo_root: &Path, file: &str, driver: &str) -> Result<(), DynError> {
    let line = format!("{} merge={}", file, driver);
    let attrs_path = repo_root.join(".gitattributes");
    let existing = std::fs::read_to_string(&attrs_path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&attrs_path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, "{}", line)?;
    Ok(())
}

/// Register one merge driver in local git config. Returns whether both config
/// writes succeeded; messaging and error policy are left to the caller.
fn register_driver(repo_root: &Path, name: &str, description: &str, driver_cmd: &str) -> bool {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .status()
    };
    let name_set = git(&["config", &format!("merge.{}.name", name), description]);
    let driver_set = git(&["config", &format!("merge.{}.driver", name), driver_cmd]);
    matches!((name_set, driver_set), (Ok(a), Ok(b)) if a.success() && b.success())
}
