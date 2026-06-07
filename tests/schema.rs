mod common;
use common::*;

/// Append a `[task_types]` declaration to the store's config.
fn declare_schema(dir: &Path) {
    let cfg_path = dir.join(".taska/config.toml");
    let mut cfg = fs::read_to_string(&cfg_path).unwrap();
    cfg.push_str(
        "\n[task_types.bug]\nclosed = true\n[task_types.bug.fields]\npoints = \"uint\"\n\
         tags = \"set<string>\"\nversion = \"string\"\n[task_types.bug.fields.severity]\n\
         type = \"enum\"\nvalues = [\"low\", \"high\"]\nrequired = true\n\
         [task_types.feature.fields.owner]\ntype = \"string\"\nrequired = true\n",
    );
    fs::write(&cfg_path, cfg).unwrap();
}

#[test]
fn write_gate_enforces_whole_task_schemas() {
    let dir = fresh_dir("schema-gate");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // A task created BEFORE schemas existed (the grandfathered case).
    ta(&dir, &["create", "legacy", "priority=1"]);
    declare_schema(&dir);

    // Create without a type: rejected, naming the display field and options.
    let out = run(ta_bin(), &dir, &["create", "t1"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing the `type` field") && stderr.contains("bug, feature"),
        "actionable: {stderr}"
    );

    // EVERY violation in ONE error — fixable in a single follow-up.
    let out = run(
        ta_bin(),
        &dir,
        &["create", "t1", "type=bug", "points=abc", "extra=1"],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    for needle in ["severity", "expected uint", "undeclared field `extra`"] {
        assert!(stderr.contains(needle), "`{needle}` in: {stderr}");
    }

    // A conforming create passes; reads show the display name.
    ta(
        &dir,
        &["create", "t1", "type=bug", "severity=low", "points=3"],
    );
    assert!(ta(&dir, &["show", "t1", "--format", "json"]).contains(r#""type":"bug""#));

    // Kind checks on update: wrong kind, enum outside values, set duplicates.
    assert!(!run(ta_bin(), &dir, &["update", "t1", "points=nope"])
        .status
        .success());
    assert!(!run(ta_bin(), &dir, &["update", "t1", "severity=urgent"])
        .status
        .success());
    // CLI input canonicalizes: a set dedups and sorts on write (the gate's
    // uniqueness check still guards non-CLI writers).
    ta(&dir, &["update", "t1", r#"tags=["b","a","b"]"#]);
    assert!(
        ta(&dir, &["show", "t1", "--format", "json"]).contains(r#""tags":["a","b"]"#),
        "set stored in canonical form"
    );

    // Unsetting a required field is rejected (null-unset convention).
    assert!(!run(ta_bin(), &dir, &["update", "t1", "severity=null"])
        .status
        .success());

    // Retype revalidates against the NEW type; one update fixes it all.
    let out = run(ta_bin(), &dir, &["update", "t1", "type=feature"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("missing required field `owner`"),
        "retype names what the new type needs"
    );
    ta(&dir, &["update", "t1", "type=feature", "owner=bob"]);

    // feature is OPEN: undeclared fields (and += onto them) are fine.
    ta(&dir, &["update", "t1", "notes+=first"]);

    // The grandfathered task: any field write must bring it into conformance.
    let out = run(ta_bin(), &dir, &["update", "legacy", "priority=2"]);
    assert!(!out.status.success(), "whole-task gate on old tasks");
    ta(
        &dir,
        &[
            "update",
            "legacy",
            "type=feature",
            "owner=ann",
            "priority=2",
        ],
    );

    // Edges are not schema fields: linking nonconforming tasks stays possible.
    ta(&dir, &["create", "t2", "type=feature", "owner=cy"]);
    ta(&dir, &["dep", "add", "t2", "depends_on=t1"]);
}

#[test]
fn schema_coercion_shapes_declared_values_on_the_real_binary() {
    let dir = fresh_dir("schema-coerce");
    init_repo(&dir);
    ta(&dir, &["init"]);
    declare_schema(&dir);

    // version is a declared string: "3.10" survives verbatim (the JSON guess
    // would store the number 3.1); points parses the quoted numeric string;
    // tags lifts a bare scalar to a singleton set.
    ta(
        &dir,
        &[
            "create",
            "c1",
            "type=bug",
            "severity=low",
            "version=3.10",
            "points=7",
            "tags=urgent",
        ],
    );
    let shown = ta(&dir, &["show", "c1", "--format", "json"]);
    assert!(shown.contains(r#""version":"3.10""#), "verbatim: {shown}");
    assert!(shown.contains(r#""points":7"#), "number: {shown}");
    assert!(shown.contains(r#""tags":["urgent"]"#), "singleton: {shown}");

    // The canonical set form reaches DISK (what merges converge on).
    ta(&dir, &["update", "c1", r#"tags=["z","a","z"]"#]);
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""tags":["a","z"]"#),
        "sorted+deduped on disk: {log}"
    );

    // An undeclared field on an OPEN type keeps the JSON-or-string guess.
    ta(
        &dir,
        &["create", "c2", "type=feature", "owner=ann", "weight=2.5"],
    );
    assert!(ta(&dir, &["show", "c2", "--format", "json"]).contains(r#""weight":2.5"#));
}

#[test]
fn accumulate_operators_dispatch_by_declared_kind() {
    let dir = fresh_dir("schema-accumulate");
    init_repo(&dir);
    ta(&dir, &["init"]);
    declare_schema(&dir);
    ta(
        &dir,
        &[
            "create",
            "n1",
            "type=bug",
            "severity=low",
            "points=3",
            r#"tags=["b"]"#,
        ],
    );

    // Numeric += / -= on a declared uint.
    ta(&dir, &["update", "n1", "points+=2"]);
    ta(&dir, &["update", "n1", "points-=1"]);
    assert!(ta(&dir, &["show", "n1", "--format", "json"]).contains(r#""points":4"#));

    // Set inserts/removes; re-adding a present element is a no-op write.
    ta(&dir, &["update", "n1", "tags+=a"]);
    assert!(ta(&dir, &["show", "n1", "--format", "json"]).contains(r#""tags":["a","b"]"#));
    assert!(
        ta(&dir, &["update", "n1", "tags+=a"]).contains("already up to date"),
        "present element insert is a no-op"
    );
    ta(&dir, &["update", "n1", "tags-=b"]);
    assert!(ta(&dir, &["show", "n1", "--format", "json"]).contains(r#""tags":["a"]"#));

    // Adding 0 writes nothing; a uint underflow is rejected with the result.
    assert!(ta(&dir, &["update", "n1", "points+=0"]).contains("already up to date"));
    let out = run(ta_bin(), &dir, &["update", "n1", "points-=10"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("expected uint"),
        "underflow rejected by the result check"
    );

    // `-=` needs a declared numeric/set field; `+=` rejects on enums.
    assert!(!run(ta_bin(), &dir, &["update", "n1", "free-=1"])
        .status
        .success());
    assert!(!run(ta_bin(), &dir, &["update", "n1", "severity+=high"])
        .status
        .success());

    // Strings (and undeclared fields on open types) keep the text append.
    ta(&dir, &["create", "n2", "type=feature", "owner=z"]);
    ta(&dir, &["update", "n2", "log+=first"]);
    ta(&dir, &["update", "n2", "log+=second"]);
    assert!(
        ta(&dir, &["show", "n2", "--format", "json"]).contains(r#""log":"first\nsecond""#),
        "text accumulation unchanged"
    );

    // The new ops are on disk under their own names.
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""op":"Add""#) && log.contains(r#""op":"Remove""#),
        "Add/Remove events logged: {log}"
    );
}
