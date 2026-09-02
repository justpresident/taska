//! The frontend-agnostic READ pipeline.
//!
//! Materializes a store into display-ready task state, surfacing non-fatal
//! conditions as [`Warning`] DATA rather than printing them - the frontend
//! decides whether that's a stderr line, a TUI status bar, or nothing at all.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::engine::Engine;
use crate::error::DynError;
use crate::model::TaskState;
use crate::schema;
use crate::schema::{schema_conformance_report, substitute_schema_defaults};
use crate::storage::EventStore;

/// A read of the store: the materialized, display-shaped task state plus any
/// [`Warning`]s the read surfaced.
pub struct Session {
    pub state: HashMap<String, TaskState>,
    pub warnings: Vec<Warning>,
    /// The log's high-water `seq` at read time - the cursor this state is as-of
    /// (0 on an empty store). The same value `status --current` prints and a
    /// `ta watch --since` loop advances.
    pub seq: u64,
}

/// A non-fatal condition a read surfaced, carrying the DATA a frontend needs to
/// render its own message.
pub enum Warning {
    /// `n` events in the log target a task that doesn't exist - a dropped
    /// `Create` (merge removal-union, a revert, or a hand edit). Cleared by the
    /// `resolve` action.
    Orphans(usize),
    /// Tasks that don't conform to their `[task_types]` schema, one report line
    /// each. Read-tolerated by design (schemas are write-time law); a write to
    /// such a task must bring it into conformance.
    NonConformance(Vec<String>),
}

/// Materialize the store into display-ready state.
///
/// Orphaned events and schema nonconformance become [`Warning`]s (computed on
/// RAW state, before the shaping below would skew the conformance check);
/// declared defaults are substituted; computed timestamps are injected under
/// their configured names; and canonical storage keys are renamed to their
/// display names. This is the inverse of the write-side canonicalization, and
/// every frontend reads through it - so columns/filters/sort see display names
/// while the log keeps canonical keys, which is what makes the names freely
/// renamable in config.
pub fn read(store: &impl EventStore) -> Result<Session, DynError> {
    let config = store.config();
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    // The log is strictly increasing by `seq`, so the last event carries the
    // high-water mark - the cursor this read is as-of.
    let seq = mutations.last().map_or(0, |e| e.seq);
    let (mut state, orphans) =
        Engine::materialize_report(baseline, mutations, &config.workflow.done_status);

    let mut warnings = Vec::new();
    if !orphans.is_empty() {
        warnings.push(Warning::Orphans(orphans.len()));
    }
    // Nonconformance is gated by config (a frontend wanting it silent sets
    // `workflow.warn_nonconforming = false`) and computed on RAW state.
    if config.workflow.warn_nonconforming {
        let report = schema_conformance_report(&state, config);
        if !report.is_empty() {
            warnings.push(Warning::NonConformance(report));
        }
    }

    // Missing/invalid declared fields READ as their declared default (after the
    // warning above, so the report reflects the stored truth) - display-only,
    // like everything below.
    substitute_schema_defaults(&mut state, config);

    // Surface the computed timestamps as ordinary RFC 3339 string fields, so
    // list/show/--sort treat them like any other column. The raw Option<DateTime>
    // stays on TaskState; injection never reaches the stored log.
    let ts = &config.timestamps;
    for task in state.values_mut() {
        inject_time(&mut task.custom_fields, &ts.create_time, task.create_time);
        inject_time(&mut task.custom_fields, &ts.update_time, task.update_time);
        inject_time(&mut task.custom_fields, &ts.close_time, task.close_time);
    }

    rename_to_display(&mut state, config);

    Ok(Session {
        state,
        warnings,
        seq,
    })
}

/// Rename the canonical storage keys (status/type) to their configured display
/// names in `state`, in place - the read-side half of the canonical<->display
/// boundary. `read` applies it after its other shaping; `watch` reuses it to shape
/// state materialized at a cursor WITHOUT the timestamp/computed-column injection,
/// which would otherwise pollute a diff with non-mutation "changes".
pub(crate) fn rename_to_display(state: &mut HashMap<String, TaskState>, config: &Config) {
    for task in state.values_mut() {
        schema::rename_to_display(&mut task.custom_fields, &config.workflow);
    }
}

/// Insert a computed timestamp as an RFC 3339 string field under `name`, so it
/// renders/searches/sorts like any field. A blank `name` disables that
/// timestamp; a `None` value (e.g. `close_time` on an open task) injects nothing
/// (the omit-absent-fields rule).
fn inject_time(fields: &mut Map<String, Value>, name: &str, value: Option<DateTime<Utc>>) {
    if name.is_empty() {
        return;
    }
    if let Some(t) = value {
        fields.insert(name.to_string(), Value::String(t.to_rfc3339()));
    }
}
