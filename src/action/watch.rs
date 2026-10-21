//! `watch` action: the tasks that changed since a cursor, matching a filter.
//!
//! Diffs state as-of `since` against the current state, restricted to tasks
//! touched by events newer than `since` and matching the list-style filter, and
//! returns each such task's before/after state - the frontend renders the
//! per-field diff (via `format::render_state_diff`, shared with `undo`), plus
//! the cursor that snapshot was taken at. `columns` narrows a watch to some
//! fields: only their changes count, and only they are returned. No
//! blocking or polling here: that (and the holdout debounce) is the CLI's job.
//! This is one pure snapshot.

use std::collections::{BTreeMap, BTreeSet};

use crate::action::columns::{full_vocabulary, ColumnUse, Vocabulary};
use crate::action::{list, materialize, read};
use crate::config::Config;
use crate::error::DynError;
use crate::model::{TaskState, BLOCKED_BY_KEY, DEPS_KEY, SUBTASKS_KEY, UNBLOCKS_KEY};
use crate::storage::EventStore;

/// One task that changed since the cursor: its state at the cursor (`before` -
/// `None` if it was created since) and now (`after` - `None` if it was deleted).
pub struct WatchUpdate {
    pub id: String,
    pub before: Option<TaskState>,
    pub after: Option<TaskState>,
}

/// One [`poll`] snapshot: the matching changes and the cursor they cover.
pub struct WatchPoll {
    /// The log's high-water `seq` in the very read the diff was computed from -
    /// so `--since <cursor>` resumes exactly after it, skipping and repeating
    /// nothing. A separate `ta status --current` can't promise that: a write may
    /// land between the two reads.
    pub cursor: u64,
    pub updates: Vec<WatchUpdate>,
}

/// The tasks that changed since `since` and match the filter, plus the cursor.
///
/// Changed = touched by an event with `seq > since`; each comes with its
/// before/after state (ordered by id). `updates` is empty when nothing new
/// matches.
///
/// A touched task is reported if it matched the filter at the cursor OR matches
/// now, so leaving the filtered set is reported like entering it: `status=x`
/// sees a task move into `x` and out of it, and a matching task's deletion (no
/// current state at all) is the same rule. Two snapshots are compared, not
/// every event between them, so a task that passes through the set entirely
/// within one batch matches at neither end and goes unreported. Errors if `since` predates the retained log (compacted away).
///
/// Criterion fields and a non-empty `columns` (display names) are checked as by
/// [`validate`]. `columns` narrows the watch: only a change to one of them
/// counts, and `before`/`after` carry only those columns. A task created or deleted without any of them set
/// changes nothing watched, so it isn't reported. Empty = every field.
pub fn poll(
    store: &impl EventStore,
    criteria: &[String],
    open: bool,
    ready: bool,
    since: u64,
    columns: &[String],
) -> Result<WatchPoll, DynError> {
    let config = store.config();
    let baseline = store.load_baseline()?;
    let muts = store.load_mutations()?;

    // Compaction watermark: if the window we need (seq > since) dips below the
    // oldest retained event, those events were folded into the baseline and can't
    // be diffed. A once-in-a-store-lifetime edge; error rather than mislead.
    if let Some(first) = muts.first() {
        if since < first.seq.saturating_sub(1) {
            return Err(format!(
                "cursor seq {since} predates the retained log (oldest is {}); those \
                 events were compacted - re-sync with `ta status --current`",
                first.seq
            )
            .into());
        }
    }

    // Fast path: nothing newer than the cursor.
    let cursor = muts.last().map_or(0, |e| e.seq);
    if since >= cursor {
        return Ok(WatchPoll {
            cursor,
            updates: Vec::new(),
        });
    }

    // State at the cursor vs now, display-shaped for consistent field names in the
    // filter and diff - but WITHOUT the timestamp/computed-column injection `read`
    // adds, so a diff shows only real mutations.
    let since_muts: Vec<_> = muts.iter().filter(|e| e.seq <= since).cloned().collect();
    let mut before = materialize(config, &baseline, &since_muts);
    let mut after = materialize(config, &baseline, &muts);
    // Names checked against THIS snapshot (a frontend may skip `validate`).
    let vocab = Vocabulary::build(config, &after, &muts);
    let fields = list::criteria_fields(criteria)?;
    vocab.check(
        &fields.iter().map(String::as_str).collect::<Vec<_>>(),
        ColumnUse::Filter,
    )?;
    list::check_ids(store, &after, &list::bare_ids(criteria))?;
    let watched = if columns.is_empty() {
        None
    } else {
        Some(resolve_columns(config, &vocab, columns)?)
    };
    read::rename_to_display(&mut before, config);
    read::rename_to_display(&mut after, config);

    // Only tasks actually touched by a newer event can have changed.
    let touched: BTreeSet<&str> = muts
        .iter()
        .filter(|e| e.seq > since)
        .map(|e| e.task_id.as_str())
        .collect();

    // Matching either side counts: entering the set, leaving it (including by
    // deletion), or changing while inside it.
    let matched_now = list::matching_ids(store, &after, criteria, open, ready)?;
    let matched_then = list::matching_ids(store, &before, criteria, open, ready)?;

    let mut updates = Vec::new();
    for id in touched {
        if !(matched_now.contains(id) || matched_then.contains(id)) {
            continue;
        }
        let (was, now) = match &watched {
            None => (before.get(id).cloned(), after.get(id).cloned()),
            Some(cols) => {
                let was = before.get(id).and_then(|t| project(t, cols));
                let now = after.get(id).and_then(|t| project(t, cols));
                if was == now {
                    continue; // only unwatched columns changed
                }
                (was, now)
            }
        };
        updates.push(WatchUpdate {
            id: id.to_string(),
            before: was,
            after: now,
        });
    }
    Ok(WatchPoll { cursor, updates })
}

/// Check a watch's criteria and `columns` against the store before blocking on
/// it, so a name that can never match is an immediate error rather than a
/// silent wait until the timeout. See [`poll`].
///
/// # Errors
/// A malformed criterion, an unknown filter field, or an unwatchable column.
pub fn validate(
    store: &impl EventStore,
    criteria: &[String],
    columns: &[String],
) -> Result<(), DynError> {
    let fields = list::criteria_fields(criteria)?;
    let names: Vec<&str> = fields.iter().map(String::as_str).collect();
    let watched: Vec<&str> = columns.iter().map(String::as_str).collect();
    let quick = Vocabulary::from_config(store.config());
    let vocab =
        if quick.covers(&names, ColumnUse::Filter) && quick.covers(&watched, ColumnUse::List) {
            quick
        } else {
            full_vocabulary(store)?
        };
    vocab.check(&names, ColumnUse::Filter)?;
    resolve_columns(store.config(), &vocab, columns)?;
    let ids = list::bare_ids(criteria);
    if !ids.is_empty() {
        let config = store.config();
        let state = materialize(config, &store.load_baseline()?, &store.load_mutations()?);
        list::check_ids(store, &state, &ids)?;
    }
    Ok(())
}

/// The watched set named by `columns` (display names).
///
/// Watchable = any column of the store's vocabulary EXCEPT the computed ones -
/// watch compares stored values, so a timestamp or graph column could never
/// fire. `id` is accepted (it never changes). A relationship name, or an unknown
/// one, gets the shared vocabulary error (`deps` / did-you-mean).
pub(crate) fn resolve_columns(
    config: &Config,
    vocab: &Vocabulary,
    columns: &[String],
) -> Result<BTreeSet<String>, DynError> {
    let ts = &config.timestamps;
    let computed = [
        ts.create_time.as_str(),
        ts.update_time.as_str(),
        ts.close_time.as_str(),
        UNBLOCKS_KEY,
        BLOCKED_BY_KEY,
        SUBTASKS_KEY,
    ];
    for col in columns {
        if !col.is_empty() && computed.contains(&col.as_str()) {
            return Err(format!(
                "`{col}` is a computed column - watch compares stored values only, so it \
                 can't watch it"
            )
            .into());
        }
        vocab.check(&[col.as_str()], ColumnUse::List)?;
    }
    Ok(columns.iter().cloned().collect())
}

/// `task` narrowed to the watched columns (`deps` keeps its relationships), or
/// `None` when it carries none of them - so a task gaining its first watched
/// value reads as an addition, and one without any is no change at all.
pub(crate) fn project(task: &TaskState, columns: &BTreeSet<String>) -> Option<TaskState> {
    let custom_fields: serde_json::Map<String, serde_json::Value> = task
        .custom_fields
        .iter()
        .filter(|(k, _)| columns.contains(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let relationships = if columns.contains(DEPS_KEY) {
        task.relationships.clone()
    } else {
        BTreeMap::new()
    };
    if custom_fields.is_empty() && relationships.values().all(Vec::is_empty) {
        return None;
    }
    Some(TaskState {
        id: task.id.clone(),
        relationships,
        custom_fields,
        create_time: None,
        update_time: None,
        close_time: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::model::{MutationEvent, OpType};
    use crate::storage::EventStore;
    use crate::test_support::InMemoryStore;

    #[test]
    fn compaction_watermark_handles_boundary_and_large_cursor() {
        let store = InMemoryStore::default();
        let mut event = MutationEvent::new(OpType::Create, "a", serde_json::Map::new());
        event.seq = 10;
        store.compact(&[], &[event]).unwrap();

        assert!(
            poll(&store, &[], false, false, 8, &[]).is_err(),
            "cursor before the retained window is rejected"
        );
        let polled = poll(&store, &[], false, false, 9, &[]).unwrap();
        assert_eq!(
            polled.updates.len(),
            1,
            "cursor exactly before the oldest retained event can be diffed"
        );
        assert_eq!(
            polled.cursor, 10,
            "the cursor is the snapshot's high-water seq"
        );
        assert!(
            poll(&store, &[], false, false, u64::MAX, &[])
                .unwrap()
                .updates
                .is_empty(),
            "a huge cursor is a clean no-op, not an overflow panic"
        );
    }
}
