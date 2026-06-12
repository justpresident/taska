//! `ta create` - parse the `key=value` fields, then create via the shared write path.

use std::collections::HashMap;

use serde_json::Map;

use crate::cli::parse_field_ops;
use crate::error::DynError;
use crate::model::TaskState;
use crate::schema::{canonicalize_field_pairs, canonicalize_fields, dispatch_accumulate, FieldOps};
use crate::storage::EventStore;

pub fn cmd_create(store: &impl EventStore, id: &str, fields: &[String]) -> Result<(), DynError> {
    let workflow = &store.config().workflow;
    let FieldOps {
        set: mut payload,
        mut append,
        subtract,
        mut raw,
    } = parse_field_ops(fields)?;
    if let Some((key, _)) = subtract.first() {
        return Err(format!(
            "`{key}-=...` is meaningless on create: a new task has nothing to remove from"
        )
        .into());
    }
    // Display names map onto their canonical storage keys before anything reads
    // the type or stamps defaults (so the event stores `status`/`task_type`
    // whatever they're called on screen); `raw` keeps its keys aligned.
    canonicalize_fields(&mut payload, workflow)?;
    canonicalize_field_pairs(&mut append, workflow)?;
    canonicalize_fields(&mut raw, workflow)?;

    // A new task's fields start absent, so each `+=` is its INITIAL value -
    // accumulate the operands by declared kind exactly as `update` would against
    // an empty field (`tags+=a tags+=b` -> both, `points+=2 points+=3` -> 5,
    // repeated `notes+=` -> joined text), then fold the result into the Create
    // payload. The payload carries the task's type, so the dispatch finds its
    // schema; `-=` was already rejected above.
    let combine = FieldOps {
        set: payload.clone(),
        append,
        subtract: Vec::new(),
        raw: Map::new(),
    };
    let empty: HashMap<String, TaskState> = HashMap::new();
    let (text, add, _remove) = dispatch_accumulate(id, &combine, &empty, store.config())?;
    for (key, value) in text.into_iter().chain(add) {
        payload.insert(key, value);
    }

    crate::action::write::create(store, id, payload, &raw)?;
    println!("Created task `{id}`");
    Ok(())
}
