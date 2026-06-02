//! State materialization (the overlay engine) and field search.
//!
//! A pure replay algorithm: it folds a mutation log over a baseline snapshot
//! and knows nothing about where either came from. Keeping it free of storage
//! dependencies makes it trivially testable and reusable.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::model::{MutationEvent, OpType, TaskState};

pub struct Engine;

impl Engine {
    /// Fold `mutations` over `baseline` to produce the current task map.
    ///
    /// Thin wrapper over [`Engine::materialize_report`] that discards the orphan
    /// report, for callers that only need the state.
    pub fn materialize_state(
        baseline: Vec<TaskState>,
        mutations: Vec<MutationEvent>,
    ) -> HashMap<String, TaskState> {
        Self::materialize_report(baseline, mutations).0
    }

    /// Like [`Engine::materialize_state`], but also reports *orphaned* events:
    /// those whose target task did not exist when the event was applied, so the
    /// event folded into nothing. These are `Update`/`AddDep`/`RemoveDep`/`Delete`
    /// events on a `task_id` absent from the state map at apply time (`Create` is
    /// never an orphan). The returned `Vec<u64>` holds their `seq`s in replay order.
    ///
    /// Replay stays non-fatal: orphans are merely counted, never errored. They can
    /// arise from the merge driver's removal-union, reverts, or manual edits that
    /// drop a task's `Create` while leaving later events that target it.
    pub fn materialize_report(
        baseline: Vec<TaskState>,
        mutations: Vec<MutationEvent>,
    ) -> (HashMap<String, TaskState>, Vec<u64>) {
        let mut state_map: HashMap<String, TaskState> =
            baseline.into_iter().map(|t| (t.id.clone(), t)).collect();
        let mut orphans: Vec<u64> = Vec::new();

        for event in mutations {
            match event.op {
                OpType::Create => {
                    // Re-creating an existing id refreshes its fields but keeps
                    // any deps already attached.
                    let entry =
                        state_map
                            .entry(event.task_id.clone())
                            .or_insert_with(|| TaskState {
                                id: event.task_id.clone(),
                                depends_on: Vec::new(),
                                custom_fields: serde_json::Map::new(),
                            });
                    for (k, v) in event.payload {
                        // A null value unsets the field (the field-unset convention),
                        // so it never reaches state, output, search, or the baseline.
                        if v.is_null() {
                            entry.custom_fields.remove(&k);
                        } else {
                            entry.custom_fields.insert(k, v);
                        }
                    }
                }
                OpType::Update => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        for (k, v) in event.payload {
                            // A null value unsets the field (see Create above).
                            if v.is_null() {
                                task.custom_fields.remove(&k);
                            } else {
                                task.custom_fields.insert(k, v);
                            }
                        }
                    } else {
                        orphans.push(event.seq);
                    }
                }
                OpType::AddDep => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        if let Some(dep_id) = event.payload.get("dep").and_then(|v| v.as_str()) {
                            let dep_id = dep_id.to_string();
                            if !task.depends_on.contains(&dep_id) {
                                task.depends_on.push(dep_id);
                            }
                        }
                    } else {
                        orphans.push(event.seq);
                    }
                }
                OpType::RemoveDep => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        if let Some(dep_id) = event.payload.get("dep").and_then(|v| v.as_str()) {
                            task.depends_on.retain(|d| d != dep_id);
                        }
                    } else {
                        orphans.push(event.seq);
                    }
                }
                OpType::Delete => {
                    if state_map.remove(&event.task_id).is_none() {
                        orphans.push(event.seq);
                    }
                }
            }
        }
        (state_map, orphans)
    }

    /// Split point for the seq-ordered mutation log: events at indices
    /// `[0, split)` are old enough to fold into the baseline, and `[split, len)`
    /// are retained in the log.
    ///
    /// An event is retained if it is within the most recent `keep_events` **or**
    /// newer than `keep_days` (0 disables the time window) — kept if either rule
    /// says so. The count rule yields a seq-suffix directly; the time rule folds
    /// only up to the first event newer than the window, so no recent event is
    /// ever folded even if timestamps are non-monotonic along the seq order (as
    /// they are after a merge restacks another branch's events). The smaller fold
    /// index keeps the union of what each rule wants to retain.
    ///
    /// The result is clamped so it never folds the *last* event: the log must
    /// stay non-empty so the compaction watermark `W = min(seq) - 1` remains
    /// derivable (an empty log is indistinguishable from a fresh store).
    pub fn retention_split(
        mutations: &[MutationEvent],
        keep_events: usize,
        keep_days: u64,
        now: DateTime<Utc>,
    ) -> usize {
        let n = mutations.len();
        let by_count = n.saturating_sub(keep_events);
        let by_time = if keep_days == 0 {
            n // time window disabled: it never forces retention
        } else {
            // Clamp absurdly large windows to `i64::MAX` days rather than wrapping;
            // either way the cutoff is far in the past and nothing is folded by time.
            let days = i64::try_from(keep_days).unwrap_or(i64::MAX);
            let cutoff = now - Duration::days(days);
            mutations
                .iter()
                .position(|e| e.timestamp >= cutoff)
                .unwrap_or(n)
        };
        // Never fold the final event, so the log can't be emptied.
        by_count.min(by_time).min(n.saturating_sub(1))
    }

    /// Return tasks whose `key` field exactly equals `val`.
    pub fn filter_tasks<'a>(
        state: &'a HashMap<String, TaskState>,
        key: &str,
        val: &Value,
    ) -> Vec<&'a TaskState> {
        state
            .values()
            .filter(|t| t.custom_fields.get(key) == Some(val))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(op: OpType, id: &str, payload: serde_json::Map<String, Value>) -> MutationEvent {
        MutationEvent::new(op, id, payload)
    }

    fn fields(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn replays_create_update_dep_and_delete() {
        let mutations = vec![
            ev(OpType::Create, "a", fields(&[("status", json!("open"))])),
            ev(OpType::Create, "b", serde_json::Map::new()),
            ev(OpType::Update, "a", fields(&[("status", json!("done"))])),
            ev(OpType::AddDep, "b", fields(&[("dep", json!("a"))])),
            ev(OpType::Create, "c", serde_json::Map::new()),
            ev(OpType::Delete, "c", serde_json::Map::new()),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations);

        assert_eq!(state.len(), 2, "c was deleted");
        assert_eq!(
            state["a"].custom_fields["status"],
            json!("done"),
            "update overwrote create"
        );
        assert_eq!(state["b"].depends_on, vec!["a".to_string()]);
    }

    #[test]
    fn mutations_overlay_the_baseline() {
        let baseline = vec![TaskState {
            id: "a".into(),
            depends_on: vec!["x".into()],
            custom_fields: fields(&[("status", json!("open"))]),
        }];
        let mutations = vec![
            ev(OpType::Update, "a", fields(&[("status", json!("done"))])),
            ev(OpType::RemoveDep, "a", fields(&[("dep", json!("x"))])),
        ];
        let state = Engine::materialize_state(baseline, mutations);

        assert_eq!(state["a"].custom_fields["status"], json!("done"));
        assert!(
            state["a"].depends_on.is_empty(),
            "dep removed from baseline task"
        );
    }

    #[test]
    fn reports_orphaned_events_and_spares_normal_ones() {
        let mutations = vec![
            // Normal create/update on `a`: never an orphan.
            ev(OpType::Create, "a", fields(&[("status", json!("open"))])),
            ev(OpType::Update, "a", fields(&[("status", json!("done"))])),
            // Update to a task that was never created: orphan.
            ev(OpType::Update, "ghost", fields(&[("x", json!(1))])),
            // `b` is created then deleted...
            ev(OpType::Create, "b", serde_json::Map::new()),
            ev(OpType::Delete, "b", serde_json::Map::new()),
            // ...so events after its deletion apply to nothing: orphans.
            ev(OpType::AddDep, "b", fields(&[("dep", json!("a"))])),
            ev(OpType::Delete, "b", serde_json::Map::new()),
        ];
        // Assign seqs so the report identifies events by their authoritative order.
        let mutations: Vec<MutationEvent> = (1u64..)
            .zip(mutations)
            .map(|(seq, mut e)| {
                e.seq = seq;
                e
            })
            .collect();

        let (state, orphans) = Engine::materialize_report(Vec::new(), mutations);

        assert_eq!(state.len(), 1, "only `a` survives");
        assert_eq!(state["a"].custom_fields["status"], json!("done"));
        // Orphans: the ghost Update (seq 3), the AddDep (seq 6) and Delete (seq 7)
        // after `b` was deleted, in replay order. The normal create/update and
        // the first delete of an existing task are not reported.
        assert_eq!(orphans, vec![3, 6, 7]);
    }

    #[test]
    fn add_dep_is_idempotent() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            ev(OpType::AddDep, "a", fields(&[("dep", json!("b"))])),
            ev(OpType::AddDep, "a", fields(&[("dep", json!("b"))])),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations);
        assert_eq!(
            state["a"].depends_on,
            vec!["b".to_string()],
            "no duplicate dep"
        );
    }

    fn aged(days: i64, now: chrono::DateTime<chrono::Utc>) -> MutationEvent {
        let mut e = MutationEvent::new(OpType::Create, "t", serde_json::Map::new());
        e.timestamp = now - chrono::Duration::days(days);
        e
    }

    #[test]
    fn retention_split_by_count() {
        let now = chrono::Utc::now();
        let events: Vec<_> = (0..10).map(|_| aged(0, now)).collect();
        // Keep the last 3, time window off -> fold the first 7.
        assert_eq!(Engine::retention_split(&events, 3, 0, now), 7);
        // Keeping more than exist -> fold nothing (below the threshold).
        assert_eq!(Engine::retention_split(&events, 100, 0, now), 0);
    }

    #[test]
    fn retention_split_never_empties_the_log() {
        let now = chrono::Utc::now();
        let events: Vec<_> = (0..3).map(|_| aged(0, now)).collect();
        // keep_events = 0 would fold everything, but the clamp keeps the last one
        // so the seq watermark stays derivable.
        assert_eq!(Engine::retention_split(&events, 0, 0, now), 2);
        // An empty log has nothing to fold.
        assert_eq!(Engine::retention_split(&[], 0, 0, now), 0);
    }

    #[test]
    fn retention_split_keeps_union_of_count_and_time() {
        let now = chrono::Utc::now();
        // 5 events from 10 days ago, then 5 from 1 day ago.
        let mut events: Vec<_> = (0..5).map(|_| aged(10, now)).collect();
        events.extend((0..5).map(|_| aged(1, now)));

        // count alone would fold 8; the 7-day window forces keeping the recent
        // 5, so only the 5 old ones fold (the union keeps more).
        assert_eq!(Engine::retention_split(&events, 2, 7, now), 5);
        // A wide window keeps everything.
        assert_eq!(Engine::retention_split(&events, 2, 365, now), 0);
    }
}
