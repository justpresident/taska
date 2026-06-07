mod common;
use common::*;

#[test]
fn config_get_set_list_validates_and_preserves_comments() {
    let dir = fresh_dir("config-cmd");
    init_repo(&dir);
    ta(&dir, &["init"]);
    let cfg_path = dir.join(".taska/config.toml");

    // get reads effective values, including defaults and nested sub-tables.
    assert_eq!(
        ta(&dir, &["config", "get", "compaction.keep_events"]).trim(),
        "5000"
    );
    assert_eq!(
        ta(&dir, &["config", "get", "workflow.status_field"]).trim(),
        "status"
    );

    // set coerces by TOML grammar and persists the change.
    ta(&dir, &["config", "set", "compaction.keep_events", "500"]);
    assert_eq!(
        ta(&dir, &["config", "get", "compaction.keep_events"]).trim(),
        "500"
    );
    ta(&dir, &["config", "set", "merge.on_conflict", "ours"]);
    assert_eq!(
        ta(&dir, &["config", "get", "merge.on_conflict"]).trim(),
        "ours"
    );

    // The documented comments survive a rewrite (toml_edit, not re-serialization).
    let text = fs::read_to_string(&cfg_path).unwrap();
    assert!(
        text.contains("Keep at least this many"),
        "comment kept: {text}"
    );

    // Invalid edits are rejected and leave the file untouched.
    for bad in [
        ["config", "set", "compaction.keep_events", "50"], // below the floor
        ["config", "set", "merge.on_conflict", "bogus"],   // unknown enum variant
        ["config", "set", "workflow.bogus_key", "x"],      // unknown key (typo guard)
    ] {
        let out = run(ta_bin(), &dir, &bad);
        assert!(!out.status.success(), "`ta {}` should fail", bad.join(" "));
    }
    // keep_events is still 500 — no rejected edit slipped through.
    assert_eq!(
        ta(&dir, &["config", "get", "compaction.keep_events"]).trim(),
        "500"
    );

    // list prints every effective value as sorted dotted keys.
    let list = ta(&dir, &["config", "list"]);
    assert!(
        list.contains("compaction.keep_events = 500"),
        "list: {list}"
    );
    assert!(list.contains("merge.on_conflict = ours"), "list: {list}");
    assert!(
        list.contains("display.column_max_width.title = 80"),
        "nested key flattened: {list}"
    );
}

#[test]
fn config_validate_flags_an_undeclared_relationship_type() {
    let dir = fresh_dir("cfg-validate-undeclared");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["dep", "add", "a", "relates_to=b"]);

    // A consistent store validates clean.
    assert!(
        ta(&dir, &["config", "validate"]).contains("Config OK"),
        "clean store validates"
    );

    // Drop `relates_to` from the config while task `a` still uses it.
    let cfg = dir.join(".taska/config.toml");
    fs::write(
        &cfg,
        // Deliberately the LEGACY `type =` spelling: the pre-rename key must keep
        // loading as an alias of `kind`.
        "[relationships.depends_on]\ntype = \"blocker\"\ninverse = \"blocks\"\n",
    )
    .unwrap();
    let out = run(ta_bin(), &dir, &["config", "validate"]);
    assert!(
        !out.status.success(),
        "undeclared-type config must be rejected"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("relates_to") && err.contains("not declared"),
        "error names the undeclared type: {err}"
    );
}

#[test]
fn config_validate_flags_a_blocker_cycle_and_set_runs_the_same_check() {
    let dir = fresh_dir("cfg-validate-cycle");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "x"]);
    ta(&dir, &["create", "y"]);
    ta(&dir, &["dep", "add", "x", "depends_on=y"]);
    ta(&dir, &["dep", "add", "y", "depends_on=x"]);

    let out = run(ta_bin(), &dir, &["config", "validate"]);
    assert!(!out.status.success(), "a blocker cycle must be reported");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cycle"),
        "error mentions the cycle: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `config set` runs the same validation, so it too refuses while the graph
    // is inconsistent (the cheap struct-only path keeps fixing commands usable).
    let set = run(
        ta_bin(),
        &dir,
        &["config", "set", "merge.on_conflict", "ours"],
    );
    assert!(!set.status.success(), "config set runs graph validation");
}

#[test]
fn renamed_status_field_is_display_only_storage_stays_canonical() {
    let dir = fresh_dir("status-display");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["config", "set", "workflow.status_field", "state"]);

    // The display name is what you type and what you see...
    ta(&dir, &["create", "t1", "state=open"]);
    ta(&dir, &["create", "t2"]); // default_status stamped
    ta(&dir, &["update", "t2", "state=closed"]);
    let shown = ta(&dir, &["show", "t1", "--format", "json"]);
    assert!(
        shown.contains(r#""state":"open""#) && !shown.contains(r#""status""#),
        "display name in output: {shown}"
    );
    // ...and the workflow machinery follows it: filtering, --open, --ready.
    assert!(lists_task(&ta(&dir, &["list", "state=open"]), "t1"));
    let open = ta(&dir, &["list", "--open"]);
    assert!(
        lists_task(&open, "t1") && !lists_task(&open, "t2"),
        "t2 is done under the renamed field: {open}"
    );

    // ...but STORAGE is canonical: events carry `status`, never `state`.
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""status":"open""#) && !log.contains(r#""state":"open""#),
        "canonical key on disk: {log}"
    );

    // The canonical spelling is not directly writable while renamed...
    let out = run(ta_bin(), &dir, &["update", "t1", "status=x"]);
    assert!(!out.status.success(), "canonical spelling rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("state"),
        "error points at the display name"
    );
    // ...and the single-valued append rejection follows the display name.
    let out = run(ta_bin(), &dir, &["update", "t1", "state+=more"]);
    assert!(!out.status.success(), "+= on the renamed status rejected");

    // Renaming AGAIN is free — the data was canonical all along.
    ta(&dir, &["config", "set", "workflow.status_field", "phase"]);
    assert!(
        ta(&dir, &["show", "t1", "--format", "json"]).contains(r#""phase":"open""#),
        "second rename needs no migration"
    );
}
