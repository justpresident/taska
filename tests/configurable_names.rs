//! e2e guard for the `tests-use-non-default-configurable-names` task.
//!
//! Drives EVERY configurable surface through non-default names (status field and
//! values, type field and type name, required field names, all relationship types
//! and inverses, timestamp column names, display columns). If production code
//! hardcodes a default (`status`/`depends_on`/`type`/`create_time`/...), a command
//! against this fully-renamed store breaks and one of these assertions fails.

mod common;
use common::names::*;
use common::*;

#[test]
fn create_list_filter_and_close_under_renamed_workflow() {
    let dir = fresh_dir("renamed-workflow");
    init_renamed(&dir);

    // Typed create; the renamed status field defaults to `backlog` (no state= given).
    ta(
        &dir,
        &[
            "create",
            "a",
            &format!("{TYPE_FIELD}={TASK_TYPE}"),
            &format!("{TITLE}=Alpha"),
            &format!("{NOTES}=first"),
        ],
    );
    ta(
        &dir,
        &[
            "create",
            "b",
            &format!("{TYPE_FIELD}={TASK_TYPE}"),
            &format!("{TITLE}=Bravo"),
            &format!("{NOTES}=second"),
            &format!("{STATUS_FIELD}={MID_STATUS}"),
        ],
    );

    // The default status was stamped onto `a` under the RENAMED field.
    let open = ta(&dir, &["list", "--open"]);
    assert!(
        lists_task(&open, "a") && lists_task(&open, "b"),
        "both open under renamed status: {open}"
    );

    // Filter by the renamed status field + a renamed value.
    let backlog = ta(&dir, &["list", &format!("{STATUS_FIELD}={DEFAULT_STATUS}")]);
    assert!(
        lists_task(&backlog, "a") && !lists_task(&backlog, "b"),
        "filter by renamed status field/value: {backlog}"
    );

    // Close `a` via the renamed status + done value.
    ta(
        &dir,
        &["update", "a", &format!("{STATUS_FIELD}={DONE_STATUS}")],
    );
    let shipped = ta(&dir, &["list", &format!("{STATUS_FIELD}={DONE_STATUS}")]);
    assert!(
        lists_task(&shipped, "a") && !lists_task(&shipped, "b"),
        "done value drives the closed filter: {shipped}"
    );
    let status = ta(&dir, &["status"]);
    assert!(
        status.contains("Closed"),
        "status summary renders: {status}"
    );
}

#[test]
fn dependencies_and_readiness_under_renamed_relationships() {
    let dir = fresh_dir("renamed-rels");
    init_renamed(&dir);
    for id in ["lib", "api", "cli"] {
        ta(
            &dir,
            &[
                "create",
                id,
                &format!("{TYPE_FIELD}={TASK_TYPE}"),
                &format!("{TITLE}={id}"),
                &format!("{NOTES}=n"),
            ],
        );
    }
    // Renamed blocker (`needs`) and hierarchy (`contains`).
    ta(&dir, &["dep", "add", "api", &format!("{BLOCKER}=lib")]);
    ta(&dir, &["dep", "add", "cli", &format!("{HIER}=api")]);

    // `--ready` is computed from the renamed blocker/hierarchy graph: only `lib`
    // (no prerequisites) is ready; `api` needs lib, `cli` contains api.
    let ready = ta(&dir, &["list", "--ready"]);
    assert!(
        lists_task(&ready, "lib") && !lists_task(&ready, "api") && !lists_task(&ready, "cli"),
        "readiness over renamed relationships: {ready}"
    );

    // The inverse edge is surfaced under the RENAMED inverse name on the target.
    let show_lib = ta(&dir, &["show", "lib", "--full"]);
    assert!(
        show_lib.contains(BLOCKER_INV) && show_lib.contains("api"),
        "renamed inverse `{BLOCKER_INV}` surfaced: {show_lib}"
    );
    assert!(
        !show_lib.contains("blocks") && !show_lib.contains("depends_on"),
        "default relationship names must not leak: {show_lib}"
    );

    // `dep tree` traverses the renamed blocker graph.
    let tree = ta(&dir, &["dep", "tree", "api"]);
    assert!(tree.contains("lib"), "tree follows renamed blocker: {tree}");

    // `dep plan` orders the renamed prerequisites.
    let plan = ta(&dir, &["dep", "plan", "api"]);
    assert!(
        plan.contains("lib"),
        "plan lists the renamed prerequisite: {plan}"
    );
}

#[test]
fn computed_timestamps_use_renamed_columns() {
    let dir = fresh_dir("renamed-timestamps");
    init_renamed(&dir);
    ta(
        &dir,
        &[
            "create",
            "a",
            &format!("{TYPE_FIELD}={TASK_TYPE}"),
            &format!("{TITLE}=A"),
            &format!("{NOTES}=n"),
        ],
    );
    ta(
        &dir,
        &["update", "a", &format!("{STATUS_FIELD}={DONE_STATUS}")],
    );

    // The computed timestamps are injected under the RENAMED column names, and the
    // defaults never appear.
    let show = ta(&dir, &["show", "a", "--full"]);
    for tok in [CREATE_TIME, UPDATE_TIME, CLOSE_TIME] {
        assert!(show.contains(tok), "renamed timestamp `{tok}`: {show}");
    }
    for leaked in [
        "create_time",
        "update_time",
        "close_time",
        "status:",
        "type:",
    ] {
        assert!(
            !show.contains(leaked),
            "default name `{leaked}` leaked: {show}"
        );
    }
    // A renamed timestamp works as a sort key and a column.
    let cols = ta(
        &dir,
        &[
            "list",
            "--columns",
            &format!("id,{CREATE_TIME}"),
            "--sort",
            CREATE_TIME,
        ],
    );
    assert!(
        cols.to_uppercase().contains(&CREATE_TIME.to_uppercase()),
        "renamed timestamp as a column: {cols}"
    );
}
