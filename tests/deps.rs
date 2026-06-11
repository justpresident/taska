mod common;
use common::*;

#[test]
fn dep_remove_makes_a_blocked_task_ready() {
    let dir = fresh_dir("dep-remove-ready");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // `api` depends on `db`, and `db` is still open, so `api` is blocked: only
    // `db` itself is ready.
    ta(&dir, &["create", "db", "status=open"]);
    ta(&dir, &["create", "api", "status=open"]);
    ta(&dir, &["dep", "add", "api", "depends_on=db"]);
    let before = ta(&dir, &["list", "--ready"]);
    assert!(lists_task(&before, "db"), "db ready: {before}");
    assert!(
        !lists_task(&before, "api"),
        "api blocked by open db: {before}"
    );

    // Removing the dependency lifts the block, so `api` becomes ready too.
    let msg = ta(&dir, &["dep", "remove", "api", "depends_on=db"]);
    assert!(
        msg.contains("Removed 1 edge(s)"),
        "dep remove should confirm: {msg}"
    );
    let after = ta(&dir, &["list", "--ready"]);
    assert!(
        lists_task(&after, "api"),
        "api ready after unblock: {after}"
    );

    // The dependency is gone from the materialized task, not just from `ready`.
    let json = ta(&dir, &["show", "api", "--format", "json"]);
    assert!(json.contains(r#""deps":{}"#), "dep removed: {json}");
}

#[test]
fn dependency_cycle_is_reported_by_ready() {
    let dir = fresh_dir("cycle");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // a -> b and b -> a form a cycle. `ready` runs the topological sort, so it
    // must refuse and name the cycle (it can't order a circular graph).
    ta(&dir, &["create", "a", "status=open"]);
    ta(&dir, &["create", "b", "status=open"]);
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    ta(&dir, &["dep", "add", "b", "depends_on=a"]);

    let out = run(ta_bin(), &dir, &["list", "--ready"]);
    assert!(
        !out.status.success(),
        "ready must exit non-zero on a dependency cycle, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("cycle"),
        "ready should report the cycle: {stderr}"
    );
}

#[test]
fn dep_command_adds_and_removes_typed_edges() {
    let dir = fresh_dir("dep-cmd");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["create", "c"]);

    // Both the depends_on and the typed relates_to edge land in the deps map.
    ta(&dir, &["dep", "add", "a", "depends_on=b", "relates_to=c"]);
    let json = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        json.contains(r#""deps":{"depends_on":["b"],"relates_to":["c"]}"#),
        "typed edges show in deps: {json}"
    );
    // The relates_to edge is recorded as a typed AddEdge event.
    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""rel":"relates_to""#) && log.contains(r#""target":"c""#),
        "typed relates_to edge in the log: {log}"
    );

    // Remove the depends_on edge; the info edge stays in the map.
    ta(&dir, &["dep", "remove", "a", "depends_on=b"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(r#""deps":{"relates_to":["c"]}"#),
        "depends_on edge removed, relates_to kept"
    );

    // An undeclared relationship type is rejected with a helpful error.
    let out = run(ta_bin(), &dir, &["dep", "add", "a", "bogus=b"]);
    assert!(!out.status.success(), "undeclared type must be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown relationship type"),
        "error names the problem: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn show_lists_forward_inverse_and_symmetric_relationships() {
    let dir = fresh_dir("show-rels-mirror");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    ta(&dir, &["create", "c"]);

    // `a depends_on b` (inverse `blocks`) and `a relates_to c` (self-inverse).
    ta(&dir, &["dep", "add", "a", "depends_on=b", "relates_to=c"]);

    // `a`'s forward edges live in the deps map, grouped by type.
    assert!(
        ta(&dir, &["show", "a", "--format", "json"])
            .contains(r#""deps":{"depends_on":["b"],"relates_to":["c"]}"#),
        "a forward edges in deps"
    );

    // `b` never named `a`, but the inverse of `depends_on` surfaces as `blocks`.
    assert!(
        ta(&dir, &["show", "b", "--format", "json"]).contains(r#""blocks":["a"]"#),
        "b inverse blocks"
    );

    // `relates_to` is self-inverse, so `c` shows the symmetric edge back to `a`.
    assert!(
        ta(&dir, &["show", "c", "--format", "json"]).contains(r#""relates_to":["a"]"#),
        "c symmetric relates_to"
    );
}

#[test]
fn dep_remove_by_inverse_name_drops_the_forward_edge() {
    let dir = fresh_dir("dep-remove-inverse");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    assert!(ta(&dir, &["show", "b", "--format", "json"]).contains(r#""blocks":["a"]"#));

    // Remove the relationship from b's side using the inverse name `blocks`.
    ta(&dir, &["dep", "remove", "b", "blocks=a"]);
    assert!(
        ta(&dir, &["show", "a", "--format", "json"]).contains(r#""deps":{}"#),
        "inverse removal dropped a's depends_on edge"
    );
    let b = ta(&dir, &["show", "b", "--format", "json"]);
    assert!(!b.contains("blocks"), "inverse edge gone from b: {b}");
}

#[test]
fn dep_tree_nests_dependencies_and_collapses_shared_nodes() {
    let dir = fresh_dir("dep-tree");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["a", "b", "c", "d", "e"] {
        ta(&dir, &["create", id]);
    }
    // a → {b, c}; both b and c → d (a shared/diamond node); d → e.
    ta(&dir, &["dep", "add", "a", "depends_on=b", "depends_on=c"]);
    ta(&dir, &["dep", "add", "b", "depends_on=d"]);
    ta(&dir, &["dep", "add", "c", "depends_on=d"]);
    ta(&dir, &["dep", "add", "d", "depends_on=e"]);

    let tree = ta(&dir, &["dep", "tree", "a"]);
    assert!(tree.contains("├─ b"), "first child branch: {tree}");
    assert!(tree.contains("└─ c"), "last child branch: {tree}");
    assert!(tree.contains("└─ e"), "e nested under d: {tree}");
    // d (with its e subtree) is reached again under c, but was already expanded
    // under b — the second occurrence collapses rather than reprinting.
    assert!(tree.contains('…'), "shared node collapsed: {tree}");
}

#[test]
fn dep_cycles_reports_circular_dependencies() {
    let dir = fresh_dir("dep-cycles");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);

    // No cycle yet.
    assert!(ta(&dir, &["dep", "cycles"]).contains("No dependency cycles"));

    // Close a → b → a into a cycle.
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    ta(&dir, &["dep", "add", "b", "depends_on=a"]);
    let cycles = ta(&dir, &["dep", "cycles"]);
    assert!(cycles.contains("a ↔ b"), "cycle members reported: {cycles}");

    // The tree marks the back-edge rather than looping forever.
    assert!(
        ta(&dir, &["dep", "tree", "a"]).contains("(cycle)"),
        "tree flags the cycle"
    );
}

#[test]
fn custom_blocker_relationship_gates_readiness() {
    let dir = fresh_dir("blocker-type");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Declare a second blocker-typed relationship beyond depends_on.
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[relationships.requires]\nkind = \"blocker\"\ninverse = \"required_by\"\n");
    fs::write(&cfg, text).unwrap();

    ta(&dir, &["create", "a", "status=open"]);
    ta(&dir, &["create", "b", "status=open"]);
    ta(&dir, &["dep", "add", "a", "requires=b"]);

    // `requires` is a blocker, so `a` is gated by still-open `b`: only `b` ready.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(lists_task(&ready, "b"), "b ready: {ready}");
    assert!(!lists_task(&ready, "a"), "a blocked by requires=b: {ready}");

    // The tree walks the typed blocker edge and labels it; the status column
    // (default display.columns) now shows next to each node.
    let tree = ta(&dir, &["dep", "tree", "a"]);
    assert!(
        tree.contains("[requires]"),
        "typed blocker labelled: {tree}"
    );
    assert!(tree.contains("open"), "status column shown in tree: {tree}");

    // A cycle through the custom blocker type is detected too.
    ta(&dir, &["dep", "add", "b", "requires=a"]);
    assert!(
        ta(&dir, &["dep", "cycles"]).contains("a ↔ b"),
        "custom-blocker cycle reported"
    );
    ta(&dir, &["dep", "remove", "b", "requires=a"]);

    // Close `b`, and `a` unblocks.
    ta(&dir, &["update", "b", "status=closed"]);
    assert!(
        lists_task(&ta(&dir, &["list", "--ready"]), "a"),
        "a ready after requires-dep done"
    );
}

#[test]
fn dep_tree_hides_the_configured_default_blocker_not_literal_depends_on() {
    let dir = fresh_dir("default-blocker-tag");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Add a blocker type whose name sorts BEFORE `depends_on`, so it (not
    // `depends_on`) becomes the default blocker — the first blocker type by name.
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[relationships.consumes]\nkind = \"blocker\"\ninverse = \"consumed_by\"\n");
    fs::write(&cfg, text).unwrap();

    for id in ["a", "b", "c"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(&dir, &["dep", "add", "a", "consumes=b"]); // via the (new) default blocker
    ta(&dir, &["dep", "add", "a", "depends_on=c"]); // via a now-non-default blocker

    let tree = ta(&dir, &["dep", "tree", "a"]);
    // The configured default blocker's tag is hidden (it's the implied relation),
    // and `depends_on` — no longer the default — is tagged like any other type.
    assert!(
        !tree.contains("[consumes]"),
        "the configured default blocker is not tagged: {tree}"
    );
    assert!(
        tree.contains("[depends_on]"),
        "depends_on is tagged once it's not the default: {tree}"
    );
}

#[test]
fn informational_relationship_does_not_gate_readiness() {
    let dir = fresh_dir("info-rel");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // `relates_to` is type=info in the default config.
    ta(&dir, &["create", "x", "status=open"]);
    ta(&dir, &["create", "y", "status=open"]);
    ta(&dir, &["dep", "add", "x", "relates_to=y"]);

    // An informational edge must not block: both are ready.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(
        lists_task(&ready, "x"),
        "x ready despite relates_to: {ready}"
    );
    assert!(lists_task(&ready, "y"), "y ready: {ready}");
}

#[test]
fn dep_plan_lists_remaining_prerequisites_in_order() {
    let dir = fresh_dir("dep-plan");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["build", "test", "ship"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    // ship depends_on test depends_on build.
    ta(&dir, &["dep", "add", "ship", "depends_on=test"]);
    ta(&dir, &["dep", "add", "test", "depends_on=build"]);

    let plan = ta(&dir, &["dep", "plan", "ship"]);
    let (pb, pt, ps) = (
        plan.find("build").unwrap(),
        plan.find("test").unwrap(),
        plan.find("ship").unwrap(),
    );
    assert!(
        pb < pt && pt < ps,
        "prerequisites before dependents: {plan}"
    );
    assert!(plan.contains("3 task(s) remaining"), "count: {plan}");

    // A done prerequisite drops out of the plan as satisfied.
    ta(&dir, &["update", "build", "status=closed"]);
    let plan = ta(&dir, &["dep", "plan", "ship"]);
    assert!(!plan.contains("build"), "done prereq dropped: {plan}");
    assert!(plan.contains("2 task(s) remaining"), "count: {plan}");

    // With everything done there's nothing left to do.
    ta(&dir, &["update", "test", "status=closed"]);
    ta(&dir, &["update", "ship", "status=closed"]);
    assert!(
        ta(&dir, &["dep", "plan", "ship"]).contains("Nothing to do"),
        "all done -> nothing to do"
    );

    // An unknown goal is an error.
    let out = run(ta_bin(), &dir, &["dep", "plan", "nope"]);
    assert!(!out.status.success(), "unknown goal must fail");
}

#[test]
fn dep_plan_critical_shows_the_longest_chain() {
    let dir = fresh_dir("dep-plan-critical");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["ship", "a1", "a2", "a3", "c1"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    // ship has a long branch (a3 -> a2 -> a1 -> ship) and a short one (c1 -> ship).
    ta(&dir, &["dep", "add", "ship", "depends_on=a1"]);
    ta(&dir, &["dep", "add", "a1", "depends_on=a2"]);
    ta(&dir, &["dep", "add", "a2", "depends_on=a3"]);
    ta(&dir, &["dep", "add", "ship", "depends_on=c1"]);

    // The full plan lists all five remaining tasks.
    let plan = ta(&dir, &["dep", "plan", "ship"]);
    assert!(
        plan.contains("c1"),
        "full plan includes the short branch: {plan}"
    );
    assert!(plan.contains("5 task(s) remaining"), "count: {plan}");

    // --critical narrows to the longest chain (a3,a2,a1,ship), dropping the short
    // branch, in dependency order.
    let crit = ta(&dir, &["dep", "plan", "ship", "--critical"]);
    assert!(!crit.contains("c1"), "short branch excluded: {crit}");
    let (p3, p2, p1, ps) = (
        crit.find("a3").unwrap(),
        crit.find("a2").unwrap(),
        crit.find("a1").unwrap(),
        crit.find("ship").unwrap(),
    );
    assert!(
        p3 < p2 && p2 < p1 && p1 < ps,
        "longest chain in order: {crit}"
    );
    assert!(
        crit.contains("critical path: 4 of 5"),
        "critical-path count: {crit}"
    );
}

#[test]
fn subtask_hierarchy_gates_readiness_and_mirrors_both_ways() {
    let dir = fresh_dir("subtask");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "build-form", "wire-auth"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    // Add from the parent side, and from the child side via the inverse — both
    // land as `has_subtask` edges on the parent.
    ta(&dir, &["dep", "add", "epic", "has_subtask=build-form"]);
    ta(&dir, &["dep", "add", "wire-auth", "subtask_of=epic"]);

    let log = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        log.contains(r#""rel":"has_subtask""#) && log.contains(r#""target":"wire-auth""#),
        "inverse add stored as has_subtask on epic: {log}"
    );

    // show surfaces both directions: parent -> has_subtask, child -> subtask_of.
    let e = ta(&dir, &["show", "epic", "--format", "json"]);
    assert!(
        e.contains(r#""has_subtask":["build-form","wire-auth"]"#),
        "epic shows its subtasks: {e}"
    );
    assert!(
        ta(&dir, &["show", "build-form", "--format", "json"]).contains(r#""subtask_of":["epic"]"#),
        "child mirrors the parent"
    );

    // Hierarchy gates like a blocker: the parent isn't ready until its subtasks are.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(
        lists_task(&ready, "build-form") && lists_task(&ready, "wire-auth"),
        "subtasks are ready: {ready}"
    );
    assert!(
        !lists_task(&ready, "epic"),
        "epic blocked by subtasks: {ready}"
    );

    // Close both subtasks -> the parent becomes ready.
    ta(&dir, &["update", "build-form", "status=closed"]);
    ta(&dir, &["update", "wire-auth", "status=closed"]);
    assert!(
        lists_task(&ta(&dir, &["list", "--ready"]), "epic"),
        "epic ready once its subtasks are done"
    );
}

#[test]
fn dep_tree_marks_subtasks_and_rolls_up_progress() {
    let dir = fresh_dir("subtask-tree");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "a", "b", "dep1"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(&dir, &["dep", "add", "epic", "has_subtask=a"]);
    ta(&dir, &["dep", "add", "epic", "has_subtask=b"]);
    ta(&dir, &["dep", "add", "epic", "depends_on=dep1"]);
    ta(&dir, &["update", "a", "status=closed"]); // 1 of 2 subtasks done

    let tree = ta(&dir, &["dep", "tree", "epic"]);
    assert!(
        tree.contains("[subtasks 1/2]"),
        "parent rolls up child completion: {tree}"
    );
    assert!(
        tree.matches("[subtask]").count() == 2,
        "both subtasks tagged: {tree}"
    );
    // A plain depends_on edge is a dependency, not a subtask — never tagged.
    assert!(
        tree.contains("dep1") && !tree.contains("dep1 [subtask]"),
        "plain dependency untagged: {tree}"
    );

    // The same structure is available as nested json.
    let json: serde_json::Value =
        serde_json::from_str(&ta(&dir, &["dep", "tree", "epic", "--format", "json"])).unwrap();
    let epic = &json[0];
    assert_eq!(epic["id"], "epic");
    assert_eq!(epic["subtasks"], serde_json::json!({"done": 1, "total": 2}));
    let kids = epic["children"].as_array().unwrap();
    assert!(
        kids.iter()
            .any(|c| c["id"] == "a" && c["edge"] == "subtask" && c["done"] == true),
        "subtask child in json: {json}"
    );
    assert!(
        kids.iter()
            .any(|c| c["id"] == "dep1" && c.get("edge").is_none()),
        "plain dependency has no edge tag: {json}"
    );
}

#[test]
fn dep_tree_exact_by_default_titles_done_marks_and_open_prune() {
    let dir = fresh_dir("tree-output");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "epic", "title=Epic goal", "status=open"]);
    ta(
        &dir,
        &["create", "open-sub", "title=Still open", "status=open"],
    );
    ta(
        &dir,
        &["create", "done-sub", "title=Finished", "status=open"],
    );
    ta(&dir, &["create", "done-mid", "status=open"]);
    ta(&dir, &["create", "deep-open", "status=open"]);
    ta(&dir, &["dep", "add", "epic", "has_subtask=open-sub"]);
    ta(&dir, &["dep", "add", "epic", "has_subtask=done-sub"]);
    ta(&dir, &["dep", "add", "epic", "depends_on=done-mid"]);
    ta(&dir, &["dep", "add", "done-mid", "depends_on=deep-open"]); // done node leads to open work
    ta(&dir, &["update", "done-sub", "status=closed"]);
    ta(&dir, &["update", "done-mid", "status=closed"]);

    // Default: the exact graph — titles shown, done tasks marked `✓`, and a done
    // mid-chain node is kept (never spliced), with its open descendant beneath it.
    let tree = ta(&dir, &["dep", "tree", "epic"]);
    assert!(tree.contains("Epic goal"), "title shown: {tree}");
    assert!(
        tree.contains("✓ done-sub"),
        "done subtask check-marked: {tree}"
    );
    assert!(
        tree.contains("✓ done-mid"),
        "done mid-chain kept + marked: {tree}"
    );
    assert!(
        tree.contains("deep-open"),
        "open descendant under done node: {tree}"
    );

    // --open: prune fully-resolved branches (the done-sub leaf), but keep the done
    // mid-chain node because it still leads to open work.
    let open = ta(&dir, &["dep", "tree", "epic", "--open"]);
    assert!(
        !open.contains("done-sub"),
        "fully-resolved leaf pruned: {open}"
    );
    assert!(
        open.contains("done-mid") && open.contains("deep-open"),
        "done->open kept: {open}"
    );
    assert!(open.contains("open-sub"), "open subtask kept: {open}");

    // --sort id orders siblings ascending; --reverse flips them.
    let asc = ta(&dir, &["dep", "tree", "epic", "--sort", "id"]);
    assert!(
        asc.find("done-mid").unwrap() < asc.find("open-sub").unwrap(),
        "ascending id: {asc}"
    );
    let desc = ta(&dir, &["dep", "tree", "epic", "--sort", "id", "--reverse"]);
    assert!(
        desc.find("open-sub").unwrap() < desc.find("done-mid").unwrap(),
        "reversed id: {desc}"
    );
}

#[test]
fn show_surfaces_typed_relationships_forward_and_inverse() {
    let dir = fresh_dir("show-rels");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["epic", "child", "other", "a", "b"] {
        ta(&dir, &["create", id, "status=open"]);
    }
    ta(&dir, &["dep", "add", "epic", "has_subtask=child"]);
    ta(&dir, &["dep", "add", "epic", "relates_to=other"]);
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);

    // The parent's record shows its typed relationships, grouped by type.
    let epic = ta(&dir, &["show", "epic"]);
    assert!(
        epic.contains("has_subtask:") && epic.contains("child"),
        "{epic}"
    );
    assert!(
        epic.contains("relates_to:") && epic.contains("other"),
        "{epic}"
    );

    // The child shows the inverse-mirrored edge (subtask_of), in json too.
    assert!(
        ta(&dir, &["show", "child", "--format", "json"]).contains(r#""subtask_of":["epic"]"#),
        "child mirrors subtask_of"
    );

    // depends_on lives in the deps map — never duplicated as a top-level
    // field; its inverse `blocks` surfaces on the depended-upon task.
    let aj = ta(&dir, &["show", "a", "--format", "json"]);
    assert!(
        aj.contains(r#""deps":{"depends_on":["b"]}"#) && aj.matches("depends_on").count() == 1,
        "depends_on only inside deps: {aj}"
    );
    assert!(
        ta(&dir, &["show", "b", "--format", "json"]).contains(r#""blocks":["a"]"#),
        "inverse blocks surfaced on b"
    );
}

#[test]
fn dep_add_enforces_single_blocker_and_single_parent() {
    let dir = fresh_dir("subtask-constraints");
    init_repo(&dir);
    ta(&dir, &["init"]);
    for id in ["a", "b", "e1", "e2", "c"] {
        ta(&dir, &["create", id]);
    }

    // At most one blocking relationship between two tasks.
    ta(&dir, &["dep", "add", "a", "depends_on=b"]);
    let out = run(ta_bin(), &dir, &["dep", "add", "a", "has_subtask=b"]);
    assert!(
        !out.status.success(),
        "second blocking edge must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("only one blocking relationship"),
        "error names the rule: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A task may have only one parent.
    ta(&dir, &["dep", "add", "e1", "has_subtask=c"]);
    let out = run(ta_bin(), &dir, &["dep", "add", "e2", "has_subtask=c"]);
    assert!(!out.status.success(), "second parent must be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("only one parent"),
        "error names the rule: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Same constraint when added from the child side via the inverse.
    let out = run(ta_bin(), &dir, &["dep", "add", "c", "subtask_of=e2"]);
    assert!(!out.status.success(), "inverse second-parent also rejected");

    // Re-adding the exact same edge is idempotent, not a conflict.
    assert!(
        run(ta_bin(), &dir, &["dep", "add", "e1", "has_subtask=c"])
            .status
            .success(),
        "idempotent re-add allowed"
    );
}
