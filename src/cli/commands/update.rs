//! `ta update` — set (`=`), accumulate (`+=`), or remove (`-=`) fields on a task.

use crate::cli::parse_field_ops;
use crate::error::DynError;
use crate::schema::canonicalize_fields;
use crate::storage::EventStore;

pub fn cmd_update(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let mut ops = parse_field_ops(fields)?;
    // Display names map onto their canonical storage keys (every map: a
    // renamed `state+=x` must hit the same single-valued rejection that
    // `status+=x` does under the default name; `raw` keeps its keys aligned
    // with `set` for the schema-aware coercion).
    let workflow = &store.config().workflow;
    for map in [
        &mut ops.set,
        &mut ops.append,
        &mut ops.subtract,
        &mut ops.raw,
    ] {
        canonicalize_fields(map, workflow)?;
    }
    let written = crate::action::write::update(store, id, &ops)?;
    if written.is_empty() {
        println!("`{id}` already up to date — no changes");
    } else {
        println!("Updated task `{id}`");
    }
    Ok(())
}
