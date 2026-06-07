mod common;
use common::*;

/// Append a `[task_types]` declaration to the store's config.
fn declare_schema(dir: &Path) {
    let cfg_path = dir.join(".taska/config.toml");
    let mut cfg = fs::read_to_string(&cfg_path).unwrap();
    cfg.push_str(
        "\n[task_types.bug]\nclosed = true\n[task_types.bug.fields]\npoints = \"uint\"\n\
         tags = \"set<string>\"\n[task_types.bug.fields.severity]\ntype = \"enum\"\n\
         values = [\"low\", \"high\"]\nrequired = true\n\
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
    assert!(
        !run(ta_bin(), &dir, &["update", "t1", r#"tags=["a","a"]"#])
            .status
            .success(),
        "set<string> rejects duplicates"
    );
    ta(&dir, &["update", "t1", r#"tags=["a","b"]"#]);

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
