//! State materialization (the overlay engine) and field search.
//!
//! Replays the mutation log on top of the compacted baseline in a single pass
//! to produce the runtime task map.

use std::collections::HashMap;

use serde_json::Value;

use crate::storage::{MutationEvent, OpType, TaskState};

pub struct Engine;

impl Engine {
    /// Fold `mutations` over `baseline` to produce the current task map.
    pub fn materialize_state(
        baseline: Vec<TaskState>,
        mutations: Vec<MutationEvent>,
    ) -> HashMap<String, TaskState> {
        let mut state_map: HashMap<String, TaskState> = baseline
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();

        for event in mutations {
            match event.op {
                OpType::Create => {
                    // Re-creating an existing id refreshes its fields but keeps
                    // any deps already attached.
                    let entry = state_map.entry(event.task_id.clone()).or_insert_with(|| {
                        TaskState {
                            id: event.task_id.clone(),
                            depends_on: Vec::new(),
                            custom_fields: serde_json::Map::new(),
                        }
                    });
                    for (k, v) in event.payload {
                        entry.custom_fields.insert(k, v);
                    }
                }
                OpType::Update => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        for (k, v) in event.payload {
                            task.custom_fields.insert(k, v);
                        }
                    }
                }
                OpType::AddDep => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        if let Some(dep_id) = event.payload.get("dep").and_then(|v| v.as_str()) {
                            let dep_id = dep_id.to_string();
                            if !task.depends_on.contains(&dep_id) {
                                task.depends_on.push(dep_id);
                            }
                        }
                    }
                }
                OpType::RemoveDep => {
                    if let Some(task) = state_map.get_mut(&event.task_id) {
                        if let Some(dep_id) = event.payload.get("dep").and_then(|v| v.as_str()) {
                            task.depends_on.retain(|d| d != dep_id);
                        }
                    }
                }
                OpType::Delete => {
                    state_map.remove(&event.task_id);
                }
            }
        }
        state_map
    }

    /// Materialize directly from storage.
    pub fn load(storage: &crate::storage::Storage) -> Result<HashMap<String, TaskState>, crate::storage::DynError> {
        let baseline = storage.load_baseline()?;
        let mutations = storage.load_mutations()?;
        Ok(Engine::materialize_state(baseline, mutations))
    }

    /// Return tasks whose `key` field exactly equals `val`.
    pub fn filter_tasks<'a>(
        state: &'a HashMap<String, TaskState>,
        key: &str,
        val: &Value,
    ) -> Vec<&'a TaskState> {
        state
            .values()
            .filter(|t| t.custom_fields.get(key) == Some(val))
            .collect()
    }
}
