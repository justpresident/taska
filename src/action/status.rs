//! `status` action: aggregate task counts.
//!
//! Buckets every task by its (user-defined) status, then computes the
//! `ready`/`blocked`/`closed` figures from the dependency graph - returning a
//! typed [`StatusSummary`] the frontend renders however it likes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::BuildHasher;

use serde_json::Value;

use crate::action::{read, Warning};
use crate::config::WorkflowConfig;
use crate::error::DynError;
use crate::graph;
use crate::model::{is_done, TaskState};
use crate::storage::EventStore;

/// Aggregate counts for `status`.
///
/// Status values are user-defined, so the per-status buckets are *discovered*
/// from the data rather than hardcoded - `done_status` is simply the bucket that
/// also feeds the `closed` count. `blocked` and `ready` are COMPUTED from the
/// dependency graph, never read from a status value: `ready` reuses the same set
/// as `list --ready`, and among not-done tasks the two partition the set (a
/// not-done task is blocked iff an existing dependency isn't done, else ready).
pub struct StatusSummary {
    pub total: usize,
    pub by_status: BTreeMap<String, usize>,
    pub no_status: usize,
    pub ready: usize,
    pub blocked: usize,
    pub closed: usize,
}

/// A `status` read: the summary, the log's high-water `seq` (the cursor the state
/// is as-of), plus any read [`Warning`]s.
pub struct StatusOutcome {
    pub summary: StatusSummary,
    pub seq: u64,
    pub warnings: Vec<Warning>,
}

/// Materialize the store and summarize it.
pub fn status(store: &impl EventStore) -> Result<StatusOutcome, DynError> {
    let session = read(store)?;
    let blockers = store.config().relationships.blocker_types();
    let summary = status_summary(&session.state, &store.config().workflow, &blockers)?;
    Ok(StatusOutcome {
        summary,
        seq: session.seq,
        warnings: session.warnings,
    })
}

/// Summarize an already-materialized state.
///
/// The pure computation, independent of any store, so a frontend holding its own
/// state can reuse it directly.
pub fn status_summary<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    workflow: &WorkflowConfig,
    blockers: &BTreeSet<String>,
) -> Result<StatusSummary, DynError> {
    let (field, done) = (&workflow.status_field, &workflow.done_status);
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut no_status = 0usize;
    for t in state.values() {
        match t.custom_fields.get(field) {
            Some(Value::String(s)) => *by_status.entry(s.clone()).or_default() += 1,
            // A non-string status still groups, keyed by its compact JSON form.
            Some(v) => {
                *by_status
                    .entry(serde_json::to_string(v).unwrap_or_default())
                    .or_default() += 1;
            }
            None => no_status += 1,
        }
    }
    let ready = graph::ready_tasks(state, field, done, blockers)?.len();
    let closed = state.values().filter(|t| is_done(t, field, done)).count();
    let blocked = state
        .values()
        .filter(|t| {
            !is_done(t, field, done)
                && graph::blocker_edges(t, blockers)
                    .into_iter()
                    .any(|(d, _)| state.get(d).is_some_and(|dep| !is_done(dep, field, done)))
        })
        .count();
    Ok(StatusSummary {
        total: state.len(),
        by_status,
        no_status,
        ready,
        blocked,
        closed,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::names::*;
    use crate::test_support::{renamed_config, state, task_rel};

    #[test]
    fn status_summary_counts_and_partitions_ready_blocked() {
        let workflow = renamed_config().workflow; // state / closed
        let tasks = vec![
            task_rel(
                "a",
                BLOCKER,
                &[],
                &[(STATUS_FIELD, serde_json::json!("todo"))],
            ), // ready (no deps)
            task_rel(
                "b",
                BLOCKER,
                &["a"],
                &[(STATUS_FIELD, serde_json::json!("todo"))],
            ), // blocked by a
            task_rel(
                "c",
                BLOCKER,
                &[],
                &[(STATUS_FIELD, serde_json::json!("closed"))],
            ), // done
            task_rel(
                "d",
                BLOCKER,
                &["c"],
                &[(STATUS_FIELD, serde_json::json!("todo"))],
            ), // ready (dep done)
            task_rel("e", BLOCKER, &[], &[]), // no status -> ready
        ];
        let st = state(&tasks);
        let blockers = BTreeSet::from([BLOCKER.to_string()]);
        let s = status_summary(&st, &workflow, &blockers).unwrap();

        assert_eq!(s.total, 5);
        assert_eq!(s.by_status.get("todo"), Some(&3));
        assert_eq!(s.by_status.get("closed"), Some(&1));
        assert_eq!(s.no_status, 1);
        assert_eq!(s.closed, 1, "one done task");
        assert_eq!(s.ready, 3, "a, d, e");
        assert_eq!(s.blocked, 1, "b");
        // Among not-done tasks, ready and blocked partition the set.
        let not_done = s.total - s.closed;
        assert_eq!(s.ready + s.blocked, not_done);
    }
}
