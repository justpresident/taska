mod common;
use common::*;

#[test]
fn auto_timestamps_lifecycle_search_and_compaction() {
    let dir = fresh_dir("timestamps");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "api", "status=open", "title=API"]);

    // create_time + update_time are materialized; close_time only once done.
    let open = ta(&dir, &["show", "api", "--format", "json"]);
    assert!(
        open.contains("create_time") && open.contains("update_time"),
        "{open}"
    );
    assert!(
        !open.contains("close_time"),
        "open task has no close_time: {open}"
    );

    ta(&dir, &["update", "api", "status=closed"]);
    assert!(
        ta(&dir, &["show", "api", "--format", "json"]).contains("close_time"),
        "closing sets close_time"
    );

    // Reopening clears close_time (the user's 'cleared on reopen' choice).
    ta(&dir, &["update", "api", "status=open"]);
    assert!(
        !ta(&dir, &["show", "api", "--format", "json"]).contains("close_time"),
        "reopen clears close_time"
    );

    // The times are ordinary string fields: searchable and selectable as columns.
    assert!(
        lists_task(&ta(&dir, &["list", "create_time~^20"]), "api"),
        "searchable by create_time"
    );

    // Renaming/disabling via config: blank name hides the column entirely.
    fs::write(
        dir.join(".taska/config.toml"),
        "[timestamps]\ncreate_time = \"made_at\"\nupdate_time = \"\"\n",
    )
    .unwrap();
    let renamed = ta(&dir, &["show", "api", "--format", "json"]);
    assert!(
        renamed.contains("made_at"),
        "create_time renamed: {renamed}"
    );
    assert!(
        !renamed.contains("update_time"),
        "update_time disabled: {renamed}"
    );

    // create_time survives compaction (it is folded into the baseline). Generate
    // enough events to fold the original Create past the keep_events floor.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();
    for i in 0..350 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    ta(&dir, &["compact"]);
    // api's Create is now folded away, yet its create_time persists via baseline.
    assert!(
        ta(&dir, &["show", "api", "--format", "json"]).contains("create_time"),
        "create_time survives compaction"
    );
}

#[test]
fn compact_folds_log_and_appends_resume() {
    let dir = fresh_dir("compact");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // A valid retention floor (>= MIN_KEEP_EVENTS = 300). We then generate MORE
    // events than that so compaction actually folds the old prefix and retains
    // exactly the recent suffix — the fold-and-resume path.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();

    // 350 creates > keep_events (300), so 50 fold into the baseline and 300 stay.
    for i in 0..350 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    ta(&dir, &["compact"]);

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        300,
        "keep_events most-recent events retained in the log"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        50,
        "the folded remainder (350 - keep_events) is in the baseline"
    );

    // Appends overlay the baseline after compaction, and the older folded tasks
    // are still visible — fold-and-resume keeps everything reachable.
    ta(&dir, &["create", "resumed"]);
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t75", "t349", "resumed"] {
        assert!(lists_task(&list, id), "missing {id} in list:\n{list}");
    }
}

#[test]
fn compact_retains_recent_events_for_merge() {
    let dir = fresh_dir("retain");
    init_repo(&dir);
    ta(&dir, &["init"]);
    // Valid retention (>= MIN_KEEP_EVENTS). With more events than keep_events, the
    // recent suffix is retained in the log so divergent branches can still merge.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 300\nkeep_days = 0\n",
    )
    .unwrap();

    // 350 creates > keep_events (300): 50 fold into baseline, 300 newest retained.
    for i in 0..350 {
        ta(&dir, &["create", &format!("t{i}")]);
    }
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("kept 300 recent event(s)"), "got: {out}");

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        300,
        "kept keep_events recent events for merge reconciliation"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        50,
        "folded the oldest 50 into baseline"
    );

    // The retained log holds the most recent creations (not the oldest, which
    // were folded away). The newest task is in the log; the oldest is not.
    let mutations = fs::read_to_string(dir.join(".taska/mutations.jsonl")).unwrap();
    assert!(
        mutations.contains(r#""task_id":"t349""#),
        "expected the newest event retained: {mutations}"
    );
    assert!(
        !mutations.contains(r#""task_id":"t0""#),
        "the oldest event should have been folded out of the log: {mutations}"
    );

    // All tasks remain visible (baseline + retained log), old and new alike.
    let list = ta(&dir, &["list"]);
    for id in ["t0", "t49", "t50", "t349"] {
        assert!(lists_task(&list, id), "missing {id}:\n{list}");
    }
}

#[test]
fn low_keep_events_is_rejected_on_the_next_command() {
    let dir = fresh_dir("validate");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // A hand-edited, unreasonably small retention value.
    fs::write(
        dir.join(".taska/config.toml"),
        "[compaction]\nkeep_events = 50\n",
    )
    .unwrap();

    // Without the test hatch, even a plain `ta list` must refuse and explain —
    // the error surfaces immediately, not weeks later at compaction time.
    let out = Command::new(ta_bin())
        .args(["list"])
        .current_dir(&dir)
        .env("PATH", path_with_bin())
        .output()
        .unwrap();
    assert!(!out.status.success(), "list should fail on invalid config");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("keep_events") && stderr.contains("300"),
        "stderr: {stderr}"
    );
}

#[test]
fn compact_is_noop_below_threshold() {
    let dir = fresh_dir("compact_noop");
    init_repo(&dir);
    ta(&dir, &["init"]); // default keep_events = 5000

    ta(&dir, &["create", "a"]);
    ta(&dir, &["create", "b"]);
    let out = ta(&dir, &["compact"]);
    assert!(out.contains("Nothing to compact"), "got: {out}");

    assert_eq!(
        rows(&dir.join(".taska/mutations.jsonl")),
        2,
        "log untouched"
    );
    assert_eq!(
        rows(&dir.join(".taska/baseline.jsonl")),
        0,
        "baseline still empty"
    );
}

/// A baseline written in the OLD on-disk format — `depends_on` as a top-level
/// field, before it was folded into the relationships map — still loads: the
/// `#[serde(from)]` compat shim merges it into `relationships`, so the dep shows
/// in the `deps` column and still gates readiness.
#[test]
fn legacy_baseline_depends_on_field_is_folded_into_relationships() {
    let dir = fresh_dir("legacy-baseline");
    init_repo(&dir);
    ta(&dir, &["init"]);
    let baseline = dir.join(".taska").join("baseline.jsonl");
    fs::write(
        &baseline,
        "{\"id\":\"a\",\"custom_fields\":{\"status\":\"open\"}}\n\
         {\"id\":\"b\",\"depends_on\":[\"a\"],\"custom_fields\":{\"status\":\"open\"}}\n",
    )
    .unwrap();

    // The legacy top-level depends_on surfaces as the `deps` column.
    let json = ta(&dir, &["show", "b", "--format", "json"]);
    assert!(
        json.contains("\"deps\":[\"a\"]"),
        "legacy dep folded: {json}"
    );

    // ...and it still gates readiness: `a` is ready, `b` is blocked by it.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(lists_task(&ready, "a"), "a ready: {ready}");
    assert!(!lists_task(&ready, "b"), "b blocked by legacy dep: {ready}");
}
