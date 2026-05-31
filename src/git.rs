//! Git integration: register the custom merge driver for the event log.
//!
//! Separated from the CLI because wiring up Git is its own responsibility that
//! changes independently of argument parsing or command dispatch.
//!
//! Note the split: `.gitattributes` is committed and travels with the repo, but
//! the `merge.taska-merge-driver.driver` setting lives in *local* git config,
//! which is per-clone and never committed. So a fresh clone of a repo that
//! already has a `.taska` store still needs the driver registered locally —
//! which is why [`setup`] is idempotent and safe to re-run via `ta init`.

use std::io::Write;
use std::path::Path;

use crate::error::DynError;

const DRIVER_NAME: &str = "taska-merge-driver";
const MUTATIONS_PATH: &str = ".taska/mutations.jsonl";

/// Wire up the `.gitattributes` entry and local git merge driver for the event
/// log.
///
/// Idempotent and safe to call at any time: the attribute line is only added if
/// absent, and `git config` simply re-asserts the same values. Best-effort — a
/// missing git repo warns rather than failing, so `ta init` still works in a
/// plain directory.
pub fn setup(repo_root: &Path) -> Result<(), DynError> {
    ensure_gitattributes(repo_root)?;
    if register_merge_driver(repo_root) {
        println!("Configured git merge driver for {}", MUTATIONS_PATH);
    } else {
        eprintln!("warning: could not configure git merge driver (is this a git repo?)");
    }
    Ok(())
}

/// Append the merge attribute if it isn't already present.
fn ensure_gitattributes(repo_root: &Path) -> Result<(), DynError> {
    let line = format!("{} merge={}", MUTATIONS_PATH, DRIVER_NAME);
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

/// Register the merge driver in local git config. Returns whether both config
/// writes succeeded; messaging and error policy are left to the caller.
fn register_merge_driver(repo_root: &Path) -> bool {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .status()
    };
    let name = git(&[
        "config",
        &format!("merge.{}.name", DRIVER_NAME),
        "Taska Auto-Resolution Log Consolidation Driver",
    ]);
    let driver = git(&[
        "config",
        &format!("merge.{}.driver", DRIVER_NAME),
        "ta git-merge %O %A %B %P",
    ]);
    matches!((name, driver), (Ok(a), Ok(b)) if a.success() && b.success())
}
