//! e2e: the soft-schema typo guard - a field name no task uses is rejected (with
//! a did-you-mean) unless `--new-field`, with an empty-store grace on the first
//! write. See also the schema unit tests for the distance/vocabulary internals.

mod common;
use common::*;

#[test]
fn unknown_field_is_blocked_with_a_suggestion_until_new_field() {
    let dir = fresh_dir("typo-guard");
    init_repo(&dir);
    ta(&dir, &["init"]);

    // First task on an EMPTY store: grace - every field is accepted, no flag,
    // seeding the vocabulary (here `priority`, which isn't a default column).
    ta(&dir, &["create", "t1", "title=First", "priority=high"]);
    // A later task reusing known names is fine.
    ta(&dir, &["create", "t2", "title=Second", "priority=low"]);

    // A typo of an existing field is rejected, suggesting the real name. The soft
    // schema is schema validation too, so it exits 2 (like the `[task_types]` gate).
    let out = run(ta_bin(), &dir, &["create", "t3", "titel=Third"]);
    assert_eq!(out.status.code(), Some(2), "soft-schema typo guard exits 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("titel") && stderr.contains("did you mean `title`"),
        "did-you-mean: {stderr}"
    );
    assert!(stderr.contains("--new-field"), "remedy: {stderr}");

    // A genuinely new field is blocked too (unknown, just not a near-typo)...
    assert!(
        !run(ta_bin(), &dir, &["update", "t1", "estimate=3"])
            .status
            .success(),
        "an unknown field needs --new-field"
    );
    // ...until --new-field introduces it; then it's known for later writes.
    ta(&dir, &["update", "t1", "--new-field", "estimate=3"]);
    ta(&dir, &["update", "t2", "estimate=5"]); // now in the vocabulary, no flag
    assert!(ta(&dir, &["show", "t2", "--format", "json"]).contains(r#""estimate":5"#));
}

#[test]
fn new_field_flag_warns_when_nothing_new_is_introduced() {
    let dir = fresh_dir("typo-redundant");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "t1", "title=First"]);

    // --new-field, but every field is already known: the write still applies, with
    // a warning that the flag had no effect (so reflexive --new-field gets nagged).
    let out = run(
        ta_bin(),
        &dir,
        &["update", "t1", "--new-field", "title=Renamed"],
    );
    assert!(out.status.success(), "the write still applies");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--new-field had no effect"),
        "redundant-flag warning: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
