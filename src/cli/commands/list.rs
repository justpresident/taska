//! `ta list` — tasks rendered per the display args, optionally filtered.
//!
//! Filtering is folded in from the former `ta search`: positional
//! `field<op>value` criteria, all of which must match — `=` exact, `~` regex,
//! `!=` not-equal, `!~` regex-no-match — plus the `--open` shortcut (not done)
//! and `--ready` (the former `ta ready`: not done and every dependency done).

use std::collections::HashSet;

use serde_json::Value;

use crate::cli::state_of;
use crate::config::{DisplayConfig, WorkflowConfig};
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
    crate::cli::inject_computed_columns(store, &mut state, workflow, display, cfg);
    // `--ready` restricts to the ready set (not done, deps satisfied); it already
    // implies not-done, so it subsumes `--open`. Computed once over the full map.
    let ready_set: Option<HashSet<String>> = if ready {
        let blockers = store.config().relationships.blocker_types();
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
        .filter(|t| criteria.iter().all(|c| c.matches(t)))
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
    print_tasks(tasks, &display, cfg, empty);
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

impl Criterion {
    /// Whether `task` satisfies this criterion. A field offers zero or more
    /// candidate values (a custom field is absent→none; `deps` is one per edge);
    /// equality/regex pass if ANY candidate matches. The negated forms are the
    /// logical NOT, so they also hold when the field is absent.
    fn matches(&self, task: &TaskState) -> bool {
        let values = field_values(task, &self.field);
        match &self.matcher {
            Matcher::Eq(q) => values.iter().any(|v| v == q),
            Matcher::Ne(q) => !values.iter().any(|v| v == q),
            Matcher::Re(re) => values.iter().any(|v| re.is_match(&value_string(v))),
            Matcher::NotRe(re) => !values.iter().any(|v| re.is_match(&value_string(v))),
        }
    }
}

/// The JSON value(s) a field offers for matching: the `id`, each dependency
/// (`deps`), or a single custom field (empty when the task lacks it). Unifying
/// the three lets every operator treat built-ins and custom fields alike.
fn field_values(task: &TaskState, field: &str) -> Vec<Value> {
    match field {
        "id" => vec![Value::String(task.id.clone())],
        "deps" => task
            .depends_on()
            .iter()
            .map(|d| Value::String(d.clone()))
            .collect(),
        _ => task.custom_fields.get(field).cloned().into_iter().collect(),
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
        let matches = |s: &str| compile_criterion(s).unwrap().matches(&t);

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
}
