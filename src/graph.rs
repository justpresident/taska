//! Dependency graph: cycle detection, topological ordering, and readiness.

use std::collections::HashMap;

use petgraph::graphmap::DiGraphMap;

use crate::storage::TaskState;

/// Field/value convention used to mark a task as finished.
const STATUS_FIELD: &str = "status";
const DONE_VALUE: &str = "done";

/// Validate the dependency DAG and return a topological ordering
/// (dependencies before dependents). Errors on any cycle.
pub fn validate_and_sort_dependencies(
    state: &HashMap<String, TaskState>,
) -> Result<Vec<String>, String> {
    let mut graph: DiGraphMap<&str, ()> = DiGraphMap::new();

    for (id, task) in state {
        graph.add_node(id.as_str());
        for dep in &task.depends_on {
            // Only wire edges to deps that actually exist; dangling deps are
            // reported separately rather than crashing the sort.
            if state.contains_key(dep) {
                graph.add_edge(dep.as_str(), id.as_str(), ());
            }
        }
    }

    if petgraph::algo::is_cyclic_directed(&graph) {
        return Err("Cycle Error: Circular dependency detected in dependency graph.".to_string());
    }

    let sorted = petgraph::algo::toposort(&graph, None)
        .map_err(|_| "Topological cycle processing failure".to_string())?
        .into_iter()
        .map(|s| s.to_string())
        .collect();

    Ok(sorted)
}

fn is_done(task: &TaskState) -> bool {
    task.custom_fields
        .get(STATUS_FIELD)
        .and_then(|v| v.as_str())
        == Some(DONE_VALUE)
}

/// Tasks that are not yet done and whose every existing dependency is done.
/// Returned in topological order so `ta ready` lists work in a sane sequence.
pub fn ready_tasks(state: &HashMap<String, TaskState>) -> Result<Vec<String>, String> {
    let order = validate_and_sort_dependencies(state)?;
    let mut ready = Vec::new();
    for id in order {
        let task = match state.get(&id) {
            Some(t) => t,
            None => continue,
        };
        if is_done(task) {
            continue;
        }
        let blocked = task
            .depends_on
            .iter()
            .any(|dep| state.get(dep).map_or(false, |d| !is_done(d)));
        if !blocked {
            ready.push(id);
        }
    }
    Ok(ready)
}
