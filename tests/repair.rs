mod common;
use common::*;

/// A legacy untyped dep event (no `type` key) is detected on read — a normal
/// command fails pointing at `ta repair --migrate` — and the migration stamps
/// the configured default blocker type, after which the store reads and the dep
/// gates readiness. Re-running the migration is a no-op.
#[test]
fn repair_migrate_types_legacy_dep_events() {
    let dir = fresh_dir("repair-migrate");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    // Plant two pre-rename events at the next seqs: a fully legacy untyped
    // AddDep (b depends on a), and a typed one still using the old op name and
    // `dep`/`type` payload keys (b relates_to a).
    let log = dir.join(".taska").join("mutations.jsonl");
    let mut content = fs::read_to_string(&log).unwrap();
    let next = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|e| e["seq"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    content.push_str(&format!(
        "{{\"seq\":{next},\"timestamp\":\"2026-01-01T00:00:00Z\",\"op\":\"AddDep\",\
         \"task_id\":\"b\",\"dep\":\"a\"}}\n"
    ));
    content.push_str(&format!(
        "{{\"seq\":{},\"timestamp\":\"2026-01-01T00:00:00Z\",\"op\":\"AddDep\",\
         \"task_id\":\"b\",\"dep\":\"a\",\"type\":\"relates_to\"}}\n",
        next + 1
    ));
    fs::write(&log, content).unwrap();

    // A normal command refuses and points at repair.
    let blocked = run(ta_bin(), &dir, &["list"]);
    assert!(!blocked.status.success(), "stale store should be refused");
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("repair --migrate"),
        "stderr should point at repair: {}",
        String::from_utf8_lossy(&blocked.stderr)
    );

    // Migrate, then it reads: the untyped dep got the default blocker type, the
    // typed edge kept relates_to, and the blocker gates readiness.
    assert!(ta(&dir, &["repair", "--migrate"]).contains("migrated"));
    assert!(ta(&dir, &["show", "b", "--format", "json"])
        .contains("\"deps\":{\"depends_on\":[\"a\"],\"relates_to\":[\"a\"]}"));
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(
        lists_task(&ready, "a") && !lists_task(&ready, "b"),
        "b blocked: {ready}"
    );

    // The rewritten log speaks ONLY the current vocabulary: AddEdge ops with
    // target/rel keys; no AddDep op names or dep/type payload keys survive.
    let migrated = fs::read_to_string(&log).unwrap();
    assert!(
        migrated.contains(r#""op":"AddEdge""#)
            && migrated.contains(r#""target":"a""#)
            && migrated.contains(r#""rel":"relates_to""#),
        "new vocabulary on disk: {migrated}"
    );
    assert!(
        !migrated.contains("AddDep") && !migrated.contains(r#""dep":"#),
        "no legacy vocabulary left: {migrated}"
    );

    // Idempotent.
    assert!(ta(&dir, &["repair", "--migrate"]).contains("up to date"));
}
