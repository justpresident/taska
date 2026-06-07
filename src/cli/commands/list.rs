//! `ta list` — tasks rendered per the display args, optionally filtered.
//!
//! Filtering is folded in from the former `ta search`: positional
//! `field<op>value` criteria, all of which must match — `=` exact, `~` regex,
//! `!=` not-equal, `!~` regex-no-match — plus the `--open` shortcut (not done)
//! and `--ready` (the former `ta ready`: not done and every dependency done).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::cli::state_of;
use crate::config::{DisplayConfig, RelationshipDef, WorkflowConfig};
use crate::error::DynError;
use crate::format::{print_tasks, DisplayArgs};
use crate::graph;
use crate::model::{is_done, TaskState};
use crate::storage::EventStore;

pub fn cmd_list(
    store: &impl EventStore,
    criteria: &[String],
    open: bool,
    ready: bool,
    workflow: &WorkflowConfig,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
) -> Result<(), DynError> {
    // Compile (and validate regexes) up front so a bad criterion errors before
    // we touch the store.
    let criteria = compile_criteria(criteria)?;
    let mut state = state_of(store)?;
    // Criteria fields count as referenced, so a filter on a computed column
    // (`unblocks=0`) injects it without needing --columns/--sort to name it.
    let criteria_fields: Vec<String> = criteria.iter().map(|c| c.field.clone()).collect();
    crate::cli::inject_computed_columns(
        store,
        &mut state,
        workflow,
        display,
        cfg,
        &criteria_fields,
    );
    // Filtering context: declared relationship types resolve as filter fields
    // (forward by type name, reverse by inverse name — see `field_values`); the
    // reverse index is built only when a criterion actually names an inverse.
    let types = &store.config().relationships.types;
    let rev = criteria
        .iter()
        .any(|c| is_inverse_name(&c.field, types))
        .then(|| inverse_index(&state, types));
    let ctx = FilterCtx {
        types,
        rev: rev.as_ref(),
    };
    // The readiness-gating types: `--ready` filters by them, and the human deps
    // cell styles its type groups by them (gating bold, informational dim).
    let blockers = store.config().relationships.blocker_types();
    // `--ready` restricts to the ready set (not done, deps satisfied); it already
    // implies not-done, so it subsumes `--open`. Computed once over the full map.
    let ready_set: Option<HashSet<String>> = if ready {
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
    let tasks: Vec<&TaskState> = state
        .values()
        .filter(|t| !open || !is_done(t, &workflow.status_field, &workflow.done_status))
        .filter(|t| ready_set.as_ref().is_none_or(|s| s.contains(&t.id)))
        .filter(|t| criteria.iter().all(|c| c.matches(t, &ctx)))
        .collect();
    // A bare `list` shows "(no tasks)"; `--ready` with nothing actionable reads as
    // "(nothing ready)"; any other filter that matched nothing as "(no matches)".
    let empty = if ready {
        "(nothing ready)"
    } else if criteria.is_empty() && !open {
        "(no tasks)"
    } else {
        "(no matches)"
    };
    // Resolve the effective layout (flag, else `[display].list_layout`).
    let mut display = display.clone();
    display.layout = Some(display.layout.unwrap_or(cfg.list_layout));
    print_tasks(tasks, &display, cfg, &blockers, empty);
    Ok(())
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
/// `ta list blocks=X` and `show X`'s `blocks:` line agree.
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
/// target under ANY type (`deps` — so `deps=x` matches an info edge too, just
/// like the column shows it); a declared relationship type or inverse name (the
/// edge targets of that type, resp. the tasks whose edge of that type points
/// here — a symmetric type is both, so both directions union); or a single
/// custom field (empty when the task lacks it). Relationship names are reserved
/// as field names, so the dispatch is unambiguous. Unifying these lets every
/// operator treat built-ins, edges, and custom fields alike.
fn field_values(task: &TaskState, field: &str, ctx: &FilterCtx) -> Vec<Value> {
    match field {
        "id" => vec![Value::String(task.id.clone())],
        "deps" => task
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
