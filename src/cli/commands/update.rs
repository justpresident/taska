//! `ta update` — set (`=`), accumulate (`+=`), or remove (`-=`) fields on a task.

use crate::cli::{
    build_field_events, canonicalize_fields, materialize, parse_field_ops, vet_events,
};
use crate::error::DynError;
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
    // Verify-then-append under the store lock: event building first (the
    // `+=`/`-=` dispatch and schema-aware coercion need the task's type from
    // live state), then vetting — which errors if the task doesn't exist and
    // drops no-op writes (re-asserting a value, inserting a present set
    // element, adding 0) so they never bloat the log.
    let config = store.config().clone();
    let written = store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        let events = build_field_events(id, &ops, &state, &config)?;
        vet_events(&events, &state, &config)
    })?;
    if written.is_empty() {
        println!("`{id}` already up to date — no changes");
    } else {
        println!("Updated task `{id}`");
    }
    Ok(())
}
