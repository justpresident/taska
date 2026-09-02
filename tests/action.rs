//! Integration test that drives the `action` layer DIRECTLY - no cli, no binary,
//! no `format` - exactly as an external frontend (a TUI, a library consumer)
//! would. This is the guard for the frontend-agnostic claim: it uses only the
//! crate's public API (`taska::action::*` + `taska::storage::FileStore`), so if
//! an action ever needs `cli` or `format`, or the public surface stops being
//! enough to drive a command, this stops compiling.

use serde_json::{json, Map};
use taska::action;
use taska::model::{OpType, ID_KEY, STATUS_KEY};
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
    action::write::create(store, id, payload, &Map::new(), false).unwrap();
}

#[test]
fn drives_create_read_and_dep_through_the_action_api_only() {
    let store = provision("crud");
    create(&store, "a", "Task A", "todo");
    create(&store, "b", "Task B", "todo");

    // Add an edge: b depends_on a - through the dep action, types from config.
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
            sort: ID_KEY,
            reverse: false,
        },
    )
    .unwrap();
    assert_eq!(list.tasks.len(), 2);
    // The action returns ORDERED data - by `id` here, so `a` before `b`.
    let ids: Vec<&str> = list.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"], "list_tasks returns sorted tasks");

    // list --ready filters to the actionable task.
    let ready = action::list_tasks(
        &store,
        &action::ListQuery {
            criteria: &[],
            open: false,
            ready: true,
            display_columns: &[],
            sort: ID_KEY,
            reverse: false,
        },
    )
    .unwrap();
    assert_eq!(ready.tasks.len(), 1);
    assert_eq!(ready.tasks[0].id, "a");

    // show: the named tasks, with the INVERSE edge surfaced (b depends_on a =>
    // a `blocks` b). Multiple ids are accepted and deduplicated.
    let show = action::show(&store, &["a".to_string(), "a".to_string()]).unwrap();
    assert_eq!(show.tasks.len(), 1, "duplicate id collapses to one task");
    assert_eq!(show.tasks[0].id, "a");
    assert_eq!(
        show.tasks[0].custom_fields.get("blocks"),
        Some(&json!(["b"])),
        "inverse edge surfaced as a field: {:?}",
        show.tasks[0].custom_fields
    );

    // prime: the config-tailored facts, as structured data - drivable without the
    // cli's markdown rendering (an external frontend would render its own).
    let prime = action::prime(&store).unwrap();
    assert_eq!(prime.facts.status_field, STATUS_KEY);
    assert_eq!(prime.facts.done_status, "closed");
    assert_eq!(prime.facts.summary.total, 2);
    assert!(
        prime
            .facts
            .relationships
            .iter()
            .any(|r| r.name == "depends_on"),
        "relationships surfaced: {:?}",
        prime.facts.relationships
    );

    // No warnings on a clean store.
    assert!(outcome.warnings.is_empty() && list.warnings.is_empty());
}

#[test]
fn write_prep_translates_display_names_to_canonical_via_public_api() {
    // A frontend honoring a renamed `status_field` translates display->canonical
    // through the PUBLIC schema helper before the write action - proving the
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
        payload.contains_key(STATUS_KEY),
        "display `state` -> canonical `status`: {payload:?}"
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

    // No field name is hardcoded - the caller names the columns it wants; the
    // node `cells` carry exactly those (here `title` is a plain field, not a
    // special one). The action orders siblings itself, given the sort column.
    let outcome = action::dep::tree(
        &store,
        &action::dep::TreeQuery {
            roots: &["parent".to_string()],
            open: false,
            reverse: false,
            columns: &["title".to_string(), STATUS_KEY.to_string()],
            sort: ID_KEY,
        },
    )
    .unwrap();

    assert_eq!(outcome.forest.len(), 1);
    let root = &outcome.forest[0];
    assert_eq!(root.id, "parent");
    let cells: std::collections::HashMap<_, _> = root.cells.iter().cloned().collect();
    assert_eq!(cells.get("title"), Some(&json!("Parent task")));
    assert_eq!(cells.get(STATUS_KEY), Some(&json!("todo")));
}

#[test]
fn conditional_write_is_an_atomic_compare_and_swap() {
    use taska::schema::FieldOps;
    let store = provision("guard");
    create(&store, "t", "Task", "todo");

    // A set-status op, the way a frontend hands `write::update` canonical fields.
    let claim = |status: &str| FieldOps {
        set: std::iter::once((STATUS_KEY.to_string(), json!(status))).collect(),
        append: vec![],
        subtract: vec![],
        raw: Map::new(),
    };
    let todo = || vec![format!("{STATUS_KEY}=todo")];

    // Guard holds (status is todo): the claim applies.
    let ok = action::write::update(&store, "t", &claim("in_progress"), false, &todo()).unwrap();
    assert_eq!(ok.written.len(), 1, "claim applied while guard held");

    // Second claim with the SAME guard loses - status is now in_progress. Even
    // though the intended end-state matches (a would-be no-op), losing the race is
    // surfaced as an error carrying exit code 3, not a silent "already up to date".
    let Err(err) = action::write::update(&store, "t", &claim("in_progress"), false, &todo()) else {
        panic!("the second claim must fail its guard");
    };
    let coded = err
        .downcast_ref::<taska::error::CodedError>()
        .expect("a precondition failure is a CodedError");
    assert_eq!(
        coded.code(),
        taska::error::ExitCode::Precondition,
        "precondition failure exits 3"
    );

    // The failed write appended nothing - state is unchanged.
    let after = action::read(&store).unwrap();
    assert_eq!(
        after.state["t"].custom_fields.get(STATUS_KEY),
        Some(&json!("in_progress")),
        "lost claim left state untouched"
    );

    // Guarding a task that doesn't exist is a plain error, NOT a coded precondition.
    let Err(missing) = action::write::update(&store, "ghost", &claim("x"), false, &todo()) else {
        panic!("guarding a missing task must error");
    };
    assert!(
        missing.downcast_ref::<taska::error::CodedError>().is_none(),
        "not-found is exit 1, not the precondition code"
    );

    // Conditional delete honors the guard: wrong condition fails (3), right removes.
    let Err(del_err) = action::write::delete(&store, "t", &todo()) else {
        panic!("delete with a failing guard must error");
    };
    assert_eq!(
        del_err
            .downcast_ref::<taska::error::CodedError>()
            .unwrap()
            .code(),
        taska::error::ExitCode::Precondition
    );
    action::write::delete(&store, "t", &[format!("{STATUS_KEY}=in_progress")]).unwrap();
    assert!(
        !action::read(&store).unwrap().state.contains_key("t"),
        "conditional delete removed the task once its guard held"
    );
}

#[test]
fn status_transitions_are_enforced_by_the_action_layer_not_the_cli() {
    // The workflow gate is domain law, so a frontend that never touches `cli`
    // still can't write its way around it - and the exit-code taxonomy travels
    // as a typed error, not as a printed string.
    use taska::error::{CodedError, ExitCode};
    use taska::schema::FieldOps;

    let set_status = |value: &str| {
        let mut set = Map::new();
        set.insert(STATUS_KEY.to_string(), json!(value));
        FieldOps {
            set,
            append: Vec::new(),
            subtract: Vec::new(),
            raw: Map::new(),
        }
    };

    let dir = std::env::temp_dir()
        .join("taska-action-test")
        .join("transitions");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".taska")).unwrap();
    let mut config = taska::config::default_toml();
    config.push_str(
        "\n[task_types.wf]\nfields = { status = { type = \"enum\", \
         values = [\"todo\", \"review\", \"closed\"], \
         transitions = { todo = [\"review\"], review = [\"todo\", \"closed\"], \
         closed = [] } } }\n",
    );
    std::fs::write(dir.join(".taska/config.toml"), config).unwrap();
    let store = FileStore::provision(dir.join(".taska")).unwrap();

    let mut payload = Map::new();
    payload.insert("task_type".to_string(), json!("wf"));
    payload.insert(STATUS_KEY.to_string(), json!("todo"));
    action::write::create(&store, "t", payload, &Map::new(), false).unwrap();

    let Err(err) = action::write::update(&store, "t", &set_status("closed"), false, &[]) else {
        panic!("todo -> closed is not a declared move");
    };
    assert_eq!(
        err.downcast_ref::<CodedError>().map(CodedError::code),
        Some(ExitCode::Schema),
        "carries the schema exit code: {err}"
    );

    // The declared move goes through the same call.
    action::write::update(&store, "t", &set_status("review"), false, &[]).unwrap();
    let session = action::read(&store).unwrap();
    assert_eq!(
        session.state["t"].custom_fields[STATUS_KEY],
        json!("review")
    );
}

/// A `FieldOps` setting one arbitrary field, for seeding the store's vocabulary.
fn set_owner(owner: &str) -> taska::schema::FieldOps {
    let mut set = Map::new();
    set.insert("owner".to_string(), json!(owner));
    taska::schema::FieldOps {
        set,
        append: Vec::new(),
        subtract: Vec::new(),
        raw: Map::new(),
    }
}

/// The `edit` FORM drives through the public action API with no cli in sight -
/// the guard that its conventions (what a save means) are the domain's, not the
/// terminal frontend's. A TUI or MCP frontend gets the same form and the same
/// answers; all this test supplies is a parsed field map.
#[test]
fn drives_the_edit_form_through_the_action_api_only() {
    use taska::action::edit::{EditForm, EditMode, Preview};

    let store = provision("edit_form");
    create(&store, "seed", "Seed", "todo");
    create(&store, "t", "Target", "todo");
    // `owner` enters the store's vocabulary through another task, so it appears
    // on `t`'s form as a blank placeholder rather than a value.
    action::write::update(&store, "seed", &set_owner("bob"), true, &[]).unwrap();

    let form = EditForm::open(&store, "t", false).unwrap();
    assert!(matches!(form.mode(), EditMode::Update));
    assert_eq!(form.template["title"], json!("Target"));
    assert_eq!(
        form.template["owner"],
        json!(""),
        "a field this task has no value for shows as the unset placeholder"
    );

    // A save that touches nothing is not a write, even though the form is full
    // of lines the task doesn't actually store.
    let Preview::Ready { set, .. } = form
        .preview(&form.template.clone(), store.config())
        .unwrap()
    else {
        panic!("a full form is not an empty document");
    };
    assert!(set.is_empty(), "an untouched form writes nothing: {set:?}");

    // Deleting the placeholder line has nothing to unset; changing the title is
    // the only real write.
    let mut saved = form.template.clone();
    saved.remove("owner");
    saved.insert("title".to_string(), json!("Renamed"));
    let Preview::Ready { set, new_fields } = form.preview(&saved, store.config()).unwrap() else {
        panic!("expected a payload");
    };
    assert!(new_fields.is_empty());
    assert_eq!(set["title"], json!("Renamed"));
    assert_eq!(
        set.len(),
        1,
        "the deleted placeholder is not a write: {set:?}"
    );

    let written = form.apply(&store, set, false).unwrap().written;
    assert_eq!(written.len(), 1);
    let session = action::read(&store).unwrap();
    assert_eq!(session.state["t"].custom_fields["title"], json!("Renamed"));

    // An emptied document is the discard, never "unset every field".
    assert!(matches!(
        form.preview(&Map::new(), store.config()).unwrap(),
        Preview::Empty
    ));
}
