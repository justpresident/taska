//! `ta update` — set (or, with `--append`, append to) fields on a task.

use crate::cli::parse_fields;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_update(
    store: &impl EventStore,
    id: &str,
    fields: &[String],
    append: bool,
) -> Result<(), DynError> {
    let payload = parse_fields(fields)?;
    let op = if append {
        OpType::Append
    } else {
        OpType::Update
    };
    store.append_events(&[MutationEvent::new(op, id, payload)])?;
    println!(
        "{} task `{id}`",
        if append { "Appended to" } else { "Updated" }
    );
    Ok(())
}
