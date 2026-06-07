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

/// A store that renamed `status_field` BEFORE storage became canonical has its
/// data keyed under the display name; detection blocks, and the migration
/// re-keys it to the canonical `status`.
#[test]
fn repair_migrate_rekeys_display_named_status() {
    let dir = fresh_dir("repair-status-key");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["config", "set", "workflow.status_field", "state"]);
    ta(&dir, &["create", "a", "state=open"]); // stored canonically

    // Plant a pre-canonical event: the status under the display name.
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
        "{{\"seq\":{next},\"timestamp\":\"2026-01-01T00:00:00Z\",\"op\":\"Create\",\
         \"task_id\":\"legacy\",\"state\":\"open\"}}\n"
    ));
    fs::write(&log, content).unwrap();

    // Blocked with the migrate pointer; migrating re-keys to canonical.
    let blocked = run(ta_bin(), &dir, &["list"]);
    assert!(!blocked.status.success(), "stale store refused");
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("repair --migrate"));
    assert!(ta(&dir, &["repair", "--migrate"]).contains("canonical-status-key"));

    // The legacy task now reads under the display name, stored canonically.
    assert!(
        ta(&dir, &["show", "legacy", "--format", "json"]).contains(r#""state":"open""#),
        "display name on read"
    );
    let migrated = fs::read_to_string(&log).unwrap();
    assert!(
        migrated.contains(r#""task_id":"legacy","status":"open""#)
            || (migrated.contains(r#""task_id":"legacy""#)
                && !migrated.contains(r#""state":"open""#)),
        "canonical key on disk: {migrated}"
    );
    assert!(ta(&dir, &["repair", "--migrate"]).contains("up to date"));
}

/// `repair --schema` fixes everything lossless by direct rewrite — numeric
/// strings, scalars to singletons, bool strings, date normalization — and
/// lists the ambiguous remainder without guessing; typing untyped tasks
/// happens only via the explicit `--set-type-if-none TYPE`. Review surface is
/// the git diff; no confirmation.
#[test]
fn repair_schema_applies_lossless_fixes_and_reports_the_rest() {
    let dir = fresh_dir("repair-schema");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Grandfathered mess, created before any schema existed: a numeric string,
    // a bare scalar destined for a set, a bool string, a short date, and a
    // number destined for a string field.
    ta(
        &dir,
        &[
            "create",
            "a",
            r#"points="3""#,
            "tags=solo",
            r#"flag="true""#,
            "due=2026-01-02",
            "version=2.5",
        ],
    );
    ta(&dir, &["create", "b", "points=7"]);
    // ONE declared type -> the backfill is unambiguous; `owner` stays a
    // suggestion (required, no default exists yet).
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str(
        "\n[task_types.job.fields]\npoints = \"uint\"\ntags = \"set<string>\"\n\
         flag = \"bool\"\ndue = \"datetime\"\nversion = \"string\"\n\
         [task_types.job.fields.owner]\ntype = \"string\"\nrequired = true\n",
    );
    fs::write(&cfg, text).unwrap();

    // `--schema` alone NEVER types a task — typing is an explicit migration
    // choice, even with a single declared type (the user may be migrating
    // gradually or keeping tasks untyped).
    let bare = ta(&dir, &["repair", "--schema"]);
    assert!(
        !bare.contains("typed") && bare.contains("missing the `type` field"),
        "no inferred typing; untyped tasks stay in the remainder: {bare}"
    );
    // An undeclared type is rejected.
    assert!(
        !run(ta_bin(), &dir, &["repair", "--set-type-if-none", "ghost"])
            .status
            .success(),
        "unknown type refused"
    );

    let out = ta(&dir, &["repair", "--schema", "--set-type-if-none", "job"]);
    assert!(out.contains("typed `type` on"), "explicit backfill: {out}");
    assert!(out.contains("`points` on `a`"), "fix reported: {out}");
    assert!(
        out.contains("still don't conform") && out.contains("owner"),
        "ambiguous remainder listed: {out}"
    );

    // The fixes landed ON DISK (rewritten records, canonical shapes).
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""task_type":"job""#),
        "backfill on disk: {log}"
    );
    assert!(log.contains(r#""points":3"#), "numeric string fixed: {log}");
    assert!(log.contains(r#""tags":["solo"]"#), "singleton lift: {log}");
    assert!(log.contains(r#""flag":true"#), "bool string fixed: {log}");
    assert!(
        log.contains(r#""due":"2026-01-02T00:00:00+00:00""#),
        "date normalized to RFC 3339: {log}"
    );
    assert!(
        log.contains(r#""version":"2.5""#),
        "number to string: {log}"
    );

    // Idempotent: a second run fixes nothing, still reporting the remainder.
    let again = ta(&dir, &["repair", "--schema"]);
    assert!(again.contains("Nothing to fix"), "{again}");
    assert!(again.contains("owner"), "remainder persists: {again}");

    // Fixing the remainder the suggested way clears the read warning.
    ta(&dir, &["update", "a", "owner=ann"]);
    ta(&dir, &["update", "b", "owner=bob"]);
    let quiet = run(ta_bin(), &dir, &["list"]);
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("do not conform"),
        "store conforms after repair + suggested updates"
    );
}

/// `repair --rename NEW=OLD` moves a stray column under its declared name,
/// with the destination's coercion applied by the lossless pass that follows.
#[test]
fn repair_rename_moves_a_column_and_coerces_it() {
    let dir = fresh_dir("repair-rename");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "t1", r#"sev="3""#]);
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[task_types.job.fields]\nseverity = \"uint\"\n");
    fs::write(&cfg, text).unwrap();

    let out = ta(
        &dir,
        &[
            "repair",
            "--rename",
            "severity=sev",
            "--set-type-if-none",
            "job",
        ],
    );
    assert!(
        out.contains("renamed `sev` -> `severity`"),
        "assignment-style spec: {out}"
    );
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""severity":3"#) && !log.contains(r#""sev""#),
        "moved AND coerced to the declared uint: {log}"
    );

    // Guards: reserved and canonical/display destinations are refused.
    assert!(!run(ta_bin(), &dir, &["repair", "--rename", "deps=sev"])
        .status
        .success());
    assert!(!run(ta_bin(), &dir, &["repair", "--rename", "type=sev"])
        .status
        .success());
}
