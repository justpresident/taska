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

    // Empty log -> seq 0 and cursor 0.
    assert_eq!(ta(&dir, &["status", "--current"]).trim(), "0");
    assert_eq!(ta(&dir, &["status", "--cursor"]).trim(), "0");

    // Each mutation prints the watch cursor it leaves (`[cursor:N]`), which is
    // exactly what `status --cursor` then reports; `--current` reports the log's
    // high-water seq. On a log that was never merged the two coincide.
    let created = ta(&dir, &["create", "foo"]);
    let c1 = ta(&dir, &["status", "--cursor"]);
    assert!(
        created.contains(&format!("[cursor:{}]", c1.trim())),
        "create={created:?} cursor={c1:?}"
    );
    assert!(
        !created.contains("[seq:"),
        "a mutation prints a cursor, not a seq: {created:?}"
    );

    let created2 = ta(&dir, &["create", "bar"]);
    let c2 = ta(&dir, &["status", "--cursor"]);
    assert!(
        created2.contains(&format!("[cursor:{}]", c2.trim())),
        "create2={created2:?} cursor={c2:?}"
    );
    assert!(
        c2.trim().parse::<u64>().unwrap() > c1.trim().parse::<u64>().unwrap(),
        "cursor advances: {c1:?} -> {c2:?}"
    );
    assert_eq!(
        ta(&dir, &["status", "--current"]).trim(),
        "2",
        "high-water seq"
    );

    // JSON forms for scripting: `{"seq":N}` and `{"cursor":N}`.
    let j = ta(&dir, &["status", "--current", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&j).unwrap();
    assert_eq!(v["seq"], 2);
    let j = ta(&dir, &["status", "--cursor", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&j).unwrap();
    assert_eq!(v["cursor"], c2.trim().parse::<u64>().unwrap());
}

/// After a pull restacks this clone's events, the same number means different
/// things as a seq and as a cursor. Both still count the events held (so
/// `--current` and `--cursor` agree here), but "everything up to N" differs: as a
/// seq, the first N events of the merged log; as a cursor, the first N events this
/// clone saw. `history --since` takes a seq, `watch --since` a cursor.
///
///   A: base@1, then + a1 + a2        B (clone at base): + b1   (B saw base, b1)
///   A pulls B -> base@1 a1@2 a2@3 b1@4;  B fast-forwards to it
///   On B: seq 2 = a1, but cursor 2 = b1. Since 2: history shows a2, b1; watch a1, a2.
#[test]
fn seq_and_cursor_name_different_events_after_a_restacking_pull() {
    let root = fresh_dir("status_seq_vs_cursor");
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    init_repo(&a);
    ta(&a, &["init", "--no-commit"]);
    ta(&a, &["create", "base"]);
    git(&a, &["add", "-A"]);
    git(&a, &["commit", "-qm", "init"]);
    git(
        &root,
        &["clone", "-q", a.to_str().unwrap(), b.to_str().unwrap()],
    );
    git(&b, &["config", "user.email", "test@taska.dev"]);
    git(&b, &["config", "user.name", "Taska Test"]);
    ta(&b, &["status"]); // registers the merge driver in the clone

    ta(&a, &["create", "a1"]);
    ta(&a, &["create", "a2"]);
    git(&a, &["commit", "-aqm", "A work"]);
    let wrote = ta(&b, &["create", "b1"]);
    assert!(wrote.contains("[cursor:2]"), "{wrote:?}");
    git(&b, &["commit", "-aqm", "B work"]);
    git(
        &a,
        &[
            "pull",
            "-q",
            "--no-rebase",
            "--no-edit",
            b.to_str().unwrap(),
            "main",
        ],
    );
    git(
        &b,
        &["pull", "-q", "--ff-only", a.to_str().unwrap(), "main"],
    );

    assert_eq!(ta(&b, &["status", "--current"]).trim(), "4");
    assert_eq!(ta(&b, &["status", "--cursor"]).trim(), "4");

    let history = ta(&b, &["history", "--since", "2"]);
    assert!(
        history.contains("a2") && history.contains("b1") && !history.contains("a1"),
        "history --since takes a seq: events at seq 3, 4:\n{history}"
    );
    let out = run(
        ta_bin(),
        &b,
        &[
            "watch",
            "--since",
            "2",
            "--timeout",
            "3s",
            "--holdout",
            "0s",
        ],
    );
    let watched = String::from_utf8_lossy(&out.stdout);
    assert!(
        watched.contains("a1:") && watched.contains("a2:") && !watched.contains("b1:"),
        "watch --since takes a cursor: the events B had not seen:\n{watched}"
    );
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
