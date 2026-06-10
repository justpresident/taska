//! `list` action: the filtered task set.
//!
//! Compiles positional `field<op>value` criteria (`=` exact, `~` regex, `!=`/
//! `!~` their negations), applies the `--open`/`--ready` shortcuts, injects the
//! graph-computed columns a query references, and returns the matching tasks as
//! data — ordering and rendering are the frontend's job.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::action::{read, Warning};
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::graph;
use crate::model::{
    is_done, TaskState, BLOCKED_BY_KEY, DEPS_KEY, ID_KEY, SUBTASKS_KEY, UNBLOCKS_KEY,
};
use crate::storage::EventStore;

/// A `list` query: the filter criteria, the not-done / ready shortcuts, and the
/// columns the frontend intends to display.
pub struct ListQuery<'a> {
    pub criteria: &'a [String],
    /// Only not-done tasks (status is not the configured done value).
    pub open: bool,
    /// Only tasks ready to work on: not done and every dependency done. Implies
    /// (and subsumes) `open`.
    pub ready: bool,
    /// Columns the frontend will display or sort by. Drives lazy injection of
    /// the graph-computed columns (`unblocks`/`blocked_by`/`subtasks`), so they
    /// cost nothing unless referenced. Criterion fields are added automatically.
    pub display_columns: &'a [String],
}

/// A `list` read: the matching tasks (filtered, with referenced computed columns
/// injected; unordered) plus any read [`Warning`]s.
pub struct ListOutcome {
    pub tasks: Vec<TaskState>,
    pub warnings: Vec<Warning>,
}

/// Materialize the store and return the tasks matching `query`.
pub fn list_tasks(store: &impl EventStore, query: &ListQuery) -> Result<ListOutcome, DynError> {
    // Compile (and validate regexes) up front so a bad criterion errors before
    // we touch the store.
    let criteria = compile_criteria(query.criteria)?;
    let session = read(store)?;
    let mut state = session.state;
    let workflow = &store.config().workflow;

    // Criterion fields count as referenced, so a filter on a computed column
    // (`unblocks=0`) injects it without --columns/--sort naming it.
    let criteria_fields: Vec<&str> = criteria.iter().map(|c| c.field.as_str()).collect();
    inject_computed_columns(store, &mut state, query.display_columns, &criteria_fields);

    // Filtering context: declared relationship types resolve as filter fields
    // (forward by type name, reverse by inverse name); the reverse index is built
    // only when a criterion actually names an inverse.
    let types = &store.config().relationships.types;
    let rev = criteria
        .iter()
        .any(|c| is_inverse_name(&c.field, types))
        .then(|| inverse_index(&state, types));
    let ctx = FilterCtx {
        types,
        rev: rev.as_ref(),
    };

    // `--ready` restricts to the ready set (not done, deps satisfied); it already
    // implies not-done, so it subsumes `--open`.
    let blockers = store.config().relationships.blocker_types();
    let ready_set: Option<HashSet<String>> = if query.ready {
        let ids = graph::ready_tasks(
            &state,
            &workflow.status_field,
            &workflow.done_status,
            &blockers,
        )?;
        Some(ids.into_iter().collect())
    } else {
        None
    };

    let tasks: Vec<TaskState> = state
        .values()
        .filter(|t| !query.open || !is_done(t, &workflow.status_field, &workflow.done_status))
        .filter(|t| ready_set.as_ref().is_none_or(|s| s.contains(&t.id)))
        .filter(|t| criteria.iter().all(|c| c.matches(t, &ctx)))
        .cloned()
        .collect();

    Ok(ListOutcome {
        tasks,
        warnings: session.warnings,
    })
}

/// Inject the graph-computed columns onto `state`, but only when the query
/// references them (a displayed/sorted column or a criterion field) — so default
/// output stays unchanged unless asked. They are surfaced as ordinary fields, so
/// `cell_value`/sorting/filtering handle them with no special-casing.
///
/// - `unblocks`/`blocked_by` — transitive not-done dependents / prerequisites
///   over the blocker edges (numbers).
/// - `subtasks` — a parent's `done/total` direct-child completion (string).
fn inject_computed_columns(
    store: &impl EventStore,
    state: &mut HashMap<String, TaskState>,
    display_columns: &[String],
    criteria_fields: &[&str],
) {
    let wants =
        |name: &str| display_columns.iter().any(|c| c == name) || criteria_fields.contains(&name);
    let workflow = &store.config().workflow;

    if wants(UNBLOCKS_KEY) || wants(BLOCKED_BY_KEY) {
        let blockers = store.config().relationships.blocker_types();
        let counts = graph::reachability_counts(
            state,
            &blockers,
            &workflow.status_field,
            &workflow.done_status,
        );
        for (id, task) in state.iter_mut() {
            if let Some(&(unblocks, blocked_by)) = counts.get(id) {
                task.custom_fields
                    .insert(UNBLOCKS_KEY.to_string(), serde_json::json!(unblocks));
                task.custom_fields
                    .insert(BLOCKED_BY_KEY.to_string(), serde_json::json!(blocked_by));
            }
        }
    }

    if wants(SUBTASKS_KEY) {
        let hierarchy = store.config().relationships.hierarchy_types();
        let progress = graph::subtask_progress(
            state,
            &hierarchy,
            &workflow.status_field,
            &workflow.done_status,
        );
        for (id, task) in state.iter_mut() {
            if let Some(&(done, total)) = progress.get(id) {
                task.custom_fields.insert(
                    SUBTASKS_KEY.to_string(),
                    serde_json::json!(format!("{done}/{total}")),
                );
            }
        }
    }
}

/// A filter operator. `=`/`!=` compare the field's value against a JSON-coerced
/// query; `~`/`!~` match a regex against the field's string form.
#[derive(Clone, Copy)]
enum FilterOp {
    Eq,
    Ne,
    Re,
    NotRe,
}

/// One parsed, compiled filter criterion: a field plus how to test it.
struct Criterion {
    field: String,
    matcher: Matcher,
}

enum Matcher {
    Eq(Value),
    Ne(Value),
    Re(regex::Regex),
    NotRe(regex::Regex),
}

/// What a criterion's field resolves against beyond the task itself: the
/// declared relationship types (forward edges by type name) and, when a
/// criterion names an inverse, the prebuilt reverse index.
struct FilterCtx<'a> {
    types: &'a BTreeMap<String, RelationshipDef>,
    rev: Option<&'a HashMap<String, BTreeMap<String, Vec<String>>>>,
}

/// Whether `field` is some declared type's inverse display name (a symmetric
/// type's own name included).
fn is_inverse_name(field: &str, types: &BTreeMap<String, RelationshipDef>) -> bool {
    types.values().any(|def| def.inverse == field)
}

/// Per-task inverse-direction edges under their inverse display names:
/// `rev[target][inverse] = owners pointing at target`. One pass over the state;
/// the same direction semantics as the inverse fields `show` injects, so
/// `list blocks=X` and `show X`'s `blocks:` line agree.
fn inverse_index(
    state: &HashMap<String, TaskState>,
    types: &BTreeMap<String, RelationshipDef>,
) -> HashMap<String, BTreeMap<String, Vec<String>>> {
    let mut rev: HashMap<String, BTreeMap<String, Vec<String>>> = HashMap::new();
    for (owner, task) in state {
        for (rel, targets) in &task.relationships {
            let Some(def) = types.get(rel) else { continue };
            if def.inverse.is_empty() {
                continue; // one-way type: no reverse surface
            }
            for target in targets {
                rev.entry(target.clone())
                    .or_default()
                    .entry(def.inverse.clone())
                    .or_default()
                    .push(owner.clone());
            }
        }
    }
    rev
}

impl Criterion {
    /// Whether `task` satisfies this criterion. A field offers zero or more
    /// candidate values (a custom field is absent→none; `deps` is one per edge);
    /// equality/regex pass if ANY candidate matches. The negated forms are the
    /// logical NOT, so they also hold when the field is absent.
    fn matches(&self, task: &TaskState, ctx: &FilterCtx) -> bool {
        let values = field_values(task, &self.field, ctx);
        match &self.matcher {
            Matcher::Eq(q) => values.iter().any(|v| v == q),
            Matcher::Ne(q) => !values.iter().any(|v| v == q),
            Matcher::Re(re) => values.iter().any(|v| re.is_match(&value_string(v))),
            Matcher::NotRe(re) => !values.iter().any(|v| re.is_match(&value_string(v))),
        }
    }
}

/// The JSON value(s) a field offers for matching: the `id`; each relationship
/// target under ANY type (`deps`); a declared relationship type or inverse name
/// (the edge targets of that type, resp. the tasks whose edge of that type points
/// here — a symmetric type is both, so both directions union); or a single custom
/// field (empty when the task lacks it). Relationship names are reserved as field
/// names, so the dispatch is unambiguous.
fn field_values(task: &TaskState, field: &str, ctx: &FilterCtx) -> Vec<Value> {
    match field {
        ID_KEY => vec![Value::String(task.id.clone())],
        DEPS_KEY => task
            .relationships
            .values()
            .flatten()
            .map(|d| Value::String(d.clone()))
            .collect(),
        _ => {
            let forward = ctx.types.contains_key(field);
            let inverse = is_inverse_name(field, ctx.types);
            if !forward && !inverse {
                return task.custom_fields.get(field).cloned().into_iter().collect();
            }
            let mut values = Vec::new();
            if forward {
                if let Some(targets) = task.relationships.get(field) {
                    values.extend(targets.iter().map(|t| Value::String(t.clone())));
                }
            }
            if inverse {
                if let Some(owners) = ctx
                    .rev
                    .and_then(|rev| rev.get(&task.id))
                    .and_then(|names| names.get(field))
                {
                    values.extend(owners.iter().map(|o| Value::String(o.clone())));
                }
            }
            values
        }
    }
}

/// A JSON value's string form for regex matching: the raw string for a JSON
/// string, else its compact JSON (so `priority~^3$` can match the number 3).
fn value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn compile_criteria(raw: &[String]) -> Result<Vec<Criterion>, DynError> {
    raw.iter().map(|r| compile_criterion(r)).collect()
}

fn compile_criterion(raw: &str) -> Result<Criterion, DynError> {
    let (field, op, value) = split_criterion(raw)?;
    let matcher = match op {
        FilterOp::Eq => Matcher::Eq(json_or_string(value)),
        FilterOp::Ne => Matcher::Ne(json_or_string(value)),
        FilterOp::Re => Matcher::Re(compile_regex(value)?),
        FilterOp::NotRe => Matcher::NotRe(compile_regex(value)?),
    };
    Ok(Criterion {
        field: field.to_string(),
        matcher,
    })
}

/// Split `field<op>value` at its FIRST operator, so an operator character inside
/// the value (e.g. a regex `~`) doesn't fool the parser. `!` is only an operator
/// when followed by `=` or `~`.
fn split_criterion(raw: &str) -> Result<(&str, FilterOp, &str), DynError> {
    let bytes = raw.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        let (op, len) = match c {
            b'=' => (FilterOp::Eq, 1),
            b'~' => (FilterOp::Re, 1),
            b'!' => match bytes.get(i + 1) {
                Some(b'=') => (FilterOp::Ne, 2),
                Some(b'~') => (FilterOp::NotRe, 2),
                _ => continue,
            },
            _ => continue,
        };
        if i == 0 {
            return Err(format!("invalid criterion `{raw}`: empty field name").into());
        }
        return Ok((&raw[..i], op, &raw[i + len..]));
    }
    Err(format!(
        "invalid criterion `{raw}`: expected field=value, field~regex, field!=value, or field!~regex"
    )
    .into())
}

/// Coerce a query string as JSON, falling back to a plain string — the same
/// coercion `create`/`update` apply, so `priority=3` matches the number 3.
fn json_or_string(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn compile_regex(pattern: &str) -> Result<regex::Regex, DynError> {
    regex::Regex::new(pattern).map_err(|e| format!("invalid regex `{pattern}`: {e}").into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::task;

    #[test]
    fn filter_criteria_compile_and_match() {
        let t = task(
            "api",
            &["db"],
            &[
                ("status", serde_json::json!("open")),
                ("priority", serde_json::json!(3)),
            ],
        );
        let types = crate::config::RelationshipConfig::default().types;
        let ctx = FilterCtx {
            types: &types,
            rev: None,
        };
        let matches = |s: &str| compile_criterion(s).unwrap().matches(&t, &ctx);

        // Exact (JSON-coerced: number 3, not "3"), regex, negation.
        assert!(matches("status=open"));
        assert!(!matches("status=closed"));
        assert!(matches("priority=3"), "number coercion");
        assert!(matches(r"status~^op"), "regex on string");
        assert!(matches(r"priority~^3$"), "regex on number's string form");
        assert!(matches("status!=closed"));
        assert!(!matches("status!~^op"));

        // Built-in id and deps fields.
        assert!(matches("id=api"));
        assert!(matches("deps=db"));
        assert!(!matches("deps=missing"));

        // A negated criterion also holds when the field is absent entirely.
        assert!(matches("owner!=bob"), "absent field passes !=");
        assert!(matches("owner!~x"), "absent field passes !~");
        assert!(!matches("owner=bob"), "absent field fails =");

        // Parse errors: no operator, empty field, bad regex.
        assert!(compile_criterion("nooperator").is_err());
        assert!(compile_criterion("=value").is_err());
        assert!(compile_criterion("title~[").is_err());

        // The first operator wins, so a regex value may contain operators.
        let (field, _, value) = split_criterion("title~a=b").unwrap();
        assert_eq!((field, value), ("title", "a=b"));
    }

    #[test]
    fn relationship_type_and_inverse_names_resolve_as_filter_fields() {
        // epic has_subtask child; child depends_on lib; child relates_to other.
        let mut epic = task("epic", &[], &[]);
        epic.relationships
            .insert("has_subtask".to_string(), vec!["child".to_string()]);
        let mut child = task("child", &["lib"], &[]);
        child
            .relationships
            .insert("relates_to".to_string(), vec!["other".to_string()]);
        let lib = task("lib", &[], &[]);
        let other = task("other", &[], &[]);

        let types = crate::config::RelationshipConfig::default().types;
        let state: HashMap<String, TaskState> = [&epic, &child, &lib, &other]
            .into_iter()
            .map(|t| (t.id.clone(), t.clone()))
            .collect();
        let rev = inverse_index(&state, &types);
        let ctx = FilterCtx {
            types: &types,
            rev: Some(&rev),
        };
        let matches = |t: &TaskState, s: &str| compile_criterion(s).unwrap().matches(t, &ctx);

        // Forward: the type name yields that type's targets, operators compose.
        assert!(matches(&child, "depends_on=lib"));
        assert!(!matches(&epic, "depends_on=lib"), "epic has no such edge");
        assert!(matches(&epic, "has_subtask=child"));
        assert!(matches(&child, r"depends_on~^li"), "regex over targets");

        // Inverse names resolve the reverse direction (as `show` surfaces them).
        assert!(matches(&child, "subtask_of=epic"), "child's parent");
        assert!(matches(&lib, "blocks=child"), "lib blocks child");
        assert!(!matches(&other, "blocks=child"));

        // Symmetric relates_to matches from BOTH sides of the stored edge.
        assert!(matches(&child, "relates_to=other"), "forward direction");
        assert!(matches(&other, "relates_to=child"), "mirrored direction");

        // Negation keeps its absent-passes logic for edge fields too.
        assert!(matches(&lib, "subtask_of!=epic"), "lib is no subtask");
        assert!(!matches(&child, "subtask_of!=epic"));
    }
}
