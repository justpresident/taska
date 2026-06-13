//! State materialization (the overlay engine).
//!
//! A pure replay algorithm: it folds a mutation log over a baseline snapshot
//! and knows nothing about where either came from. Keeping it free of storage
//! dependencies makes it trivially testable and reusable.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value};

use crate::model::{edge_rel, edge_target, is_done, MutationEvent, OpType, TaskState, STATUS_KEY};

pub struct Engine;

/// A field value's text form for `Append`: a raw string for a JSON string, else
/// its compact JSON - so appending to a non-string field still yields readable
/// text rather than a quoted blob. `pub(crate)` so the write gate can fold
/// repeated `+=` operands into one event with the SAME join replay uses.
pub(crate) fn append_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Apply a `Create`/`Update` payload: set each field, or remove it when the value
/// is JSON `null` (the field-unset convention - null never reaches state).
/// `pub(crate)` so the write gate can PREVIEW a draft's resulting fields with
/// the same semantics replay will use - never a parallel implementation.
pub(crate) fn apply_set(fields: &mut Map<String, Value>, payload: Map<String, Value>) {
    for (k, v) in payload {
        if v.is_null() {
            fields.remove(&k);
        } else {
            fields.insert(k, v);
        }
    }
}

/// Apply an `AddEdge`/`RemoveEdge` (`add` = true/false). The edge's `rel` keys it
/// in the `relationships` map - every type, `depends_on` included, is stored
/// uniformly. An emptied entry is dropped so the map stays clean. An edge missing
/// its `target` or `rel` is malformed (e.g. a pre-1.0 untyped event that was
/// never migrated) and is skipped.
fn apply_dep(task: &mut TaskState, payload: &Map<String, Value>, add: bool) {
    let (Some(dep_id), Some(rel_type)) = (edge_target(payload), edge_rel(payload)) else {
        return;
    };

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

/// Apply an `Add` (`add` = true) or `Remove` payload - see [`OpType::Add`] for
/// the contract. Config-free and shape-dispatched, so replay stays
/// deterministic from the log alone:
/// - number operand onto a number (or missing, as 0): arithmetic `+/-`;
/// - array operand: set-style insert/remove on the current array (missing =
///   empty), deduped and kept in [`crate::model::cmp_json`] order - the
///   canonical form concurrent branches converge on;
/// - anything else: a deterministic no-op.
///
/// `pub(crate)` for the write gate's preview, like [`apply_set`].
pub(crate) fn apply_accumulate(
    fields: &mut Map<String, Value>,
    payload: Map<String, Value>,
    add: bool,
) {
    for (k, operand) in payload {
        match (&operand, fields.get(&k)) {
            (Value::Number(n), current @ (None | Some(Value::Number(_)))) => {
                if let Some(result) = accumulate_numbers(current, n, add) {
                    fields.insert(k, result);
                }
            }
            (Value::Array(elements), current @ (None | Some(Value::Array(_)))) => {
                let mut items: Vec<Value> = current
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if add {
                    items.extend(elements.iter().cloned());
                    items.sort_by(crate::model::cmp_json);
                    items.dedup_by(|a, b| a == b);
                } else {
                    items.retain(|item| !elements.contains(item));
                }
                fields.insert(k, Value::Array(items));
            }
            _ => {} // mismatched shapes: a deterministic no-op
        }
    }
}

/// `current +/- operand` as the narrowest JSON number: exact `i64` math while
/// both sides are integral (so `int`/`uint` fields keep their kind and
/// precision), falling back to `f64`. `None` (overflowing integers whose float
/// form is not representable, infinite results) means "leave the field
/// unchanged" - a deterministic no-op, never a stored `null`. `pub(crate)` so the
/// write gate can fold repeated numeric `+=`/`-=` operands the same way replay
/// accumulates them.
pub(crate) fn accumulate_numbers(
    current: Option<&Value>,
    operand: &serde_json::Number,
    add: bool,
) -> Option<Value> {
    let current_int = current.map_or(Some(0), Value::as_i64);
    if let (Some(c), Some(d)) = (current_int, operand.as_i64()) {
        let result = if add {
            c.checked_add(d)
        } else {
            c.checked_sub(d)
        };
        if let Some(result) = result {
            return Some(Value::from(result));
        }
    }
    let c = current.map_or(Some(0.0), Value::as_f64)?;
    let d = operand.as_f64()?;
    let result = if add { c + d } else { c - d };
    serde_json::Number::from_f64(result).map(Value::Number)
}

/// Update `close_time` to reflect a task's CURRENT closure: set it on a
/// transition INTO done (`was_done` -> `now_done`), clear it whenever the task is
/// currently not done. Staying done leaves the prior close time untouched - so it
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
    /// `status_field` is a display name, applied later by `action::read`).
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
                OpType::Add | OpType::Remove => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        apply_accumulate(
                            &mut task.custom_fields,
                            event.payload,
                            matches!(event.op, OpType::Add),
                        );
                        // Numeric/set accumulation never touches the status.
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
    /// newer than `keep_days` (0 disables the time window) - kept if either rule
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
    use crate::model::{OP_KEY, REL_KEY, SEQ_KEY, TARGET_KEY, TASK_ID_KEY, TIMESTAMP_KEY};
    use crate::test_support::names::*;
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
            mk(1, OpType::Create, fields(&[(STATUS_KEY, json!("open"))]), 0),
            mk(
                2,
                OpType::Update,
                fields(&[(STATUS_KEY, json!(DONE_STATUS))]),
                10,
            ), // close
            mk(3, OpType::Update, fields(&[("priority", json!(2))]), 20), // stays closed
            mk(
                4,
                OpType::Update,
                fields(&[(STATUS_KEY, json!("open"))]),
                30,
            ), // reopen -> clear
            mk(
                5,
                OpType::Update,
                fields(&[(STATUS_KEY, json!(DONE_STATUS))]),
                40,
            ), // re-close
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);
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
            fields(&[(STATUS_KEY, json!("open"))]),
        )];
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);
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
            ev(OpType::Create, "a", fields(&[(STATUS_KEY, json!("open"))])),
            ev(OpType::Create, "b", serde_json::Map::new()),
            ev(OpType::Update, "a", fields(&[(STATUS_KEY, json!("done"))])),
            ev(
                OpType::AddEdge,
                "b",
                fields(&[(TARGET_KEY, json!("a")), (REL_KEY, json!(BLOCKER))]),
            ),
            ev(OpType::Create, "c", serde_json::Map::new()),
            ev(OpType::Delete, "c", serde_json::Map::new()),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);

        assert_eq!(state.len(), 2, "c was deleted");
        assert_eq!(
            state["a"].custom_fields[STATUS_KEY],
            json!("done"),
            "update overwrote create"
        );
        assert_eq!(state["b"].relationships[BLOCKER], vec!["a".to_string()]);
    }

    #[test]
    fn add_and_remove_accumulate_numbers_and_sets() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            // Numbers: a missing field counts as 0; integers stay exact.
            ev(OpType::Add, "a", fields(&[("points", json!(5))])),
            ev(OpType::Add, "a", fields(&[("points", json!(2))])),
            ev(OpType::Remove, "a", fields(&[("points", json!(3))])),
            ev(OpType::Add, "a", fields(&[("score", json!(1.5))])),
            // Sets: array operands insert/remove elements; the stored form is
            // canonical (sorted, deduped); removing an absent element no-ops.
            ev(OpType::Add, "a", fields(&[("tags", json!(["b"]))])),
            ev(OpType::Add, "a", fields(&[("tags", json!(["a", "b"]))])),
            ev(OpType::Remove, "a", fields(&[("tags", json!(["b", "x"]))])),
            // A shape mismatch (string operand onto a number) is a no-op.
            ev(OpType::Add, "a", fields(&[("points", json!("nope"))])),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);
        assert_eq!(state["a"].custom_fields["points"], json!(4));
        assert_eq!(state["a"].custom_fields["score"], json!(1.5));
        assert_eq!(state["a"].custom_fields["tags"], json!(["a"]));
    }

    #[test]
    fn edge_event_missing_rel_is_skipped() {
        // An edge with no `rel` (a pre-1.0 untyped event that escaped migration)
        // is malformed and dropped rather than silently defaulting to depends_on.
        let raw = [
            format!(
                r#"{{"{SEQ_KEY}":1,"{TIMESTAMP_KEY}":"2026-01-01T00:00:00Z","{OP_KEY}":"Create","{TASK_ID_KEY}":"a"}}"#
            ),
            format!(
                r#"{{"{SEQ_KEY}":2,"{TIMESTAMP_KEY}":"2026-01-01T00:00:00Z","{OP_KEY}":"Create","{TASK_ID_KEY}":"b"}}"#
            ),
            format!(
                r#"{{"{SEQ_KEY}":3,"{TIMESTAMP_KEY}":"2026-01-01T00:00:00Z","{OP_KEY}":"AddEdge","{TASK_ID_KEY}":"b","{TARGET_KEY}":"a"}}"#
            ),
        ];
        let mutations: Vec<MutationEvent> = raw
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);
        assert!(
            state["b"].relationships.is_empty(),
            "edge with no rel is skipped, not defaulted"
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
        let (state, orphans) = Engine::materialize_report(Vec::new(), mutations, DONE_STATUS);
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
            // Two BLOCKER edges land in the BLOCKER relationship.
            ev(
                OpType::AddEdge,
                "a",
                fields(&[(TARGET_KEY, json!("b")), (REL_KEY, json!(BLOCKER))]),
            ),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[(TARGET_KEY, json!("c")), (REL_KEY, json!(BLOCKER))]),
            ),
            // A different type lands under its own key in the relationships map.
            ev(
                OpType::AddEdge,
                "a",
                fields(&[(TARGET_KEY, json!("d")), (REL_KEY, json!(INFO))]),
            ),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[(TARGET_KEY, json!("e")), (REL_KEY, json!(INFO))]),
            ),
            ev(
                OpType::RemoveEdge,
                "a",
                fields(&[(TARGET_KEY, json!("d")), (REL_KEY, json!(INFO))]),
            ),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);
        let a = &state["a"];
        assert_eq!(
            a.relationships[BLOCKER],
            vec!["b".to_string(), "c".to_string()]
        );
        assert_eq!(a.relationships[INFO], vec!["e".to_string()]);
    }

    #[test]
    fn removing_last_typed_edge_drops_the_map_entry() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[(TARGET_KEY, json!("d")), (REL_KEY, json!(INFO))]),
            ),
            ev(
                OpType::RemoveEdge,
                "a",
                fields(&[(TARGET_KEY, json!("d")), (REL_KEY, json!(INFO))]),
            ),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);
        assert!(
            state["a"].relationships.is_empty(),
            "an emptied typed entry is removed, leaving a clean map"
        );
    }

    #[test]
    fn mutations_overlay_the_baseline() {
        let baseline = vec![TaskState {
            id: "a".into(),
            relationships: BTreeMap::from([(BLOCKER.to_string(), vec!["x".into()])]),
            custom_fields: fields(&[(STATUS_KEY, json!("open"))]),
            create_time: None,
            update_time: None,
            close_time: None,
        }];
        let mutations = vec![
            ev(OpType::Update, "a", fields(&[(STATUS_KEY, json!("done"))])),
            ev(
                OpType::RemoveEdge,
                "a",
                fields(&[(TARGET_KEY, json!("x")), (REL_KEY, json!(BLOCKER))]),
            ),
        ];
        let state = Engine::materialize_state(baseline, mutations, DONE_STATUS);

        assert_eq!(state["a"].custom_fields[STATUS_KEY], json!("done"));
        assert!(
            !state["a"].relationships.contains_key(BLOCKER),
            "dep removed from baseline task"
        );
    }

    #[test]
    fn reports_orphaned_events_and_spares_normal_ones() {
        let mutations = vec![
            // Normal create/update on `a`: never an orphan.
            ev(OpType::Create, "a", fields(&[(STATUS_KEY, json!("open"))])),
            ev(OpType::Update, "a", fields(&[(STATUS_KEY, json!("done"))])),
            // Update to a task that was never created: orphan.
            ev(OpType::Update, "ghost", fields(&[("x", json!(1))])),
            // `b` is created then deleted...
            ev(OpType::Create, "b", serde_json::Map::new()),
            ev(OpType::Delete, "b", serde_json::Map::new()),
            // ...so events after its deletion apply to nothing: orphans.
            ev(
                OpType::AddEdge,
                "b",
                fields(&[(TARGET_KEY, json!("a")), (REL_KEY, json!(BLOCKER))]),
            ),
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

        let (state, orphans) = Engine::materialize_report(Vec::new(), mutations, DONE_STATUS);

        assert_eq!(state.len(), 1, "only `a` survives");
        assert_eq!(state["a"].custom_fields[STATUS_KEY], json!("done"));
        // Orphans: the ghost Update (seq 3), the AddEdge (seq 6) and Delete (seq 7)
        // after `b` was deleted, in replay order. The normal create/update and
        // the first delete of an existing task are not reported.
        assert_eq!(orphans, vec![3, 6, 7]);
    }

    #[test]
    fn add_dep_is_idempotent() {
        let mutations = vec![
            ev(OpType::Create, "a", serde_json::Map::new()),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[(TARGET_KEY, json!("b")), (REL_KEY, json!(BLOCKER))]),
            ),
            ev(
                OpType::AddEdge,
                "a",
                fields(&[(TARGET_KEY, json!("b")), (REL_KEY, json!(BLOCKER))]),
            ),
        ];
        let state = Engine::materialize_state(Vec::new(), mutations, DONE_STATUS);
        assert_eq!(
            state["a"].relationships[BLOCKER],
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
