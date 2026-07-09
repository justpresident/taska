mod common;
use common::names::*;
use common::*;
use taska::model::{DEPS_KEY, OP_KEY, REL_KEY, STATUS_KEY, TARGET_KEY};

#[test]
fn undo_uncommitted_truncates_the_tail() {
    let dir = fresh_dir("undo-uncommitted");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    // Nothing is committed, so undo just truncates the last (uncommitted) event.
    ta(&dir, &["undo", "--force"]);
    assert_eq!(rows(&log), before - 1, "one event truncated from the log");

    let list = ta(&dir, &["list"]);
    assert!(lists_task(&list, "a"), "a should remain: {list}");
    assert!(!lists_task(&list, "b"), "b should be gone: {list}");
}

#[test]
fn undo_count_removes_the_last_n() {
    let dir = fresh_dir("undo-count");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["create", "c"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    ta(&dir, &["undo", "--count", "2", "--force"]);
    assert_eq!(rows(&log), before - 2, "two events truncated");

    let list = ta(&dir, &["list"]);
    assert!(lists_task(&list, "a"), "a remains: {list}");
    assert!(!lists_task(&list, "b"), "b undone: {list}");
    assert!(!lists_task(&list, "c"), "c undone: {list}");
}

#[test]
fn undo_committed_appends_a_compensating_event() {
    let dir = fresh_dir("undo-committed");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);
    // Commit the create AND the update so the undone event is already committed.
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=closed")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Undo the committed update (no --remove): because the update is committed,
    // the log must GROW (a compensating event is appended), the committed event
    // stays, and the materialized state reverts.
    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    ta(&dir, &["undo", "--force"]);
    assert_eq!(
        rows(&log),
        before + 1,
        "committed undo appends a compensating event (log grows)"
    );

    // state reverts to its prior committed value.
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        json.contains(&format!(r#""{STATUS_FIELD}":"open""#)),
        "state reverted to open: {json}"
    );
}

#[test]
fn undo_committed_unsets_a_newly_added_field() {
    let dir = fresh_dir("undo-committed-unset");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);
    // Commit the create AND the field-add so the undone event is committed and
    // the compensating (append-only) path runs rather than truncation.
    ta(&dir, &["update", "a", "--new-field", "owner=bob"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    // Undo the committed field-add. The compensating Update must UNSET the field
    // (via the null convention), and the log must grow rather than shrink.
    ta(&dir, &["undo", "--force"]);
    assert_eq!(rows(&log), before + 1, "compensating event appended");

    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(!json.contains("owner"), "owner unset after undo: {json}");
    assert!(
        json.contains(&format!(r#""{STATUS_FIELD}":"open""#)),
        "state preserved: {json}"
    );
}

#[test]
fn undo_remove_truncates_committed_and_warns() {
    let dir = fresh_dir("undo-remove");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=closed")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);

    // --remove on a committed event truncates (log shrinks) and warns on stderr.
    let out = run(ta_bin(), &dir, &["undo", "--remove", "--force"]);
    assert!(out.status.success(), "undo --remove should succeed");
    assert_eq!(
        rows(&log),
        before - 1,
        "--remove truncates the committed event (log shrinks)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("DANGER"),
        "--remove on a committed event warns loudly: {stderr}"
    );

    // The earlier (still-committed) create remains; state reverts.
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        json.contains(&format!(r#""{STATUS_FIELD}":"open""#)),
        "the update was removed, leaving the create: {json}"
    );
}

#[test]
fn undo_with_empty_log_is_a_noop() {
    let dir = fresh_dir("undo-empty");
    init_renamed_open(&dir);
    let out = ta(&dir, &["undo", "--force"]);
    assert!(out.contains("Nothing to undo"), "got: {out}");
}

#[test]
fn undo_committed_compensates_typed_edges() {
    let dir = fresh_dir("undo-typed-edges");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["create", "b", &format!("{STATUS_FIELD}=open")]);
    // Commit everything up to and including a typed (non-needs) edge.
    ta(&dir, &["dep", "add", "a", &format!("{INFO}=b")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Undo the committed typed add: the log must GROW (typed RemoveEdge
    // compensation appended) and the edge must be gone from state.
    let log = dir.join(".taska/mutations.jsonl");
    let before = rows(&log);
    ta(&dir, &["undo", "--force"]);
    assert_eq!(
        rows(&log),
        before + 1,
        "compensation appended, not truncated"
    );
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(&format!(r#""{DEPS_KEY}":{{}}"#)),
        "typed edge compensated away"
    );
    let tail = std::fs::read_to_string(&log).unwrap();
    let last = tail.lines().last().unwrap();
    assert!(
        last.contains(&format!(r#""{OP_KEY}":"RemoveEdge""#))
            && last.contains(&format!(r#""{REL_KEY}":"{INFO}""#))
            && last.contains(&format!(r#""{TARGET_KEY}":"b""#)),
        "compensation is a TYPED RemoveEdge: {last}"
    );

    // Now the inverse: commit a typed REMOVE, undo it, and the edge returns.
    ta(&dir, &["dep", "add", "a", &format!("{INFO}=b")]);
    ta(&dir, &["dep", "remove", "a", &format!("{INFO}=b")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "removed"]);
    ta(&dir, &["undo", "--force"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"])
            .contains(&format!(r#""{DEPS_KEY}":{{"{INFO}":["b"]}}"#)),
        "undoing a committed typed remove restores the edge"
    );
}

#[test]
fn undo_compensates_committed_events_in_a_nested_store() {
    // The store lives BELOW the repo root: the committed-count probe must
    // resolve the blob path relative to the store's parent, or every committed
    // event reads as uncommitted and undo TRUNCATES shared history.
    let dir = fresh_dir("undo-nested");
    let sub = dir.join("svc");
    fs::create_dir_all(&sub).unwrap();
    run(ta_bin(), &sub, &["init"]); // store first, in a plain subdir...
    init_repo(&dir); // ...the repo appears ABOVE it
    run(ta_bin(), &sub, &["init"]); // register the drivers in the new repo

    ta(&sub, &["create", "a", &format!("{STATUS_KEY}=open")]);
    ta(&sub, &["update", "a", &format!("{STATUS_KEY}=closed")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    let log = sub.join(".taska/mutations.jsonl");
    let before = rows(&log);
    ta(&sub, &["undo", "--force"]);
    assert_eq!(
        rows(&log),
        before + 1,
        "committed undo must compensate (log grows), never truncate"
    );
    assert!(
        ta(&sub, &["show", "a", "--format", "json"]).contains(&format!(r#""{STATUS_KEY}":"open""#)),
        "state walked back to the committed prior value"
    );
}

#[test]
fn undo_preview_renders_a_field_diff_of_changed_columns() {
    let dir = fresh_dir("undo-diff");
    init_renamed_open(&dir); // status displayed as `state`, blocker as `needs`
    ta(&dir, &["create", "api", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["create", "db", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["update", "api", &format!("{STATUS_FIELD}=closed")]);
    ta(&dir, &["dep", "add", "api", &format!("{BLOCKER}=db")]);

    // The preview (printed even under --force, before applying) is a per-task diff
    // of ONLY the lines that change: current-state lines marked `-`, reverted `+`.
    // Field keys are CANONICAL (the events are), so status shows as `status`; a
    // reverted edge shows as its bare `type: target` line.
    let out = ta(&dir, &["undo", "--count", "2", "--force"]);
    assert!(out.contains("Undoing 2 event(s)"), "header: {out}");
    assert!(
        out.contains(&format!("- {STATUS_KEY}: closed")),
        "current status removed: {out}"
    );
    assert!(
        out.contains(&format!("+ {STATUS_KEY}: open")),
        "reverted status added: {out}"
    );
    assert!(
        out.contains(&format!("- {BLOCKER}: db")),
        "the added edge is shown removed: {out}"
    );
    // Only changed columns/tasks: the untouched `db` task never appears.
    assert!(!out.contains("db:"), "unaffected task absent: {out}");
}

/// The headline: repeated `undo` keeps walking BACK through real history instead
/// of bouncing on its own compensations.
#[test]
fn undo_walks_back_through_committed_history() {
    let dir = fresh_dir("undo-walkback");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=s0")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=s1")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=s2")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=s3")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "hist"]);

    let shows = |val: &str| {
        ta(&dir, &["show", "a", "--format", "json"])
            .contains(&format!(r#""{STATUS_FIELD}":"{val}""#))
    };
    assert!(shows("s3"), "starts at s3");
    ta(&dir, &["undo", "--force"]);
    assert!(shows("s2"), "undo #1 -> s2");
    // The crux: a second undo must walk to s1, NOT redo s3.
    ta(&dir, &["undo", "--force"]);
    assert!(
        shows("s1"),
        "undo #2 walks back to s1, never redoes its own compensation"
    );
    ta(&dir, &["undo", "--force"]);
    assert!(shows("s0"), "undo #3 -> s0");
}

/// Each committed undo records the seq it reverses under `_meta.undoes`, which is
/// what marks an original as already-undone for the next `undo`.
#[test]
fn undo_records_the_undone_seq_in_meta() {
    let dir = fresh_dir("undo-marker");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=closed")]); // seq 2
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "h"]);

    ta(&dir, &["undo", "--force"]); // undoes seq 2, appends a marked compensation
    let log = std::fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""_meta":{"undoes":2}"#),
        "the compensation marks the seq it undoes: {log}"
    );
}

/// `--seq` undoes a specific event; naming one that is already undone, is itself a
/// compensation, or doesn't exist is a hard error.
#[test]
fn undo_seq_targets_a_specific_event_and_validates_it() {
    let dir = fresh_dir("undo-seq");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=p0")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=p1")]); // seq 2
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=p2")]); // seq 3
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "h"]);

    // Undo the mid-history seq 2 (shadowed by seq 3, so state stays p2) - it is
    // still recorded as undone.
    ta(&dir, &["undo", "--seq", "2", "--force"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(&format!(r#""{STATUS_FIELD}":"p2""#)),
        "seq 2 was shadowed by seq 3, so state is unchanged"
    );

    // Re-undoing the same seq is rejected.
    let out = run(ta_bin(), &dir, &["undo", "--seq", "2", "--force"]);
    assert!(
        !out.status.success(),
        "re-undo of an already-undone seq must fail"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already undone"),
        "error names the cause: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An unknown seq is rejected.
    let out = run(ta_bin(), &dir, &["undo", "--seq", "999", "--force"]);
    assert!(
        !out.status.success()
            && String::from_utf8_lossy(&out.stderr).contains("no event with seq 999"),
        "unknown seq errors: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A plain `undo` now skips the already-undone seq 2 and reverses seq 3. With
    // both seq 2 and seq 3 undone, only the create (p0) survives.
    ta(&dir, &["undo", "--force"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(&format!(r#""{STATUS_FIELD}":"p0""#)),
        "the next undo skips undone seq 2 and reverts seq 3, leaving only p0"
    );
}

/// `--seq S --count N` starts at S and undoes N events going older.
#[test]
fn undo_seq_with_count_walks_older() {
    let dir = fresh_dir("undo-seq-count");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=v0")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=v1")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=v2")]);
    ta(&dir, &["update", "a", &format!("{STATUS_FIELD}=v3")]); // seq 4
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "h"]);

    // From seq 4, undo 3 events going older (seq 4, 3, 2), leaving only the create.
    ta(&dir, &["undo", "--seq", "4", "--count", "3", "--force"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(&format!(r#""{STATUS_FIELD}":"v0""#)),
        "undoing seq 4,3,2 walks the status back to v0"
    );
}
