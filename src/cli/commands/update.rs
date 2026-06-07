//! `ta update` — set (`=`) and/or append to (`+=`) fields on a task.

use crate::cli::{canonicalize_fields, materialize, parse_field_ops, vet_events};
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_update(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let (mut set, mut append) = parse_field_ops(fields)?;
    // Display names map onto their canonical storage keys (the append map too:
    // a renamed `state+=x` must hit the same single-valued-status rejection
    // that `status+=x` does under the default name).
    let workflow = &store.config().workflow;
    canonicalize_fields(&mut set, workflow)?;
    canonicalize_fields(&mut append, workflow)?;
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
    // Verify-then-append under the store lock: errors if the task doesn't exist,
    // and drops fields already at their target value (so re-asserting the same
    // value writes nothing rather than bloating the log).
    let config = store.config().clone();
    let written = store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        vet_events(&events, &state, &config)
    })?;
    if written.is_empty() {
        println!("`{id}` already up to date — no changes");
    } else {
        println!("Updated task `{id}`");
    }
    Ok(())
}
