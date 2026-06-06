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
        eprintln!(
            "warning: git merge drivers not configured (not a git repository?); \
             run `git init`, then `ta init` again to enable safe .taska merges"
        );
    }
    Ok(())
}

/// Append a `<path> merge=<driver>` attribute line if it isn't already present.
fn ensure_gitattribute(repo_root: &Path, file: &str, driver: &str) -> Result<(), DynError> {
    let line = format!("{file} merge={driver}");
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
    writeln!(f, "{line}")?;
    Ok(())
}

/// The SCM owning a directory: the nearest `.git` or `.hg` walking UP from
/// `start` (the store's parent — which need not be the SCM root: a `.taska`
/// nested deeper inside a repo is supported). `.git` may be a FILE (worktrees,
/// submodules), so `exists()` not `is_dir()`. `None` means a plain directory.
fn detect_scm(start: &Path) -> Option<Scm> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(Scm::Git);
        }
        if dir.join(".hg").is_dir() {
            return Some(Scm::Mercurial);
        }
        dir = dir.parent()?;
    }
}

enum Scm {
    Git,
    Mercurial,
}

/// A health warning when the store's merge protection is incomplete, for the
/// CLI to print on stderr before every store-backed command.
///
/// Never blocking — the store itself is fine, the *clone* is missing setup:
/// `.gitattributes` travels with the repo, but the driver *definitions* live in
/// per-clone local git config — so `ta init` before `git init`, or any fresh
/// clone, leaves git silently falling back to its text merge on a diverged log
/// (conflict markers inside the JSONL). Detection walks up from the store's
/// parent and is ordered cheapest-first: no SCM anywhere costs only stats and
/// stays quiet (plain-dir use is deliberate; `ta init` warned once); mercurial
/// gets an unsupported-SCM warning; git costs one `git config` spawn (git
/// resolves config from any directory inside the repo, so a nested store needs
/// no special-casing) plus a file read.
pub fn health_warning(repo_root: &Path) -> Option<String> {
    match detect_scm(repo_root)? {
        Scm::Mercurial => Some(
            "mercurial repository detected; taska's merge protection currently \
             supports only git — merging concurrent .taska edits in hg can corrupt \
             the task log"
                .to_string(),
        ),
        Scm::Git => {
            if !drivers_registered(repo_root) {
                return Some(
                    "git merge drivers for .taska are not registered in this clone; run \
                     `ta init` to set them up (without them, a git merge can corrupt the \
                     task log)"
                        .to_string(),
                );
            }
            // Check the .gitattributes where `setup` writes it: the store's
            // parent (valid for a nested store too — attribute files apply to
            // the tree below their directory).
            let attrs =
                std::fs::read_to_string(repo_root.join(".gitattributes")).unwrap_or_default();
            let has = |file: &str, driver: &str| {
                let line = format!("{file} merge={driver}");
                attrs.lines().any(|l| l.trim() == line)
            };
            if !has(MUTATIONS_PATH, LOG_DRIVER) || !has(BASELINE_PATH, BASELINE_DRIVER) {
                return Some(
                    ".gitattributes is missing the .taska merge-driver entries; run \
                     `ta init` to restore them (without them, a git merge can corrupt \
                     the task log)"
                        .to_string(),
                );
            }
            None
        }
    }
}

/// Whether both merge-driver definitions resolve in git config (any scope —
/// a globally registered driver works just as well as a local one). One spawn
/// for both drivers; a missing repo or git binary reads as "not registered".
fn drivers_registered(repo_root: &Path) -> bool {
    let output = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["config", "--get-regexp", r"^merge\.taska-.*\.driver$"])
        .output();
    let Ok(out) = output else {
        return false;
    };
    let config = String::from_utf8_lossy(&out.stdout);
    let defines = |driver: &str| {
        config
            .lines()
            .any(|l| l.starts_with(&format!("merge.{driver}.driver ")))
    };
    out.status.success() && defines(LOG_DRIVER) && defines(BASELINE_DRIVER)
}

/// Register one merge driver in local git config. Returns whether both config
/// writes succeeded; messaging and error policy are left to the caller.
fn register_driver(repo_root: &Path, name: &str, description: &str, driver_cmd: &str) -> bool {
    // Capture the child's output rather than inheriting stderr: outside a git
    // repo every `git config` call would otherwise leak its own `fatal: not in
    // a git directory` to the terminal before `setup` prints its one warning.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .output()
    };
    let name_set = git(&["config", &format!("merge.{name}.name"), description]);
    let driver_set = git(&["config", &format!("merge.{name}.driver"), driver_cmd]);
    matches!((name_set, driver_set), (Ok(a), Ok(b)) if a.status.success() && b.status.success())
}
