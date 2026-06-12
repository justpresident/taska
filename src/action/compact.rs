//! `compact` action: fold the old prefix of the log into the baseline snapshot.

use chrono::{DateTime, Utc};

use crate::action::materialize;
use crate::config::CompactionConfig;
use crate::engine::Engine;
use crate::error::DynError;
use crate::model::TaskState;
use crate::storage::EventStore;

/// What a `compact` did - enough for a frontend to report it.
pub enum CompactOutcome {
    /// The log already fits the retention policy; nothing folded.
    NothingToDo { log_len: usize, keep_events: usize },
    /// Folded `folded` events into a baseline of `baseline_tasks`, keeping `kept`
    /// recent events in the log.
    Compacted {
        folded: usize,
        baseline_tasks: usize,
        kept: usize,
    },
}

/// Fold the old prefix of the log into the baseline per the retention policy.
///
/// The recent suffix stays in the log so divergent branches can still be
/// reconciled by event id; the folded baseline is materialized through the
/// store's workflow config so it carries the computed timestamps.
pub fn compact(
    store: &impl EventStore,
    cfg: &CompactionConfig,
    now: DateTime<Utc>,
) -> Result<CompactOutcome, DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;

    let split = Engine::retention_split(&mutations, cfg.keep_events, cfg.keep_days, now);
    if split == 0 {
        return Ok(CompactOutcome::NothingToDo {
            log_len: mutations.len(),
            keep_events: cfg.keep_events,
        });
    }

    let (to_fold, to_keep) = mutations.split_at(split);
    let folded = materialize(store.config(), &baseline, to_fold);
    let mut new_baseline: Vec<TaskState> = folded.into_values().collect();
    new_baseline.sort_by(|a, b| a.id.cmp(&b.id));

    store.compact(&new_baseline, to_keep)?;
    Ok(CompactOutcome::Compacted {
        folded: split,
        baseline_tasks: new_baseline.len(),
        kept: to_keep.len(),
    })
}
