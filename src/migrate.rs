//! On-disk format migrations, run by `ta repair --migrate`.
//!
//! A migration is an idempotent **pass** - a discrete `vN -> vN+1` transform over
//! the raw store snapshot (log + baseline). `repair --migrate` runs every pass in
//! [`MIGRATIONS`] in sequence, so a store several versions behind is brought fully
//! current in one go; a new migration is added by appending to that list. Each
//! pass reports (via [`Migration::pending`]) whether it has anything to do - so
//! the read path can flag a stale store without transforming it - and is a no-op
//! on already-current data.
//!
//! **There are currently no passes:** v1.0 is the format floor, and every pre-1.0
//! migration was dropped at the cut. A PRE-1.0 store (legacy `depends_on` edges,
//! `AddDep`/`RemoveDep` ops, `dep`/`type` payload keys, a top-level `depends_on`
//! baseline field, or a display-named status key) is NOT migrated here - upgrade
//! it by running `ta repair --migrate` with the last 0.x release first;
//! [`crate::storage::FileStore::detect_legacy_format`] refuses one with that hint.
//! New v1.0+ format migrations go in [`MIGRATIONS`].

use crate::config::Config;
use crate::model::{MutationEvent, TaskState};

/// The mutable store contents the passes rewrite.
pub struct Snapshot {
    pub log: Vec<MutationEvent>,
    pub baseline: Vec<TaskState>,
}

/// One migration pass. Passes stack: see the module docs.
pub struct Migration {
    /// Stable id, shown in the `repair` report.
    pub id: &'static str,
    /// Cheap check: `Some(reason)` if this pass would change `snap`, else `None`.
    pending: fn(&Snapshot, &Config) -> Option<String>,
    /// Apply the pass in place; return the number of changes (0 = no-op).
    apply: fn(&mut Snapshot, &Config) -> usize,
}

/// Every migration, oldest first.
///
/// **Append new passes here** - never reorder or remove, since a store may be at
/// any version at or above the v1.0 floor. Empty today: v1.0 is the floor, so
/// there is nothing to migrate within the 1.x line yet.
pub const MIGRATIONS: &[Migration] = &[];

/// The first thing a stale store needs, if any - for the read path to surface
/// "run `ta repair --migrate`" before doing work, without rewriting anything.
pub fn pending(snap: &Snapshot, config: &Config) -> Option<String> {
    MIGRATIONS.iter().find_map(|m| (m.pending)(snap, config))
}

/// Run every pass in order, returning a per-pass `(id, changes)` report for the
/// passes that did something. The caller persists `snap` if the report is
/// non-empty.
pub fn run_all(snap: &mut Snapshot, config: &Config) -> Vec<(&'static str, usize)> {
    MIGRATIONS
        .iter()
        .filter_map(|m| {
            let changed = (m.apply)(snap, config);
            (changed > 0).then_some((m.id, changed))
        })
        .collect()
}
