//! End-to-end tests for the Mercurial merge tools (`ta hg-merge` /
//! `ta hg-merge-baseline`), the hg analogue of `tests/merge.rs`.
//!
//! These drive REAL classic Mercurial, so the whole theme is gated on `hg` being
//! a classic-Mercurial binary on PATH (`hg_available`): a machine without it -
//! or with only Sapling's `hg` shim - skips rather than fails, matching the
//! task's "gate on `hg`" requirement. Install `mercurial` (pip or the distro
//! package) to exercise them. Like the git merge tests, the test-binary dir is
//! prepended to PATH so hg's `taska-merge` tool resolves to the `ta` under test.
mod common;
use common::*;

use std::process::{Command, Output};

/// Whether a classic-Mercurial `hg` is on PATH. Gated on the "Mercurial
/// Distributed SCM" version banner specifically, so Sapling's `hg` shim (whose
/// merge workflow differs) doesn't make the theme flaky - Sapling compatibility
/// rides on the same shared merge code and is validated separately.
fn hg_available() -> bool {
    Command::new("hg")
        .arg("--version")
        .env("PATH", path_with_bin())
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains("Mercurial Distributed SCM")
        })
        .unwrap_or(false)
}

/// Run `hg` in `dir` with a hermetic environment: no user/system hgrc
/// (`HGRCPATH` empty; the per-repo `.hg/hgrc` taska writes is still read), a
/// fixed commit identity, and the test-binary dir on PATH so the `taska-merge`
/// tool resolves to `ta`.
fn hg(dir: &Path, args: &[&str]) -> Output {
    Command::new("hg")
        .args(args)
        .current_dir(dir)
        .env("PATH", path_with_bin())
        .env("HGUSER", "Taska Test <test@taska.dev>")
        .env("HGRCPATH", "")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `hg {}`: {e}", args.join(" ")))
}

/// Run `hg`, asserting success and returning stdout.
fn hg_ok(dir: &Path, args: &[&str]) -> String {
    let out = hg(dir, args);
    assert!(
        out.status.success(),
        "`hg {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Provision an hg-backed store: `hg init`, a commit identity, then `ta init`
/// (which registers the merge tools into `.hg/hgrc` AND commits the store). Uses
/// the default (open) config. The identity goes in `.hg/hgrc` (always read,
/// regardless of `HGRCPATH`) so `ta init`'s internal `hg commit` has an author.
fn init_hg(dir: &Path) {
    hg_ok(dir, &["init"]);
    fs::write(
        dir.join(".hg/hgrc"),
        "[ui]\nusername = Taska Test <test@taska.dev>\n",
    )
    .unwrap();
    ta(dir, &["init"]);
}

/// The base revision id after the first commit, used to spawn a second head.
fn base_rev(dir: &Path) -> String {
    hg_ok(dir, &["id", "-i"]).trim().to_string()
}

#[test]
fn ta_init_registers_the_merge_tools_in_hgrc() {
    if !hg_available() {
        eprintln!("skipping: classic mercurial (`hg`) not on PATH");
        return;
    }
    let dir = fresh_dir("hg-register");
    init_hg(&dir);

    let hgrc = fs::read_to_string(dir.join(".hg/hgrc")).expect("hgrc written");
    assert!(
        hgrc.contains("[merge-patterns]"),
        "patterns section: {hgrc}"
    );
    assert!(
        hgrc.contains(".taska/mutations.jsonl = taska-merge")
            && hgrc.contains(".taska/baseline.jsonl = taska-baseline"),
        "file->tool mapping: {hgrc}"
    );
    assert!(
        hgrc.contains("taska-merge.args = hg-merge $base $local $other $output")
            && hgrc.contains("taska-merge.premerge = False"),
        "tool definition with premerge off: {hgrc}"
    );

    // Idempotent: a second `ta init` neither warns nor duplicates the block.
    let out = run(ta_bin(), &dir, &["init"]);
    assert!(out.status.success());
    let hgrc2 = fs::read_to_string(dir.join(".hg/hgrc")).unwrap();
    assert_eq!(
        hgrc2.matches("# BEGIN TASKA MERGE TOOLS").count(),
        1,
        "exactly one managed block: {hgrc2}"
    );
}

#[test]
fn ta_init_commits_the_store_in_an_hg_repo() {
    if !hg_available() {
        eprintln!("skipping: classic mercurial (`hg`) not on PATH");
        return;
    }
    // Same turnkey behavior as git: a fresh init version-controls the store so it's
    // tracked from the first command (inlined, not via init_hg, to capture the
    // first init's output).
    let dir = fresh_dir("hg-init-commit");
    hg_ok(&dir, &["init"]);
    fs::write(
        dir.join(".hg/hgrc"),
        "[ui]\nusername = Taska Test <test@taska.dev>\n",
    )
    .unwrap();

    let out = ta(&dir, &["init"]);
    assert!(
        out.contains("Committed taska store"),
        "init reports the commit: {out}"
    );

    // The working tree is clean - everything init wrote is committed.
    let status = hg_ok(&dir, &["status"]);
    assert!(
        status.trim().is_empty(),
        "clean tree after init: {status:?}"
    );
    // The store and the agent file are tracked (hg writes no `.gitattributes`).
    let tracked = hg_ok(&dir, &["files"]);
    for path in [".taska/config.toml", ".taska/mutations.jsonl", "AGENTS.md"] {
        assert!(
            tracked.lines().any(|l| l == path),
            "{path} is committed: {tracked}"
        );
    }
    let desc = hg_ok(&dir, &["log", "-r", ".", "-T", "{desc}"]);
    assert!(
        desc.contains("Initialize taska store"),
        "commit subject: {desc}"
    );
}

#[test]
fn hg_merge_tool_unions_divergent_appends() {
    if !hg_available() {
        eprintln!("skipping: classic mercurial (`hg`) not on PATH");
        return;
    }
    let dir = fresh_dir("hg-merge");
    init_hg(&dir);

    ta(&dir, &["create", "base"]);
    hg_ok(&dir, &["add", ".taska"]);
    hg_ok(&dir, &["commit", "-m", "base"]);
    let base = base_rev(&dir);

    // One head appends a task...
    ta(&dir, &["create", "on-main"]);
    hg_ok(&dir, &["commit", "-m", "main task"]);

    // ...a second head branches from base and appends a different task.
    hg_ok(&dir, &["update", "-q", "-r", &base]);
    ta(&dir, &["create", "on-feature"]);
    hg_ok(&dir, &["commit", "-m", "feature task"]);

    // Both heads edited mutations.jsonl (colliding on the same seq); the tool must
    // restack them into a clean union rather than leaving conflict markers.
    let merge = hg(&dir, &["merge"]);
    assert!(
        merge.status.success(),
        "hg merge should auto-resolve, got:\n{}\n{}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );
    hg_ok(&dir, &["commit", "-m", "merge"]);

    let list = ta(&dir, &["list"]);
    for id in ["base", "on-main", "on-feature"] {
        assert!(list.contains(id), "missing {id} after merge:\n{list}");
    }
}

#[test]
fn hg_surface_conflict_fails_merge_and_resolve_clears_it() {
    if !hg_available() {
        eprintln!("skipping: classic mercurial (`hg`) not on PATH");
        return;
    }
    let dir = fresh_dir("hg-conflict");
    init_hg(&dir);

    ta(&dir, &["create", "t", "status=todo"]);
    hg_ok(&dir, &["add", ".taska"]);
    hg_ok(&dir, &["commit", "-m", "base"]);
    let base = base_rev(&dir);

    // Both heads set the SAME field of the SAME task to different values.
    ta(&dir, &["update", "t", "status=main-val"]);
    hg_ok(&dir, &["commit", "-m", "main edit"]);
    hg_ok(&dir, &["update", "-q", "-r", &base]);
    ta(&dir, &["update", "t", "status=feat-val"]);
    hg_ok(&dir, &["commit", "-m", "feature edit"]);

    // Default policy is `surface`, so the tool fails and hg leaves the file
    // unresolved (exit 1), with a marker written for `ta resolve`.
    let merge = hg(&dir, &["merge"]);
    assert!(
        !merge.status.success(),
        "surface policy must fail the merge:\n{}",
        String::from_utf8_lossy(&merge.stdout)
    );
    assert!(
        dir.join(".taska/merge-conflict.json").exists(),
        "a conflict marker should be written"
    );

    // `ta resolve` reports the per-field conflict and clears the marker.
    let resolved = ta(&dir, &["resolve"]);
    assert!(
        resolved.contains("conflict"),
        "resolve should report the conflict: {resolved}"
    );
    assert!(
        resolved.contains("status"),
        "resolve should name the conflicting field: {resolved}"
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "marker should be cleared"
    );
}
