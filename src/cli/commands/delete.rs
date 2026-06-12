//! `ta delete` - remove a task via the shared write path.

use crate::error::DynError;
use crate::storage::EventStore;

pub fn cmd_delete(store: &impl EventStore, id: &str) -> Result<(), DynError> {
    crate::action::write::delete(store, id)?;
    println!("Deleted task `{id}`");
    Ok(())
}
