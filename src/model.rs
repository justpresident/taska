//! Domain model: the event schema and the materialized task shape.
//!
//! Pure data with no knowledge of how it is stored, replayed, or displayed.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Payload key for an edge's target task id in `AddEdge`/`RemoveEdge` events.
///
/// Together with [`REL_KEY`] this is an on-disk event-schema contract: every
/// reader (engine, merge) and writer (the `dep`/`undo` commands, merge
/// resolutions) must agree, so the strings are named once here.
pub const TARGET_KEY: &str = "target";

/// Payload key for an edge's relationship type name. Every edge carries one
/// explicitly. See [`TARGET_KEY`].
pub const REL_KEY: &str = "rel";

/// CANONICAL storage key of the workflow status field.
///
/// Events and the baseline always store the status under this key, regardless
/// of the configured `[workflow] status_field`, which is a DISPLAY name only:
/// commands map the display name to this key on write, and `action::read` surfaces
/// the stored value back under the display name on read. That split is what
/// makes the display name freely renamable in config without touching disk -
/// and lets clones with different display configs merge cleanly.
pub const STATUS_KEY: &str = "status";

/// CANONICAL storage key of the task-type discriminator (the schema feature's
/// field). Same display-vs-storage split as [`STATUS_KEY`]; the configurable
/// display name defaults to `type`.
pub const TASK_TYPE_KEY: &str = "task_type";

/// Built-in display/query column: a task's id.
///
/// Its identity, not a stored field. Part of the column vocabulary every
/// frontend and the filter language understand, so it's named here rather than
/// spelled out at each dispatch site.
pub const ID_KEY: &str = "id";

/// Built-in display/query column: a task's typed relationships map
/// (`{type: [targets...]}`). `deps=x` matches an edge of any type.
pub const DEPS_KEY: &str = "deps";

/// Reserved field name: reads like a dependency, so it's rejected as a field
/// (use `ta dep add`). Not itself a column.
pub const DEP_KEY: &str = "dep";

/// Computed column: how many not-done tasks this one transitively UNBLOCKS.
/// Graph-derived, injected at read time only when a query references it.
pub const UNBLOCKS_KEY: &str = "unblocks";

/// Computed column: how many not-done prerequisites transitively BLOCK this task.
pub const BLOCKED_BY_KEY: &str = "blocked_by";

/// Computed column: a parent's `done/total` direct-child completion.
pub const SUBTASKS_KEY: &str = "subtasks";

/// Field names never legal as user fields under ANY config.
///
/// Rejectable at parse time, before a store or config exists. Two reasons,
/// interleaved in the list: the event-envelope keys (payload fields are
/// serde-flattened next to them on the log line, so a user field would collide
/// with the envelope or be swallowed by `_meta`), and the static
/// computed/injected columns (their value is derived at read time, so a stored
/// field would be silently shadowed; `dep` additionally reads like a
/// dependency - use `ta dep add`). The config-DEPENDENT reserved names
/// (timestamp columns, relationship types and inverses) can't live in a const;
/// `cli::reserved_field_names` unions them in for the write gate, and
/// `Config::validate` checks `[task_types]` field declarations against this
/// same list.
pub const RESERVED_FIELD_KEYS: &[&str] = &[
    // event envelope
    "seq",
    "timestamp",
    "op",
    "task_id",
    "_meta",
    // static built-in + computed/injected columns (the column vocabulary)
    ID_KEY,
    DEPS_KEY,
    DEP_KEY,
    UNBLOCKS_KEY,
    BLOCKED_BY_KEY,
    SUBTASKS_KEY,
];

/// A total order over heterogeneous JSON scalars.
///
/// Numbers compare numerically, strings/bools by their natural order, and any
/// mismatch falls back to a stable per-type rank then the value's string form -
/// so mixed types still order deterministically. Shared by display sorting
/// (`--sort`, filters) and the engine's canonical set form, so replay and
/// presentation agree on order.
#[must_use]
pub fn cmp_json(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => value_rank(a)
            .cmp(&value_rank(b))
            .then_with(|| a.to_string().cmp(&b.to_string())),
    }
}

/// Stable per-type ordinal so values of different JSON types compare consistently.
const fn value_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

/// An edge event's target id.
#[must_use]
pub fn edge_target(payload: &Map<String, Value>) -> Option<&str> {
    payload.get(TARGET_KEY).and_then(Value::as_str)
}

/// An edge event's relationship type name. Every edge carries one; `None` means a
/// malformed event (replay skips it).
#[must_use]
pub fn edge_rel(payload: &Map<String, Value>) -> Option<&str> {
    payload.get(REL_KEY).and_then(Value::as_str)
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OpType {
    Create,
    Update,
    /// Append text to a field (one entry per line) instead of overwriting it.
    /// Unlike `Update`, concurrent `Append`s to the same field commute - replay
    /// concatenates them in `seq` order - so a running notes/comments log
    /// accumulates conflict-free across branches. FROZEN as text accumulation:
    /// numeric/set `+=` is [`OpType::Add`], so old logs never re-materialize
    /// differently across versions.
    Append,
    /// Accumulate into a field (`+=` on numeric and set fields), with
    /// kind-dispatched, config-free replay semantics defined from birth:
    /// number onto number (or onto a missing field, as 0) adds arithmetically;
    /// an ARRAY operand inserts its elements set-style - deduped, kept in the
    /// canonical sorted order - so concurrent adds commute like `Append`;
    /// any other shape is a deterministic no-op. The command layer emits this
    /// only for declared numeric/set fields; scalars destined for a set are
    /// lifted to singleton arrays so the set path is unambiguous.
    Add,
    /// The inverse of [`OpType::Add`] (`-=`): numbers subtract (a missing
    /// field counts as 0), an array operand removes its elements from a set
    /// (absent elements are a no-op); any other shape is a no-op.
    Remove,
    Delete,
    /// Add a typed relationship edge (`target` + `rel` payload keys).
    AddEdge,
    /// Remove a typed relationship edge.
    RemoveEdge,
}

/// A single append-only record in the mutation log.
///
/// `seq` is a per-store autoincrement assigned by the store at append time. It
/// is the *authoritative order* - replay, compaction, and merge all key off it,
/// never off the wall clock. It survives compaction: a folded baseline stands in
/// for every `seq` up to a watermark, so events can never be reordered relative
/// to the baseline. Sequences start at 1; `0` is the "unassigned draft" sentinel
/// a value carries between construction and the store appending it.
///
/// `timestamp` is informational only (for display such as "created 3 days ago").
/// It is deliberately *not* used to order or merge events, because wall clocks
/// interleave arbitrarily across concurrent branches.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MutationEvent {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub op: OpType,
    pub task_id: String,

    /// Optional, non-materialized annotation - currently merge provenance written
    /// by the merge driver (which fields it resolved, the values it chose between,
    /// and the strategy used). Replay ignores it, so it never reaches a task's
    /// state and is dropped when its event folds into the baseline at compaction.
    /// User commands never set it; the leading `_` marks the key reserved.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,

    // Catch-all for schema-agnostic field management.
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

impl MutationEvent {
    /// Build an unsequenced draft. The store assigns `seq` when it appends, so
    /// the returned value's `seq` is `0` until then.
    pub fn new(op: OpType, task_id: impl Into<String>, payload: Map<String, Value>) -> Self {
        Self {
            seq: 0,
            timestamp: Utc::now(),
            op,
            task_id: task_id.into(),
            meta: None,
            payload,
        }
    }
}

/// Verify a log slice is strictly increasing by `seq`.
///
/// Strictly increasing, *not* contiguous: a `git revert` that drops committed
/// events leaves gaps in the sequence, and that is a normal, supported state -
/// only an out-of-order or duplicate `seq` is corruption.
///
/// Every write path - append, compaction, and merge restack - produces
/// strictly-ordered output, so a violation is never a normal state: it means the
/// log was hand-edited, merged by the wrong tool, or corrupted. We surface it
/// loudly instead of silently repairing it, so the user can investigate rather
/// than trust a quietly reordered history.
pub fn verify_seq_order(events: &[MutationEvent]) -> Result<(), String> {
    for pair in events.windows(2) {
        if pair[1].seq <= pair[0].seq {
            return Err(format!(
                "event log out of order: seq {} appears after seq {}. The log must be strictly \
                 increasing by seq; it looks hand-edited or corrupted. Inspect it before continuing.",
                pair[1].seq, pair[0].seq
            ));
        }
    }
    Ok(())
}

/// The materialized final state of a single task (lives only in memory, or as a
/// compacted baseline record).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub id: String,

    /// Typed relationship edges, `type name -> target ids` - including the default
    /// blocker (conventionally `depends_on`). This whole map IS the `deps` column
    /// (grouped by type), and the readiness gate walks its blocker-kind entries.
    /// Every declared type (`depends_on`, `relates_to`, `blocks`, `duplicates`, ...)
    /// lives here, so the engine and graph treat them uniformly. No type name is
    /// privileged in code; the set is whatever `[relationships]` declares.
    /// `skip_serializing_if` keeps it off the line for a task with no edges.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relationships: BTreeMap<String, Vec<String>>,

    #[serde(default)]
    pub custom_fields: Map<String, Value>,

    /// Computed, best-effort timestamps materialized from the event log - never
    /// user-set. They are persisted into the baseline so they survive
    /// compaction (their source events get folded away), then extended on each
    /// replay. Best-effort because event timestamps are informational only
    /// (`seq` is the authoritative order), so after a merge restacks another
    /// branch's events these can be non-monotonic. `create_time` is the first
    /// `Create`'s timestamp; `update_time` the latest touching event's;
    /// `close_time` the most recent transition of `status` into `done_status`,
    /// cleared whenever the task is currently not done. `#[serde(default,
    /// skip_serializing_if)]` keeps old baselines readable and unset times out
    /// of the serialized line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_time: Option<DateTime<Utc>>,
}

/// Whether a task counts as done: its `status_field` equals `done_status`.
///
/// A pure predicate over [`TaskState`] (no I/O, no config dependency), so the
/// engine (computing `close_time`), the graph (readiness), and the `status`
/// summary all agree on what "done" means without duplicating the check.
pub fn is_done(task: &TaskState, status_field: &str, done_status: &str) -> bool {
    task.custom_fields.get(status_field).and_then(Value::as_str) == Some(done_status)
}

/// The value of `column` for a task as a JSON `Value` - the single source of
/// truth shared by JSON output, human rendering, sorting, and filtering.
///
/// `id` is the id string, `deps` the task's typed relationships map
/// (`{type: [targets...]}` - every edge keyed by relationship type, `{}` when
/// none), and anything else a custom or computed field. `None` only for a
/// missing custom field (the built-ins always resolve), which is how JSON omits
/// absent fields and sorting orders them last.
#[must_use]
pub fn cell_value(task: &TaskState, column: &str) -> Option<Value> {
    match column {
        ID_KEY => Some(Value::String(task.id.clone())),
        DEPS_KEY => Some(Value::Object(
            task.relationships
                .iter()
                .map(|(rel, targets)| {
                    let arr = targets.iter().cloned().map(Value::String).collect();
                    (rel.clone(), Value::Array(arr))
                })
                .collect(),
        )),
        _ => task.custom_fields.get(column).cloned(),
    }
}

/// Compare two tasks by one `column`, ascending, with `id` as the stable
/// tiebreaker - a present value sorts before a missing one.
///
/// The shared ordering behind `list`'s `--sort` and `dep tree`'s sibling sort
/// (both keyed off [`cell_value`] and [`cmp_json`]); `--reverse` flips the
/// result at the call site.
#[must_use]
pub fn task_cmp(a: &TaskState, b: &TaskState, column: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ord = match (cell_value(a, column), cell_value(b, column)) {
        (Some(x), Some(y)) => cmp_json(&x, &y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    ord.then_with(|| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seq: u64) -> MutationEvent {
        let mut e = MutationEvent::new(OpType::Create, "t", Map::new());
        e.seq = seq;
        e
    }

    #[test]
    fn verify_seq_order_accepts_strictly_increasing() {
        assert!(verify_seq_order(&[]).is_ok());
        assert!(verify_seq_order(&[at(1)]).is_ok());
        assert!(verify_seq_order(&[at(1), at(2), at(5)]).is_ok());
    }

    #[test]
    fn verify_seq_order_rejects_disorder_and_duplicates() {
        assert!(verify_seq_order(&[at(2), at(1)]).is_err(), "out of order");
        assert!(verify_seq_order(&[at(1), at(1)]).is_err(), "duplicate seq");
    }

    #[test]
    fn cmp_json_orders_numbers_strings_and_mixed_types() {
        use serde_json::json;
        use std::cmp::Ordering;
        assert_eq!(
            cmp_json(&json!(2), &json!(10)),
            Ordering::Less,
            "numeric, not lexical"
        );
        assert_eq!(cmp_json(&json!("a"), &json!("b")), Ordering::Less);
        // Mixed types fall back to a stable per-type rank (number < string).
        assert_eq!(cmp_json(&json!(1), &json!("1")), Ordering::Less);
    }

    #[test]
    fn cell_value_projects_columns_and_task_cmp_orders_by_them() {
        use crate::test_support::names::*;
        use crate::test_support::{task, task_rel};
        use serde_json::json;
        use std::cmp::Ordering;

        let mut t = task_rel("api", BLOCKER, &["db", "web"], &[("priority", json!(3))]);
        t.relationships
            .insert(INFO.to_string(), vec!["infra".to_string()]);

        // cell_value: id string, deps as the typed relationships map, custom
        // passthrough, and None for a field the task lacks.
        assert_eq!(cell_value(&t, ID_KEY), Some(json!("api")));
        assert_eq!(
            cell_value(&t, DEPS_KEY),
            Some(json!({BLOCKER: ["db", "web"], INFO: ["infra"]}))
        );
        assert_eq!(cell_value(&t, "priority"), Some(json!(3)));
        assert_eq!(cell_value(&t, "missing"), None);

        // task_cmp: numeric (not lexical) order on the column; a present value
        // sorts before a missing one; equal columns fall back to the id.
        let lo = task("z", &[], &[("priority", json!(2))]);
        let hi = task("a", &[], &[("priority", json!(10))]);
        assert_eq!(
            task_cmp(&lo, &hi, "priority"),
            Ordering::Less,
            "2 < 10 numerically, ignoring ids"
        );
        let none = task("b", &[], &[]);
        assert_eq!(
            task_cmp(&hi, &none, "priority"),
            Ordering::Less,
            "present sorts before missing"
        );
        let a2 = task("a", &[], &[("priority", json!(2))]);
        let z2 = task("z", &[], &[("priority", json!(2))]);
        assert_eq!(
            task_cmp(&a2, &z2, "priority"),
            Ordering::Less,
            "equal column -> id tiebreak"
        );

        // Driving a whole-vector sort: ascending with the missing-value task
        // last, an unknown column collapsing to the id tiebreak, and reverse as
        // a plain flip on top - the policy `list`/`dep tree` apply.
        let pri3 = task("a", &[], &[("priority", json!(3))]);
        let pri1 = task("b", &[], &[("priority", json!(1))]);
        let pri2 = task("c", &[], &[("priority", json!(2))]);
        let none = task("d", &[], &[]);
        let ids = |v: &[&TaskState]| -> Vec<String> { v.iter().map(|t| t.id.clone()).collect() };

        let mut list = vec![&pri3, &pri1, &pri2, &none];
        list.sort_by(|a, b| task_cmp(a, b, "priority"));
        assert_eq!(ids(&list), ["b", "c", "a", "d"], "asc, missing last");
        list.reverse();
        assert_eq!(ids(&list), ["d", "a", "c", "b"], "reversed");

        let mut unknown = vec![&pri2, &pri3, &pri1];
        unknown.sort_by(|a, b| task_cmp(a, b, "nope"));
        assert_eq!(ids(&unknown), ["a", "b", "c"], "unknown column -> by id");
    }
}
