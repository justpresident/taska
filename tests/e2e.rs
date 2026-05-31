//! End-to-end tests driving the real `ta` binary against throwaway git repos.
//!
//! Cargo builds the binary and hands us its path via `CARGO_BIN_EXE_ta`, and a
//! per-suite scratch directory via `CARGO_TARGET_TMPDIR`. Each test gets its own
//! subdirectory so the suite is parallel-safe. The git merge-driver test needs
//! git to find `ta` on `PATH`, so every spawned command runs with the binary's
//! directory prepended to `PATH`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn ta_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ta")
}

fn bin_dir() -> PathBuf {
    Path::new(ta_bin()).parent().unwrap().to_path_buf()
}

/// `PATH` with the test binary's directory prepended, so git's merge driver
/// (`ta git-merge ...`) resolves to the binary under test.
fn path_with_bin() -> OsString {
    let mut dirs = vec![bin_dir()];
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).unwrap()
}

/// A fresh, empty scratch directory unique to `name`.
fn fresh_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("PATH", path_with_bin())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{} {}`: {}", program, args.join(" "), e))
}

/// Run `ta`, asserting success and returning stdout.
fn ta(dir: &Path, args: &[&str]) -> String {
    let out = run(ta_bin(), dir, args);
    assert!(
        out.status.success(),
        "`ta {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run `git`, asserting success and returning stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = run("git", dir, args);
    assert!(
        out.status.success(),
        "`git {}` failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Whether `id` appears as a listed task. `print_task` emits `"{id}  {fields}"`,
/// so we match on the line prefix rather than a bare substring — otherwise a
/// dependency reference like `deps=["db"]` would be mistaken for a `db` task.
fn lists_task(output: &str, id: &str) -> bool {
    output.lines().any(|l| l.starts_with(&format!("{id}  ")))
}

/// Initialize a git repo with a deterministic default branch and an identity.
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@taska.dev"]);
    git(dir, &["config", "user.name", "Taska Test"]);
}

#[test]
fn init_creates_config_and_registers_merge_driver() {
    let dir = fresh_dir("init");
    init_repo(&dir);

    let out = ta(&dir, &["init"]);
    assert!(out.contains("Initialized taska store"), "got: {out}");

    let cfg = fs::read_to_string(dir.join(".taska/config.toml")).unwrap();
    assert!(cfg.contains("[compaction]") && cfg.contains("[workflow]"), "config: {cfg}");
    assert!(cfg.contains("keep_events = 1000"), "config: {cfg}");
    assert!(cfg.contains("status_field = \"status\""), "config: {cfg}");

    let attrs = fs::read_to_string(dir.join(".gitattributes")).unwrap();
    assert!(attrs.contains("mutations.jsonl merge=taska-merge-driver"), "attrs: {attrs}");

    let driver = git(&dir, &["config", "--get", "merge.taska-merge-driver.driver"]);
    assert!(driver.contains("ta git-merge"), "driver: {driver}");
}

#[test]
fn crud_search_and_ready_workflow() {
    let dir = fresh_dir("crud");
    init_repo(&dir);
    ta(&dir, &["init"]);

    ta(&dir, &["create", "db", "status=done"]);
    ta(&dir, &["create", "api", "status=open", "priority=3"]);
    ta(&dir, &["block", "api", "db"]);

    // priority=3 is coerced to a JSON number, not a string.
    let list = ta(&dir, &["list"]);
    assert!(list.contains(r#""priority":3"#), "list: {list}");
    assert!(list.contains(r#"deps=["db"]"#), "list: {list}");

    let search = ta(&dir, &["search", "status", "open"]);
    assert!(lists_task(&search, "api"), "search: {search}");
    assert!(!lists_task(&search, "db"), "db is done, not open: {search}");

    // db is done, so api's only dependency is satisfied -> api is ready.
    let ready = ta(&dir, &["ready"]);
    assert!(lists_task(&ready, "api"), "ready: {ready}");

    // Once api is done too, nothing is ready.
    ta(&dir, &["update", "api", "status=done"]);
    assert_eq!(ta(&dir, &["ready"]).trim(), "(nothing ready)");

    ta(&dir, &["delete", "db"]);
    assert!(!lists_task(&ta(&dir, &["list"]), "db"), "db should be gone");
}

fn rows(path: &Path) -> usize {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

#[test]
fn compact_folds_log_and_appends_resume() {
    let dir = fresh_dir("compact");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Keep nothing so the whole log folds, exercising the full-compaction path.
    fs::write(dir.join(".taska/config.toml"), "[compaction]\nkeep_events = 0\nkeep_days = 0\n").unwrap();

    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["compact"]);

    assert_eq!(rows(&dir.join(".taska/mutations.jsonl")), 0, "log empty after full compact");
    assert_eq!(rows(&dir.join(".taska/baseline.jsonl")), 2, "two tasks folded into baseline");

    // Appends overlay the baseline after compaction.
    ta(&dir, &["create", "c"]);
    let list = ta(&dir, &["list"]);
    for id in ["a", "b", "c"] {
        assert!(lists_task(&list, id), "missing {id} in list:\n{list}");
    }
}

#[test]
fn compact_retains_recent_events_for_merge() {
    let dir = fresh_dir("retain");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(dir.join(".taska/config.toml"), "[compaction]\nkeep_events = 2\nkeep_days = 0\n").unwrap();

    for id in ["a", "b", "c", "d", "e"] {
        ta(&dir, &["create", id]);
    }
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("kept 2 recent event(s)"), "got: {out}");

    // 3 oldest folded into baseline, 2 newest retained in the log.
    assert_eq!(rows(&dir.join(".taska/mutations.jsonl")), 2, "kept 2 recent events");
    assert_eq!(rows(&dir.join(".taska/baseline.jsonl")), 3, "folded 3 into baseline");

    // The retained events are the two most recent creations.
    let mutations = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(mutations.contains(r#""task_id":"d""#), "expected d retained: {mutations}");
    assert!(mutations.contains(r#""task_id":"e""#), "expected e retained: {mutations}");

    // All five tasks remain visible (baseline + retained log).
    let list = ta(&dir, &["list"]);
    for id in ["a", "b", "c", "d", "e"] {
        assert!(lists_task(&list, id), "missing {id}:\n{list}");
    }
}

#[test]
fn compact_is_noop_below_threshold() {
    let dir = fresh_dir("compact_noop");
    init_repo(&dir);
    ta(&dir, &["init"]); // default keep_events = 1000

    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("Nothing to compact"), "got: {out}");

    assert_eq!(rows(&dir.join(".taska/mutations.jsonl")), 2, "log untouched");
    assert_eq!(rows(&dir.join(".taska/baseline.jsonl")), 0, "baseline still empty");
}

#[test]
fn git_merge_driver_resolves_divergent_appends() {
    let dir = fresh_dir("merge");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "base", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Branch off, then append a distinct task on each branch.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["create", "on-main"]);
    git(&dir, &["commit", "-aqm", "main task"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["create", "on-feature"]);
    git(&dir, &["commit", "-aqm", "feature task"]);

    // Both branches edited mutations.jsonl; the driver must union them cleanly.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "merge should auto-resolve, got:\n{}",
        String::from_utf8_lossy(&merge.stderr)
    );

    let list = ta(&dir, &["list"]);
    for id in ["base", "on-main", "on-feature"] {
        assert!(list.contains(id), "missing {id} after merge:\n{list}");
    }
}

#[test]
fn workflow_config_override_changes_ready_semantics() {
    let dir = fresh_dir("config_override");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // Rename the status convention to state/closed via config.
    fs::write(
        dir.join(".taska/config.toml"),
        "[workflow]\nstatus_field = \"state\"\ndone_status = \"closed\"\n",
    )
    .unwrap();

    ta(&dir, &["create", "db", "state=closed"]);
    ta(&dir, &["create", "api", "state=open"]);
    ta(&dir, &["block", "api", "db"]);

    // With the override, db counts as done, so api becomes ready.
    let ready = ta(&dir, &["ready"]);
    assert!(lists_task(&ready, "api"), "api should be ready: {ready}");
    assert!(!lists_task(&ready, "db"), "db is closed/done: {ready}");
}

#[test]
fn reinit_is_idempotent_and_preserves_edited_config() {
    let dir = fresh_dir("reinit");
    init_repo(&dir);
    ta(&dir, &["init"]);

    fs::write(dir.join(".taska/config.toml"), "[workflow]\ndone_status = \"closed\"\n").unwrap();

    let out = ta(&dir, &["init"]);
    assert!(out.contains("already present"), "should reuse store: {out}");

    let cfg = fs::read_to_string(dir.join(".taska/config.toml")).unwrap();
    assert!(cfg.contains("closed"), "edited config must survive re-init: {cfg}");
}

#[test]
fn init_from_subdirectory_reuses_existing_store() {
    let dir = fresh_dir("subdir");
    init_repo(&dir);
    ta(&dir, &["init"]);

    let nested = dir.join("src/deep");
    fs::create_dir_all(&nested).unwrap();

    let out = ta(&nested, &["init"]);
    assert!(out.contains("already present"), "should reuse: {out}");
    assert!(!nested.join(".taska").exists(), "must not create a nested .taska");
}
