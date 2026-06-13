mod common;
use common::names::*;
use common::*;
use taska::model::STATUS_KEY;

#[test]
fn git_merge_driver_resolves_divergent_appends() {
    let dir = fresh_dir("merge");
    init_renamed_open(&dir);
    ta(&dir, &["create", "base", &format!("{STATUS_FIELD}=open")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Branch off, then append a distinct task on each branch.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["create", "on-main"]);
    git(&dir, &["commit", "-aqm", "main task"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["create", "on-feature"]);
    git(&dir, &["commit", "-aqm", "feature task"]);

    // Both branches edited mutations.jsonl; the driver must union them cleanly.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "merge should auto-resolve, got:\n{}",
        String::from_utf8_lossy(&merge.stderr)
    );

    let list = ta(&dir, &["list"]);
    for id in ["base", "on-main", "on-feature"] {
        assert!(list.contains(id), "missing {id} after merge:\n{list}");
    }
}

#[test]
fn surface_conflict_fails_merge_and_resolve_clears_it() {
    let dir = fresh_dir("conflict");
    init_renamed_open(&dir);
    ta(&dir, &["create", "t", &format!("{STATUS_FIELD}=open")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Both branches set the SAME field of the SAME task to different values.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", &format!("{STATUS_FIELD}=main")]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", &format!("{STATUS_FIELD}=feature")]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    // Default policy is `surface`, so the driver must fail the merge.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        !merge.status.success(),
        "surface policy must fail the merge"
    );
    assert!(
        dir.join(".taska/merge-conflict.json").exists(),
        "a conflict marker should be written"
    );

    // `ta resolve` reports the conflict (per-field) and clears the marker.
    let resolved = ta(&dir, &["resolve"]);
    assert!(
        resolved.contains("conflict"),
        "resolve should report the conflict: {resolved}"
    );
    assert!(
        resolved.contains(STATUS_KEY),
        "resolve should name the conflicting field: {resolved}"
    );
    assert!(
        resolved.contains("kept ours"),
        "surface resolves tentatively as ours: {resolved}"
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "marker should be cleared"
    );

    // A second resolve is a clean no-op.
    let again = ta(&dir, &["resolve"]);
    assert!(again.contains("Nothing to resolve"), "got: {again}");
}

#[test]
fn theirs_policy_resolves_conflict_without_failing() {
    let dir = fresh_dir("theirs");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Opt into silent resolution: the branch merged IN wins conflicts.
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"theirs\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "theirs policy must resolve cleanly: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "auto resolution leaves no marker"
    );

    // Merging feature INTO main with `theirs` keeps feature's value.
    let list = ta(&dir, &["list", "--format", "json"]);
    assert!(
        list.contains("\"status\":\"feature\""),
        "theirs (feature) should win: {list}"
    );
}

#[test]
fn per_field_merge_keeps_disjoint_fields_and_resolves_overlap() {
    let dir = fresh_dir("perfield");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"theirs\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "X", "status=new"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // main and feature overlap on status+owner, but each adds a disjoint field.
    git(&dir, &["branch", "feature"]);
    ta(
        &dir,
        &[
            "update",
            "X",
            "status=closed",
            "owner=alice",
            "scope=project",
        ],
    );
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(
        &dir,
        &["update", "X", "status=open", "owner=bob", "priority=3"],
    );
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "should resolve: {}",
        String::from_utf8_lossy(&merge.stderr)
    );

    let list = ta(&dir, &["list", "--full", "--format", "json"]);
    // Overlapping fields go to theirs (feature); disjoint fields both survive.
    assert!(
        list.contains("\"status\":\"open\""),
        "status -> theirs: {list}"
    );
    assert!(
        list.contains("\"owner\":\"bob\""),
        "owner -> theirs: {list}"
    );
    assert!(
        list.contains("\"scope\":\"project\""),
        "ours-only scope survives: {list}"
    );
    assert!(
        list.contains("\"priority\":3"),
        "theirs-only priority survives: {list}"
    );
}

#[test]
fn clean_disjoint_field_merge_has_no_conflict() {
    let dir = fresh_dir("disjoint-fields");
    init_renamed_open(&dir); // default on_conflict = surface
    ta(&dir, &["create", "t", &format!("{STATUS_FIELD}=open")]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Each branch sets a DIFFERENT field of the same task: no overlap, so even the
    // strict `surface` policy must merge cleanly with no marker and no failure.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "owner=alice"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "priority=3"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "disjoint-field edits must merge cleanly under surface: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "no conflict marker for a clean merge"
    );

    // Both disjoint edits survive.
    let json = ta(&dir, &["show", "t", "--format", "json"]);
    assert!(
        json.contains(r#""owner":"alice""#),
        "ours field kept: {json}"
    );
    assert!(
        json.contains(r#""priority":3"#),
        "theirs field kept: {json}"
    );
}

#[test]
fn ours_policy_keeps_the_branch_merged_into() {
    let dir = fresh_dir("ours");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"ours\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    // Merge feature INTO main: `ours` keeps main's value, with no marker/failure.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "ours policy must resolve cleanly: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        !dir.join(".taska/merge-conflict.json").exists(),
        "auto resolution leaves no marker"
    );
    let json = ta(&dir, &["list", "--format", "json"]);
    assert!(
        json.contains(r#""status":"main""#),
        "ours (main) should win: {json}"
    );
}

#[test]
fn latest_policy_keeps_the_newest_write() {
    let dir = fresh_dir("latest");
    init_repo(&dir);
    ta(&dir, &["init"]);
    fs::write(
        dir.join(".taska/config.toml"),
        "[merge]\non_conflict = \"latest\"\n",
    )
    .unwrap();
    ta(&dir, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Write main's edit FIRST, then feature's: the feature write has the later
    // timestamp, so `latest` must keep it regardless of merge direction.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "latest policy must resolve cleanly: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    let json = ta(&dir, &["list", "--format", "json"]);
    assert!(
        json.contains(r#""status":"feature""#),
        "latest (the newer feature write) should win: {json}"
    );
}

#[test]
fn baseline_keep_ours_merges_after_both_branches_compact() {
    let dir = fresh_dir("baseline-merge");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Compaction needs more than keep_events to fold anything into baseline.jsonl,
    // which is what exercises the keep-ours baseline driver on merge.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();

    // 350 creates > keep_events (300): 50 fold into the baseline, 300 stay.
    for i in 0..350 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Both branches compact independently (folding the shared prefix), so both
    // baseline.jsonl AND mutations.jsonl diverge and must merge via their drivers.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["compact"]);
    git(&dir, &["commit", "-aqm", "main compact"]);

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["create", "extra"]);
    ta(&dir, &["compact"]);
    git(&dir, &["commit", "-aqm", "feature compact"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "compacted baselines must merge cleanly (keep-ours): {}",
        String::from_utf8_lossy(&merge.stderr)
    );

    // ours' baseline is kept verbatim (the 50 folded tasks), and the log driver
    // still reconciles the recent suffix, so every task - old, new, and feature's
    // late `extra` - remains visible after the merge.
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        50,
        "keep-ours retains our own baseline depth"
    );
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t349", "extra"] {
        assert!(lists_task(&list, id), "missing {id} after merge:\n{list}");
    }
}

#[test]
fn reverts_converge_regardless_of_merge_direction() {
    // A git revert of the commit that ADDED some tasks must converge to the same
    // surviving set no matter which way the branches are later merged - the merge
    // driver unions both sides' removals. We build the identical history twice and
    // merge it both directions, then assert the materialized task sets match.
    fn build(dir: &Path) {
        init_renamed_open(dir);
        ta(dir, &["create", "keep1"]);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-qm", "c0 base"]);
        ta(dir, &["create", "drop1"]);
        ta(dir, &["create", "drop2"]);
        git(dir, &["commit", "-aqm", "c1 adds drop1 drop2"]);
        ta(dir, &["create", "keep2"]);
        git(dir, &["commit", "-aqm", "c2 adds keep2"]);
        // Revert the commit that introduced drop1/drop2 (its Create events vanish
        // from the log), leaving keep1/keep2.
        git(dir, &["revert", "--no-edit", "HEAD~1"]);
        // Branch and add one distinct task per side.
        git(dir, &["branch", "feature"]);
        ta(dir, &["create", "on_main"]);
        git(dir, &["commit", "-aqm", "main task"]);
        git(dir, &["checkout", "-q", "feature"]);
        ta(dir, &["create", "on_feature"]);
        git(dir, &["commit", "-aqm", "feature task"]);
    }

    fn task_ids(dir: &Path) -> Vec<String> {
        let mut ids: Vec<String> = ta(dir, &["list"])
            .lines()
            .skip(1) // header row
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect();
        ids.sort();
        ids
    }

    // Direction 1: merge feature INTO main.
    let d1 = fresh_dir("revert-fwd");
    build(&d1);
    git(&d1, &["checkout", "-q", "main"]);
    let m1 = run("git", &d1, &["merge", "feature", "-m", "merge"]);
    assert!(
        m1.status.success(),
        "fwd merge: {}",
        String::from_utf8_lossy(&m1.stderr)
    );

    // Direction 2: merge main INTO feature.
    let d2 = fresh_dir("revert-rev");
    build(&d2);
    // Currently on `feature`; merge main in.
    let m2 = run("git", &d2, &["merge", "main", "-m", "merge"]);
    assert!(
        m2.status.success(),
        "rev merge: {}",
        String::from_utf8_lossy(&m2.stderr)
    );

    let fwd = task_ids(&d1);
    let rev = task_ids(&d2);
    assert_eq!(
        fwd, rev,
        "revert must converge both directions: {fwd:?} vs {rev:?}"
    );
    // The reverted tasks are gone; everything else survives, both ways.
    assert_eq!(
        fwd,
        ["keep1", "keep2", "on_feature", "on_main"],
        "surviving set after a reverted add: {fwd:?}"
    );
}

#[test]
fn revert_to_empty_log_is_handled() {
    // Reverting the commit that introduced the only task empties (or removes)
    // mutations.jsonl. The CLI must treat that degenerate empty / None-watermark
    // state as "no tasks", never erroring.
    let dir = fresh_dir("revert-empty");
    init_renamed_open(&dir);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    ta(&dir, &["create", "only"]);
    git(&dir, &["commit", "-aqm", "add only"]);
    // Reverting the create drops its line, leaving the log empty.
    git(&dir, &["revert", "--no-edit", "HEAD"]);

    assert!(
        !lists_task(&ta(&dir, &["list"]), "only"),
        "the reverted task must be gone and `list` must not error"
    );
    assert!(
        ta(&dir, &["status", "--format", "jsonl"]).contains(r#""total":0"#),
        "an emptied log reports zero tasks"
    );
}

#[test]
fn merge_warns_when_one_branch_reverts_a_shared_event() {
    // main and feature share a committed task `shared`; main alone reverts it.
    // The merge reconciles (the revert wins) but must WARN that a shared event was
    // reverted on one branch and kept on the other - not silently drop it.
    let dir = fresh_dir("revert-warn");
    init_renamed_open(&dir);
    ta(&dir, &["create", "base"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "base"]);
    ta(&dir, &["create", "shared"]);
    git(&dir, &["commit", "-aqm", "add shared"]);

    // Branch BEFORE reverting, so feature keeps `shared` while main drops it.
    // main only reverts and does NOT create afterwards: the freed seq stays unused,
    // so this is the pure presence-divergence the detector catches (a later create
    // would reuse the seq and surface as a content mismatch instead). The revert
    // auto-commits.
    git(&dir, &["branch", "feature"]);
    git(&dir, &["revert", "--no-edit", "HEAD"]); // main reverts the "add shared" commit

    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["create", "on_feature"]);
    git(&dir, &["commit", "-aqm", "feature task"]);

    git(&dir, &["checkout", "-q", "main"]);
    let m = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        m.status.success(),
        "merge should succeed (warn, not fail): {}",
        String::from_utf8_lossy(&m.stderr)
    );
    assert!(
        String::from_utf8_lossy(&m.stderr).contains("reverted on one branch"),
        "expected the shared-revert warning on stderr, got: {}",
        String::from_utf8_lossy(&m.stderr)
    );

    // The revert wins convergently: `shared` is gone, everything else survives.
    let list = ta(&dir, &["list"]);
    assert!(
        !lists_task(&list, "shared"),
        "reverted shared task is gone: {list}"
    );
    for id in ["base", "on_feature"] {
        assert!(lists_task(&list, id), "missing {id}: {list}");
    }
}

#[test]
fn concurrent_appends_merge_without_conflict() {
    let dir = fresh_dir("append-merge");
    init_renamed_open(&dir);
    ta(&dir, &["create", "log"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["branch", "feature"]);

    // Each branch appends to the SAME field since the fork.
    ta(&dir, &["update", "log", "notes+=from main"]);
    git(&dir, &["commit", "-aqm", "main note"]);
    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "log", "notes+=from feature"]);
    git(&dir, &["commit", "-aqm", "feature note"]);

    // Default on_conflict=surface FAILS the merge on a real conflict - so a clean
    // merge here proves appends commute. Both entries must survive.
    git(&dir, &["checkout", "-q", "main"]);
    let m = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        m.status.success(),
        "concurrent appends must merge cleanly: {}",
        String::from_utf8_lossy(&m.stderr)
    );
    let json = ta(&dir, &["show", "log", "--format", "json"]);
    assert!(
        json.contains("from main") && json.contains("from feature"),
        "both appends present after merge: {json}"
    );
}

#[test]
fn nested_store_merge_honors_the_configured_conflict_policy() {
    // The store lives BELOW the repo root. Git runs the merge driver at the
    // repo root, where walk-up discovery can't see the store - it must be
    // located via %P (the merged file's path). Proof: the nested store
    // configures on_conflict=theirs; an unfound store would fall back to the
    // default `surface` and FAIL this merge.
    let dir = fresh_dir("merge-nested");
    let sub = dir.join("svc");
    fs::create_dir_all(&sub).unwrap();
    run(ta_bin(), &sub, &["init"]); // store first, in a plain subdir...
    init_repo(&dir); // ...the repo appears ABOVE it
    run(ta_bin(), &sub, &["init"]); // register the drivers in the new repo
    ta(&sub, &["config", "set", "merge.on_conflict", "theirs"]);
    ta(&sub, &["create", "t", "status=open"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Both branches set the SAME field to different values: a real conflict.
    git(&dir, &["branch", "feature"]);
    ta(&sub, &["update", "t", "status=main"]);
    git(&dir, &["commit", "-aqm", "main edit"]);
    git(&dir, &["checkout", "-q", "feature"]);
    ta(&sub, &["update", "t", "status=feature"]);
    git(&dir, &["commit", "-aqm", "feature edit"]);

    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "nested store's `theirs` policy must auto-resolve:\n{}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        ta(&sub, &["show", "t", "--format", "json"]).contains(r#""status":"feature""#),
        "theirs won, proving the driver read the nested config"
    );
}

#[test]
fn concurrent_numeric_adds_commute_across_merge() {
    let dir = fresh_dir("merge-add");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Declare the schema so `+=` dispatches to the commutative Add op.
    let cfg = dir.join(".taska/config.toml");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[task_types.counter.fields]\npoints = \"uint\"\ntags = \"set<string>\"\n");
    fs::write(&cfg, text).unwrap();
    ta(&dir, &["create", "c", "type=counter", "points=0"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    // Each branch accumulates concurrently: numbers add, set elements insert.
    git(&dir, &["branch", "feature"]);
    ta(&dir, &["update", "c", "points+=2", "tags+=m"]);
    git(&dir, &["commit", "-aqm", "main add"]);
    git(&dir, &["checkout", "-q", "feature"]);
    ta(&dir, &["update", "c", "points+=3", "tags+=f"]);
    git(&dir, &["commit", "-aqm", "feature add"]);

    // The merge must auto-resolve (accumulates never conflict) and SUM.
    git(&dir, &["checkout", "-q", "main"]);
    let merge = run("git", &dir, &["merge", "feature", "-m", "merge"]);
    assert!(
        merge.status.success(),
        "accumulates merge cleanly:\n{}",
        String::from_utf8_lossy(&merge.stderr)
    );
    let shown = ta(&dir, &["show", "c", "--format", "json"]);
    assert!(shown.contains(r#""points":5"#), "2+3 commute: {shown}");
    assert!(
        shown.contains(r#""tags":["f","m"]"#),
        "set union in canonical order: {shown}"
    );
}
