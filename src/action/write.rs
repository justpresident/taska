//! The shared write choreography.
//!
//! Every task write funnels through one verify-then-append path -
//! materialize the current state, build/coerce the events, run the schema gate
//! ([`vet_events`]), and append, all under the store lock so the checks can't
//! race a concurrent writer. A single implementation serves every frontend; the
//! payloads arrive in CANONICAL form (the frontend maps its display names first).

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::action::materialize;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType, STATUS_KEY};
use crate::schema::{
    build_field_events, coerce_event_fields, schema_default_stamps, vet_events, FieldOps,
};
use crate::storage::EventStore;

/// Create a new task from a CANONICAL payload (plus the `raw` inline-token map
/// backing declared-string coercion).
///
/// Stamps the workflow default status (unless the payload already names it - even
/// as JSON `null`, the explicit-unset convention) and the task type's schema
/// defaults, then coerces, vets (which rejects a duplicate create atomically),
/// and appends. A create is never a no-op, so on success it always wrote.
pub fn create(
    store: &impl EventStore,
    id: &str,
    mut payload: Map<String, Value>,
    raw: &Map<String, Value>,
) -> Result<(), DynError> {
    let workflow = &store.config().workflow;
    if !workflow.default_status.is_empty() && !payload.contains_key(STATUS_KEY) {
        payload.insert(
            STATUS_KEY.to_string(),
            Value::String(workflow.default_status.clone()),
        );
    }
    // The declared schema defaults (same convention: an explicit value - or null -
    // in the payload wins over the default).
    let stamps = schema_default_stamps(None, &payload, &BTreeSet::default(), store.config());
    for (key, value) in stamps {
        payload.insert(key, value);
    }

    let draft = MutationEvent::new(OpType::Create, id, payload);
    let config = store.config().clone();
    store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        let mut events = vec![draft.clone()];
        coerce_event_fields(&mut events, raw, &state, &config);
        vet_events(&events, &state, &config)
    })?;
    Ok(())
}

/// Apply CANONICAL field ops to an existing task, returning the events actually
/// written (empty = nothing changed).
///
/// Builds the events (the `+=`/`-=` dispatch, schema coercion, and heal-on-write
/// defaults), then vets - which errors if the task doesn't exist and drops no-op
/// writes (re-asserting a value, inserting a present element, adding 0).
pub fn update(
    store: &impl EventStore,
    id: &str,
    ops: &FieldOps,
) -> Result<Vec<MutationEvent>, DynError> {
    let config = store.config().clone();
    store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        let events = build_field_events(id, ops, &state, &config)?;
        vet_events(&events, &state, &config)
    })
}

/// Delete a task. Deleting a missing task is a typo, so it errors (under the lock)
/// rather than appending a `Delete` that applies to nothing.
pub fn delete(store: &impl EventStore, id: &str) -> Result<(), DynError> {
    let draft = MutationEvent::new(OpType::Delete, id, Map::new());
    let config = store.config().clone();
    store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        vet_events(std::slice::from_ref(&draft), &state, &config)
    })?;
    Ok(())
}
