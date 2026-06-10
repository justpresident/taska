//! `show` action: one task, with its inverse relationship edges surfaced.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::BuildHasher;

use serde_json::Value;

use crate::action::{read, Warning};
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::model::TaskState;
use crate::storage::EventStore;

/// A `show` read.
///
/// The task (with inverse edges injected as array fields) plus any read
/// [`Warning`]s.
pub struct ShowOutcome {
    pub task: TaskState,
    pub warnings: Vec<Warning>,
}

/// Materialize the store and return one task by id.
///
/// The INVERSE edges of other tasks pointing here are surfaced as ordinary array
/// fields under their configured inverse names — the task's own forward edges
/// already live in `deps`, grouped by type, so they're not duplicated here.
pub fn show(store: &impl EventStore, id: &str) -> Result<ShowOutcome, DynError> {
    let session = read(store)?;
    let mut task = session
        .state
        .get(id)
        .cloned()
        .ok_or_else(|| format!("no task `{id}`"))?;
    let types = &store.config().relationships.types;
    for (name, targets) in inverse_edges(&session.state, id, types) {
        let arr = targets.into_iter().map(Value::String).collect();
        task.custom_fields.insert(name, Value::Array(arr));
    }
    Ok(ShowOutcome {
        task,
        warnings: session.warnings,
    })
}

/// A task's INVERSE relationship edges.
///
/// For every OTHER task with an edge pointing at `id`, that edge's configured
/// `inverse` name (an empty inverse is one-way and not surfaced). Keyed by
/// display name → sorted owner ids.
pub fn inverse_edges<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    id: &str,
    types: &BTreeMap<String, RelationshipDef>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut display: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (other_id, other) in state {
        if other_id == id {
            continue;
        }
        for (rel_type, targets) in &other.relationships {
            if !targets.iter().any(|t| t == id) {
                continue;
            }
            if let Some(def) = types.get(rel_type) {
                if !def.inverse.is_empty() {
                    display
                        .entry(def.inverse.clone())
                        .or_default()
                        .insert(other_id.clone());
                }
            }
        }
    }
    display
}
