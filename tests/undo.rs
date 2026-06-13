mod common;
use common::names::*;
use common::*;
use taska::model::{DEPS_KEY, REL_KEY, TARGET_KEY};

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
    ta(&dir, &["update", "a", "owner=bob"]);
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
        last.contains(r#""op":"RemoveEdge""#)
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

    ta(&sub, &["create", "a", "status=open"]);
    ta(&sub, &["update", "a", "status=closed"]);
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
        ta(&sub, &["show", "a", "--format", "json"]).contains(r#""status":"open""#),
        "state walked back to the committed prior value"
    );
}
