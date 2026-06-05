//! `ta delete` — append a `Delete` event removing a task.

use serde_json::Map;

use crate::cli::vet_events;
use crate::engine::Engine;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_delete(store: &impl EventStore, id: &str) -> Result<(), DynError> {
    // Verify-then-append under the store lock: deleting a missing task is a typo,
    // so it errors rather than writing a Delete event that applies to nothing.
    let draft = MutationEvent::new(OpType::Delete, id, Map::new());
    let config = store.config().clone();
    store.append_checked(&|baseline, log| {
        let state = Engine::materialize_state(
            baseline.to_vec(),
            log.to_vec(),
            &config.workflow.status_field,
            &config.workflow.done_status,
        );
        vet_events(std::slice::from_ref(&draft), &state, &config)
    })?;
    println!("Deleted task `{id}`");
    Ok(())
}
