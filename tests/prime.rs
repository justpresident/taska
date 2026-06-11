//! e2e: `ta prime` — the config-tailored agent primer.
//!
//! Drives the real binary to prove the primer is RUNNABLE against the actual
//! store (right vocabulary, right commands) and CONFIG-DRIVEN (renaming the
//! workflow re-tailors every example, no recompile).

mod common;
use common::*;

/// The primer names this store's status field, lists its relationships, and
/// frames the core commands in that vocabulary.
#[test]
fn prime_reflects_the_store_vocabulary() {
    let dir = fresh_dir("prime-vocab");
    init_repo(&dir);
    ta(&dir, &["init"]);

    let out = ta(&dir, &["prime"]);
    assert!(
        out.contains("field `status`"),
        "names the status field: {out}"
    );
    assert!(
        out.contains("ta update <id> status=closed"),
        "close example uses the done status: {out}"
    );
    assert!(out.contains("`depends_on`"), "lists relationships: {out}");
    assert!(
        out.contains("ta list --ready"),
        "core commands present: {out}"
    );
}

/// Config-DRIVEN: renaming the workflow's display fields re-tailors every example
/// with no recompile — the whole point of a generated primer over a static doc.
#[test]
fn prime_tailors_to_renamed_workflow_fields() {
    let dir = fresh_dir("prime-tailor");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["config", "set", "workflow.status_field", "state"]);
    ta(&dir, &["config", "set", "workflow.done_status", "done"]);

    let out = ta(&dir, &["prime"]);
    assert!(out.contains("field `state`"), "renamed status field: {out}");
    assert!(
        out.contains("ta update <id> state=done"),
        "examples track the rename: {out}"
    );
    assert!(
        !out.contains("status=closed"),
        "the old vocabulary is gone: {out}"
    );
}

/// `--format json` emits the structured facts, so a non-Claude agent can build
/// its own prompt from them.
#[test]
fn prime_json_carries_the_facts() {
    let dir = fresh_dir("prime-json");
    init_repo(&dir);
    ta(&dir, &["init"]);
    ta(&dir, &["create", "a", "status=todo"]);

    let json = ta(&dir, &["prime", "--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(v["status_field"], "status");
    assert_eq!(v["done_status"], "closed");
    assert!(
        v["relationships"].is_array(),
        "relationships present: {json}"
    );
    assert_eq!(v["summary"]["total"], 1, "summary counts the task: {json}");
}
