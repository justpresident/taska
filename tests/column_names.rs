//! Column names e2e - every surface that takes a column name rejects an unknown
//! one with a did-you-mean (never a silent blank column or `id` order), and
//! accepts every name the store really knows.
mod common;
use common::*;
use taska::model::{ID_KEY, STATUS_KEY};

/// A store with a few tasks, an edge, and a `mgr` field (plus history).
fn store(name: &str) -> std::path::PathBuf {
    let dir = fresh_dir(name);
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    ta(
        &dir,
        &[
            "create",
            "a",
            &format!("{STATUS_KEY}=todo"),
            "mgr=amy",
            "--new-field",
        ],
    );
    ta(&dir, &["create", "b", &format!("{STATUS_KEY}=todo")]);
    ta(&dir, &["dep", "add", "b", "depends_on=a"]);
    dir
}

/// Run `ta` expecting failure; returns stderr after asserting exit 1.
fn rejects(dir: &std::path::Path, args: &[&str]) -> String {
    let out = run(ta_bin(), dir, args);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{args:?} must fail with 1: {stderr}"
    );
    stderr
}

#[test]
fn unknown_sort_column_is_rejected_with_a_suggestion() {
    let dir = store("cols-sort");
    let err = rejects(&dir, &["list", "--sort", "mrg"]);
    assert!(err.contains("unknown sort column `mrg`"), "{err}");
    assert!(err.contains("did you mean `mgr`?"), "{err}");
    // Every key of a multi-key spec is checked, direction prefix aside.
    let err = rejects(&dir, &["list", "--sort", "mgr,-statsu"]);
    assert!(
        err.contains("`statsu`") && err.contains(&format!("`{STATUS_KEY}`")),
        "{err}"
    );
    let err = rejects(&dir, &["dep", "tree", "--sort", "nosuch"]);
    assert!(err.contains("unknown sort column `nosuch`"), "{err}");
}

#[test]
fn unknown_or_relationship_list_column_is_rejected() {
    let dir = store("cols-list");
    let err = rejects(&dir, &["list", "--columns", "id,mrg"]);
    assert!(
        err.contains("unknown column `mrg`") && err.contains("`mgr`"),
        "{err}"
    );
    // A relationship renders blank in `list` - edges live under `deps`.
    let err = rejects(&dir, &["list", "--columns", "id,blocks"]);
    assert!(
        err.contains("relationship type") && err.contains("`deps`"),
        "{err}"
    );
}

#[test]
fn show_columns_accept_relationships_but_not_typos() {
    let dir = store("cols-show");
    // `show` renders each relationship and inverse.
    let out = ta(&dir, &["show", "a", "--columns", "id,blocks"]);
    assert!(out.contains("blocks:"), "{out}");
    let err = rejects(&dir, &["show", "a", "--columns", "id,mrg"]);
    assert!(err.contains("did you mean `mgr`?"), "{err}");
}

#[test]
fn unknown_filter_field_is_rejected_but_known_ones_work() {
    let dir = store("cols-filter");
    let err = rejects(&dir, &["list", "statsu=todo"]);
    assert!(err.contains("unknown filter field `statsu`"), "{err}");
    assert!(
        err.contains(&format!("did you mean `{STATUS_KEY}`?")),
        "{err}"
    );
    // Relationship types/inverses and computed columns are real filter fields.
    assert!(ta(&dir, &["list", "depends_on=a", "--sort", ID_KEY]).contains('b'));
    assert!(ta(&dir, &["list", "blocks=b", "--sort", ID_KEY]).contains('a'));
    ta(&dir, &["list", "unblocks>0", "--sort", "-unblocks"]);
}

/// A field cleared from every task stays filterable while the retained log
/// remembers it - a claim pattern (`--if owner=`) must not break once nobody
/// holds a claim.
#[test]
fn a_field_cleared_everywhere_is_still_known() {
    let dir = store("cols-history");
    ta(&dir, &["update", "a", "mgr="]);
    let out = ta(&dir, &["list", "mgr=amy"]);
    assert!(
        out.contains("(no matches)"),
        "an empty match, not an error: {out}"
    );
}

#[test]
fn guards_reject_unknown_fields_but_know_the_fields_their_write_adds() {
    let dir = store("cols-guard");
    // A typo'd guard is a plain error (1), never a failed condition (3).
    let err = rejects(
        &dir,
        &[
            "update",
            "a",
            &format!("{STATUS_KEY}=in_progress"),
            "--if",
            "statsu=todo",
        ],
    );
    assert!(
        err.contains(&format!("did you mean `{STATUS_KEY}`?")),
        "{err}"
    );
    let err = rejects(&dir, &["delete", "a", "--if", "mrg=amy"]);
    assert!(err.contains("did you mean `mgr`?"), "{err}");
    // A first claim names the very field it introduces.
    ta(
        &dir,
        &["update", "b", "owner=me", "--new-field", "--if", "owner="],
    );
    assert!(ta(&dir, &["show", "b"]).contains("owner"));
}

/// A bad `[display] sort` warns at read time (a config typo mustn't lock out
/// `list`), and is an error where config is written or checked.
#[test]
fn configured_sort_typo_warns_on_read_and_fails_validation() {
    let dir = store("cols-config");
    let cfg = dir.join(".taska/config.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    let edited = text.replace("sort = \"create_time\"", "sort = \"mrg\"");
    assert_ne!(text, edited, "the template sets a default sort");
    std::fs::write(&cfg, edited).unwrap();

    let out = run(ta_bin(), &dir, &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "list still works: {stderr}");
    assert!(
        stderr.contains("[display] sort") && stderr.contains("did you mean `mgr`?"),
        "{stderr}"
    );
    let err = rejects(&dir, &["config", "validate"]);
    assert!(
        err.contains("display.sort") && err.contains("`mrg`"),
        "{err}"
    );

    // `config set` refuses a bad sort or width key, and accepts a good one.
    let err = rejects(&dir, &["config", "set", "display.sort", "mgr,-statsu"]);
    assert!(err.contains("`statsu`"), "{err}");
    let err = rejects(
        &dir,
        &["config", "set", "display.column_max_width.mrg", "10"],
    );
    assert!(err.contains("did you mean `mgr`?"), "{err}");
    ta(&dir, &["config", "set", "display.sort", "mgr,-create_time"]);
    let out = run(ta_bin(), &dir, &["list"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).trim().is_empty(),
        "the warning is gone once fixed"
    );
}

/// With no close match the error lists every valid name, so it's actionable
/// in a store the user doesn't know.
#[test]
fn an_unknown_name_without_a_close_match_lists_the_known_ones() {
    let dir = store("cols-known-list");
    let err = rejects(&dir, &["list", "--sort", "zzzzzz"]);
    assert!(
        err.contains("(known: ") && err.contains("`mgr`") && err.contains("`unblocks`"),
        "{err}"
    );
}

/// A field a schema declares but no task uses yet is a real column: an empty
/// column (or sort key) is a meaningful thing to ask for.
#[test]
fn a_declared_but_unused_field_is_accepted() {
    let dir = fresh_dir("cols-declared");
    init_repo(&dir);
    ta(&dir, &["init", "--no-commit"]);
    let cfg = dir.join(".taska/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[task_types.bug]\nfields = { severity = \"string\" }\n");
    std::fs::write(&cfg, text).unwrap();
    ta(&dir, &["create", "a", "type=bug"]);

    ta(
        &dir,
        &["list", "--columns", "id,severity", "--sort", "-severity"],
    );
    ta(&dir, &["list", "severity="]);
    ta(&dir, &["config", "set", "display.sort", "severity"]);
}
