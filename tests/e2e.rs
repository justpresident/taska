//! End-to-end tests driving the real `ta` binary against throwaway git repos.
//!
//! Cargo builds the binary and hands us its path via `CARGO_BIN_EXE_ta`. Each
//! test gets its own throwaway subdirectory under the *system* temp dir —
//! deliberately outside the project tree, so `ta`'s walk-up store discovery
//! can't climb into the repo's own `.taska` (e.g. a dogfooding store) and
//! read/write that instead of the test's isolated one. The git merge-driver
//! test needs git to find `ta` on `PATH`, so every spawned command runs with the
//! binary's directory prepended to `PATH`.

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

/// A fresh, empty scratch directory unique to `name`, under the system temp dir
/// (NOT `CARGO_TARGET_TMPDIR`, which lives inside the project tree where `ta`'s
/// store discovery would find the repo's own `.taska` in an ancestor).
fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("taska-e2e").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("PATH", path_with_bin())
        // The suite deliberately drives sub-100 `keep_events` to exercise
        // compaction; this opts past the production floor (see `enforce_config`).
        .env("TASKA_ALLOW_UNSAFE_RETENTION", "1")
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

/// Whether `id` appears as a listed task. The human table puts the id in the
/// first column, so we check each line's first whitespace-delimited token —
/// robust to alignment padding and to a later `deps` column naming the id.
fn lists_task(output: &str, id: &str) -> bool {
    output
        .lines()
        .any(|l| l.split_whitespace().next() == Some(id))
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
    assert!(
        cfg.contains("[compaction]") && cfg.contains("[workflow]"),
        "config: {cfg}"
    );
    assert!(cfg.contains("keep_events = 1000"), "config: {cfg}");
    assert!(cfg.contains("status_field = \"status\""), "config: {cfg}");

    let attrs = fs::read_to_string(dir.join(".gitattributes")).unwrap();
    assert!(
        attrs.contains("mutations.jsonl merge=taska-merge-driver"),
        "attrs: {attrs}"
    );

    let driver = git(
        &dir,
        &["config", "--get", "merge.taska-merge-driver.driver"],
    );
    assert!(driver.contains("ta git-merge"), "driver: {driver}");
}

#[test]
fn crud_search_and_ready_workflow() {
    let dir = fresh_dir("crud");
    init_repo(&dir);
    ta(&dir, &["init"]);

    ta(&dir, &["create", "db", "status=closed"]);
    ta(&dir, &["create", "api", "status=open", "priority=3"]);
    ta(&dir, &["block", "api", "db"]);

    // The human table lists ids; `--full --format json` exposes every field —
    // priority coerced to a JSON number, and deps as a JSON array.
    assert!(
        lists_task(&ta(&dir, &["list"]), "api"),
        "api should be listed"
    );
    let json = ta(&dir, &["list", "--full", "--format", "json"]);
    assert!(json.contains(r#""priority":3"#), "json: {json}");
    assert!(json.contains(r#""deps":["db"]"#), "json: {json}");

    let search = ta(&dir, &["search", "status", "open"]);
    assert!(lists_task(&search, "api"), "search: {search}");
    assert!(!lists_task(&search, "db"), "db is done, not open: {search}");

    // db is done, so api's only dependency is satisfied -> api is ready.
    let ready = ta(&dir, &["ready"]);
    assert!(lists_task(&ready, "api"), "ready: {ready}");

    // Once api is done too, nothing is ready.
    ta(&dir, &["update", "api", "status=closed"]);
    assert_eq!(ta(&dir, &["ready"]).trim(), "(nothing ready)");

    ta(&dir, &["delete", "db"]);
    assert!(!lists_task(&ta(&dir, &["list"]), "db"), "db should be gone");
}

#[test]
fn update_with_no_fields_fails_and_appends_nothing() {
    let dir = fresh_dir("empty-update");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "api", "status=open"]);

    let log = dir.join(".taska").join("mutations.jsonl");
    let before = rows(&log);

    // `ta update api` with no field=value args must fail (non-zero exit) and
    // must NOT append a no-op empty Update event.
    let out = run(ta_bin(), &dir, &["update", "api"]);
    assert!(
        !out.status.success(),
        "`ta update api` with no fields should exit non-zero, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(rows(&log), before, "no event should have been appended");
}

#[test]
fn output_format_columns_and_json() {
    let dir = fresh_dir("output");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(
        &dir,
        &["create", "a", "title=Alpha", "status=open", "priority=3"],
    );

    // Human: uppercase header + the title column value.
    let human = ta(&dir, &["list"]);
    assert!(
        human.contains("ID") && human.contains("STATUS"),
        "header: {human}"
    );
    assert!(human.contains("Alpha"), "title column: {human}");

    // Default json: the default columns (id,title,status,deps) as an array,
    // but NOT priority (not a default column).
    let json = ta(&dir, &["list", "--format", "json"]);
    assert!(json.trim_start().starts_with('['), "json array: {json}");
    assert!(json.contains(r#""status":"open""#), "status shown: {json}");
    assert!(
        !json.contains("priority"),
        "priority not a default column: {json}"
    );

    // --full exposes every field; --columns selects an explicit set.
    assert!(
        ta(&dir, &["list", "--full", "--format", "json"]).contains(r#""priority":3"#),
        "--full shows priority"
    );
    let cols = ta(
        &dir,
        &["list", "--columns", "id,priority", "--format", "json"],
    );
    assert!(
        cols.contains(r#""priority":3"#) && !cols.contains("status"),
        "columns select + restrict: {cols}"
    );
}

#[test]
fn show_displays_full_task_and_rejects_unknown_id() {
    let dir = fresh_dir("show");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(
        &dir,
        &["create", "a", "title=Alpha", "status=open", "priority=3"],
    );
    ta(&dir, &["create", "dep"]);
    ta(&dir, &["block", "a", "dep"]);

    // `show` defaults to the FULL task: every field, even ones that are not
    // default `list` columns (e.g. priority), plus deps.
    let human = ta(&dir, &["show", "a"]);
    assert!(lists_task(&human, "a"), "show should list the task: {human}");
    assert!(human.contains("Alpha"), "title field: {human}");
    assert!(human.contains("PRIORITY"), "priority header shown: {human}");
    assert!(human.contains("dep"), "deps shown: {human}");

    // json emits the same fields (a one-element array is fine, as for list).
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(json.trim_start().starts_with('['), "json array: {json}");
    assert!(json.contains(r#""priority":3"#), "priority in show json: {json}");
    assert!(json.contains(r#""status":"open""#), "status in show json: {json}");

    // An explicit --columns still restricts.
    let cols = ta(&dir, &["show", "a", "--columns", "id,status", "--format", "json"]);
    assert!(
        cols.contains(r#""status":"open""#) && !cols.contains("priority"),
        "explicit columns restrict show: {cols}"
    );

    // An unknown id exits non-zero.
    let out = run(ta_bin(), &dir, &["show", "missing"]);
    assert!(
        !out.status.success(),
        "show of an unknown id must exit non-zero, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
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
    // keep_events = 0 folds everything but the last event, exercising the
    // maximal-compaction path while keeping the log non-empty (so the seq
    // watermark stays derivable).
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 0\nkeep_days = 0\n",
    )
    .unwrap();

    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["compact"]);

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        1,
        "one event retained"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        1,
        "the rest folded into baseline"
    );

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
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 2\nkeep_days = 0\n",
    )
    .unwrap();

    for id in ["a", "b", "c", "d", "e"] {
        ta(&dir, &["create", id]);
    }
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("kept 2 recent event(s)"), "got: {out}");

    // 3 oldest folded into baseline, 2 newest retained in the log.
    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        2,
        "kept 2 recent events"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        3,
        "folded 3 into baseline"
    );

    // The retained events are the two most recent creations.
    let mutations = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        mutations.contains(r#""task_id":"d""#),
        "expected d retained: {mutations}"
    );
    assert!(
        mutations.contains(r#""task_id":"e""#),
        "expected e retained: {mutations}"
    );

    // All five tasks remain visible (baseline + retained log).
    let list = ta(&dir, &["list"]);
    for id in ["a", "b", "c", "d", "e"] {
        assert!(lists_task(&list, id), "missing {id}:\n{list}");
    }
}

#[test]
fn low_keep_events_is_rejected_on_the_next_command() {
    let dir = fresh_dir("validate");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // A hand-edited, unreasonably small retention value.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 50\n",
    )
    .unwrap();

    // Without the test hatch, even a plain `ta list` must refuse and explain —
    // the error surfaces immediately, not weeks later at compaction time.
    let out = Command::new(ta_bin())
        .args(["list"])
        .current_dir(&dir)
        .env("PATH", path_with_bin())
        .output()
        .unwrap();
    assert!(!out.status.success(), "list should fail on invalid config");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("keep_events") && stderr.contains("100"),
        "stderr: {stderr}"
    );
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

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        2,
        "log untouched"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        0,
        "baseline still empty"
    );
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
fn surface_conflict_fails_merge_and_resolve_clears_it() {
    let dir = fresh_dir("conflict");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Both branches set the SAME field of the SAME task to different values.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    // Default policy is `surface`, so the driver must fail the merge.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        !merge.status.success(),
        "surface policy must fail the merge"
    );
    assert!(
        dir.join(".taska/merge-conflict.json").exists(),
        "a conflict marker should be written"
    );

    // `ta resolve` reports the conflict (per-field) and clears the marker.
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
        resolved.contains("kept ours"),
        "surface resolves tentatively as ours: {resolved}"
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "marker should be cleared"
    );

    // A second resolve is a clean no-op.
    let again = ta(&dir, &["resolve"]);
    assert!(again.contains("No merge conflicts"), "got: {again}");
}

#[test]
fn theirs_policy_resolves_conflict_without_failing() {
    let dir = fresh_dir("theirs");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Opt into silent resolution: the branch merged IN wins conflicts.
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"theirs\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "theirs policy must resolve cleanly: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "auto resolution leaves no marker"
    );

    // Merging feature INTO main with `theirs` keeps feature's value.
    let list = ta(&dir, &["list", "--format", "json"]);
    assert!(
        list.contains("\"status\":\"feature\""),
        "theirs (feature) should win: {list}"
    );
}

#[test]
fn per_field_merge_keeps_disjoint_fields_and_resolves_overlap() {
    let dir = fresh_dir("perfield");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"theirs\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "X", "status=new"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // main and feature overlap on status+owner, but each adds a disjoint field.
    git(&dir, &["branch", "feature"]);
    ta(
        &dir,
        &[
            "update",
            "X",
            "status=closed",
            "owner=alice",
            "scope=project",
        ],
    );
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(
        &dir,
        &["update", "X", "status=open", "owner=bob", "priority=3"],
    );
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "should resolve: {}",
        String::from_utf8_lossy(&merge.stderr)
    );

    let list = ta(&dir, &["list", "--full", "--format", "json"]);
    // Overlapping fields go to theirs (feature); disjoint fields both survive.
    assert!(
        list.contains("\"status\":\"open\""),
        "status -> theirs: {list}"
    );
    assert!(
        list.contains("\"owner\":\"bob\""),
        "owner -> theirs: {list}"
    );
    assert!(
        list.contains("\"scope\":\"project\""),
        "ours-only scope survives: {list}"
    );
    assert!(
        list.contains("\"priority\":3"),
        "theirs-only priority survives: {list}"
    );
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

    fs::write(
        dir.join(".taska/config.toml"),
        "[workflow]\ndone_status = \"closed\"\n",
    )
    .unwrap();

    let out = ta(&dir, &["init"]);
    assert!(out.contains("already present"), "should reuse store: {out}");

    let cfg = fs::read_to_string(dir.join(".taska/config.toml")).unwrap();
    assert!(
        cfg.contains("closed"),
        "edited config must survive re-init: {cfg}"
    );
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
    assert!(
        !nested.join(".taska").exists(),
        "must not create a nested .taska"
    );
}
