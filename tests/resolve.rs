mod common;
use common::names::*;
use common::*;

#[test]
fn orphaned_events_warn_on_read_and_resolve_drops_them() {
    let dir = fresh_dir("orphan");
    init_renamed_open(&dir);

    // Create then delete `a`, then plant an Update event for the (now gone) `a` -
    // an orphan that applies to nothing at replay. (The gate rejects `ta update a`
    // on a missing task, so we write the orphan directly, as a merge/revert would.)
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["delete", "a"]);
    let log = dir.join(".taska").join("mutations.jsonl");
    append_orphan_update(&log, "a");

    let before = rows(&log);

    // A read command warns about the orphan on STDERR (without failing).
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(
        out.status.success(),
        "list should still succeed despite orphans"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("orphaned event") && stderr.contains("ta resolve"),
        "list should warn about the orphan: {stderr}"
    );

    // `ta resolve --force` drops the orphan and rewrites the log without it.
    let resolved = ta(&dir, &["resolve", "--force"]);
    assert!(
        resolved.contains("orphaned event"),
        "resolve should report dropping the orphan: {resolved}"
    );
    assert_eq!(
        rows(&log),
        before - 1,
        "exactly the one orphan event was removed from the log"
    );

    // The warning is gone on the next read, and a second resolve is a clean no-op.
    let after = run(ta_bin(), &dir, &["list"]);
    assert!(after.status.success(), "list should still succeed");
    assert!(
        !String::from_utf8_lossy(&after.stderr).contains("orphaned event"),
        "no orphan warning after resolve: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    let again = ta(&dir, &["resolve"]);
    assert!(again.contains("Nothing to resolve"), "got: {again}");
}

#[test]
fn resolve_orphans_requires_confirmation() {
    let dir = fresh_dir("resolve-confirm");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a"]);
    ta(&dir, &["delete", "a"]);
    let log = dir.join(".taska/mutations.jsonl");
    append_orphan_update(&log, "a"); // orphan: Update targeting the deleted `a`
    let before = rows(&log);
    // No --force and no stdin (EOF) declines: the orphan is listed but kept.
    let out = ta(&dir, &["resolve"]);
    assert!(out.contains("would be dropped"), "verbose listing: {out}");
    assert_eq!(rows(&log), before, "without --force the log is unchanged");
    // --force drops it.
    ta(&dir, &["resolve", "--force"]);
    assert!(rows(&log) < before, "--force drops the orphan");
}

/// Orphan pruning applies against the CURRENT log under the write lock and
/// matches the orphan by content: while the confirmation prompt is open a sync
/// lands an event AND restacks the log so the new event takes the orphan's seq.
/// Pruning by the previewed seq would drop the new event and keep the orphan.
#[test]
fn resolve_keeps_a_write_that_lands_while_its_prompt_is_open() {
    let dir = fresh_dir("resolve-prompt-race");
    init_renamed_open(&dir);
    ta(&dir, &["create", "a", &format!("{STATUS_FIELD}=open")]);
    ta(&dir, &["delete", "a"]);
    let log = dir.join(".taska").join("mutations.jsonl");
    append_orphan_update(&log, "a");
    let before = rows(&log);
    let out = ta_with_prompt_open(&dir, &["resolve"], || {
        ta(&dir, &["create", "landed-during-prompt"]);
        // Restack as a merge would: the new event moves in front of the orphan
        // and takes its seq.
        let text = std::fs::read_to_string(&log).unwrap();
        let mut lines: Vec<serde_json::Value> =
            text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let n = lines.len();
        let (orphan_seq, new_seq) = (lines[n - 2]["seq"].clone(), lines[n - 1]["seq"].clone());
        lines.swap(n - 2, n - 1);
        lines[n - 2]["seq"] = orphan_seq;
        lines[n - 1]["seq"] = new_seq;
        let body: String = lines.iter().map(|v| format!("{v}\n")).collect();
        std::fs::write(&log, body).unwrap();
    }, "y");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(rows(&log), before, "orphan dropped (-1), concurrent create kept (+1)");
    let list = run(ta_bin(), &dir, &["list"]);
    assert!(lists_task(&String::from_utf8_lossy(&list.stdout), "landed-during-prompt"));
    assert!(!String::from_utf8_lossy(&list.stderr).contains("orphaned event"));
}
