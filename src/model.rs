//! Domain model: the event schema and the materialized task shape.
//!
//! Pure data with no knowledge of how it is stored, replayed, or displayed.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The canonical name of the default blocker relationship.
///
/// A declared relationship type stored in [`TaskState::relationships`] like any
/// other (read via [`TaskState::depends_on`], surfaced as the `deps` column, and
/// what the readiness gate walks). Defined once here so the string lives in a
/// single place: internal logic compares against this constant, and the literal
/// text surfaces only at parse / serialize / print boundaries.
pub const DEPENDS_ON: &str = "depends_on";

/// Payload key for the dependency target id in `AddDep`/`RemoveDep` events.
///
/// Together with [`DEP_TYPE_KEY`] this is an on-disk event-schema contract: every
/// reader (engine, merge) and writer (the `dep`/`undo` commands, merge
/// resolutions) must agree, so the strings are named once here.
pub const DEP_KEY: &str = "dep";

/// Payload key for a dependency edge's relationship type (absent = the default
/// [`DEPENDS_ON`]). See [`DEP_KEY`].
pub const DEP_TYPE_KEY: &str = "type";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OpType {
    Create,
    Update,
    /// Append text to a field (one entry per line) instead of overwriting it.
    /// Unlike `Update`, concurrent `Append`s to the same field commute — replay
    /// concatenates them in `seq` order — so a running notes/comments log
    /// accumulates conflict-free across branches.
    Append,
    Delete,
    AddDep,
    RemoveDep,
}

/// A single append-only record in the mutation log.
///
/// `seq` is a per-store autoincrement assigned by the store at append time. It
/// is the *authoritative order* — replay, compaction, and merge all key off it,
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

    /// Optional, non-materialized annotation — currently merge provenance written
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
/// events leaves gaps in the sequence, and that is a normal, supported state —
/// only an out-of-order or duplicate `seq` is corruption.
///
/// Every write path — append, compaction, and merge restack — produces
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
#[serde(from = "TaskStateRepr")]
pub struct TaskState {
    pub id: String,

    /// Typed relationship edges, `type name → target ids` — including the default
    /// blocker [`DEPENDS_ON`] (read via [`TaskState::depends_on`], surfaced as the
    /// `deps` column, and what the readiness gate walks). Every declared type
    /// (`depends_on`, `relates_to`, `blocks`, `duplicates`, …) lives here, so the
    /// engine and graph treat them uniformly. `skip_serializing_if` keeps it off
    /// the line for a task with no edges.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relationships: BTreeMap<String, Vec<String>>,

    pub custom_fields: Map<String, Value>,

    /// Computed, best-effort timestamps materialized from the event log — never
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

impl TaskState {
    /// The `depends_on` edges — the default blocker relationship. Stored in
    /// `relationships` like every other type; this is the read accessor the `deps`
    /// column and readiness gate use.
    #[must_use]
    pub fn depends_on(&self) -> &[String] {
        self.relationships
            .get(DEPENDS_ON)
            .map_or(&[], Vec::as_slice)
    }
}

/// On-disk shape for *deserializing* [`TaskState`], with backward compatibility:
/// baselines written before `depends_on` was folded into `relationships` carry a
/// top-level `depends_on` field, which this merges into the map. New baselines
/// omit it (it lives in `relationships`), so reads round-trip either way.
#[derive(Deserialize)]
struct TaskStateRepr {
    id: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    relationships: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    custom_fields: Map<String, Value>,
    #[serde(default)]
    create_time: Option<DateTime<Utc>>,
    #[serde(default)]
    update_time: Option<DateTime<Utc>>,
    #[serde(default)]
    close_time: Option<DateTime<Utc>>,
}

impl From<TaskStateRepr> for TaskState {
    fn from(r: TaskStateRepr) -> Self {
        let mut relationships = r.relationships;
        if !r.depends_on.is_empty() {
            let entry = relationships.entry(DEPENDS_ON.to_string()).or_default();
            for dep in r.depends_on {
                if !entry.contains(&dep) {
                    entry.push(dep);
                }
            }
        }
        Self {
            id: r.id,
            relationships,
            custom_fields: r.custom_fields,
            create_time: r.create_time,
            update_time: r.update_time,
            close_time: r.close_time,
        }
    }
}

/// Whether a task counts as done: its `status_field` equals `done_status`.
///
/// A pure predicate over [`TaskState`] (no I/O, no config dependency), so the
/// engine (computing `close_time`), the graph (readiness), and the `status`
/// summary all agree on what "done" means without duplicating the check.
pub fn is_done(task: &TaskState, status_field: &str, done_status: &str) -> bool {
    task.custom_fields.get(status_field).and_then(Value::as_str) == Some(done_status)
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
}
