//! `list` action: the filtered task set.
//!
//! Compiles positional `field<op>value` criteria (`=` exact, `=~` regex, `!=`/
//! `!~` their negations, `>`/`>=`/`<`/`<=` ordering), applies the
//! `--open`/`--ready` shortcuts, injects the graph-computed columns a query
//! references, and returns the matching tasks ordered by the query's sort
//! column — rendering is the frontend's job.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::action::{inject_computed_columns, read, Warning};
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::graph;
use crate::model::{cmp_json, is_done, task_cmp, TaskState, DEPS_KEY, ID_KEY};
use crate::storage::EventStore;

/// A `list` query: the filter criteria, the not-done / ready shortcuts, the
/// columns the frontend intends to display, and the sibling ordering.
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
    /// The column to order by, ascending (`id` tiebreak, missing values last —
    /// see [`task_cmp`]). May be `id`, `deps`, a custom field, or an injected
    /// computed column.
    pub sort: &'a str,
    /// Flip the order to descending.
    pub reverse: bool,
}

/// A `list` read: the matching tasks (filtered, with referenced computed columns
/// injected, ordered by the query's sort column) plus any read [`Warning`]s.
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

    // The columns this query touches: what the frontend will display/sort, plus
    // the criterion fields (so a filter on `unblocks=0` injects the column without
    // --columns/--sort naming it).
    let mut wanted: Vec<&str> = query.display_columns.iter().map(String::as_str).collect();
    wanted.extend(criteria.iter().map(|c| c.field.as_str()));
    inject_computed_columns(store, &mut state, &wanted);

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

    let mut tasks: Vec<TaskState> = state
        .values()
        .filter(|t| !query.open || !is_done(t, &workflow.status_field, &workflow.done_status))
        .filter(|t| ready_set.as_ref().is_none_or(|s| s.contains(&t.id)))
        .filter(|t| criteria.iter().all(|c| c.matches(t, &ctx)))
        .cloned()
        .collect();

    // Return ordered data: the frontend renders the slice as-is.
    tasks.sort_by(|a, b| task_cmp(a, b, query.sort));
    if query.reverse {
        tasks.reverse();
    }

    Ok(ListOutcome {
        tasks,
        warnings: session.warnings,
    })
}

/// A filter operator. `=`/`!=` compare the field's value against a JSON-coerced
/// query; `=~`/`!~` match a regex against the field's string form; `>`/`>=`/`<`/
/// `<=` order it against the query (see [`Matcher::Cmp`]).
#[derive(Clone, Copy)]
enum FilterOp {
    Eq,
    Ne,
    Re,
    NotRe,
    Gt,
    Ge,
    Lt,
    Le,
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
    /// An ordering comparison (`op` ∈ `Gt`/`Ge`/`Lt`/`Le`) of the field's value
    /// against the JSON-coerced query, via the shared [`cmp_json`] order. The
    /// comparison only holds when the two share a comparable type (both numbers,
    /// strings, or bools) — a cross-type compare never matches.
    Cmp(FilterOp, Value),
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
    /// candidate values (absent→none, a scalar→one, a multi-valued field — a
    /// set/array, `deps`, a relationship type — one per element). The positive
    /// forms (`=`/`=~`/comparisons) pass if ANY candidate matches; the negated
    /// forms (`!=`/`!~`) are their logical NOT, so they pass when NONE does — and
    /// thus also when the field is empty or absent.
    fn matches(&self, task: &TaskState, ctx: &FilterCtx) -> bool {
        let values = field_values(task, &self.field, ctx);
        match &self.matcher {
            Matcher::Eq(q) => values.iter().any(|v| v == q),
            Matcher::Ne(q) => !values.iter().any(|v| v == q),
            Matcher::Re(re) => values.iter().any(|v| re.is_match(&value_string(v))),
            Matcher::NotRe(re) => !values.iter().any(|v| re.is_match(&value_string(v))),
            Matcher::Cmp(op, q) => values.iter().any(|v| cmp_holds(*op, v, q)),
        }
    }
}

/// Whether a single candidate `v <op> q` holds under the shared [`cmp_json`]
/// order. Comparisons are SCALAR: `v` and the query compare just when both are
/// numbers, both strings, or both bools; any other pairing (a cross-type
/// number-vs-string, or a non-scalar element) yields no match rather than ranking
/// by type. A multi-valued field is compared element by element (see
/// [`field_values`]), so `scores>=5` holds when ANY member does.
///
/// Within a type it's [`cmp_json`], so strings/dates order lexicographically for
/// free (RFC 3339 timestamps sort chronologically). There are no negated forms:
/// `<=` is the negation of `>`.
fn cmp_holds(op: FilterOp, v: &Value, q: &Value) -> bool {
    let comparable = matches!(
        (v, q),
        (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Bool(_), Value::Bool(_))
    );
    if !comparable {
        return false;
    }
    let ord = cmp_json(v, q);
    match op {
        FilterOp::Gt => ord == Ordering::Greater,
        FilterOp::Ge => ord != Ordering::Less,
        FilterOp::Lt => ord == Ordering::Less,
        FilterOp::Le => ord != Ordering::Greater,
        // Non-comparison ops never build a `Matcher::Cmp`.
        FilterOp::Eq | FilterOp::Ne | FilterOp::Re | FilterOp::NotRe => false,
    }
}

/// The JSON value(s) a field offers for matching: the `id`; each relationship
/// target under ANY type (`deps`); a declared relationship type or inverse name
/// (the edge targets of that type, resp. the tasks whose edge of that type points
/// here — a symmetric type is both, so both directions union); or a custom
/// field's value — flattened to one candidate PER ELEMENT when it's an array
/// (a `set`/`array` field), the whole value when it's a scalar, none when absent.
/// So every multi-valued field (custom array, `deps`, relationship type) matches
/// element-wise alike. Relationship names are reserved as field names, so the
/// dispatch is unambiguous.
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
                return match task.custom_fields.get(field) {
                    Some(Value::Array(items)) => items.clone(),
                    Some(value) => vec![value.clone()],
                    None => Vec::new(),
                };
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
/// string, else its compact JSON (so `priority=~^3$` can match the number 3).
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
        FilterOp::Gt | FilterOp::Ge | FilterOp::Lt | FilterOp::Le => {
            Matcher::Cmp(op, json_or_string(value))
        }
    };
    Ok(Criterion {
        field: field.to_string(),
        matcher,
    })
}

/// Split `field<op>value` at its FIRST operator, so an operator character inside
/// the value (e.g. a regex's `~`) doesn't fool the parser. The regex match is
/// `=~` (perl/bash spelling), its negation `!~`. `=`/`!`/`>`/`<` peek the next
/// byte for their two-char forms (`=~`, `!=`, `!~`, `>=`, `<=`); a bare `!` or
/// `~` is not an operator.
fn split_criterion(raw: &str) -> Result<(&str, FilterOp, &str), DynError> {
    let bytes = raw.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        let (op, len) = match c {
            b'=' => match bytes.get(i + 1) {
                Some(b'~') => (FilterOp::Re, 2), // `=~` regex match
                _ => (FilterOp::Eq, 1),
            },
            b'>' => match bytes.get(i + 1) {
                Some(b'=') => (FilterOp::Ge, 2),
                _ => (FilterOp::Gt, 1),
            },
            b'<' => match bytes.get(i + 1) {
                Some(b'=') => (FilterOp::Le, 2),
                _ => (FilterOp::Lt, 1),
            },
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
    // A bare `~` was the old regex spelling — point it at `=~`.
    if raw.contains('~') {
        return Err(format!(
            "invalid criterion `{raw}`: the regex match operator is `=~` (e.g. `field=~regex`), its negation `!~`; a bare `~` is not an operator"
        )
        .into());
    }
    Err(format!(
        "invalid criterion `{raw}`: expected field=value, field=~regex, field!=value, field!~regex, or a comparison (field>value, field>=value, field<value, field<=value)"
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
        assert!(matches(r"status=~^op"), "regex on string");
        assert!(matches(r"priority=~^3$"), "regex on number's string form");
        assert!(matches("status!=closed"));
        assert!(!matches("status!~^op"));

        // The regex operator is `=~` (perl/bash spelling), its negation `!~`.
        assert!(matches!(
            split_criterion("status=~^op").unwrap().1,
            FilterOp::Re
        ));
        assert!(matches!(
            split_criterion("status!~^op").unwrap().1,
            FilterOp::NotRe
        ));
        // A bare `~` is no longer an operator (it was the old spelling).
        assert!(compile_criterion("status~^op").is_err(), "bare ~ rejected");
        // `=`/`!=` still parse as themselves when no `~` follows.
        assert!(matches!(
            split_criterion("status=open").unwrap().1,
            FilterOp::Eq
        ));
        assert!(matches!(
            split_criterion("status!=open").unwrap().1,
            FilterOp::Ne
        ));

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
        assert!(compile_criterion("title=~[").is_err());

        // The first operator wins, so a regex value may contain operators.
        let (field, _, value) = split_criterion("title=~a=b").unwrap();
        assert_eq!((field, value), ("title", "a=b"));
    }

    #[test]
    fn comparison_operators_compile_and_match() {
        let t = task(
            "api",
            &[],
            &[
                ("priority", serde_json::json!(3)),
                ("title", serde_json::json!("api server")),
                ("created", serde_json::json!("2026-06-05")),
                // A composite (set/array) field value — one whole candidate.
                ("scores", serde_json::json!([3, 8])),
            ],
        );
        let types = crate::config::RelationshipConfig::default().types;
        let ctx = FilterCtx {
            types: &types,
            rev: None,
        };
        let matches = |s: &str| compile_criterion(s).unwrap().matches(&t, &ctx);

        // Two-char vs one-char operators parse at the first operator byte.
        assert!(matches!(
            split_criterion("priority>=4").unwrap().1,
            FilterOp::Ge
        ));
        assert!(matches!(
            split_criterion("priority>4").unwrap().1,
            FilterOp::Gt
        ));
        assert!(matches!(
            split_criterion("priority<=4").unwrap().1,
            FilterOp::Le
        ));
        assert!(matches!(
            split_criterion("priority<4").unwrap().1,
            FilterOp::Lt
        ));

        // Numeric comparison (not lexical): 3 < 10, so `priority<10` holds but
        // a string compare would say "3" > "10".
        assert!(matches("priority>2"));
        assert!(!matches("priority>3"), "strict >, equal value excluded");
        assert!(matches("priority>=3"), ">= includes the boundary");
        assert!(matches("priority<10"), "numeric, not lexical");
        assert!(matches("priority<=3"));
        assert!(!matches("priority<3"));

        // A cross-type compare never matches (number field vs string query, and
        // vice versa) — rather than ranking number-before-string.
        assert!(!matches("priority>=x"), "number field vs string query");
        assert!(!matches("title>=3"), "string field vs number query");

        // String/date fields order lexicographically — which is chronological for
        // RFC 3339 / ISO dates.
        assert!(matches("created>=2026-06-01"));
        assert!(matches("created<2026-07-01"));
        assert!(!matches("created>=2026-07-01"));
        assert!(matches("title>=ant"), "lexical string order");

        // An absent field offers no candidates, so every comparison is false.
        assert!(!matches("missing>0"));
        assert!(!matches("missing<=0"));

        // A set/array field compares element-wise: the comparison holds when ANY
        // member does. scores = [3, 8].
        assert!(matches("scores>=5"), "8 >= 5");
        assert!(matches("scores>1"), "both members exceed 1");
        assert!(matches("scores<100"));
        assert!(!matches("scores>10"), "no member > 10");
        assert!(!matches("scores<2"), "no member < 2");
    }

    #[test]
    fn multivalued_fields_match_any_element() {
        // A custom array/set field flattens to one candidate per element, so every
        // operator works on membership — uniformly with `deps`/relationship fields.
        let t = task(
            "api",
            &[],
            &[
                ("tags", serde_json::json!(["urgent", "backend"])),
                ("scores", serde_json::json!([3, 8])),
            ],
        );
        let types = crate::config::RelationshipConfig::default().types;
        let ctx = FilterCtx {
            types: &types,
            rev: None,
        };
        let m = |s: &str| compile_criterion(s).unwrap().matches(&t, &ctx);

        // `=` is membership; `!=` holds when NOT a member (and when absent).
        assert!(m("tags=urgent"));
        assert!(m("tags=backend"));
        assert!(!m("tags=frontend"));
        assert!(m("tags!=frontend"), "frontend is not a member");
        assert!(!m("tags!=urgent"), "urgent IS a member");
        assert!(m("missing!=anything"), "absent field passes negation");

        // Regex runs per element: anchors bind to a member, not the JSON blob.
        assert!(m(r"tags=~^urgent$"));
        assert!(!m(r"tags=~,"), "no member contains the serialization comma");

        // Numeric comparison holds when any member qualifies.
        assert!(m("scores>=8"));
        assert!(!m("scores>8"));
        assert!(m("scores<5"), "3 < 5");
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
        assert!(matches(&child, r"depends_on=~^li"), "regex over targets");

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
