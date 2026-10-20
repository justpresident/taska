//! `watch` action: the tasks that changed since a cursor, matching a filter.
//!
//! Diffs state as-of `since` against the current state, restricted to tasks
//! touched by events newer than `since` and matching the list-style filter, and
//! returns each such task's before/after state - the frontend renders the
//! per-field diff (via `format::render_state_diff`, shared with `undo`), plus
//! the cursor that snapshot was taken at. No
//! blocking or polling here: that (and the holdout debounce) is the CLI's job.
//! This is one pure snapshot.

use std::collections::BTreeSet;

use crate::action::{list, materialize, read};
use crate::error::DynError;
use crate::model::TaskState;
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
pub fn poll(
    store: &impl EventStore,
    criteria: &[String],
    open: bool,
    ready: bool,
    since: u64,
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
        if matched_now.contains(id) || matched_then.contains(id) {
            updates.push(WatchUpdate {
                id: id.to_string(),
                before: before.get(id).cloned(),
                after: after.get(id).cloned(),
            });
        }
    }
    Ok(WatchPoll { cursor, updates })
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
            poll(&store, &[], false, false, 8).is_err(),
            "cursor before the retained window is rejected"
        );
        let polled = poll(&store, &[], false, false, 9).unwrap();
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
            poll(&store, &[], false, false, u64::MAX)
                .unwrap()
                .updates
                .is_empty(),
            "a huge cursor is a clean no-op, not an overflow panic"
        );
    }
}
