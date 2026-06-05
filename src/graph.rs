//! Dependency graph: cycle detection, topological ordering, and readiness.

use std::collections::{BTreeSet, HashMap, HashSet};
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

/// A task's `(done, total)` direct hierarchy children — its subtask completion.
pub fn subtask_counts<S: BuildHasher>(
    task: &TaskState,
    state: &HashMap<String, TaskState, S>,
    hierarchy: &BTreeSet<String>,
    status_field: &str,
    done_status: &str,
) -> (usize, usize) {
    let (mut done, mut total) = (0usize, 0usize);
    for htype in hierarchy {
        for child in task.relationships.get(htype).into_iter().flatten() {
            total += 1;
            if state
                .get(child)
                .is_some_and(|t| is_done(t, status_field, done_status))
            {
                done += 1;
            }
        }
    }
    (done, total)
}

/// `(done, total)` subtask completion per task that has hierarchy children.
pub fn subtask_progress<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    hierarchy: &BTreeSet<String>,
    status_field: &str,
    done_status: &str,
) -> HashMap<String, (usize, usize)> {
    state
        .iter()
        .filter_map(|(id, task)| {
            let (done, total) = subtask_counts(task, state, hierarchy, status_field, done_status);
            (total > 0).then(|| (id.clone(), (done, total)))
        })
        .collect()
}

/// Per-task `(unblocks, blocked_by)` over the blocker graph.
///
/// `unblocks` is how many still-not-done tasks transitively depend on this one
/// ("finish it to unblock N"); `blocked_by` is how many still-not-done tasks it
/// transitively depends on. Both walk the blocker edges and count distinct
/// reachable not-done tasks (the task itself excluded); cycles are tolerated.
pub fn reachability_counts<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    blockers: &BTreeSet<String>,
    status_field: &str,
    done_status: &str,
) -> HashMap<String, (usize, usize)> {
    // Blocker adjacency to existing tasks: prerequisites (forward) and dependents
    // (reverse).
    let mut prereqs: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, task) in state {
        for (dep, _) in blocker_edges(task, blockers) {
            if state.contains_key(dep) {
                prereqs.entry(id.as_str()).or_default().push(dep);
                dependents.entry(dep).or_default().push(id.as_str());
            }
        }
    }
    state
        .keys()
        .map(|id| {
            let blocked_by =
                reach_not_done(id.as_str(), &prereqs, state, status_field, done_status);
            let unblocks =
                reach_not_done(id.as_str(), &dependents, state, status_field, done_status);
            (id.clone(), (unblocks, blocked_by))
        })
        .collect()
}

/// Count the distinct not-done tasks reachable from `start` over `adj` (the start
/// itself excluded). Traversal passes through done tasks but only not-done ones
/// are counted.
fn reach_not_done<'a, S: BuildHasher>(
    start: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    state: &HashMap<String, TaskState, S>,
    status_field: &str,
    done_status: &str,
) -> usize {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack = vec![start];
    let mut count = 0;
    while let Some(node) = stack.pop() {
        let Some(next) = adj.get(node) else { continue };
        for &m in next {
            if seen.insert(m) {
                if state
                    .get(m)
                    .is_some_and(|t| !is_done(t, status_field, done_status))
                {
                    count += 1;
                }
                stack.push(m);
            }
        }
    }
    count
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

/// Find dependency cycles in the blocker graph.
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
/// Returned in topological order so `ta list --ready` lists work in a sane sequence.
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
