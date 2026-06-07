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

use serde_json::Value;

use crate::config::Config;
use crate::model::{
    MutationEvent, OpType, TaskState, LEGACY_REL_KEY, LEGACY_TARGET_KEY, REL_KEY, TARGET_KEY,
};

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
/// remove, since a store may be at any earlier version.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        id: "typed-dep-edges",
        pending: pending_typed_dep_edges,
        apply: apply_typed_dep_edges,
    },
    Migration {
        id: "edge-vocabulary",
        pending: pending_edge_vocabulary,
        apply: apply_edge_vocabulary,
    },
];

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

/// Whether an edge event predates typed relationships: it carries a rel under
/// neither the current `rel` key nor the legacy `type` key (an untyped edge
/// historically meant the default blocker).
fn is_untyped_dep_event(event: &MutationEvent) -> bool {
    matches!(event.op, OpType::AddEdge | OpType::RemoveEdge)
        && !event.payload.contains_key(REL_KEY)
        && !event.payload.contains_key(LEGACY_REL_KEY)
}

fn pending_typed_dep_edges(snap: &Snapshot, _config: &Config) -> Option<String> {
    let n = snap.log.iter().filter(|e| is_untyped_dep_event(e)).count();
    (n > 0).then(|| format!("{n} dependency event(s) are in the legacy untyped format"))
}

/// v1: stamp the configured default blocker type onto every untyped dependency
/// event, so the log no longer relies on an implicit `depends_on`. Stamps the
/// current `rel` key; pre-rename `type` stamps from older runs are converted by
/// the `edge-vocabulary` pass that follows.
fn apply_typed_dep_edges(snap: &mut Snapshot, config: &Config) -> usize {
    let Some(default) = config.relationships.default_blocker() else {
        return 0; // `Config::validate` requires one; be defensive.
    };
    let default = default.to_string();
    let mut changed = 0;
    for event in &mut snap.log {
        if is_untyped_dep_event(event) {
            event
                .payload
                .insert(REL_KEY.to_string(), Value::String(default.clone()));
            changed += 1;
        }
    }
    changed
}

/// Whether an edge event still uses the pre-rename payload vocabulary
/// (`dep`/`type` instead of `target`/`rel`). The op NAME needs no detection:
/// `AddDep`/`RemoveDep` parse as aliases and re-serialize canonically whenever
/// the log is rewritten — including by this pass.
fn has_legacy_edge_keys(event: &MutationEvent) -> bool {
    matches!(event.op, OpType::AddEdge | OpType::RemoveEdge)
        && (event.payload.contains_key(LEGACY_TARGET_KEY)
            || event.payload.contains_key(LEGACY_REL_KEY))
}

fn pending_edge_vocabulary(snap: &Snapshot, _config: &Config) -> Option<String> {
    let n = snap.log.iter().filter(|e| has_legacy_edge_keys(e)).count();
    (n > 0).then(|| format!("{n} edge event(s) use the legacy `dep`/`type` payload keys"))
}

/// v2: rename the edge payload keys `dep` -> `target` and `type` -> `rel`
/// (defensively keeping an already-present current key over a stray legacy
/// duplicate). Re-serialization normalizes the op spellings in the same write.
fn apply_edge_vocabulary(snap: &mut Snapshot, _config: &Config) -> usize {
    let mut changed = 0;
    for event in &mut snap.log {
        if !has_legacy_edge_keys(event) {
            continue;
        }
        for (legacy, current) in [(LEGACY_TARGET_KEY, TARGET_KEY), (LEGACY_REL_KEY, REL_KEY)] {
            if let Some(value) = event.payload.remove(legacy) {
                event.payload.entry(current.to_string()).or_insert(value);
            }
        }
        changed += 1;
    }
    changed
}
