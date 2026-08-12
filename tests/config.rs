mod common;
use common::*;
use taska::model::{STATUS_KEY, TASK_TYPE_KEY};

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
        STATUS_KEY
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
    // keep_events is still 500 - no rejected edit slipped through.
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
        shown.contains(r#""state":"open""#) && !shown.contains(STATUS_KEY),
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
        log.contains(&format!("\"{}\":\"open\"", STATUS_KEY)) && !log.contains(r#""state":"open""#),
        "canonical key on disk: {log}"
    );

    // The canonical spelling is not directly writable while renamed...
    let out = run(
        ta_bin(),
        &dir,
        &["update", "t1", &format!("{STATUS_KEY}=x")],
    );
    assert!(!out.status.success(), "canonical spelling rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("state"),
        "error points at the display name"
    );
    // ...and the single-valued append rejection follows the display name.
    let out = run(ta_bin(), &dir, &["update", "t1", "state+=more"]);
    assert!(!out.status.success(), "+= on the renamed status rejected");

    // Renaming AGAIN is free - the data was canonical all along.
    ta(&dir, &["config", "set", "workflow.status_field", "phase"]);
    assert!(
        ta(&dir, &["show", "t1", "--format", "json"]).contains(r#""phase":"open""#),
        "second rename needs no migration"
    );
}

#[test]
fn task_type_schemas_validate_and_the_discriminator_maps_canonically() {
    let dir = fresh_dir("task-types");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // Declare a sound schema (no enforcement yet - config layer only).
    let cfg_path = dir.join(".taska/config.toml");
    let mut cfg = fs::read_to_string(&cfg_path).unwrap();
    cfg.push_str(
        "\n[task_types.bug]\nclosed = true\n[task_types.bug.fields]\npoints = \"uint\"\n\
         tags = \"array<string>\"\n[task_types.bug.fields.severity]\ntype = \"enum\"\n\
         values = [\"low\", \"high\"]\nrequired = true\n",
    );
    fs::write(&cfg_path, &cfg).unwrap();
    ta(&dir, &["config", "validate"]); // asserts success

    // The discriminator rides the canonical-storage mechanism: type=bug on the
    // keyboard and in output, task_type on disk.
    ta(&dir, &["create", "t1", "type=bug", "severity=low"]);
    let shown = ta(&dir, &["show", "t1", "--format", "json"]);
    assert!(
        shown.contains(r#""type":"bug""#) && !shown.contains(TASK_TYPE_KEY),
        "display name in output: {shown}"
    );
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(&format!("\"{}\":\"bug\"", TASK_TYPE_KEY)) && !log.contains(r#""type":"bug""#),
        "canonical key on disk: {log}"
    );
    // Canonical spelling not directly writable; += rejected (single-valued).
    assert!(!run(
        ta_bin(),
        &dir,
        &["update", "t1", &format!("{TASK_TYPE_KEY}=feature")]
    )
    .status
    .success());
    assert!(!run(ta_bin(), &dir, &["update", "t1", "type+=x"])
        .status
        .success());

    // A hand-edited broken declaration blocks store commands with the problem
    // named, while `config validate` (which bypasses the gate) reports it too.
    cfg.push_str("[task_types.bad.fields.sev]\ntype = \"enum\"\n");
    fs::write(&cfg_path, &cfg).unwrap();
    let blocked = run(ta_bin(), &dir, &["list"]);
    assert!(!blocked.status.success(), "bad schema blocks the store");
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("values"),
        "problem named: {}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    let validate = run(ta_bin(), &dir, &["config", "validate"]);
    assert!(!validate.status.success(), "validate reports it");
}

/// Replace the store's `[task_types]` declaration with `block` and return what
/// `ta config validate` says about it.
fn validate_with(dir: &Path, block: &str) -> String {
    let cfg_path = dir.join(".taska/config.toml");
    let base = fs::read_to_string(&cfg_path).unwrap();
    fs::write(&cfg_path, format!("{base}\n{block}\n")).unwrap();
    let out = run(ta_bin(), dir, &["config", "validate"]);
    fs::write(&cfg_path, base).unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn transitions_must_be_total_reachable_and_speak_the_enum() {
    let dir = fresh_dir("config-transitions");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // The reference workflow: linear with an implement <-> review cycle, and a
    // reopen edge out of done. Accepted.
    let good = "[task_types.wf]\nfields = { status = { type = \"enum\", \
                values = [\"todo\", \"implement\", \"review\", \"closed\"], \
                transitions = { todo = [\"implement\"], implement = [\"review\"], \
                review = [\"implement\", \"closed\"], closed = [\"todo\"] } } }";
    assert!(
        validate_with(&dir, good).contains("Config OK"),
        "a cyclic workflow is a legal shape"
    );

    // A state left out would be terminal by ACCIDENT, so omission is an error
    // and the fix is spelled out.
    let missing = validate_with(
        &dir,
        "[task_types.wf]\nfields = { status = { type = \"enum\", \
         values = [\"todo\", \"closed\"], transitions = { todo = [\"closed\"] } } }",
    );
    assert!(
        missing.contains("no entry for state(s) `closed`") && missing.contains("`closed = []`"),
        "totality is enforced and the remedy named: {missing}"
    );

    // Every state must be able to finish: a subgraph that loops forever without
    // reaching done_status declares tasks that can never be completed.
    let stranded = validate_with(
        &dir,
        "[task_types.wf]\nfields = { status = { type = \"enum\", \
         values = [\"todo\", \"implement\", \"review\", \"closed\"], \
         transitions = { todo = [\"implement\"], implement = [\"review\"], \
         review = [\"implement\"], closed = [] } } }",
    );
    assert!(
        stranded.contains("cannot reach `closed`")
            && stranded.contains("`todo`")
            && stranded.contains("`review`"),
        "the stranded cycle is reported in full: {stranded}"
    );

    // A typo can only ever be a config error, never a silently weaker workflow.
    let typo = validate_with(
        &dir,
        "[task_types.wf]\nfields = { status = { type = \"enum\", \
         values = [\"todo\", \"closed\"], \
         transitions = { todo = [\"clsoed\"], closed = [] } } }",
    );
    assert!(
        typo.contains("target `clsoed` is not one of the declared values"),
        "unknown target rejected: {typo}"
    );

    // Reachability is a STATUS notion; it needs done_status to be declarable.
    let unreachable_done = validate_with(
        &dir,
        "[task_types.wf]\nfields = { status = { type = \"enum\", \
         values = [\"todo\", \"shipped\"], \
         transitions = { todo = [\"shipped\"], shipped = [] } } }",
    );
    assert!(
        unreachable_done.contains("done_status` is `closed`, which is not one of the declared"),
        "a done_status outside the enum is one clear problem, not per-state spam: \
         {unreachable_done}"
    );

    // A state machine needs a single current state to move out of.
    let non_enum = validate_with(
        &dir,
        "[task_types.wf]\nfields = { title = { type = \"string\", \
         transitions = { a = [\"b\"] } } }",
    );
    assert!(
        non_enum.contains("`transitions` only applies to the scalar enum kind"),
        "non-enum rejected: {non_enum}"
    );
}
