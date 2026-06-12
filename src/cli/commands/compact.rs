//! `ta compact` - fold old events into the baseline snapshot.

use chrono::{DateTime, Utc};

use crate::action::compact::{compact, CompactOutcome};
use crate::config::CompactionConfig;
use crate::error::DynError;
use crate::storage::EventStore;

pub fn cmd_compact(
    store: &impl EventStore,
    cfg: &CompactionConfig,
    now: DateTime<Utc>,
) -> Result<(), DynError> {
    match compact(store, cfg, now)? {
        CompactOutcome::NothingToDo {
            log_len,
            keep_events,
        } => {
            println!("Nothing to compact ({log_len} event(s) in log, keep_events = {keep_events})");
        }
        CompactOutcome::Compacted {
            folded,
            baseline_tasks,
            kept,
        } => println!(
            "Compacted {folded} event(s) into baseline ({baseline_tasks} task(s)); \
             kept {kept} recent event(s)"
        ),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::action::read;
    use crate::cli::commands::create::cmd_create;
    use crate::test_support::InMemoryStore;

    #[test]
    fn compact_folds_log_into_baseline() {
        let store = InMemoryStore::default();
        cmd_create(&store, "a", &[]).unwrap();
        cmd_create(&store, "b", &[]).unwrap();
        // keep_events = 0 still retains the most recent event (the log never
        // empties, so the seq watermark stays derivable); the rest folds.
        let cfg = CompactionConfig {
            keep_events: 0,
            keep_days: 0,
        };
        cmd_compact(&store, &cfg, Utc::now()).unwrap();
        assert_eq!(
            store.load_mutations().unwrap().len(),
            1,
            "one event retained"
        );
        assert_eq!(store.load_baseline().unwrap().len(), 1, "the rest folded");
        // A later Create still appends to the log and overlays the baseline post-compaction.
        cmd_create(&store, "c", &[]).unwrap();
        assert_eq!(read(&store).unwrap().state.len(), 3);
    }

    #[test]
    fn compact_retains_recent_events() {
        let store = InMemoryStore::default();
        for id in ["a", "b", "c", "d", "e"] {
            cmd_create(&store, id, &[]).unwrap();
        }
        // Keep the 2 most recent, time window off.
        let cfg = CompactionConfig {
            keep_events: 2,
            keep_days: 0,
        };
        cmd_compact(&store, &cfg, Utc::now()).unwrap();
        assert_eq!(
            store.load_mutations().unwrap().len(),
            2,
            "kept 2 recent events"
        );
        assert_eq!(
            store.load_baseline().unwrap().len(),
            3,
            "folded 3 into baseline"
        );
        assert_eq!(
            read(&store).unwrap().state.len(),
            5,
            "all tasks still visible"
        );
    }
}
