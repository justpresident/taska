mod common;
use common::*;

#[test]
fn orphaned_events_warn_on_read_and_resolve_drops_them() {
    let dir = fresh_dir("orphan");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // Create then delete `a`, then plant an Update event for the (now gone) `a` -
    // an orphan that applies to nothing at replay. (The gate rejects `ta update a`
    // on a missing task, so we write the orphan directly, as a merge/revert would.)
    ta(&dir, &["create", "a", "status=open"]);
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
    init_repo(&dir);
    ta(&dir, &["init"]);
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
