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
/// (filtered on its at-cursor state, since it no longer exists).
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

/// `--format json` emits one object per changed task with structured
/// removed/added deltas (not full state), parseable by an agent.
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
    let arr = v.as_array().expect("a JSON array");
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
