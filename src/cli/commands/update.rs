//! `ta update` — set (`=`) and/or append to (`+=`) fields on a task.

use crate::cli::parse_field_ops;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_update(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let (set, append) = parse_field_ops(fields)?;
    // Emit the set (`Update`) before the append (`Append`) so a same-field
    // `field=reset field+=add` applies the reset first, then accumulates onto it —
    // independent of token order on the command line. A mix yields two events; one
    // operator yields one.
    let mut events = Vec::new();
    if !set.is_empty() {
        events.push(MutationEvent::new(OpType::Update, id, set));
    }
    if !append.is_empty() {
        events.push(MutationEvent::new(OpType::Append, id, append));
    }
    store.append_events(&events)?;
    println!("Updated task `{id}`");
    Ok(())
}
