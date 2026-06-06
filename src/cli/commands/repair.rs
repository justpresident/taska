//! `ta repair` — bring a store up to the current on-disk format.

use crate::error::DynError;
use crate::migrate::{run_all, Snapshot};
use crate::storage::EventStore;

pub fn cmd_repair(store: &impl EventStore, migrate: bool) -> Result<(), DynError> {
    if !migrate {
        println!("Nothing to do. Pass `--migrate` to update the store's on-disk format.");
        return Ok(());
    }
    let mut snap = Snapshot {
        log: store.load_mutations()?,
        baseline: store.load_baseline()?,
    };
    let report = run_all(&mut snap, store.config());
    if report.is_empty() {
        println!("Already up to date; nothing to migrate.");
        return Ok(());
    }
    // Rewrite both files in the current format under the lock. `compact` given the
    // *full* log folds nothing — it's just a normalized rewrite of log + baseline
    // (the baseline was read through the format-compat path, so it's already
    // current in memory).
    store.compact(&snap.baseline, &snap.log)?;
    for (id, count) in &report {
        println!("migrated `{id}`: {count} event(s)");
    }
    println!("Done.");
    Ok(())
}
