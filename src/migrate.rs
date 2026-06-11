//! On-disk format migrations, run by `ta repair --migrate`.
//!
//! Migrations are an ordered, append-only list of idempotent **passes**. Each is
//! a discrete `vN → vN+1` transform over the raw store snapshot (log + baseline).
//! `repair --migrate` runs every pass in sequence, so a store several versions
//! behind is brought fully current in one go, and a new migration is added simply
//! by appending to [`MIGRATIONS`]. Each pass:
//! - reports (via [`Migration::pending`]) whether it has anything to do, cheaply,
//!   so the read path can detect a stale store without transforming it; and
//! - is a **no-op on already-current data** (so re-running `--migrate`, or running
//!   it on a partially-migrated store, is safe).
//!
//! These passes cover **v1.0+** formats only. A PRE-1.0 store (legacy `depends_on`
//! edges, `AddDep`/`RemoveDep` ops, `dep`/`type` payload keys, or a top-level
//! `depends_on` baseline field) is NOT migrated here — those shims were dropped
//! at v1. Upgrade such a store by running `ta repair --migrate` with the last 0.x
//! release first; [`crate::storage::FileStore::detect_legacy_format`] refuses one
//! with exactly that hint.

use crate::config::Config;
use crate::model::{MutationEvent, OpType, TaskState, STATUS_KEY};

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

/// Every migration, oldest first. **Append new passes here** — never reorder or
/// remove, since a store may be at any version at or above the v1.0 floor.
pub const MIGRATIONS: &[Migration] = &[Migration {
    id: "canonical-status-key",
    pending: pending_canonical_status_key,
    apply: apply_canonical_status_key,
}];

/// The first thing a stale store needs, if any — for the read path to surface
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

/// Whether a field-carrying event stores the status under the configured
/// DISPLAY name instead of the canonical [`STATUS_KEY`] — the pre-canonical
/// behavior of a store whose `[workflow] status_field` was renamed: back then
/// the configured name WAS the storage key. Skips (defensively) any event that
/// already carries a canonical key too: re-keying would clobber it, and such an
/// event can only be hand-made.
fn stores_status_under_display_name(event: &MutationEvent, display: &str) -> bool {
    matches!(event.op, OpType::Create | OpType::Update | OpType::Append)
        && event.payload.contains_key(display)
        && !event.payload.contains_key(STATUS_KEY)
}

fn pending_canonical_status_key(snap: &Snapshot, config: &Config) -> Option<String> {
    let display = &config.workflow.status_field;
    if display == STATUS_KEY {
        return None; // default name: storage was always canonical
    }
    let n = snap
        .log
        .iter()
        .filter(|e| stores_status_under_display_name(e, display))
        .count()
        + snap
            .baseline
            .iter()
            .filter(|t| {
                t.custom_fields.contains_key(display.as_str())
                    && !t.custom_fields.contains_key(STATUS_KEY)
            })
            .count();
    (n > 0).then(|| {
        format!(
            "{n} record(s) store the status under its display name `{display}`; \
             the storage key is now canonical `{STATUS_KEY}`"
        )
    })
}

/// Re-key the status from the configured display name to the canonical
/// [`STATUS_KEY`] in event payloads and baseline tasks. Before storage became
/// canonical, renaming `[workflow] status_field` silently orphaned old data;
/// after this pass the display name is pure presentation and renames are free.
fn apply_canonical_status_key(snap: &mut Snapshot, config: &Config) -> usize {
    let display = config.workflow.status_field.clone();
    if display == STATUS_KEY {
        return 0;
    }
    let mut changed = 0;
    for event in &mut snap.log {
        if stores_status_under_display_name(event, &display) {
            if let Some(value) = event.payload.remove(display.as_str()) {
                event.payload.insert(STATUS_KEY.to_string(), value);
                changed += 1;
            }
        }
    }
    for task in &mut snap.baseline {
        if task.custom_fields.contains_key(STATUS_KEY) {
            continue;
        }
        if let Some(value) = task.custom_fields.remove(display.as_str()) {
            task.custom_fields.insert(STATUS_KEY.to_string(), value);
            changed += 1;
        }
    }
    changed
}
