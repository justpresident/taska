//! Dependency graph: cycle detection, topological ordering, and readiness.

use std::collections::HashMap;

use petgraph::graphmap::DiGraphMap;

use crate::model::TaskState;

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

fn is_done(task: &TaskState, status_field: &str, done_status: &str) -> bool {
    task.custom_fields
        .get(status_field)
        .and_then(|v| v.as_str())
        == Some(done_status)
}

/// Tasks that are not yet done and whose every existing dependency is done.
/// Returned in topological order so `ta ready` lists work in a sane sequence.
///
/// `status_field`/`done_status` come from `[workflow]` config, so projects can
/// rename the convention (e.g. `state`/`closed`).
pub fn ready_tasks(
    state: &HashMap<String, TaskState>,
    status_field: &str,
    done_status: &str,
) -> Result<Vec<String>, String> {
    let order = validate_and_sort_dependencies(state)?;
    let mut ready = Vec::new();
    for id in order {
        let task = match state.get(&id) {
            Some(t) => t,
            None => continue,
        };
        if is_done(task, status_field, done_status) {
            continue;
        }
        let blocked = task
            .depends_on
            .iter()
            .any(|dep| state.get(dep).is_some_and(|d| !is_done(d, status_field, done_status)));
        if !blocked {
            ready.push(id);
        }
    }
    Ok(ready)
}
