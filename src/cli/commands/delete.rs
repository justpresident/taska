//! `ta delete` - remove a task via the shared write path.

use crate::error::DynError;
use crate::storage::EventStore;

pub fn cmd_delete(store: &impl EventStore, id: &str, guard: &[String]) -> Result<(), DynError> {
    let outcome = crate::action::write::delete(store, id, guard)?;
    let seq = outcome.written.last().map_or(0, |e| e.seq);
    println!("[seq:{seq}] Deleted task `{id}`");
    Ok(())
}
