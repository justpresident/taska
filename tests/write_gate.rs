mod common;
use common::names::*;
use common::*;
use taska::model::{DEPS_KEY, DEP_KEY, ID_KEY, UNBLOCKS_KEY};

#[test]
fn reserved_field_keys_are_rejected() {
    let dir = fresh_dir("reserved");
    init_renamed_open(&dir);

    let log = dir.join(".taska/mutations.jsonl");
    // Each reserved envelope key must be refused up front (non-zero exit) and
    // append nothing, so it can never shadow the event envelope.
    for key in ["seq", "op", "task_id", "timestamp", "_meta"] {
        let out = run(ta_bin(), &dir, &["create", "x", &format!("{key}=v")]);
        assert!(
            !out.status.success(),
            "reserved key `{key}` must be rejected, got:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("reserved"),
            "stderr should explain the reservation for `{key}`: {stderr}"
        );
        assert!(
            !log.exists() || rows(&log) == 0,
            "a rejected reserved-key create must append nothing"
        );
    }
}

/// A new write must REFUSE to mint a `seq` when the log holds a line it can't
/// parse - otherwise `max_seq` would under-count and hand out a duplicate seq,
/// corrupting the append-only order. The classic trigger is a stale binary that
/// predates a newer `OpType`; here we simulate it with an unknown-op line that
/// already carries `seq` 2, so a tolerant (skip-and-mint) writer would re-mint 2.
#[test]
fn append_refuses_to_mint_over_an_unparseable_log_line() {
    let dir = fresh_dir("append-corrupt-log");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", "title=A"]); // seq 1

    let log = dir.join(".taska").join("mutations.jsonl");
    let mut content = fs::read_to_string(&log).unwrap();
    content.push_str(
        "{\"seq\":2,\"timestamp\":\"2026-01-01T00:00:00Z\",\"op\":\"FutureOp\",\"task_id\":\"b\"}\n",
    );
    fs::write(&log, &content).unwrap();

    let out = run(ta_bin(), &dir, &["create", "c", "title=C"]);
    assert!(
        !out.status.success(),
        "create must fail rather than mint a duplicate seq over an unparseable line"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unparseable"),
        "error should name the unparseable line: {stderr}"
    );
    // And it must NOT have appended anything (no duplicate seq 2 written).
    let after = fs::read_to_string(&log).unwrap();
    assert_eq!(after, content, "the log must be left untouched");
}

/// The write-time gate: reject invalid mutations (duplicate create, missing
/// target, self-reference, `+=` on status) and silently drop no-ops (a field
/// already at its value, an edge already present), all before anything is logged.
#[test]
fn write_gate_rejects_invalid_and_skips_noops() {
    let dir = fresh_dir("write-gate");
    init_renamed_open(&dir);
    let log = dir.join(".taska/mutations.jsonl");
    let open = || format!("{STATUS_FIELD}=open");
    let closed = || format!("{STATUS_FIELD}=closed");
    ta(&dir, &["create", "a", &open()]);
    ta(&dir, &["create", "b", &open()]);

    // Duplicate create -> error, nothing written.
    let n = rows(&log);
    let dup = run(ta_bin(), &dir, &["create", "a", &open()]);
    assert!(!dup.status.success(), "duplicate create must fail");
    assert!(String::from_utf8_lossy(&dup.stderr).contains("already exists"));
    assert_eq!(rows(&log), n, "duplicate create wrote nothing");

    // Mutating a non-existent task -> error.
    let ghost = run(ta_bin(), &dir, &["update", "nope", &closed()]);
    assert!(
        !ghost.status.success(),
        "update of a missing task must fail"
    );
    assert!(String::from_utf8_lossy(&ghost.stderr).contains("no task"));

    // No-op update (same value) writes nothing; a real change writes one event.
    let n = rows(&log);
    let noop = ta(&dir, &["update", "a", &open()]);
    assert!(noop.contains("no changes"), "got: {noop}");
    assert_eq!(rows(&log), n, "a no-op update writes nothing");
    ta(&dir, &["update", "a", &closed()]);
    assert_eq!(rows(&log), n + 1, "a real change writes one event");

    // Multi-field update drops the unchanged field, keeps the changed one.
    let n = rows(&log);
    ta(&dir, &["update", "a", &closed(), "owner=alice"]);
    assert_eq!(rows(&log), n + 1, "only the changed field is written");
    assert!(ta(&dir, &["show", "a", "--format", "json"]).contains("\"owner\":\"alice\""));

    // Self-reference -> error.
    let selfref = run(
        ta_bin(),
        &dir,
        &["dep", "add", "a", &format!("{BLOCKER}=a")],
    );
    assert!(!selfref.status.success(), "self-reference must fail");
    assert!(String::from_utf8_lossy(&selfref.stderr).contains("itself"));

    // dep add is idempotent: the same edge a second time is a no-op.
    let n = rows(&log);
    ta(&dir, &["dep", "add", "b", &format!("{BLOCKER}=a")]);
    assert_eq!(rows(&log), n + 1, "first edge writes");
    assert!(ta(&dir, &["dep", "add", "b", &format!("{BLOCKER}=a")]).contains("no changes"));
    assert_eq!(rows(&log), n + 1, "a duplicate edge writes nothing");

    // `+=` is rejected on the single-valued status field, fine on free text.
    let bad = run(
        ta_bin(),
        &dir,
        &["update", "b", &format!("{STATUS_FIELD}+=x")],
    );
    assert!(!bad.status.success(), "+= on status must fail");
    assert!(String::from_utf8_lossy(&bad.stderr).contains(STATUS_FIELD));
    ta(&dir, &["update", "b", "notes+=hello"]);
}

/// More gate rules: a dependency on a missing task, a reserved/computed field
/// name (`deps`/`dep`/`id`/timestamp/graph/relationship columns), and deleting a
/// task that doesn't exist are all rejected up front.
#[test]
fn gate_rejects_dangling_targets_reserved_fields_and_missing_delete() {
    let dir = fresh_dir("gate-more");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);

    // A dependency on a non-existent task is rejected (no dangling edge).
    let dangling = run(
        ta_bin(),
        &dir,
        &["dep", "add", "a", &format!("{BLOCKER}=ghost")],
    );
    assert!(
        !dangling.status.success(),
        "dep on a missing task must fail"
    );
    assert!(String::from_utf8_lossy(&dangling.stderr).contains("ghost"));

    // Reserved/computed field names can't be set - they'd be silently shadowed.
    // The structural/computed ones (`deps`/`dep`/`id`/`unblocks`) are fixed, but
    // the timestamp column and the relationship inverse are CONFIGURED names, so
    // the renamed `made_at`/`feeds` must be reserved (not the defaults) - guarding
    // that the reserved check reads config, never hardcodes `create_time`/`blocks`.
    for field in [
        format!("{DEPS_KEY}=x"),
        format!("{DEP_KEY}=x"),
        format!("{ID_KEY}=x"),
        format!("{CREATE_TIME}=x"),
        format!("{UNBLOCKS_KEY}=x"),
        format!("{BLOCKER_INV}=x"),
    ] {
        let out = run(ta_bin(), &dir, &["update", "a", &field]);
        assert!(!out.status.success(), "setting `{field}` must fail");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("reserved or computed"),
            "{field}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        !run(ta_bin(), &dir, &["create", "z", &format!("{DEPS_KEY}=x")])
            .status
            .success(),
        "create with a reserved field must fail"
    );
    // A normal field still works.
    ta(&dir, &["update", "a", "owner=bob"]);

    // Deleting a missing task errors rather than writing a no-op Delete.
    let baddelete = run(ta_bin(), &dir, &["delete", "ghost"]);
    assert!(
        !baddelete.status.success(),
        "delete of a missing task must fail"
    );
    assert!(String::from_utf8_lossy(&baddelete.stderr).contains("no task"));
}
