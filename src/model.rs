//! Domain model: the event schema and the materialized task shape.
//!
//! Pure data with no knowledge of how it is stored, replayed, or displayed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OpType {
    Create,
    Update,
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
pub struct TaskState {
    pub id: String,
    pub depends_on: Vec<String>,
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
