//! `show` action: one or more tasks, each with its inverse relationship edges
//! surfaced.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;

use serde_json::Value;

use crate::action::{read, Warning};
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::model::TaskState;
use crate::storage::EventStore;

/// A `show` read.
///
/// The requested tasks (each with inverse edges injected as array fields), in the
/// deduplicated order they were asked for, plus any read [`Warning`]s.
pub struct ShowOutcome {
    pub tasks: Vec<TaskState>,
    pub warnings: Vec<Warning>,
}

/// Materialize the store once and return the named tasks, in the order given with
/// duplicates dropped (first occurrence wins). Every unknown id is reported in a
/// single error.
///
/// For each task, the INVERSE edges of other tasks pointing at it are surfaced as
/// ordinary array fields under their configured inverse names - the task's own
/// forward edges already live in `deps`, grouped by type, so they're not
/// duplicated here.
pub fn show(store: &impl EventStore, ids: &[String]) -> Result<ShowOutcome, DynError> {
    let session = read(store)?;
    let types = &store.config().relationships.types;

    let mut tasks = Vec::with_capacity(ids.len());
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id.as_str()) {
            continue; // duplicate id: show it once
        }
        match session.state.get(id.as_str()) {
            Some(task) => {
                let mut task = task.clone();
                for (name, targets) in inverse_edges(&session.state, id, types) {
                    let arr = targets.into_iter().map(Value::String).collect();
                    task.custom_fields.insert(name, Value::Array(arr));
                }
                tasks.push(task);
            }
            None => missing.push(id.as_str()),
        }
    }
    if !missing.is_empty() {
        return Err(missing_error(&missing).into());
    }
    Ok(ShowOutcome {
        tasks,
        warnings: session.warnings,
    })
}

/// One error naming every unknown id. A single miss keeps the bare
/// "no task `<id>`" wording; several are listed together.
fn missing_error(missing: &[&str]) -> String {
    match missing {
        [one] => format!("no task `{one}`"),
        many => {
            let list = many
                .iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("no tasks: {list}")
        }
    }
}

/// A task's INVERSE relationship edges.
///
/// For every OTHER task with an edge pointing at `id`, that edge's configured
/// `inverse` name (an empty inverse is one-way and not surfaced). Keyed by
/// display name -> sorted owner ids.
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
