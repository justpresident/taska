//! `history` action: the per-event log of matching tasks - `watch`'s past-tense
//! twin.
//!
//! Where `watch` diffs state at a cursor against state now (one NET change per
//! task), `history` replays the retained log once and reports every event of
//! the selected tasks with that event's own before/after - how a task got where
//! it is. Tasks are selected exactly as `list` selects them (current state; a
//! deleted task by its last state), so `history <filter>` is the history of
//! what `list <filter>` shows. The
//! frontend renders the per-field diff (`format::state_diff`, shared with
//! `watch`/`undo`). The returned `cursor` is the log's high-water seq, so a
//! caller can catch up with `history` and continue with `watch --since cursor`
//! with nothing skipped or repeated.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};

use crate::action::columns::{ColumnUse, Vocabulary};
use crate::action::{list, watch};
use crate::engine::Engine;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType, TaskState};
use crate::schema::rename_to_display;
use crate::storage::EventStore;

/// A `history` query (every name in DISPLAY form).
pub struct HistoryQuery<'a> {
    /// List-grammar criteria, bare ids included. They select TASKS as `list`
    /// does - by current state (a deleted task by its last state) - and each
    /// selected task's whole history is reported.
    pub criteria: &'a [String],
    /// Only tasks that are not done now.
    pub open: bool,
    /// Only tasks ready to work on now: not done and every dependency done.
    pub ready: bool,
    /// Only events after this seq (`None` = all retained history).
    pub since: Option<u64>,
    /// Only the last `count` matching events.
    pub count: Option<usize>,
    /// Non-empty: only events that changed one of these columns, with each
    /// entry's states narrowed to them (`watch --columns` semantics).
    pub columns: &'a [String],
    /// Newest-first instead of the default oldest-first (applied after `count`,
    /// which always keeps the LATEST events).
    pub reverse: bool,
}

/// One event of a matching task, with the task's state either side of it
/// (display-named; `None` before a create / after a delete).
pub struct HistoryEntry {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub op: OpType,
    pub id: String,
    pub before: Option<TaskState>,
    pub after: Option<TaskState>,
    /// The seq this event undoes, for an `undo` compensation.
    pub undoes: Option<u64>,
    /// The merge strategy, for a resolution event the merge driver wrote.
    pub merge_strategy: Option<String>,
}

/// A `history` read: the matching events oldest-first, the cursor they run up
/// to, and - when compaction has folded older events into the baseline - the
/// last seq that history can no longer show.
pub struct HistoryOutcome {
    pub cursor: u64,
    pub entries: Vec<HistoryEntry>,
    pub compacted_through: Option<u64>,
}

/// Replay the retained log and collect the events of matching tasks.
///
/// # Errors
/// A malformed criterion, an unknown filter field / id / column, or a `since`
/// below the compaction watermark (those events are gone).
pub fn history(store: &impl EventStore, query: &HistoryQuery) -> Result<HistoryOutcome, DynError> {
    let config = store.config();
    let baseline = store.load_baseline()?;
    let log = store.load_mutations()?;
    let cursor = log.last().map_or(0, |e| e.seq);
    // The watermark: everything at or below it lives only in the baseline.
    let watermark = log.first().map_or(0, |e| e.seq.saturating_sub(1));
    if let Some(since) = query.since {
        if since < watermark {
            return Err(format!(
                "seq {since} predates the retained log (oldest is {}); those events were \
                 compacted into the baseline",
                watermark + 1
            )
            .into());
        }
    }

    let closed = &config.workflow.closed_statuses;
    let watched = check_names(store, query, &baseline, &log)?;
    let filter = list::TaskFilter::new(query.criteria, query.open, query.ready)?;

    // One pass. `raw` is what the engine folds into; `shown` mirrors it with
    // display names for the criteria, updated per event for the ONE task the
    // event touched.
    let mut raw: HashMap<String, TaskState> =
        baseline.into_iter().map(|t| (t.id.clone(), t)).collect();
    let mut shown: HashMap<String, TaskState> = raw
        .iter()
        .map(|(id, t)| (id.clone(), display(t, config)))
        .collect();
    let since = query.since.unwrap_or(0);
    let mut entries = Vec::new();
    let mut deleted: HashMap<String, bool> = HashMap::new();
    for event in log {
        let (seq, id) = (event.seq, event.task_id.clone());
        if seq <= since {
            Engine::apply(&mut raw, event, closed);
            sync(&raw, &mut shown, &id, config);
            continue;
        }
        let before = shown.get(&id).cloned();
        if event.op == OpType::Delete && before.is_some() {
            // A deleted task has no final state: it is selected by its LAST one.
            deleted.insert(id.clone(), filter.matches(&shown, &id, config));
        }
        let meta = EventMeta::of(&event);
        Engine::apply(&mut raw, event, closed);
        sync(&raw, &mut shown, &id, config);
        let after = shown.get(&id).cloned();
        if before.is_none() && after.is_none() {
            continue; // an orphan: no task either side
        }
        let (before, after) = match &watched {
            None => (before, after),
            Some(cols) => {
                let was = before.and_then(|t| watch::project(&t, cols));
                let now = after.and_then(|t| watch::project(&t, cols));
                if was == now {
                    continue; // only unwatched columns changed
                }
                (was, now)
            }
        };
        entries.push(HistoryEntry {
            seq,
            timestamp: meta.timestamp,
            op: meta.op,
            id,
            before,
            after,
            undoes: meta.undoes,
            merge_strategy: meta.merge_strategy,
        });
    }
    // Select TASKS the way `list` does: by their final state (a task deleted
    // along the way by its last one), then keep their whole history. Cached per
    // id - `ready` and inverse-name criteria scan the store.
    let mut selected: HashMap<String, bool> = HashMap::new();
    entries.retain(|e| {
        *selected.entry(e.id.clone()).or_insert_with(|| {
            if shown.contains_key(&e.id) {
                filter.matches(&shown, &e.id, config)
            } else {
                deleted.get(&e.id).copied().unwrap_or(false)
            }
        })
    });
    if let Some(count) = query.count {
        let drop = entries.len().saturating_sub(count);
        entries.drain(..drop);
    }
    if query.reverse {
        entries.reverse();
    }
    Ok(HistoryOutcome {
        cursor,
        entries,
        compacted_through: (watermark > 0).then_some(watermark),
    })
}

/// Check every name the query uses - filter fields, bare ids, `--columns` -
/// against the final state plus everything the log wrote, returning the watched
/// column set (`None` = every column).
fn check_names(
    store: &impl EventStore,
    query: &HistoryQuery,
    baseline: &[TaskState],
    log: &[MutationEvent],
) -> Result<Option<BTreeSet<String>>, DynError> {
    let config = store.config();
    let now = Engine::materialize_state(
        baseline.to_vec(),
        log.to_vec(),
        &config.workflow.closed_statuses,
    );
    let vocab = Vocabulary::build(config, &now, log);
    let fields = list::criteria_fields(query.criteria)?;
    vocab.check(
        &fields.iter().map(String::as_str).collect::<Vec<_>>(),
        ColumnUse::Filter,
    )?;
    list::check_ids(store, &now, &list::bare_ids(query.criteria))?;
    if query.columns.is_empty() {
        return Ok(None);
    }
    watch::resolve_columns(config, &vocab, query.columns).map(Some)
}

/// The command vocabulary for an event's op - what the user typed to make it.
#[must_use]
pub const fn op_label(op: &OpType) -> &'static str {
    match op {
        OpType::Create => "create",
        OpType::Update => "update",
        OpType::Append => "append",
        OpType::Add => "add",
        OpType::Remove => "remove",
        OpType::Delete => "delete",
        OpType::AddEdge => "dep add",
        OpType::RemoveEdge => "dep remove",
    }
}

/// What an entry keeps of its event once the event is consumed by replay.
struct EventMeta {
    timestamp: DateTime<Utc>,
    op: OpType,
    undoes: Option<u64>,
    merge_strategy: Option<String>,
}

impl EventMeta {
    fn of(event: &MutationEvent) -> Self {
        Self {
            timestamp: event.timestamp,
            op: event.op.clone(),
            undoes: event.undoes(),
            merge_strategy: event
                .meta
                .as_ref()
                .and_then(|m| m.get("strategy"))
                .and_then(|s| s.as_str())
                .map(str::to_string),
        }
    }
}

/// `task` with its canonical status/type keys renamed to display names, and
/// without the computed timestamps - a diff shows only real mutations.
fn display(task: &TaskState, config: &crate::config::Config) -> TaskState {
    let mut shown = task.clone();
    rename_to_display(&mut shown.custom_fields, &config.workflow);
    shown.create_time = None;
    shown.update_time = None;
    shown.close_time = None;
    shown
}

/// Bring `shown`'s copy of task `id` in line with `raw` after an event.
fn sync(
    raw: &HashMap<String, TaskState>,
    shown: &mut HashMap<String, TaskState>,
    id: &str,
    config: &crate::config::Config,
) {
    match raw.get(id) {
        Some(task) => {
            shown.insert(id.to_string(), display(task, config));
        }
        None => {
            shown.remove(id);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::InMemoryStore;

    fn query(since: Option<u64>) -> HistoryQuery<'static> {
        HistoryQuery {
            criteria: &[],
            open: false,
            ready: false,
            since,
            count: None,
            columns: &[],
            reverse: false,
        }
    }

    #[test]
    fn compacted_events_are_reported_and_not_reachable() {
        let store = InMemoryStore::default();
        let mut event = MutationEvent::new(OpType::Create, "a", serde_json::Map::new());
        event.seq = 10;
        store.compact(&[], &[event]).unwrap();

        let all = history(&store, &query(None)).unwrap();
        assert_eq!(
            all.compacted_through,
            Some(9),
            "seqs 1..=9 live in the baseline"
        );
        assert_eq!(all.entries.len(), 1);
        assert_eq!(all.cursor, 10);
        assert!(
            history(&store, &query(Some(8))).is_err(),
            "a since below the watermark asks for events that are gone"
        );
        assert_eq!(history(&store, &query(Some(9))).unwrap().entries.len(), 1);
    }
}
