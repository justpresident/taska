//! `ta history` e2e - the per-event log: one entry per event with that event's
//! own diff, the criteria rule (matched before OR after), bare ids incl.
//! deleted tasks, --count/--reverse/--columns, undo annotations, and the
//! cursor hand-off to `ta watch`.
mod common;
use common::*;
use taska::model::STATUS_KEY;

fn st(v: &str) -> String {
    format!("{STATUS_KEY}={v}")
}

/// The `seq` of each event in a `--format json` history, in output order.
fn seqs(dir: &std::path::Path, args: &[&str]) -> Vec<u64> {
    let mut all = vec!["history", "--format", "json"];
    all.extend_from_slice(args);
    let v: serde_json::Value = serde_json::from_str(&ta(dir, &all)).unwrap();
    v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect()
}

#[test]
fn each_event_gets_its_own_diff_oldest_first() {
    let dir = fresh_dir("history-basic");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "api", &st("todo")]);
    ta(&dir, &["update", "api", &st("in_progress")]);
    ta(&dir, &["update", "api", &st("closed")]);

    let out = ta(&dir, &["history", "api"]);
    // Three entries, not watch's one net change.
    let (create, start, close) = (
        out.find("create  `api`").unwrap(),
        out.find("+ status: in_progress").unwrap(),
        out.find("+ status: closed").unwrap(),
    );
    assert!(create < start && start < close, "oldest-first: {out}");
    assert!(
        out.contains("- status: todo"),
        "each event's own before: {out}"
    );
    // Closed by the cursor a `watch --since` continues from.
    let cursor = ta(&dir, &["status", "--current"]);
    assert_eq!(
        out.lines().last(),
        Some(format!("[seq:{}]", cursor.trim()).as_str()),
        "{out}"
    );
}

#[test]
fn criteria_bare_ids_and_deleted_tasks() {
    let dir = fresh_dir("history-criteria");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "api", &st("in_progress")]); // seq 1
    ta(&dir, &["create", "db", &st("todo")]); // seq 2
    ta(&dir, &["update", "api", &st("closed")]); // seq 3
    ta(&dir, &["delete", "db"]); // seq 4

    // Criteria select TASKS by current state, then show their whole history:
    // api is closed now, so its in_progress past is under `status=closed`...
    assert_eq!(seqs(&dir, &[&st("closed")]), [1, 3]);
    assert_eq!(seqs(&dir, &[&st("in_progress")]), [] as [u64; 0]);
    // ...and a deleted task is selected by its LAST state.
    assert_eq!(seqs(&dir, &[&st("todo")]), [2, 4]);
    // Bare ids OR; a deleted task's history is still there.
    assert_eq!(seqs(&dir, &["db"]), [2, 4]);
    assert_eq!(seqs(&dir, &["api", "db"]), [1, 2, 3, 4]);
    // An unknown id is an error with a did-you-mean.
    let out = run(ta_bin(), &dir, &["history", "dv"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("did you mean `db`?"));
}

#[test]
fn since_count_reverse_and_columns_compose() {
    let dir = fresh_dir("history-flags");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "api", &st("todo")]); // 1
    ta(&dir, &["update", "api", "notes=a", "--new-field"]); // 2
    ta(&dir, &["update", "api", &st("in_progress")]); // 3
    ta(&dir, &["update", "api", "notes+=b"]); // 4
    ta(&dir, &["update", "api", &st("closed")]); // 5

    assert_eq!(seqs(&dir, &["--since", "3"]), [4, 5]);
    assert_eq!(seqs(&dir, &["--count", "2"]), [4, 5], "the LATEST two");
    assert_eq!(seqs(&dir, &["--count", "2", "--reverse"]), [5, 4]);
    assert_eq!(
        seqs(&dir, &["--columns", STATUS_KEY]),
        [1, 3, 5],
        "only events changing a watched column"
    );
    let out = ta(&dir, &["history", "--columns", STATUS_KEY]);
    assert!(
        !out.contains("notes"),
        "diffs narrowed to the columns: {out}"
    );
}

#[test]
fn undo_compensations_are_annotated() {
    let dir = fresh_dir("history-undo");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "api", &st("todo")]);
    ta(&dir, &["update", "api", &st("closed")]); // seq 2
                                                 // Committed, so undo appends a compensation instead of truncating.
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "work"]);
    ta(&dir, &["undo", "--force"]);

    let out = ta(&dir, &["history", "api"]);
    assert!(out.contains("(undoes seq 2)"), "{out}");
    let v: serde_json::Value =
        serde_json::from_str(&ta(&dir, &["history", "api", "--format", "json"])).unwrap();
    assert_eq!(v["events"][2]["undoes"], 2, "{v}");
}

/// Catch up with `history`, then continue with `watch --since <cursor>`: the
/// next change arrives, nothing is repeated.
#[test]
fn the_cursor_hands_off_to_watch() {
    let dir = fresh_dir("history-handoff");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "api", &st("todo")]);
    let v: serde_json::Value =
        serde_json::from_str(&ta(&dir, &["history", "--format", "json"])).unwrap();
    let cursor = v["cursor"].to_string();
    ta(&dir, &["update", "api", &st("closed")]);

    let out = run(
        ta_bin(),
        &dir,
        &[
            "watch",
            "--since",
            &cursor,
            "--timeout",
            "5s",
            "--holdout",
            "0s",
            "--format",
            "json",
        ],
    );
    let w: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let added = &w["changes"][0]["added"];
    assert_eq!(added, &serde_json::json!(["status: closed"]), "{w}");
}

#[test]
fn an_empty_history_still_reports_the_cursor() {
    let dir = fresh_dir("history-empty");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "api", &st("todo")]);
    let out = ta(&dir, &["history", "--since", "1"]);
    assert!(
        out.contains("(no history)") && out.contains("[seq:1]"),
        "{out}"
    );
}

/// `--open`/`--ready` select tasks exactly as `list --open`/`--ready` do - by
/// their state NOW - and show each selected task's whole history.
#[test]
fn open_and_ready_select_tasks_by_current_state() {
    let dir = fresh_dir("history-open-ready");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "a", &st("todo")]); // 1
    ta(&dir, &["create", "b", &st("todo")]); // 2
    ta(&dir, &["dep", "add", "a", "depends_on=b"]); // 3: a now blocked
    ta(&dir, &["update", "a", "notes=x", "--new-field"]); // 4: a blocked
    ta(&dir, &["update", "b", &st("closed")]); // 5: b closes, a unblocks
    ta(&dir, &["update", "a", "notes+=y"]); // 6: a ready
    ta(&dir, &["update", "b", "notes=late"]); // 7: b closed throughout

    // b is closed now: only a is open - and all of a's history shows.
    assert_eq!(seqs(&dir, &["--open"]), [1, 3, 4, 6]);
    // a is ready now (b is done): its whole history, blocked stretch included.
    assert_eq!(seqs(&dir, &["--ready"]), [1, 3, 4, 6]);
    // Block a again: it drops out of `--ready` entirely, but stays `--open`.
    ta(&dir, &["create", "c", &st("todo")]); // 8
    ta(&dir, &["dep", "add", "a", "depends_on=c"]); // 9
    assert!(!seqs(&dir, &["--ready"]).contains(&1), "a is blocked now");
    assert!(seqs(&dir, &["--open"]).contains(&9));
}
