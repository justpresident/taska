//! `ta delete` - remove a task via the shared write path.

use crate::error::DynError;
use crate::format::{cursor_tag, task_ref, want_color};
use crate::storage::EventStore;

pub fn cmd_delete(store: &impl EventStore, id: &str, guard: &[String]) -> Result<(), DynError> {
    let outcome = crate::action::write::delete(store, id, guard)?;
    let color = want_color(false);
    println!(
        "{} Deleted task {}",
        cursor_tag(outcome.cursor, color),
        task_ref(id, color)
    );
    Ok(())
}
