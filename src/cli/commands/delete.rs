//! `ta delete` — append a `Delete` event removing a task.

use serde_json::Map;

use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_delete(store: &impl EventStore, id: &str) -> Result<(), DynError> {
    store.append_events(&[MutationEvent::new(OpType::Delete, id, Map::new())])?;
    println!("Deleted task `{id}`");
    Ok(())
}
