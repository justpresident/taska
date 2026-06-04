//! Dependency graph: cycle detection, topological ordering, and readiness.

use std::collections::{BTreeSet, HashMap};
use std::hash::BuildHasher;

use petgraph::graphmap::DiGraphMap;

use crate::model::{is_done, TaskState};

/// A task's blocker edges as `(target, type)` pairs.
///
/// These are the dependencies that gate readiness: the `depends_on` field plus
/// any relationship whose type name is in `blockers` (i.e. configured
/// `type = "blocker"`). Informational relationships are skipped.
pub fn blocker_edges<'a>(
    task: &'a TaskState,
    blockers: &BTreeSet<String>,
) -> Vec<(&'a str, &'a str)> {
    let mut edges = Vec::new();
    if blockers.contains("depends_on") {
        edges.extend(task.depends_on.iter().map(|t| (t.as_str(), "depends_on")));
    }
    for (rel, targets) in &task.relationships {
        if blockers.contains(rel.as_str()) {
            edges.extend(targets.iter().map(|t| (t.as_str(), rel.as_str())));
        }
    }
    edges
}

/// Validate the dependency DAG and return a topological ordering
/// (dependencies before dependents). Errors on any cycle.
pub fn validate_and_sort_dependencies<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    blockers: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let mut graph: DiGraphMap<&str, ()> = DiGraphMap::new();

    for (id, task) in state {
        graph.add_node(id.as_str());
        for (dep, _) in blocker_edges(task, blockers) {
            // Only wire edges to deps that actually exist; dangling deps are
            // reported separately rather than crashing the sort.
            if state.contains_key(dep) {
                graph.add_edge(dep, id.as_str(), ());
            }
        }
    }

    if petgraph::algo::is_cyclic_directed(&graph) {
        return Err("Cycle Error: Circular dependency detected in dependency graph.".to_string());
    }

    let sorted = petgraph::algo::toposort(&graph, None)
        .map_err(|_| "Topological cycle processing failure".to_string())?
        .into_iter()
        .map(std::string::ToString::to_string)
        .collect();

    Ok(sorted)
}

/// Find dependency cycles in the `depends_on` graph.
///
/// Returns one entry per cycle: a multi-task strongly-connected component, or a
/// single task that depends on itself. Members are sorted, and the list is
/// sorted, so the output is deterministic regardless of map iteration order.
pub fn dependency_cycles<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    blockers: &BTreeSet<String>,
) -> Vec<Vec<String>> {
    let mut graph: DiGraphMap<&str, ()> = DiGraphMap::new();
    let mut self_loops: Vec<String> = Vec::new();
    for (id, task) in state {
        graph.add_node(id.as_str());
        for (dep, _) in blocker_edges(task, blockers) {
            if state.contains_key(dep) {
                graph.add_edge(id.as_str(), dep, ());
                if dep == id.as_str() {
                    self_loops.push(id.clone());
                }
            }
        }
    }

    let mut cycles: Vec<Vec<String>> = Vec::new();
    for scc in petgraph::algo::tarjan_scc(&graph) {
        if scc.len() > 1 {
            let mut members: Vec<String> = scc.iter().map(|s| (*s).to_string()).collect();
            members.sort();
            cycles.push(members);
        }
    }
    for id in self_loops {
        cycles.push(vec![id]);
    }
    cycles.sort();
    cycles
}

/// Tasks that are not yet done and whose every existing dependency is done.
/// Returned in topological order so `ta ready` lists work in a sane sequence.
///
/// `status_field`/`done_status` come from `[workflow]` config, so projects can
/// rename the convention (e.g. `state`/`closed`).
pub fn ready_tasks<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    status_field: &str,
    done_status: &str,
    blockers: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let order = validate_and_sort_dependencies(state, blockers)?;
    let mut ready = Vec::new();
    for id in order {
        let Some(task) = state.get(&id) else {
            continue;
        };
        if is_done(task, status_field, done_status) {
            continue;
        }
        let blocked = blocker_edges(task, blockers).into_iter().any(|(dep, _)| {
            state
                .get(dep)
                .is_some_and(|d| !is_done(d, status_field, done_status))
        });
        if !blocked {
            ready.push(id);
        }
    }
    Ok(ready)
}
