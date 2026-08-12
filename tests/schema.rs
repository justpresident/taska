mod common;
use common::*;
use taska::model::OP_KEY;

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

    // Create without a type: rejected, naming the display field and options. A
    // schema-conformance rejection is exit code 2 (distinct from a general error).
    let out = run(ta_bin(), &dir, &["create", "t1"]);
    assert_eq!(out.status.code(), Some(2), "schema violation exits 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing the `type` field") && stderr.contains("bug, feature"),
        "actionable: {stderr}"
    );

    // EVERY violation in ONE error - fixable in a single follow-up.
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

    // feature is OPEN: undeclared fields (and += onto them) are fine for the
    // schema - but a never-before-seen name still needs --new-field (typo guard).
    ta(&dir, &["update", "t1", "--new-field", "notes+=first"]);

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
        &[
            "create",
            "c2",
            "--new-field",
            "type=feature",
            "owner=ann",
            "weight=2.5",
        ],
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
    ta(&dir, &["update", "n2", "--new-field", "log+=first"]);
    ta(&dir, &["update", "n2", "log+=second"]);
    assert!(
        ta(&dir, &["show", "n2", "--format", "json"]).contains(r#""log":"first\nsecond""#),
        "text accumulation unchanged"
    );

    // The new ops are on disk under their own names.
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(&format!(r#""{OP_KEY}":"Add""#))
            && log.contains(&format!(r#""{OP_KEY}":"Remove""#)),
        "Add/Remove events logged: {log}"
    );
}

#[test]
fn repeated_compound_assign_in_one_command_accumulates() {
    // Regression for `repeated-compound-assign-drops-values`: two `field+=`/
    // `field-=` tokens for ONE field in ONE command used to keep only the last
    // (the operands were a map slot). Now they accumulate, dispatched by kind.
    let dir = fresh_dir("repeated-accumulate");
    init_repo(&dir);
    ta(&dir, &["init"]);
    declare_schema(&dir);
    ta(
        &dir,
        &["create", "t", "type=bug", "severity=low", "points=10"],
    );

    // Set: both elements inserted, not just `b`.
    ta(&dir, &["update", "t", "tags+=a", "tags+=b"]);
    assert!(
        ta(&dir, &["show", "t", "--format", "json"]).contains(r#""tags":["a","b"]"#),
        "both set members inserted in one command"
    );

    // Numeric: operands sum (10 + 2 + 3 = 15).
    ta(&dir, &["update", "t", "points+=2", "points+=3"]);
    assert!(ta(&dir, &["show", "t", "--format", "json"]).contains(r#""points":15"#));

    // Set remove: both removed in one command.
    ta(&dir, &["update", "t", "tags-=a", "tags-=b"]);
    assert!(ta(&dir, &["show", "t", "--format", "json"]).contains(r#""tags":[]"#));

    // Text (undeclared field on an OPEN type): operands join with `\n`, in order.
    ta(&dir, &["create", "f", "type=feature", "owner=z"]);
    ta(
        &dir,
        &["update", "f", "--new-field", "log+=one", "log+=two"],
    );
    assert!(
        ta(&dir, &["show", "f", "--format", "json"]).contains(r#""log":"one\ntwo""#),
        "text operands join in token order"
    );

    // CREATE accumulates repeated `+=` the same way: a new field starts absent,
    // so the combined operands are its initial value.
    ta(
        &dir,
        &[
            "create",
            "c",
            "type=bug",
            "severity=low",
            "tags+=x",
            "tags+=y",
            "points+=2",
            "points+=3",
        ],
    );
    let created = ta(&dir, &["show", "c", "--format", "json"]);
    assert!(
        created.contains(r#""tags":["x","y"]"#) && created.contains(r#""points":5"#),
        "create accumulates repeated += (not last-wins): {created}"
    );
}

#[test]
fn nonconforming_tasks_are_read_tolerated_with_one_warning() {
    let dir = fresh_dir("schema-tolerance");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Grandfathered data: created before the schema existed.
    ta(&dir, &["create", "legacy", "priority=1"]);
    ta(&dir, &["create", "old2", "priority=2"]);
    declare_schema(&dir);

    // Reads SUCCEED - tolerance is the law - with exactly ONE warning naming
    // the count, an example, and the detail surface.
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(out.status.success(), "reads never fail on nonconformance");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("do not conform").count(),
        1,
        "one warning, not per task: {stderr}"
    );
    assert!(
        stderr.contains("2 task(s)") && stderr.contains("config validate"),
        "count + pointer: {stderr}"
    );
    // The data itself is fully readable.
    assert!(ta(&dir, &["show", "legacy", "--format", "json"]).contains(r#""priority":1"#));

    // `config validate` stays exit-0 (grandfather: a schema declared over an
    // existing store must not lock config commands) and lists the details.
    let v = run(ta_bin(), &dir, &["config", "validate"]);
    assert!(v.status.success(), "conformance is a report, not an error");
    let v_err = String::from_utf8_lossy(&v.stderr);
    assert!(
        v_err.contains("task `legacy`") && v_err.contains("task `old2`"),
        "per-task detail: {v_err}"
    );
    assert!(
        String::from_utf8_lossy(&v.stdout).contains("2 not conforming"),
        "summary counts them"
    );

    // The silence switch.
    ta(
        &dir,
        &["config", "set", "workflow.warn_nonconforming", "false"],
    );
    let quiet = run(ta_bin(), &dir, &["list"]);
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("do not conform"),
        "silenced"
    );

    // A conforming store never warns (switch back on first).
    ta(
        &dir,
        &["config", "set", "workflow.warn_nonconforming", "true"],
    );
    ta(
        &dir,
        &["update", "legacy", "type=feature", "owner=a", "priority=1"],
    );
    ta(
        &dir,
        &["update", "old2", "type=feature", "owner=b", "priority=2"],
    );
    let clean = run(ta_bin(), &dir, &["list"]);
    assert!(
        !String::from_utf8_lossy(&clean.stderr).contains("do not conform"),
        "conforming store is quiet"
    );
}

/// The `workflow.untyped_tasks` migration ladder: allow (sanctioned, silent),
/// warn (tolerated, reported), deny (a type is mandatory - the default).
#[test]
fn untyped_tasks_policy_walks_allow_warn_deny() {
    let dir = fresh_dir("schema-untyped-ladder");
    init_repo(&dir);
    ta(&dir, &["init"]);
    declare_schema(&dir);

    // deny (default): untyped creations are rejected.
    assert!(
        !run(ta_bin(), &dir, &["create", "naked"]).status.success(),
        "deny makes the type mandatory"
    );

    // allow: untyped tasks are fully sanctioned - created, written to, never
    // reported anywhere.
    ta(&dir, &["config", "set", "workflow.untyped_tasks", "allow"]);
    ta(&dir, &["create", "free1", "priority=1"]);
    ta(&dir, &["update", "free1", "priority=2"]);
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("do not conform"),
        "allow: silent"
    );
    assert!(
        !ta(&dir, &["config", "validate"]).contains("not conforming"),
        "allow: not even in the validate report"
    );

    // warn: still tolerated (creations and writes work), but reported.
    ta(&dir, &["config", "set", "workflow.untyped_tasks", "warn"]);
    ta(&dir, &["create", "free2", "--new-field", "note=x"]);
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("do not conform"),
        "warn: reported"
    );

    // deny again: writes to the untyped tasks are blocked until typed.
    ta(&dir, &["config", "set", "workflow.untyped_tasks", "deny"]);
    assert!(
        !run(ta_bin(), &dir, &["update", "free1", "priority=3"])
            .status
            .success(),
        "deny blocks writes to untyped tasks"
    );
}

/// Constraints (min/max, pattern, lengths, item counts) and the `default`
/// life-cycle: stamped at create, substituted at read for grandfathered tasks,
/// healed onto any write, stamped in bulk by `repair --schema`.
#[test]
fn constraints_and_defaults_full_circle() {
    let dir = fresh_dir("schema-constraints");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Pre-schema tasks: typed (the field maps, no validation yet) but missing
    // the soon-to-be-required `title`.
    ta(&dir, &["create", "legacy", "type=card", "points=5"]);
    ta(&dir, &["create", "legacy2", "type=card", "points=7"]);
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str(
        "\n[task_types.card.fields.points]\ntype = \"uint\"\nmin = 1\nmax = 10\ndefault = 1\n\
         [task_types.card.fields.title]\ntype = \"string\"\nrequired = true\n\
         default = \"Task\"\npattern = \"^[A-Z]\"\nmin_len = 3\nmax_len = 8\n\
         [task_types.card.fields.tags]\ntype = \"set<string>\"\nmax_items = 2\n",
    );
    fs::write(&cfg, text).unwrap();

    // Create stamps the defaults: no need to spell out defaulted fields.
    ta(&dir, &["create", "c1", "type=card"]);
    let shown = ta(&dir, &["show", "c1", "--format", "json"]);
    assert!(
        shown.contains(r#""points":1"#) && shown.contains(r#""title":"Task""#),
        "defaults stamped at create: {shown}"
    );

    // Constraint violations reject with precise messages.
    let out = run(ta_bin(), &dir, &["create", "c2", "type=card", "points=11"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("exceeds max 10"),
        "max violation named"
    );
    let out = run(ta_bin(), &dir, &["create", "c2", "type=card", "title=ab"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        stderr.contains("pattern") && stderr.contains("min_len"),
        "BOTH constraint violations in one error: {stderr}"
    );
    assert!(
        !run(
            ta_bin(),
            &dir,
            &["create", "c2", "type=card", r#"tags=["a","b","c"]"#]
        )
        .status
        .success(),
        "max_items enforced"
    );
    assert!(
        !run(ta_bin(), &dir, &["update", "c1", "points=0"])
            .status
            .success(),
        "min enforced on update"
    );

    // Grandfathered tasks: storage lacks `title`, so the warning fires - but
    // reads SUBSTITUTE the default (display-only).
    assert!(
        ta(&dir, &["show", "legacy", "--format", "json"]).contains(r#""title":"Task""#),
        "read-side default substitution"
    );
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("2 task(s) do not conform"),
        "substitution does not hide the stored truth"
    );

    // Heal-on-write: ANY write stamps the missing defaults alongside.
    ta(&dir, &["update", "legacy", "points=6"]);
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""title":"Task""#),
        "default healed into the update event: {log}"
    );
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("1 task(s) do not conform"),
        "legacy now conforms in storage"
    );

    // repair --schema stamps required defaults onto the rest, no write needed.
    let out = ta(&dir, &["repair", "--schema"]);
    assert!(
        out.contains("stamped default"),
        "repair stamps required defaults: {out}"
    );
    let quiet = run(ta_bin(), &dir, &["list"]);
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("do not conform"),
        "store fully conforms"
    );
}

/// Declare `wf` with the cyclic review workflow: todo -> plan -> implement,
/// implement <-> review, review -> closed, closed reopens to todo.
fn declare_workflow(dir: &Path) {
    let cfg_path = dir.join(".taska/config.toml");
    let mut cfg = fs::read_to_string(&cfg_path).unwrap();
    cfg.push_str(
        "\n[task_types.wf]\nfields = {\n  \
         status = { type = \"enum\", \
         values = [\"todo\", \"plan\", \"implement\", \"review\", \"closed\"], \
         transitions = { todo = [\"plan\"], plan = [\"implement\", \"review\"], \
         implement = [\"review\"], review = [\"implement\", \"closed\"], \
         closed = [\"todo\"] } },\n}\n",
    );
    fs::write(&cfg_path, cfg).unwrap();
}

/// The status of `id`, read back through `ta show`.
fn status_of(dir: &Path, id: &str) -> String {
    let json = ta(
        dir,
        &["show", id, "--columns", "status", "--format", "json"],
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    parsed[0]["status"].as_str().unwrap().to_string()
}

#[test]
fn write_gate_walks_a_declared_status_workflow_including_its_cycle() {
    let dir = fresh_dir("transitions-walk");
    init_repo(&dir);
    ta(&dir, &["init"]);
    declare_workflow(&dir);

    // `default_status` seeds the entry state - create doesn't declare one.
    ta(&dir, &["create", "t", "type=wf"]);
    assert_eq!(status_of(&dir, "t"), "todo");

    // Skipping ahead is rejected, with the legal moves named. Exit 2 (schema),
    // not 1, so an agent can branch on the KIND of failure.
    let out = run(ta_bin(), &dir, &["update", "t", "status=closed"]);
    assert_eq!(out.status.code(), Some(2), "illegal transition exits 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("can't go `todo` -> `closed`") && stderr.contains("allowed from `todo`"),
        "names the move and the way out: {stderr}"
    );
    assert_eq!(status_of(&dir, "t"), "todo", "rejected write left no trace");

    // The declared path, including bouncing implement <-> review twice - the
    // whole point of a cyclic workflow.
    for next in [
        "plan",
        "implement",
        "review",
        "implement",
        "review",
        "closed",
    ] {
        ta(&dir, &["update", "t", &format!("status={next}")]);
        assert_eq!(status_of(&dir, "t"), next);
    }

    // `closed = ["todo"]` - reopening is declared, so it is allowed, but only
    // to the one declared target.
    let out = run(ta_bin(), &dir, &["update", "t", "status=review"]);
    assert_eq!(out.status.code(), Some(2), "closed -> review undeclared");
    ta(&dir, &["update", "t", "status=todo"]);
    assert_eq!(status_of(&dir, "t"), "todo", "reopen honoured");
}

#[test]
fn transitions_gate_only_actual_changes_of_a_declared_field() {
    let dir = fresh_dir("transitions-scope");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Grandfathered: a task carrying a status the workflow never mentions.
    ta(&dir, &["create", "old", "type=wf", "status=archived"]);
    declare_workflow(&dir);

    // Create is not a transition - there is no prior value to move out of, so
    // any declared state is a legal starting point.
    ta(&dir, &["create", "mid", "type=wf", "status=review"]);
    assert_eq!(status_of(&dir, "mid"), "review");

    // An unknown prior value is unconstrained, or grandfathered data could
    // never be repaired onto the workflow.
    ta(&dir, &["update", "old", "status=plan"]);
    assert_eq!(status_of(&dir, "old"), "plan");
    // ...and from there the workflow applies as usual.
    let out = run(ta_bin(), &dir, &["update", "old", "status=todo"]);
    assert_eq!(out.status.code(), Some(2), "back under the workflow");

    // A write that doesn't touch the status is untouched by the gate, even
    // though `review` -> `review` would be no move at all.
    ta(&dir, &["update", "mid", "note=hello", "--new-field"]);
    assert_eq!(status_of(&dir, "mid"), "review");

    // A workflow hangs off the task TYPE, so on the lax rungs of the migration
    // ladder an untyped task has none to enforce.
    ta(&dir, &["config", "set", "workflow.untyped_tasks", "allow"]);
    ta(&dir, &["create", "free", "status=review"]);
    ta(&dir, &["update", "free", "status=todo"]);
    assert_eq!(status_of(&dir, "free"), "todo");
}

#[test]
fn transitions_apply_to_any_enum_field_not_just_status() {
    let dir = fresh_dir("transitions-any-field");
    init_repo(&dir);
    ta(&dir, &["init"]);
    let cfg_path = dir.join(".taska/config.toml");
    let mut cfg = fs::read_to_string(&cfg_path).unwrap();
    // `stage` is a plain enum: it gets a state machine, but NOT the status
    // field's reachability rule - `final` is terminal and never reaches
    // done_status, which is fine for a non-status field.
    cfg.push_str(
        "\n[task_types.doc]\nfields = {\n  \
         stage = { type = \"enum\", values = [\"draft\", \"edit\", \"final\"], \
         transitions = { draft = [\"edit\"], edit = [\"draft\", \"final\"], final = [] } },\n}\n",
    );
    fs::write(&cfg_path, cfg).unwrap();
    ta(&dir, &["config", "validate"]);

    ta(&dir, &["create", "d", "type=doc", "stage=draft"]);
    let out = run(ta_bin(), &dir, &["update", "d", "stage=final"]);
    assert_eq!(out.status.code(), Some(2), "draft -> final is not declared");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("`stage` can't go `draft` -> `final`"),
        "names the offending field, not just the status"
    );
    ta(&dir, &["update", "d", "stage=edit"]);
    ta(&dir, &["update", "d", "stage=final"]);

    // `final = []` is terminal: nothing leaves it.
    let out = run(ta_bin(), &dir, &["update", "d", "stage=edit"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("`final` is terminal"),
        "terminal states say so"
    );
}
