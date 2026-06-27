//! Git integration: register the custom merge drivers for the event log.
//!
//! Separated from the CLI because wiring up Git is its own responsibility that
//! changes independently of argument parsing or command dispatch.
//!
//! Note the split: `.gitattributes` is committed and travels with the repo, but
//! the `merge.<driver>.driver` settings live in *local* git config, which is
//! per-clone and never committed. So a fresh clone of a repo that already has a
//! `.taska` store still needs the drivers registered locally - which is why
//! [`setup`] is idempotent and safe to re-run via `ta init`, and why
//! [`ensure_scm_health`] silently re-registers them on any store command once
//! `.gitattributes` already declares them (the driver command is a taska-owned
//! constant, so auto-registering it can't run anything the repo chose).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::DynError;

const LOG_DRIVER: &str = "taska-merge-driver";
const BASELINE_DRIVER: &str = "taska-baseline-keep-ours";
const MUTATIONS_PATH: &str = ".taska/mutations.jsonl";
const BASELINE_PATH: &str = ".taska/baseline.jsonl";

/// Wire up the `.gitattributes` entries and local git merge drivers: a restack
/// driver for the event log, and a keep-ours driver for the compacted baseline.
///
/// Idempotent and safe to call at any time: attribute lines are only added if
/// absent, and `git config` simply re-asserts the same values. Best-effort - a
/// missing git repo warns rather than failing, so `ta init` still works in a
/// plain directory.
pub fn setup(repo_root: &Path) -> Result<(), DynError> {
    ensure_gitattribute(repo_root, MUTATIONS_PATH, LOG_DRIVER)?;
    ensure_gitattribute(repo_root, BASELINE_PATH, BASELINE_DRIVER)?;

    if register_drivers(repo_root) {
        println!("Configured git merge drivers for the taska event log");
    } else {
        eprintln!(
            "warning: git merge drivers not configured (not a git repository?); \
             run `git init`, then `ta init` again to enable safe .taska merges"
        );
    }
    Ok(())
}

/// Stage and commit exactly `paths` as a single commit with `message`, returning
/// the new commit's short hash.
///
/// Path-scoped on purpose (`git commit -- <paths>`): a user's unrelated staged or
/// working-tree changes are left untouched. A no-op - returning `None` - when the
/// committable paths hold nothing new (so a re-init never makes an empty commit),
/// when every path is gitignored, or when git is unavailable / this isn't a repo
/// (so a plain-directory `ta init` doesn't fail, matching the best-effort spirit
/// of [`setup`]).
///
/// Gitignored paths are dropped up front: explicitly `git add`-ing an ignored
/// path is an error (exit 1) that also leaves the rest half-staged, so a repo
/// that ignores an expected file - say CLAUDE.md, or even `.gitattributes` or the
/// whole `.taska` store - commits what it can rather than failing the whole step.
pub fn commit_paths(repo_root: &Path, paths: &[PathBuf], message: &str) -> Option<String> {
    use std::collections::HashSet;
    use std::ffi::OsStr;

    if paths.is_empty() {
        return None;
    }

    // Run git in the repo as `<leading...> -- <these>`, scoping the subcommand to
    // exactly `these` paths.
    let scoped = |leading: &[&OsStr], these: &[&OsStr]| -> Option<std::process::Output> {
        let mut args: Vec<&OsStr> = leading.to_vec();
        args.push(OsStr::new("--"));
        args.extend_from_slice(these);
        std::process::Command::new("git")
            .current_dir(repo_root)
            .args(&args)
            .output()
            .ok()
    };

    let all: Vec<&OsStr> = paths.iter().map(|p| p.as_os_str()).collect();

    // Drop any path git would ignore. `check-ignore` echoes the ignored pathspecs
    // verbatim (exit 0 = some ignored, 1 = none, anything else = no repo / no git);
    // it already consults the index, so an already-tracked file is never reported.
    let ignored: HashSet<&OsStr> = match scoped(&[OsStr::new("check-ignore")], &all) {
        Some(o) if o.status.code() == Some(0) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|line| all.iter().copied().find(|p| p.to_str() == Some(line)))
            .collect(),
        _ => HashSet::new(),
    };
    let committable: Vec<&OsStr> = all
        .iter()
        .copied()
        .filter(|p| !ignored.contains(p))
        .collect();
    if committable.is_empty() {
        return None; // everything we'd commit is gitignored - nothing to track
    }

    // Stage the committable paths. A non-repo or missing git fails here, no-op.
    scoped(&[OsStr::new("add")], &committable).filter(|o| o.status.success())?;

    // Nothing staged among them => nothing to commit (a clean re-init):
    // `diff --cached --quiet` exits 0 only when there's no staged change.
    if scoped(
        &[
            OsStr::new("diff"),
            OsStr::new("--cached"),
            OsStr::new("--quiet"),
        ],
        &committable,
    )?
    .status
    .success()
    {
        return None;
    }

    scoped(
        &[OsStr::new("commit"), OsStr::new("-m"), OsStr::new(message)],
        &committable,
    )
    .filter(|o| o.status.success())?;

    let head = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&head.stdout).trim().to_owned())
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
/// `start` (the store's parent - which need not be the SCM root: a `.taska`
/// nested deeper inside a repo is supported), together with the checkout root
/// it was found at. `.git` may be a FILE (worktrees, submodules), so `exists()`
/// not `is_dir()`. `None` means a plain directory.
fn detect_scm(start: &Path) -> Option<(Scm, &Path)> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some((Scm::Git, dir));
        }
        if dir.join(".hg").is_dir() {
            return Some((Scm::Mercurial, dir));
        }
        dir = dir.parent()?;
    }
}

enum Scm {
    Git,
    Mercurial,
}

/// The root of the SCM checkout containing `start`, if any.
///
/// `ta init` places a NEW store here rather than at the invocation directory,
/// so the store lands where it travels with the repo and every clone's walk-up
/// discovery finds it.
pub fn scm_root(start: &Path) -> Option<&Path> {
    detect_scm(start).map(|(_, root)| root)
}

/// Bring this clone's `.taska` merge protection up to health, returning a
/// residual warning only for what can't be auto-fixed.
///
/// Never blocking - the store itself is fine; it's the *clone* whose setup may
/// be incomplete. `.gitattributes` travels with the repo, but the driver
/// *definitions* live in per-clone local git config - so `ta init` before
/// `git init`, or any fresh clone, has the attributes but not the definitions,
/// and git silently falls back to its text merge on a diverged log (conflict
/// markers inside the JSONL). When `.gitattributes` already declares the
/// drivers, that gap is closed SILENTLY here: the registered command is a
/// taska-owned constant, not read from the repo, so auto-registering it carries
/// none of the arbitrary-command risk that keeps driver definitions per-clone in
/// the first place. A warning is returned only when auto-healing can't apply or
/// fails: a missing `.gitattributes` entry (an explicit `ta init` must rewrite
/// that committed file), a failed git-config write, or an unsupported SCM.
///
/// Detection walks up from the store's parent and is ordered cheapest-first: no
/// SCM anywhere costs only stats and stays quiet (plain-dir use is deliberate;
/// `ta init` warned once); mercurial gets an unsupported-SCM warning; git costs
/// a `.gitattributes` read plus one `git config` spawn (git resolves config from
/// any directory inside the repo, so a nested store needs no special-casing),
/// and once per clone the registration writes.
pub fn ensure_scm_health(repo_root: &Path) -> Option<String> {
    match detect_scm(repo_root)?.0 {
        Scm::Mercurial => Some(
            "mercurial repository detected; taska's merge protection currently \
             supports only git - merging concurrent .taska edits in hg can corrupt \
             the task log"
                .to_string(),
        ),
        Scm::Git => {
            // Check the .gitattributes where `setup` writes it: the store's
            // parent (valid for a nested store too - attribute files apply to
            // the tree below their directory). It's the committed half of the
            // setup, so a missing entry is a real gap that only `ta init` should
            // close - we don't silently rewrite a tracked file the user may have
            // edited.
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
            // `.gitattributes` already declares the drivers, so the only thing a
            // fresh clone is missing is the per-clone driver *definition*. Heal
            // it silently instead of nagging: the command we register is the
            // taska-owned constant `ta git-merge ...`, never read from the repo,
            // so this reintroduces none of the arbitrary-command risk that keeps
            // driver definitions out of the committed tree. Warn only if the
            // local git-config write itself fails (read-only HOME, locked
            // config), so protection is never silently absent.
            if !drivers_registered(repo_root) && !register_drivers(repo_root) {
                return Some(
                    "git merge drivers for .taska could not be auto-registered in this \
                     clone; run `ta init` to set them up (without them, a git merge can \
                     corrupt the task log)"
                        .to_string(),
                );
            }
            None
        }
    }
}

/// Whether both merge-driver definitions resolve in git config (any scope -
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

/// Register both merge-driver definitions in local git config, returning whether
/// every write succeeded. The driver commands are taska-owned constants, never
/// read from the repo - so this is safe to run unprompted (see
/// [`ensure_scm_health`]); messaging and error policy are left to the caller.
fn register_drivers(repo_root: &Path) -> bool {
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
    log_ok && baseline_ok
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
