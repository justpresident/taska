//! `ta update` — append an `Update` event setting fields on a task.

use crate::cli::parse_fields;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_update(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let payload = parse_fields(fields)?;
    store.append_events(&[MutationEvent::new(OpType::Update, id, payload)])?;
    println!("Updated task `{id}`");
    Ok(())
}
