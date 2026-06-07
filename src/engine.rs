//! State materialization (the overlay engine).
//!
//! A pure replay algorithm: it folds a mutation log over a baseline snapshot
//! and knows nothing about where either came from. Keeping it free of storage
//! dependencies makes it trivially testable and reusable.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value};

use crate::model::{
    edge_rel, edge_target, is_done, MutationEvent, OpType, TaskState, DEPENDS_ON, STATUS_KEY,
};

pub struct Engine;

/// A field value's text form for `Append`: a raw string for a JSON string, else
/// its compact JSON — so appending to a non-string field still yields readable
/// text rather than a quoted blob.
fn append_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Apply a `Create`/`Update` payload: set each field, or remove it when the value
/// is JSON `null` (the field-unset convention — null never reaches state).
/// `pub(crate)` so the write gate can PREVIEW a draft's resulting fields with
/// the same semantics replay will use — never a parallel implementation.
pub(crate) fn apply_set(fields: &mut Map<String, Value>, payload: Map<String, Value>) {
    for (k, v) in payload {
        if v.is_null() {
            fields.remove(&k);
        } else {
            fields.insert(k, v);
        }
    }
}

/// Apply an `AddEdge`/`RemoveEdge` (`add` = true/false). The edge's `rel` (absent
/// = the default [`DEPENDS_ON`]) keys it in the `relationships` map — every type,
/// `depends_on` included, is stored uniformly. An emptied entry is dropped so the
/// map stays clean. Reads via [`edge_target`]/[`edge_rel`], so legacy `dep`/`type`
/// payload keys replay correctly until v1 drops them.
fn apply_dep(task: &mut TaskState, payload: &Map<String, Value>, add: bool) {
    let Some(dep_id) = edge_target(payload) else {
        return;
    };
    let rel_type = edge_rel(payload).unwrap_or(DEPENDS_ON);

    if add {
        let targets = task.relationships.entry(rel_type.to_string()).or_default();
        if !targets.iter().any(|d| d == dep_id) {
            targets.push(dep_id.to_string());
        }
    } else if let Some(targets) = task.relationships.get_mut(rel_type) {
        targets.retain(|d| d != dep_id);
        if targets.is_empty() {
            task.relationships.remove(rel_type);
        }
    }
}

/// Apply an `Append` payload: append each value's text to its field, one entry
/// per line; a `null` value adds nothing, and the first write to an absent field
/// simply sets it. `pub(crate)` for the write gate's preview, like [`apply_set`].
pub(crate) fn apply_append(fields: &mut Map<String, Value>, payload: Map<String, Value>) {
    for (k, v) in payload {
        if v.is_null() {
            continue;
        }
        let added = append_text(&v);
        let combined = match fields.get(&k).map(append_text) {
            Some(prev) if !prev.is_empty() => format!("{prev}\n{added}"),
            _ => added,
        };
        fields.insert(k, Value::String(combined));
    }
}

/// Update `close_time` to reflect a task's CURRENT closure: set it on a
/// transition INTO done (`was_done` → `now_done`), clear it whenever the task is
/// currently not done. Staying done leaves the prior close time untouched — so it
/// records the *most recent* close and resets to empty on reopen.
const fn refresh_close_time(
    task: &mut TaskState,
    was_done: bool,
    now_done: bool,
    ts: DateTime<Utc>,
) {
    if now_done {
        if !was_done {
            task.close_time = Some(ts);
        }
    } else {
        task.close_time = None;
    }
}

impl Engine {
    /// Fold `mutations` over `baseline` to produce the current task map.
    ///
    /// Thin wrapper over [`Engine::materialize_report`] that discards the orphan
    /// report, for callers that only need the state. `done_status` is needed
    /// only to compute each task's `close_time`; the status itself always lives
    /// under the canonical [`STATUS_KEY`] in raw state (the configured
    /// `status_field` is a display name, applied later by `state_of`).
    pub fn materialize_state(
        baseline: Vec<TaskState>,
        mutations: Vec<MutationEvent>,
        done_status: &str,
    ) -> HashMap<String, TaskState> {
        Self::materialize_report(baseline, mutations, done_status).0
    }

    /// Like [`Engine::materialize_state`], but also reports *orphaned* events:
    /// those whose target task did not exist when the event was applied, so the
    /// event folded into nothing. These are `Update`/`Append`/`AddEdge`/`RemoveEdge`/
    /// `Delete` events on a `task_id` absent from the state map at apply time
    /// (`Create` is never an orphan). The returned `Vec<u64>` holds their `seq`s in
    /// replay order.
    ///
    /// Replay stays non-fatal: orphans are merely counted, never errored. They can
    /// arise from the merge driver's removal-union, reverts, or manual edits that
    /// drop a task's `Create` while leaving later events that target it.
    ///
    /// Along the way it materializes each task's computed timestamps (see
    /// [`TaskState`]): `create_time` (first `Create`), `update_time` (latest
    /// touching event), and `close_time` (most recent transition of
    /// the canonical status into `done_status`, cleared while currently not done).
    pub fn materialize_report(
        baseline: Vec<TaskState>,
        mutations: Vec<MutationEvent>,
        done_status: &str,
    ) -> (HashMap<String, TaskState>, Vec<u64>) {
        let mut state_map: HashMap<String, TaskState> =
            baseline.into_iter().map(|t| (t.id.clone(), t)).collect();
        let mut orphans: Vec<u64> = Vec::new();

        for event in mutations {
            let ts = event.timestamp;
            match event.op {
                OpType::Create => {
                    // Re-creating an existing id refreshes its fields but keeps
                    // any deps already attached.
                    let entry =
                        state_map
                            .entry(event.task_id.clone())
                            .or_insert_with(|| TaskState {
                                id: event.task_id.clone(),
                                relationships: BTreeMap::new(),
                                custom_fields: serde_json::Map::new(),
                                create_time: None,
                                update_time: None,
                                close_time: None,
                            });
                    let was_done = is_done(entry, STATUS_KEY, done_status);
                    apply_set(&mut entry.custom_fields, event.payload);
                    // First Create wins for create_time (a re-Create keeps it).
                    if entry.create_time.is_none() {
                        entry.create_time = Some(ts);
                    }
                    entry.update_time = Some(ts);
                    refresh_close_time(
                        entry,
                        was_done,
                        is_done(entry, STATUS_KEY, done_status),
                        ts,
                    );
                }
                OpType::Update => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        let was_done = is_done(task, STATUS_KEY, done_status);
                        apply_set(&mut task.custom_fields, event.payload);
                        task.update_time = Some(ts);
                        refresh_close_time(
                            task,
                            was_done,
                            is_done(task, STATUS_KEY, done_status),
                            ts,
                        );
                    } else {
                        orphans.push(event.seq);
                    }
                }
                OpType::Append => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        apply_append(&mut task.custom_fields, event.payload);
                        // Text accumulation touches the task but not its done
                        // status, so no close_time recompute.
                        task.update_time = Some(ts);
                    } else {
                        orphans.push(event.seq);
                    }
                }
                OpType::AddEdge => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        apply_dep(task, &event.payload, true);
                        // A dep change touches the task but never its status.
                        task.update_time = Some(ts);
                    } else {
                        orphans.push(event.seq);
                    }
                }
                OpType::RemoveEdge => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        apply_dep(task, &event.payload, false);
                        task.update_time = Some(ts);
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use serde_json::{json, Value};

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
    fn materializes_timestamps_and_resets_close_on_reopen() {
        let base = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = |secs: i64| base + Duration::seconds(secs);
        let mk = |seq, op, payload, secs| {
            let mut e = MutationEvent::new(op, "a", payload);
            e.seq = seq;
            e.timestamp = at(secs);
            e
        };
        let mutations = vec![
            mk(1, OpType::Create, fields(&[("status", json!("open"))]), 0),
            mk(
                2,
                OpType::Update,
                fields(&[("status", json!("closed"))]),
                10,
            ), // close
            mk(3, OpType::Update, fields(&[("priority", json!(2))]), 20), // stays closed
            mk(4, OpType::Update, fields(&[("status", json!("open"))]), 30), // reopen -> clear
            mk(
                5,
                OpType::Update,
                fields(&[("status", json!("closed"))]),
                40,
            ), // re-close
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, "closed");
        let a = &state["a"];
        assert_eq!(a.create_time, Some(at(0)), "first Create's time");
        assert_eq!(a.update_time, Some(at(40)), "latest event's time");
        // Cleared on reopen, then set to the MOST RECENT close (not the first).
        assert_eq!(a.close_time, Some(at(40)), "most recent close");
    }

    #[test]
    fn open_task_has_create_and_update_but_no_close_time() {
        let mutations = vec![ev(
            OpType::Create,
            "a",
            fields(&[("status", json!("open"))]),
        )];
        let state = Engine::materialize_state(Vec::new(), mutations, "closed");
        assert!(state["a"].create_time.is_some());
        assert!(state["a"].update_time.is_some());
        assert!(
            state["a"].close_time.is_none(),
            "an open task is never closed"
        );
    }

    #[test]
    fn replays_create_update_dep_and_delete() {
        let mutations = vec![
            ev(OpType::Create, "a", fields(&[("status", json!("open"))])),
            ev(OpType::Create, "b", serde_json::Map::new()),
            ev(OpType::Update, "a", fields(&[("status", json!("done"))])),
            ev(OpType::AddEdge, "b", fields(&[("target", json!("a"))])),
            ev(OpType::Create, "c", serde_json::Map::new()),
            ev(OpType::Delete, "c", serde_json::Map::new()),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, "closed");

        assert_eq!(state.len(), 2, "c was deleted");
        assert_eq!(
            state["a"].custom_fields["status"],
            json!("done"),
            "update overwrote create"
        );
        assert_eq!(state["b"].depends_on(), vec!["a".to_string()]);
    }

    #[test]
    fn legacy_edge_op_aliases_and_payload_keys_replay() {
        // Pre-rename logs: ops spelled `AddDep`/`RemoveDep` with `dep`/`type`
        // payload keys. The op parses via its serde alias, the keys via the
        // edge_target/edge_rel fallbacks — both tolerated until v1.
        let raw = concat!(
            r#"{"seq":1,"timestamp":"2026-01-01T00:00:00Z","op":"Create","task_id":"a"}"#,
            "\n",
            r#"{"seq":2,"timestamp":"2026-01-01T00:00:00Z","op":"Create","task_id":"b"}"#,
            "\n",
            r#"{"seq":3,"timestamp":"2026-01-01T00:00:00Z","op":"AddDep","task_id":"b","dep":"a","type":"relates_to"}"#,
            "\n",
            r#"{"seq":4,"timestamp":"2026-01-01T00:00:00Z","op":"AddDep","task_id":"a","dep":"b"}"#,
        );
        let mutations: Vec<MutationEvent> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(mutations[2].op, OpType::AddEdge, "alias parses as new op");
        let state = Engine::materialize_state(Vec::new(), mutations, "closed");
        assert_eq!(
            state["b"].relationships["relates_to"],
            vec!["a".to_string()],
            "legacy typed edge lands under its rel"
        );
        assert_eq!(
            state["a"].depends_on(),
            vec!["b".to_string()],
            "legacy untyped edge defaults to depends_on"
        );
    }

    #[test]
    fn append_accumulates_text_and_orphans_when_taskless() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            ev(OpType::Append, "a", fields(&[("log", json!("first"))])),
            ev(OpType::Append, "a", fields(&[("log", json!("second"))])),
            // An append to a non-existent task is an orphan, never an error.
            ev(OpType::Append, "ghost", fields(&[("log", json!("x"))])),
        ];
        let (state, orphans) = Engine::materialize_report(Vec::new(), mutations, "closed");
        assert_eq!(
            state["a"].custom_fields["log"],
            json!("first\nsecond"),
            "appends accumulate one entry per line"
        );
        assert_eq!(orphans.len(), 1, "append to a missing task is orphaned");
    }

    #[test]
    fn typed_deps_route_to_field_or_map() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            // Legacy untyped, and explicit depends_on, both land in depends_on.
            ev(OpType::AddEdge, "a", fields(&[("target", json!("b"))])),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[("target", json!("c")), ("rel", json!("depends_on"))]),
            ),
            // A typed edge lands in the relationships map (not depends_on).
            ev(
                OpType::AddEdge,
                "a",
                fields(&[("target", json!("d")), ("rel", json!("relates_to"))]),
            ),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[("target", json!("e")), ("rel", json!("relates_to"))]),
            ),
            ev(
                OpType::RemoveEdge,
                "a",
                fields(&[("target", json!("d")), ("rel", json!("relates_to"))]),
            ),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, "closed");
        let a = &state["a"];
        assert_eq!(a.depends_on(), vec!["b".to_string(), "c".to_string()]);
        assert_eq!(a.relationships["relates_to"], vec!["e".to_string()]);
    }

    #[test]
    fn removing_last_typed_edge_drops_the_map_entry() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[("target", json!("d")), ("rel", json!("relates_to"))]),
            ),
            ev(
                OpType::RemoveEdge,
                "a",
                fields(&[("target", json!("d")), ("rel", json!("relates_to"))]),
            ),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, "closed");
        assert!(
            state["a"].relationships.is_empty(),
            "an emptied typed entry is removed, leaving a clean map"
        );
    }

    #[test]
    fn mutations_overlay_the_baseline() {
        let baseline = vec![TaskState {
            id: "a".into(),
            relationships: BTreeMap::from([("depends_on".to_string(), vec!["x".into()])]),
            custom_fields: fields(&[("status", json!("open"))]),
            create_time: None,
            update_time: None,
            close_time: None,
        }];
        let mutations = vec![
            ev(OpType::Update, "a", fields(&[("status", json!("done"))])),
            ev(OpType::RemoveEdge, "a", fields(&[("target", json!("x"))])),
        ];
        let state = Engine::materialize_state(baseline, mutations, "closed");

        assert_eq!(state["a"].custom_fields["status"], json!("done"));
        assert!(
            state["a"].depends_on().is_empty(),
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
            ev(OpType::AddEdge, "b", fields(&[("target", json!("a"))])),
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

        let (state, orphans) = Engine::materialize_report(Vec::new(), mutations, "closed");

        assert_eq!(state.len(), 1, "only `a` survives");
        assert_eq!(state["a"].custom_fields["status"], json!("done"));
        // Orphans: the ghost Update (seq 3), the AddEdge (seq 6) and Delete (seq 7)
        // after `b` was deleted, in replay order. The normal create/update and
        // the first delete of an existing task are not reported.
        assert_eq!(orphans, vec![3, 6, 7]);
    }

    #[test]
    fn add_dep_is_idempotent() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            ev(OpType::AddEdge, "a", fields(&[("target", json!("b"))])),
            ev(OpType::AddEdge, "a", fields(&[("target", json!("b"))])),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, "closed");
        assert_eq!(
            state["a"].depends_on(),
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
