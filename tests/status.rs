//! `ta status` e2e - the `--current` cursor shortcut. The counts themselves are
//! unit-tested in `action::status` / `cli::commands::status`; here we drive the
//! real binary to cover the seq high-water behavior a `ta watch` loop relies on.
mod common;
use common::*;

/// `status --current` prints the log's high-water `seq`: 0 on an empty store,
/// then the exact seq each mutation prints, and the JSON form for scripting.
#[test]
fn status_current_tracks_the_high_water_seq() {
    let dir = fresh_dir("status_current");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);

    // Empty log -> cursor 0.
    assert_eq!(ta(&dir, &["status", "--current"]).trim(), "0");

    // Each mutation advances the cursor to the very seq it printed (`[seq:N]`).
    let created = ta(&dir, &["create", "foo"]);
    let c1 = ta(&dir, &["status", "--current"]);
    assert!(
        created.contains(&format!("[seq:{}]", c1.trim())),
        "create={created:?} current={c1:?}"
    );

    let created2 = ta(&dir, &["create", "bar"]);
    let c2 = ta(&dir, &["status", "--current"]);
    assert!(
        created2.contains(&format!("[seq:{}]", c2.trim())),
        "create2={created2:?} current={c2:?}"
    );
    assert!(
        c2.trim().parse::<u64>().unwrap() > c1.trim().parse::<u64>().unwrap(),
        "cursor advances: {c1:?} -> {c2:?}"
    );

    // JSON form for scripting: `{"seq":N}`.
    let j = ta(&dir, &["status", "--current", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&j).unwrap();
    assert_eq!(v["seq"], c2.trim().parse::<u64>().unwrap());
}

/// The default `status` view (no `--current`) reports the same high-water `seq`
/// alongside the counts: a `Seq` line in the human table and a `seq` field in the
/// JSON object, both equal to what `status --current` prints.
#[test]
fn status_reports_the_seq_cursor_with_the_counts() {
    let dir = fresh_dir("status_seq_default");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(&dir, &["create", "foo"]);
    ta(&dir, &["create", "bar"]);

    let cursor: u64 = ta(&dir, &["status", "--current"]).trim().parse().unwrap();

    // Human: a labeled `Seq` line carrying the cursor (alignment padding varies,
    // so match on the line, not exact spacing).
    let human = ta(&dir, &["status"]);
    assert!(human.contains("Total"), "still shows counts: {human}");
    let seq_line = human
        .lines()
        .find(|l| l.trim_start().starts_with("Seq"))
        .unwrap_or("");
    assert!(
        seq_line.contains(&cursor.to_string()),
        "human status Seq line shows the cursor {cursor}: {human}"
    );

    // JSON: a `seq` field equal to the cursor, next to the counts.
    let j = ta(&dir, &["status", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&j).unwrap();
    assert_eq!(v["seq"], cursor, "json seq matches --current: {j}");
    assert_eq!(v["total"], 2, "counts still present: {j}");
}
