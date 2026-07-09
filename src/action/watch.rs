//! `watch` action: the tasks that changed since a cursor, matching a filter.
//!
//! Diffs state as-of `since` against the current state, restricted to tasks
//! touched by events newer than `since` and matching the list-style filter, and
//! returns each such task's before/after state - the frontend renders the
//! per-field diff (via `format::render_state_diff`, shared with `undo`). No
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

/// The tasks that changed since `since` (events with `seq > since`) and match the
/// filter, each with its before/after state (ordered by id). Empty when nothing
/// new matches.
///
/// An existing task is filtered on its current state; a task deleted since the
/// cursor is filtered on its at-cursor state, so a matching task's deletion is
/// still reported. Errors if `since` predates the retained log (compacted away).
pub fn poll(
    store: &impl EventStore,
    criteria: &[String],
    open: bool,
    ready: bool,
    since: u64,
) -> Result<Vec<WatchUpdate>, DynError> {
    let config = store.config();
    let baseline = store.load_baseline()?;
    let muts = store.load_mutations()?;

    // Compaction watermark: if the window we need (seq > since) dips below the
    // oldest retained event, those events were folded into the baseline and can't
    // be diffed. A once-in-a-store-lifetime edge; error rather than mislead.
    if let Some(first) = muts.first() {
        if since + 1 < first.seq {
            return Err(format!(
                "cursor seq {since} predates the retained log (oldest is {}); those \
                 events were compacted - re-sync with `ta status --current`",
                first.seq
            )
            .into());
        }
    }

    // Fast path: nothing newer than the cursor.
    let max_seq = muts.last().map_or(0, |e| e.seq);
    if since >= max_seq {
        return Ok(Vec::new());
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

    // Filter each side: an existing task on its current state, a deleted one on
    // its at-cursor state.
    let matched_now = list::matching_ids(store, &after, criteria, open, ready)?;
    let matched_then = list::matching_ids(store, &before, criteria, open, ready)?;

    let mut updates = Vec::new();
    for id in touched {
        let after_state = after.get(id);
        let matches = if after_state.is_some() {
            matched_now.contains(id)
        } else {
            matched_then.contains(id)
        };
        if matches {
            updates.push(WatchUpdate {
                id: id.to_string(),
                before: before.get(id).cloned(),
                after: after_state.cloned(),
            });
        }
    }
    Ok(updates)
}
