//! Integration test that drives the `action` layer DIRECTLY — no cli, no binary,
//! no `format` — exactly as an external frontend (a TUI, a library consumer)
//! would. This is the guard for the frontend-agnostic claim: it uses only the
//! crate's public API (`taska::action::*` + `taska::storage::FileStore`), so if
//! an action ever needs `cli` or `format`, or the public surface stops being
//! enough to drive a command, this stops compiling.

use serde_json::{json, Map};
use taska::action;
use taska::model::{OpType, TaskState};
use taska::storage::{EventStore, FileStore};

/// A throwaway file store under the system temp dir (outside the repo tree).
fn provision(name: &str) -> FileStore {
    let dir = std::env::temp_dir().join("taska-action-test").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    FileStore::provision(dir.join(".taska")).unwrap()
}

/// Create a task the way a frontend would: build the payload with the configured
/// DISPLAY field names, translate to canonical via the public
/// [`taska::schema::canonicalize_fields`], then call the write action (which
/// speaks canonical). No cli plumbing involved.
fn create(store: &FileStore, id: &str, title: &str, status: &str) {
    let workflow = &store.config().workflow;
    let mut payload = Map::new();
    payload.insert("title".to_string(), json!(title));
    payload.insert(workflow.status_field.clone(), json!(status));
    taska::schema::canonicalize_fields(&mut payload, workflow).unwrap();
    action::write::create(store, id, payload, &Map::new()).unwrap();
}

#[test]
fn drives_create_read_and_dep_through_the_action_api_only() {
    let store = provision("crud");
    create(&store, "a", "Task A", "todo");
    create(&store, "b", "Task B", "todo");

    // Add an edge: b depends_on a — through the dep action, types from config.
    let types = store.config().relationships.types.clone();
    let written = action::dep::apply_edges(
        &store,
        "b",
        &["depends_on=a".to_string()],
        &OpType::AddEdge,
        &types,
    )
    .unwrap();
    assert_eq!(written, 1, "one stored edge written");

    // status: the typed summary, computed from the graph.
    let outcome = action::status(&store).unwrap();
    assert_eq!(outcome.summary.total, 2);
    assert_eq!(outcome.summary.ready, 1, "a is ready (no deps)");
    assert_eq!(outcome.summary.blocked, 1, "b is blocked by a");

    // list: both tasks come back as data (unordered is fine here).
    let list = action::list_tasks(
        &store,
        &action::ListQuery {
            criteria: &[],
            open: false,
            ready: false,
            display_columns: &[],
        },
    )
    .unwrap();
    assert_eq!(list.tasks.len(), 2);

    // list --ready filters to the actionable task.
    let ready = action::list_tasks(
        &store,
        &action::ListQuery {
            criteria: &[],
            open: false,
            ready: true,
            display_columns: &[],
        },
    )
    .unwrap();
    assert_eq!(ready.tasks.len(), 1);
    assert_eq!(ready.tasks[0].id, "a");

    // show: one task, with the INVERSE edge surfaced (b depends_on a ⇒ a `blocks` b).
    let show = action::show(&store, "a").unwrap();
    assert_eq!(show.task.id, "a");
    assert_eq!(
        show.task.custom_fields.get("blocks"),
        Some(&json!(["b"])),
        "inverse edge surfaced as a field: {:?}",
        show.task.custom_fields
    );

    // No warnings on a clean store.
    assert!(outcome.warnings.is_empty() && list.warnings.is_empty());
}

#[test]
fn write_prep_translates_display_names_to_canonical_via_public_api() {
    // A frontend honoring a renamed `status_field` translates display→canonical
    // through the PUBLIC schema helper before the write action — proving the
    // write half is usable without the cli's (private) plumbing.
    use taska::config::WorkflowConfig;
    use taska::schema::canonicalize_fields;
    let workflow = WorkflowConfig {
        status_field: "state".to_string(),
        ..WorkflowConfig::default()
    };

    let mut payload = Map::new();
    payload.insert("state".to_string(), json!("done"));
    payload.insert("title".to_string(), json!("X"));
    canonicalize_fields(&mut payload, &workflow).unwrap();

    assert!(
        payload.contains_key("status"),
        "display `state` → canonical `status`: {payload:?}"
    );
    assert!(!payload.contains_key("state"), "display key consumed");
    assert_eq!(payload.get("title"), Some(&json!("X")), "others untouched");
}

#[test]
fn dep_plan_and_cycles_return_typed_graph_data() {
    let store = provision("graph");
    create(&store, "lib", "Library", "todo");
    create(&store, "api", "API", "todo");
    create(&store, "ui", "UI", "todo");
    let types = store.config().relationships.types.clone();
    let add = |task: &str, dep: &str| {
        action::dep::apply_edges(
            &store,
            task,
            &[format!("depends_on={dep}")],
            &OpType::AddEdge,
            &types,
        )
        .unwrap();
    };
    add("api", "lib"); // api -> lib
    add("ui", "api"); // ui -> api -> lib

    // plan toward ui: lib, api, ui in dependency order.
    let plan = action::dep::plan(&store, &["ui".to_string()], false).unwrap();
    let order: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(order, ["lib", "api", "ui"], "prerequisites first");

    // no cycles in this DAG.
    let cycles = action::dep::cycles(&store).unwrap();
    assert!(cycles.cycles.is_empty());
}

#[test]
fn dep_tree_returns_the_requested_columns_per_node() {
    let store = provision("tree-columns");
    create(&store, "parent", "Parent task", "todo");
    create(&store, "child", "Child task", "todo");
    let types = store.config().relationships.types.clone();
    action::dep::apply_edges(
        &store,
        "parent",
        &["depends_on=child".to_string()],
        &OpType::AddEdge,
        &types,
    )
    .unwrap();

    // No field name is hardcoded — the caller names the columns it wants; the
    // node `cells` carry exactly those (here `title` is a plain field, not a
    // special one). Order is irrelevant for one root, so a trivial comparator.
    let cmp = |_a: &TaskState, _b: &TaskState| std::cmp::Ordering::Equal;
    let outcome = action::dep::tree(
        &store,
        &action::dep::TreeQuery {
            roots: &["parent".to_string()],
            open: false,
            reverse: false,
            columns: &["title".to_string(), "status".to_string()],
        },
        &cmp,
    )
    .unwrap();

    assert_eq!(outcome.forest.len(), 1);
    let root = &outcome.forest[0];
    assert_eq!(root.id, "parent");
    let cells: std::collections::HashMap<_, _> = root.cells.iter().cloned().collect();
    assert_eq!(cells.get("title"), Some(&json!("Parent task")));
    assert_eq!(cells.get("status"), Some(&json!("todo")));
}
