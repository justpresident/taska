//! SCM integration: register the custom merge drivers for the event log, for
//! both **git** and **mercurial** (which also covers Sapling - it reads the same
//! `.hg/hgrc` merge-tool config).
//!
//! Separated from the CLI because wiring up source control is its own
//! responsibility that changes independently of argument parsing or dispatch.
//!
//! Note the git split: `.gitattributes` is committed and travels with the repo,
//! but the `merge.<driver>.driver` settings live in *local* git config, which is
//! per-clone and never committed. So a fresh clone of a repo that already has a
//! `.taska` store still needs the drivers registered locally - which is why
//! [`setup`] is idempotent and safe to re-run via `ta init`, and why
//! [`ensure_scm_health`] silently re-registers them on any store command once
//! `.gitattributes` already declares them (the driver command is a taska-owned
//! constant, so auto-registering it can't run anything the repo chose).
//!
//! Mercurial has **no committed half**: both the file->tool mapping
//! (`[merge-patterns]`) and the tool definition (`[merge-tools]`) live in the
//! per-clone `.hg/hgrc`. So every clone must run `ta init` (or let
//! [`ensure_scm_health`] heal it) - the whole registration is silently
//! re-writable, since `.hg/hgrc` is untracked and the tool command is the same
//! taska-owned constant.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::DynError;

const LOG_DRIVER: &str = "taska-merge-driver";
const BASELINE_DRIVER: &str = "taska-baseline-keep-ours";
const MUTATIONS_PATH: &str = ".taska/mutations.jsonl";
const BASELINE_PATH: &str = ".taska/baseline.jsonl";

/// Mercurial merge-tool names (the hgrc analogue of the git driver names).
const HG_LOG_TOOL: &str = "taska-merge";
const HG_BASELINE_TOOL: &str = "taska-baseline";
/// Delimiters for the taska-managed block spliced into `.hg/hgrc` - the same
/// managed-block idiom as the agent-integration block, so a re-run replaces it
/// in place rather than appending duplicates.
const HG_BLOCK_BEGIN: &str = "# BEGIN TASKA MERGE TOOLS (managed by `ta init`)";
const HG_BLOCK_END: &str = "# END TASKA MERGE TOOLS";

/// Wire up the merge drivers for the detected SCM: a restack driver for the event
/// log and a keep-ours driver for the compacted baseline.
///
/// For git this writes the `.gitattributes` entries plus the local `git config`
/// driver definitions; for mercurial it splices the managed `[merge-patterns]`/
/// `[merge-tools]` block into per-clone `.hg/hgrc`. Idempotent and safe to call
/// at any time (attribute/config lines and the hgrc block are only added if
/// absent). Best-effort - a plain directory (no SCM) warns rather than failing,
/// so `ta init` still works.
pub fn setup(repo_root: &Path) -> Result<(), DynError> {
    match detect_scm(repo_root) {
        Some((Scm::Git, _)) => {
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
        }
        Some((Scm::Mercurial, hg_root)) => {
            let prefix = store_prefix(repo_root, hg_root);
            if register_hg_drivers(&hg_root.join(".hg"), &prefix) {
                println!("Configured mercurial merge tools for the taska event log");
            } else {
                eprintln!(
                    "warning: mercurial merge tools not configured (could not write \
                     .hg/hgrc?); merging concurrent .taska edits in hg can corrupt the \
                     task log"
                );
            }
        }
        None => {
            eprintln!(
                "warning: git merge drivers not configured (not a git repository?); \
                 run `git init`, then `ta init` again to enable safe .taska merges"
            );
        }
    }
    Ok(())
}

/// Stage and commit exactly `paths` as a single commit with `message`.
///
/// Returns the new commit's short hash - the initial version-control step
/// `ta init` runs so the store is tracked from the first command.
///
/// Path-scoped on purpose: a user's unrelated staged or working-tree changes are
/// left untouched. A no-op - returning `None` - when the committable paths hold
/// nothing new (so a re-init never makes an empty commit), or when the SCM is
/// unavailable / this isn't a repo (so a plain-directory `ta init` doesn't fail,
/// matching the best-effort spirit of [`setup`]). Dispatches by the detected SCM,
/// so `ta init` version-controls the store identically under git and mercurial.
pub fn commit_paths(repo_root: &Path, paths: &[PathBuf], message: &str) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    match detect_scm(repo_root)? {
        (Scm::Git, _) => git_commit_paths(repo_root, paths, message),
        (Scm::Mercurial, _) => hg_commit_paths(repo_root, paths, message),
    }
}

/// The merge-driver registration files `init` should commit alongside the store.
///
/// For the detected SCM: git's `.gitattributes` is the *committed* half of its
/// driver setup, so it belongs in the commit; mercurial's whole registration
/// lives in the untracked, per-clone `.hg/hgrc`, so it contributes nothing
/// committable (and blindly listing `.gitattributes` would sweep an unrelated
/// pre-existing one into the taska commit). Empty for a plain directory.
pub fn committed_registration_paths(repo_root: &Path) -> Vec<PathBuf> {
    match detect_scm(repo_root) {
        Some((Scm::Git, _)) => vec![repo_root.join(".gitattributes")],
        Some((Scm::Mercurial, _)) | None => Vec::new(),
    }
}

/// The git half of [`commit_paths`].
///
/// Gitignored paths are dropped up front: explicitly `git add`-ing an ignored
/// path is an error (exit 1) that also leaves the rest half-staged, so a repo
/// that ignores an expected file - say CLAUDE.md, or even `.gitattributes` or the
/// whole `.taska` store - commits what it can rather than failing the whole step.
fn git_commit_paths(repo_root: &Path, paths: &[PathBuf], message: &str) -> Option<String> {
    use std::collections::HashSet;
    use std::ffi::OsStr;

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

/// The mercurial half of [`commit_paths`].
///
/// hg has no staging area, so this is add-then-commit-these-paths. Non-existent
/// paths are dropped first (in an hg repo there's no `.gitattributes`, but `init`
/// still passes it), since `hg add` of a missing file fails the batch. Ignored
/// paths need no special handling: taska writes no `.hgignore`, and an explicitly
/// named path is added regardless. `hg commit <paths>` is inherently path-scoped,
/// so unrelated working-tree changes are untouched.
fn hg_commit_paths(repo_root: &Path, paths: &[PathBuf], message: &str) -> Option<String> {
    use std::ffi::OsStr;

    let bin = hg_binary(repo_root)?;
    let hg = |leading: &[&OsStr], these: &[&OsStr]| -> Option<std::process::Output> {
        let mut args: Vec<&OsStr> = leading.to_vec();
        args.extend_from_slice(these);
        Command::new(bin)
            .current_dir(repo_root)
            .args(&args)
            .output()
            .ok()
    };

    let committable: Vec<&OsStr> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| p.as_os_str())
        .collect();
    if committable.is_empty() {
        return None;
    }

    // Stage new files; `hg add` of an already-tracked or unchanged path is a
    // harmless no-op.
    hg(&[OsStr::new("add")], &committable).filter(|o| o.status.success())?;

    // Nothing changed among these paths => no-op (a clean re-init). `hg status`
    // prints one line per changed path; empty output means nothing to commit.
    let status = hg(&[OsStr::new("status")], &committable).filter(|o| o.status.success())?;
    if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return None;
    }

    hg(
        &[OsStr::new("commit"), OsStr::new("-m"), OsStr::new(message)],
        &committable,
    )
    .filter(|o| o.status.success())?;

    // The new commit's short hash (working parent).
    let id = Command::new(bin)
        .current_dir(repo_root)
        .args(["log", "-r", ".", "-T", "{node|short}"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&id.stdout).trim().to_owned())
}

/// The mercurial binary to drive the repo at `repo_root`: `hg`, else Sapling's
/// `sl`. Probed with `<bin> root`, NOT `<bin> --version` - the binary must be able
/// to open THIS repo, not merely be installed. A classic-Mercurial `hg` can't read
/// a Sapling checkout (and vice versa), so on a machine with both binaries a
/// `--version` probe would happily pick one that fails on every real command -
/// including `undo`'s committed-count read, which then reads 0 and can truncate
/// committed history. `root` succeeds only for the binary that owns the repo.
/// `None` when neither can open it (or neither is installed).
fn hg_binary(repo_root: &Path) -> Option<&'static str> {
    ["hg", "sl"].into_iter().find(|bin| {
        Command::new(bin)
            .current_dir(repo_root)
            .arg("root")
            .output()
            .is_ok_and(|o| o.status.success())
    })
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
/// that committed file), a failed git-config write, or a failed `.hg/hgrc` write.
///
/// Detection walks up from the store's parent and is ordered cheapest-first: no
/// SCM anywhere costs only stats and stays quiet (plain-dir use is deliberate;
/// `ta init` warned once); mercurial reads `.hg/hgrc` and silently writes the
/// managed block when it's absent (warning only if that fails); git costs
/// a `.gitattributes` read plus one `git config` spawn (git resolves config from
/// any directory inside the repo, so a nested store needs no special-casing),
/// and once per clone the registration writes.
pub fn ensure_scm_health(repo_root: &Path) -> Option<String> {
    let (scm, scm_root) = detect_scm(repo_root)?;
    match scm {
        // Mercurial has no committed half - the whole registration lives in the
        // untracked, per-clone `.hg/hgrc` - so heal it silently (like git's local
        // config), warning only if the write itself fails. The tool command is
        // the same taska-owned constant, so this runs nothing the repo chose.
        Scm::Mercurial => {
            let prefix = store_prefix(repo_root, scm_root);
            let hg_dir = scm_root.join(".hg");
            if !hg_drivers_registered(&hg_dir, &prefix) && !register_hg_drivers(&hg_dir, &prefix) {
                return Some(
                    "mercurial merge tools for .taska could not be written to .hg/hgrc; \
                     run `ta init` to set them up (without them, an hg merge can corrupt \
                     the task log)"
                        .to_string(),
                );
            }
            None
        }
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

// ---------------------------------------------------------------------------
// Mercurial (and Sapling) registration
// ---------------------------------------------------------------------------

/// The store's parent relative to the SCM root, forward-slashed, `""` at the
/// root. Mercurial matches `[merge-patterns]` against ROOT-relative paths from a
/// single `.hg/hgrc`, so a store nested below the checkout root needs its prefix
/// baked into the pattern (git handles this differently, via a `.gitattributes`
/// placed in the store's parent).
fn store_prefix(repo_root: &Path, scm_root: &Path) -> String {
    repo_root
        .strip_prefix(scm_root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// The taska-managed `.hg/hgrc` block: a `[merge-patterns]` mapping the log and
/// baseline to their tools, and a `[merge-tools]` definition for each. `premerge
/// = False` is load-bearing - it stops hg from text-merging the JSONL (which
/// could resolve an append cleanly and never call our restacker, or worse leave
/// conflict markers), so the tool always runs on the raw ours/base/other. The
/// tool `executable` is `ta`, the same taska-owned constant the git driver uses.
fn hg_block(store_prefix: &str) -> String {
    let p = if store_prefix.is_empty() {
        String::new()
    } else {
        format!("{store_prefix}/")
    };
    format!(
        "{HG_BLOCK_BEGIN}\n\
         [merge-patterns]\n\
         {p}{MUTATIONS_PATH} = {HG_LOG_TOOL}\n\
         {p}{BASELINE_PATH} = {HG_BASELINE_TOOL}\n\
         [merge-tools]\n\
         {HG_LOG_TOOL}.executable = ta\n\
         {HG_LOG_TOOL}.args = hg-merge $base $local $other $output\n\
         {HG_LOG_TOOL}.premerge = False\n\
         {HG_BASELINE_TOOL}.executable = ta\n\
         {HG_BASELINE_TOOL}.args = hg-merge-baseline $base $local $other $output\n\
         {HG_BASELINE_TOOL}.premerge = False\n\
         {HG_BLOCK_END}"
    )
}

/// Splice the managed block into an existing `.hg/hgrc` body: replace a prior
/// block in place, or append when absent; `None` when the body already holds
/// exactly this block (so registration is a no-op). Same idiom as the
/// agent-integration block, so a re-run never appends duplicate tool sections.
fn splice_hg_block(existing: &str, block: &str) -> Option<String> {
    if let Some(start) = existing.find(HG_BLOCK_BEGIN) {
        // A well-formed block ends at its END marker; a BEGIN with no END is a
        // corrupt/truncated block, so treat everything from BEGIN to end-of-file
        // as the block to replace rather than appending a second one below it.
        let end = existing[start..]
            .find(HG_BLOCK_END)
            .map_or(existing.len(), |end_off| {
                start + end_off + HG_BLOCK_END.len()
            });
        if existing[start..end] == *block {
            return None;
        }
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..start]);
        out.push_str(block);
        out.push_str(&existing[end..]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return Some(out);
    }
    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    Some(out)
}

/// Register the mercurial merge tools by splicing the managed block into
/// `.hg/hgrc` (hg does not create that file itself). Idempotent; returns whether
/// the write succeeded (or was already current). The tool command is a
/// taska-owned constant, so this is safe to run unprompted.
fn register_hg_drivers(hg_dir: &Path, store_prefix: &str) -> bool {
    let path = hg_dir.join("hgrc");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    splice_hg_block(&existing, &hg_block(store_prefix))
        .is_none_or(|updated| std::fs::write(&path, updated).is_ok())
}

/// Whether `.hg/hgrc` already holds the CURRENT managed block for this store.
fn hg_drivers_registered(hg_dir: &Path, store_prefix: &str) -> bool {
    let existing = std::fs::read_to_string(hg_dir.join("hgrc")).unwrap_or_default();
    splice_hg_block(&existing, &hg_block(store_prefix)).is_none()
}

// ---------------------------------------------------------------------------
// Committed log inspection (SCM-dispatched)
// ---------------------------------------------------------------------------

/// Count the committed `.taska/mutations.jsonl` lines at the current tip.
///
/// The tip is git's `HEAD` / mercurial's `.`; the count is of non-empty lines.
/// Returns 0 when the file isn't committed yet, there's no tip, or there's no
/// (recognized) SCM, which `undo` treats as "nothing committed" (every event safe
/// to truncate).
///
/// Dispatched by the detected SCM so `undo` need not know which one backs the
/// store. For mercurial both `hg` and `sl` (Sapling) are tried, since either may
/// be the installed binary.
pub fn committed_mutation_count(repo_root: &Path) -> usize {
    let content = match detect_scm(repo_root) {
        Some((Scm::Git, _)) => git_committed_mutations(repo_root),
        Some((Scm::Mercurial, _)) => hg_committed_mutations(repo_root),
        None => None,
    };
    content.map_or(0, |s| s.lines().filter(|l| !l.trim().is_empty()).count())
}

/// The git-committed `mutations.jsonl` blob (`HEAD:`). The `./` prefix makes the
/// path relative to `repo_root` (the store's parent, via `-C`) rather than the
/// repo root, so a store NESTED below the root reads its own committed events.
fn git_committed_mutations(repo_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", "HEAD:./.taska/mutations.jsonl"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The mercurial-committed `mutations.jsonl` at the working parent (`-r .`). Run
/// with cwd at `repo_root` (the store's parent) so the path resolves for a nested
/// store, and hg/sl walk up to find the checkout.
fn hg_committed_mutations(repo_root: &Path) -> Option<String> {
    let bin = hg_binary(repo_root)?;
    let out = Command::new(bin)
        .current_dir(repo_root)
        .args(["cat", "-r", ".", ".taska/mutations.jsonl"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("taska-git-unit").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hg_block_defines_tools_patterns_and_disables_premerge() {
        let b = hg_block("");
        assert!(b.starts_with(HG_BLOCK_BEGIN) && b.ends_with(HG_BLOCK_END));
        assert!(b.contains(".taska/mutations.jsonl = taska-merge"));
        assert!(b.contains(".taska/baseline.jsonl = taska-baseline"));
        assert!(b.contains("taska-merge.args = hg-merge $base $local $other $output"));
        assert!(b.contains("taska-baseline.args = hg-merge-baseline $base $local $other $output"));
        // premerge off is load-bearing: hg must always hand the raw file to `ta`.
        assert!(b.contains("taska-merge.premerge = False"));
        assert!(b.contains("taska-baseline.premerge = False"));
    }

    #[test]
    fn hg_block_roots_patterns_at_a_nested_store_prefix() {
        let b = hg_block("crates/app");
        assert!(b.contains("crates/app/.taska/mutations.jsonl = taska-merge"));
        assert!(b.contains("crates/app/.taska/baseline.jsonl = taska-baseline"));
    }

    #[test]
    fn store_prefix_is_empty_at_root_and_relative_when_nested() {
        let root = Path::new("/repo");
        assert_eq!(store_prefix(root, root), "");
        assert_eq!(
            store_prefix(Path::new("/repo/crates/app"), root),
            "crates/app"
        );
    }

    #[test]
    fn splice_appends_then_replaces_in_place_then_no_ops() {
        let block_a = hg_block("");
        // Append into a hgrc that already has unrelated user config.
        let existing = "[ui]\nusername = Me <me@x.dev>\n";
        let after = splice_hg_block(existing, &block_a).expect("appends");
        assert!(after.contains("[ui]"), "keeps user config: {after}");
        assert!(after.contains(&block_a), "adds the block");

        // Re-splicing the identical block is a no-op.
        assert!(
            splice_hg_block(&after, &block_a).is_none(),
            "identical block is unchanged"
        );

        // A changed block (different prefix) replaces the old one IN PLACE.
        let block_b = hg_block("sub");
        let updated = splice_hg_block(&after, &block_b).expect("replaces");
        assert_eq!(
            updated.matches(HG_BLOCK_BEGIN).count(),
            1,
            "exactly one managed block remains: {updated}"
        );
        assert!(updated.contains("sub/.taska/mutations.jsonl = taska-merge"));
    }

    #[test]
    fn splice_replaces_a_corrupt_block_missing_its_end_marker() {
        // A hand-truncated hgrc: a BEGIN marker with no matching END. Splicing
        // must replace from BEGIN to end-of-file (not append a second block below
        // the orphan), leaving exactly one well-formed managed block.
        let block = hg_block("");
        let corrupt =
            format!("[ui]\nusername = Me <me@x.dev>\n{HG_BLOCK_BEGIN}\n[merge-tools]\ntruncated");
        let fixed = splice_hg_block(&corrupt, &block).expect("replaces the corrupt block");
        assert!(fixed.contains("[ui]"), "keeps user config: {fixed}");
        assert_eq!(
            fixed.matches(HG_BLOCK_BEGIN).count(),
            1,
            "exactly one BEGIN marker: {fixed}"
        );
        assert_eq!(
            fixed.matches(HG_BLOCK_END).count(),
            1,
            "the replacement closes the block: {fixed}"
        );
        assert!(
            !fixed.contains("truncated"),
            "the truncated remnant is gone: {fixed}"
        );
        // And the repair is now stable - a re-splice is a no-op.
        assert!(
            splice_hg_block(&fixed, &block).is_none(),
            "repair is stable"
        );
    }

    #[test]
    fn register_then_detect_registered_roundtrips_and_is_idempotent() {
        let root = scratch("hg-register");
        let hg_dir = root.join(".hg");
        std::fs::create_dir_all(&hg_dir).unwrap();

        assert!(!hg_drivers_registered(&hg_dir, ""), "not registered yet");
        assert!(register_hg_drivers(&hg_dir, ""), "first write succeeds");
        assert!(hg_drivers_registered(&hg_dir, ""), "now registered");

        // A second registration neither fails nor duplicates the block.
        assert!(
            register_hg_drivers(&hg_dir, ""),
            "re-register is a clean no-op"
        );
        let hgrc = std::fs::read_to_string(hg_dir.join("hgrc")).unwrap();
        assert_eq!(hgrc.matches(HG_BLOCK_BEGIN).count(), 1, "one block: {hgrc}");
    }
}
