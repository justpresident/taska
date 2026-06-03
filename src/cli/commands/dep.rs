//! `ta block` / `ta unblock` — add or remove a dependency edge.

use serde_json::{Map, Value};

use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
use crate::storage::EventStore;

pub fn cmd_dep(
    store: &impl EventStore,
    task_id: &str,
    depends_on: &str,
    op: OpType,
) -> Result<(), DynError> {
    let mut payload = Map::new();
    payload.insert("dep".to_string(), Value::String(depends_on.to_string()));
    let is_add = matches!(op, OpType::AddDep);
    store.append_events(&[MutationEvent::new(op, task_id, payload)])?;
    if is_add {
        println!("`{task_id}` now depends on `{depends_on}`");
    } else {
        println!("`{task_id}` no longer depends on `{depends_on}`");
    }
    Ok(())
}
