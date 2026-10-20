//! `ta watch` e2e - change detection, filtering, deletion, the diff output, and
//! the timeout/exit-code contract, driven against the real binary. `watch` can
//! exit 1 (no updates), so these use `run` (which captures the status) rather than
//! `ta` (which asserts success). Tests that expect updates pass `--holdout 0s` so
//! they don't wait out the batching window.
mod common;
use common::*;

/// From cursor 0, every change since the start is an update: the matching task's
/// header and its `+` diff lines print, and the command exits 0.
#[test]
fn watch_reports_matching_changes_since_the_cursor() {
    let dir = fresh_dir("watch-basic");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["update", "a", "status=needs-review"]);

    let out = run(
        ta_bin(),
        &dir,
        &[
            "watch",
            "--since",
            "0",
            "--timeout",
            "5s",
            "--holdout",
            "0s",
        ],
    );
    assert!(
        out.status.success(),
        "exit 0 on updates; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("a:"), "task header: {stdout}");
    assert!(
        stdout.contains("+ status: needs-review"),
        "shows the current status as added: {stdout}"
    );
    let current = ta(&dir, &["status", "--current"]);
    assert_eq!(
        stdout.lines().last(),
        Some(format!("[seq:{}]", current.trim()).as_str()),
        "the diff closes with the cursor it covers: {stdout}"
    );
}

/// With nothing after the cursor, a short timeout prints `No updates yet` to
/// stderr and exits 1, leaving stdout clean (so a script can capture the diff).
#[test]
fn watch_times_out_with_exit_1_when_nothing_matches() {
    let dir = fresh_dir("watch-timeout");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a"]);
    let since = ta(&dir, &["status", "--current"]);

    let out = run(
        ta_bin(),
        &dir,
        &["watch", "--since", since.trim(), "--timeout", "1s"],
    );
    assert_eq!(out.status.code(), Some(1), "exit 1 on timeout");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("No updates yet"),
        "stderr message: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout clean on no updates: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A criterion narrows the reported set to matching tasks only.
#[test]
fn watch_filters_by_criteria() {
    let dir = fresh_dir("watch-filter");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "keep"]);
    ta(&dir, &["create", "skip"]);
    ta(&dir, &["update", "keep", "status=needs-review"]);

    let out = run(
        ta_bin(),
        &dir,
        &[
            "watch",
            "--since",
            "0",
            "--timeout",
            "5s",
            "--holdout",
            "0s",
            "status=needs-review",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit 0: {stdout}");
    assert!(stdout.contains("keep:"), "matching task shown: {stdout}");
    assert!(
        !stdout.contains("skip:"),
        "non-matching task hidden: {stdout}"
    );
}

/// A matching task deleted after the cursor is reported as an all-removed diff
/// (it matched at the cursor - deletion is one way of leaving the filter).
#[test]
fn watch_reports_a_deleted_matching_task() {
    let dir = fresh_dir("watch-delete");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "gone"]);
    ta(&dir, &["update", "gone", "status=needs-review"]);
    let since = ta(&dir, &["status", "--current"]); // cursor after it matches
    ta(&dir, &["delete", "gone"]);

    let out = run(
        ta_bin(),
        &dir,
        &[
            "watch",
            "--since",
            since.trim(),
            "--timeout",
            "5s",
            "--holdout",
            "0s",
            "status=needs-review",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit 0: {stdout}");
    assert!(stdout.contains("gone:"), "deleted task reported: {stdout}");
    assert!(
        stdout.contains("- status: needs-review"),
        "shows the removal: {stdout}"
    );
}

/// `--format json` emits `{cursor, changes}`: the snapshot's cursor and one
/// object per changed task with structured removed/added deltas (not full
/// state), parseable by an agent.
#[test]
fn watch_json_emits_structured_deltas() {
    let dir = fresh_dir("watch-json");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["update", "a", "status=needs-review"]);

    let out = run(
        ta_bin(),
        &dir,
        &[
            "watch",
            "--since",
            "0",
            "--timeout",
            "5s",
            "--holdout",
            "0s",
            "--format",
            "json",
        ],
    );
    assert!(out.status.success(), "exit 0");
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("watch --format json is valid JSON");
    let current = ta(&dir, &["status", "--current"]);
    assert_eq!(v["cursor"].to_string(), current.trim(), "cursor: {v}");
    let arr = v["changes"].as_array().expect("a changes array");
    let entry = arr
        .iter()
        .find(|e| e["id"] == "a")
        .expect("task `a` in the array");
    let added = entry["added"].as_array().expect("added array");
    assert!(
        added.iter().any(|l| l == "status: needs-review"),
        "added carries the status line: {entry}"
    );
}

/// Resuming from the reported cursor sees exactly the writes after it: the
/// earlier change isn't repeated and the later one isn't skipped. jsonl carries
/// the same envelope on one line.
#[test]
fn watch_cursor_resumes_without_skips_or_repeats() {
    let dir = fresh_dir("watch-resume");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "first"]);

    let watch_jsonl = |since: &str| {
        let out = run(
            ta_bin(),
            &dir,
            &[
                "watch",
                "--since",
                since,
                "--timeout",
                "5s",
                "--holdout",
                "0s",
                "--format",
                "jsonl",
            ],
        );
        assert!(out.status.success(), "exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(
            stdout.lines().count(),
            1,
            "jsonl is one envelope line: {stdout}"
        );
        serde_json::from_str::<serde_json::Value>(&stdout).expect("valid json")
    };
    let ids = |v: &serde_json::Value| -> Vec<String> {
        v["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect()
    };

    let first = watch_jsonl("0");
    assert_eq!(ids(&first), ["first"]);
    ta(&dir, &["create", "second"]);

    let second = watch_jsonl(&first["cursor"].to_string());
    assert_eq!(
        ids(&second),
        ["second"],
        "only the write after the cursor: {second}"
    );
    assert!(
        second["cursor"].as_u64() > first["cursor"].as_u64(),
        "the cursor advances: {second}"
    );
}

/// Leaving the filter is reported like entering it: watching `status=in_progress`
/// sees the task move OUT to closed, with the diff showing which way it went.
#[test]
fn watch_reports_a_task_leaving_the_filter() {
    let dir = fresh_dir("watch-leave");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a", "status=in_progress"]);
    ta(&dir, &["create", "other", "status=todo"]);
    let since = ta(&dir, &["status", "--current"]);
    ta(&dir, &["update", "a", "status=closed"]);
    ta(&dir, &["update", "other", "status=closed"]); // never matched

    let out = run(
        ta_bin(),
        &dir,
        &[
            "watch",
            "--since",
            since.trim(),
            "--timeout",
            "5s",
            "--holdout",
            "0s",
            "status=in_progress",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit 0 on leaving the filter: {stdout}"
    );
    assert!(
        stdout.contains("a:")
            && stdout.contains("- status: in_progress")
            && stdout.contains("+ status: closed"),
        "the exit transition is reported: {stdout}"
    );
    assert!(
        !stdout.contains("other:"),
        "a task matching at neither end is not: {stdout}"
    );
}

/// `--columns` decides both what wakes a watch and what it prints: a change to
/// an unwatched field alone reports nothing, and a batch that touches both
/// shows only the watched column.
#[test]
fn watch_columns_narrow_both_trigger_and_diff() {
    let dir = fresh_dir("watch-columns");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a", "status=todo", "notes=start"]);
    let since = ta(&dir, &["status", "--current"]);
    let watch = |since: &str| {
        run(
            ta_bin(),
            &dir,
            &[
                "watch",
                "--since",
                since,
                "--timeout",
                "1s",
                "--holdout",
                "0s",
                "--columns",
                "status",
            ],
        )
    };

    ta(&dir, &["update", "a", "notes+=more"]);
    let quiet = watch(since.trim());
    assert_eq!(
        quiet.status.code(),
        Some(1),
        "an unwatched change doesn't wake it: {}",
        String::from_utf8_lossy(&quiet.stdout)
    );

    ta(&dir, &["update", "a", "status=in_progress", "notes+=again"]);
    // A task created without the watched column changes nothing watched
    // (`status=` opts out of the default status stamp).
    ta(&dir, &["create", "bare", "status=", "notes=x"]);
    let out = watch(since.trim());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "a watched change wakes it: {stdout}");
    assert!(
        stdout.contains("- status: todo") && stdout.contains("+ status: in_progress"),
        "the watched column's diff: {stdout}"
    );
    assert!(
        !stdout.contains("notes"),
        "unwatched columns are left out: {stdout}"
    );
    assert!(
        !stdout.contains("bare:"),
        "nothing watched changed on `bare`: {stdout}"
    );
}

/// `deps` watches relationship changes.
#[test]
fn watch_columns_deps_reports_edge_changes() {
    let dir = fresh_dir("watch-columns-deps");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a", "status=todo"]);
    ta(&dir, &["create", "b", "status=todo"]);
    let since = ta(&dir, &["status", "--current"]);
    ta(&dir, &["update", "a", "status=in_progress"]);
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);

    let out = run(
        ta_bin(),
        &dir,
        &[
            "watch",
            "--since",
            since.trim(),
            "--timeout",
            "5s",
            "--holdout",
            "0s",
            "--columns",
            "deps",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit 0: {stdout}");
    assert!(
        stdout.contains("depends_on") && stdout.contains('b'),
        "the new edge: {stdout}"
    );
    assert!(!stdout.contains("status"), "status isn't watched: {stdout}");
}

/// A column that could never fire is an immediate error, not a silent wait
/// until the timeout: a typo (with a did-you-mean), a computed column, and a
/// relationship type name.
#[test]
fn watch_columns_rejects_unwatchable_names_up_front() {
    let dir = fresh_dir("watch-columns-reject");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a", "status=todo"]);

    for (column, expected) in [
        ("statsu", "did you mean `status`?"),
        ("update_time", "computed column"),
        ("depends_on", "show under the `deps` column"),
    ] {
        let started = std::time::Instant::now();
        let out = run(
            ta_bin(),
            &dir,
            &[
                "watch",
                "--since",
                "0",
                "--timeout",
                "30s",
                "--columns",
                column,
            ],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "`{column}` rejected: {stderr}");
        assert!(stderr.contains(expected), "`{column}`: {stderr}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "rejected before blocking"
        );
    }
}
