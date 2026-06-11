mod common;
use common::*;

/// A PRE-1.0 store (a legacy `AddDep` op / `dep`/`type` edge keys) can no longer
/// be read OR migrated in v1 — the read shims and the depends_on migration passes
/// are gone. Both a normal command and `repair` refuse, pointing at the last 0.x
/// release's `ta repair --migrate` (the sanctioned 0.x→v1 upgrade path), rather
/// than silently dropping the legacy edge.
#[test]
fn pre_1_0_store_is_refused_with_a_migration_hint() {
    let dir = fresh_dir("repair-legacy-refused");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    // Plant a legacy untyped AddDep edge (b depends on a) at the next seq.
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
    fs::write(&log, &content).unwrap();

    // A normal command refuses, explaining the pre-1.0 upgrade path.
    let blocked = run(ta_bin(), &dir, &["list"]);
    assert!(!blocked.status.success(), "pre-1.0 store must be refused");
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains("pre-1.0") && stderr.contains("repair --migrate"),
        "stderr should explain the pre-1.0 upgrade path: {stderr}"
    );

    // `repair --migrate` ALSO refuses: v1 has no pass for a pre-1.0 store, so it
    // must not load-and-rewrite (which would drop the legacy edge) — it points
    // back at the last 0.x. The store is left untouched.
    let repaired = run(ta_bin(), &dir, &["repair", "--migrate"]);
    assert!(
        !repaired.status.success(),
        "repair must refuse a pre-1.0 store rather than silently corrupt it"
    );
    assert!(
        String::from_utf8_lossy(&repaired.stderr).contains("repair --migrate"),
        "repair stderr points at the last 0.x: {}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        content,
        "the refused store's log is left byte-for-byte unchanged"
    );
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

    // Guards: reserved destinations and the status field are refused (the
    // TYPE field is a legal destination — covered separately).
    assert!(!run(ta_bin(), &dir, &["repair", "--rename", "deps=sev"])
        .status
        .success());
    assert!(!run(ta_bin(), &dir, &["repair", "--rename", "status=sev"])
        .status
        .success());
}

/// `--rename type=OLD` adopts a de-facto discriminator column as the task
/// type — converting ONLY records whose value names a declared type (repair
/// never writes data the schema would reject).
#[test]
fn repair_rename_adopts_a_type_column_only_for_declared_values() {
    let dir = fresh_dir("repair-type-adopt");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "c1", "category=bug"]);
    ta(&dir, &["create", "c2", "category=misc"]);
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[task_types.bug]\n[task_types.feature]\n");
    fs::write(&cfg, text).unwrap();

    let out = ta(&dir, &["repair", "--rename", "type=category"]);
    assert!(
        out.contains("renamed `category` -> `type`: 1 record(s)"),
        "declared value converted: {out}"
    );
    assert!(
        out.contains("kept `category` on 1 record(s)"),
        "undeclared value kept, reported: {out}"
    );

    // c1 is typed (canonical key on disk, display name on read), its old
    // column gone; c2 keeps the column, untyped, in the remainder.
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(log.contains(r#""task_type":"bug""#), "canonical key: {log}");
    let c1 = ta(&dir, &["show", "c1", "--format", "json"]);
    assert!(
        c1.contains(r#""type":"bug""#) && !c1.contains("category"),
        "{c1}"
    );
    let c2 = ta(&dir, &["show", "c2", "--format", "json"]);
    assert!(
        c2.contains(r#""category":"misc""#) && !c2.contains(r#""type""#),
        "{c2}"
    );
    assert!(out.contains("still don't conform"), "c2 reported: {out}");
}
