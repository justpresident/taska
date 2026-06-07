//! `ta update` — set (`=`) and/or append to (`+=`) fields on a task.

use crate::cli::{
    canonicalize_fields, coerce_event_fields, materialize, parse_field_ops, vet_events, FieldOps,
};
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_update(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let FieldOps {
        mut set,
        mut append,
        mut raw,
    } = parse_field_ops(fields)?;
    // Display names map onto their canonical storage keys (the append map too:
    // a renamed `state+=x` must hit the same single-valued-status rejection
    // that `status+=x` does under the default name; `raw` keeps its keys
    // aligned with `set` for the schema-aware coercion below).
    let workflow = &store.config().workflow;
    canonicalize_fields(&mut set, workflow)?;
    canonicalize_fields(&mut append, workflow)?;
    canonicalize_fields(&mut raw, workflow)?;
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
    // Verify-then-append under the store lock: schema-aware coercion first
    // (it needs the task's type from the live state), then vetting — which
    // errors if the task doesn't exist and drops fields already at their
    // target value (so re-asserting the same value writes nothing).
    let config = store.config().clone();
    let written = store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        let mut events = events.clone();
        coerce_event_fields(&mut events, &raw, &state, &config);
        vet_events(&events, &state, &config)
    })?;
    if written.is_empty() {
        println!("`{id}` already up to date — no changes");
    } else {
        println!("Updated task `{id}`");
    }
    Ok(())
}
