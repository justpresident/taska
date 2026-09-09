//! `ta delete` - remove a task via the shared write path.

use crate::error::DynError;
use crate::format::{seq_tag, task_ref, want_color};
use crate::storage::EventStore;

pub fn cmd_delete(store: &impl EventStore, id: &str, guard: &[String]) -> Result<(), DynError> {
    let outcome = crate::action::write::delete(store, id, guard)?;
    let seq = outcome.written.last().map_or(0, |e| e.seq);
    let color = want_color(false);
    println!(
        "{} Deleted task {}",
        seq_tag(seq, color),
        task_ref(id, color)
    );
    Ok(())
}
