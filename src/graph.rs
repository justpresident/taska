//! Dependency graph: cycle detection, topological ordering, and readiness.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::BuildHasher;

use petgraph::graphmap::DiGraphMap;

use crate::model::{is_done, TaskState, DEPENDS_ON};

/// A run-local, integer handle for a task *inside the graph*. The persistent id
/// is the `String` key in the state map; [`IdIndex`] interns those to a dense
/// `TaskId` so whole-graph traversal works on integers (cheap hashing,
/// contiguous adjacency, a stamped `seen` array) rather than `String`/`&str`.
/// These numbers are regenerated each run and never touch the on-disk log or
/// baseline, which stay string-keyed.
type TaskId = usize;

/// A run-local interning of the task ids to dense [`TaskId`]s (and back), built
/// once per graph operation. Indices are assigned in sorted id order so any
/// derived ordering is deterministic.
struct IdIndex<'a> {
    ids: Vec<&'a str>,
    to_ix: HashMap<&'a str, TaskId>,
}

impl<'a> IdIndex<'a> {
    fn new<S: BuildHasher>(state: &'a HashMap<String, TaskState, S>) -> Self {
        let mut ids: Vec<&'a str> = state.keys().map(String::as_str).collect();
        ids.sort_unstable();
        let to_ix = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        Self { ids, to_ix }
    }

    fn ix(&self, id: &str) -> Option<TaskId> {
        self.to_ix.get(id).copied()
    }
}

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
    if blockers.contains(DEPENDS_ON) {
        edges.extend(task.depends_on.iter().map(|t| (t.as_str(), DEPENDS_ON)));
    }
    for (rel, targets) in &task.relationships {
        if blockers.contains(rel.as_str()) {
            edges.extend(targets.iter().map(|t| (t.as_str(), rel.as_str())));
        }
    }
    edges
}

/// Pairs with more than one blocker-gating edge between them.
///
/// Returns `(task, target, conflicting type names)`. At most one blocking
/// relationship is allowed between two tasks (e.g. not both `depends_on` and
/// `has_subtask`).
pub fn duplicate_blocker_edges<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    blockers: &BTreeSet<String>,
) -> Vec<(String, String, Vec<String>)> {
    let mut out = Vec::new();
    for (id, task) in state {
        let mut by_target: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (target, kind) in blocker_edges(task, blockers) {
            by_target.entry(target).or_default().insert(kind);
        }
        for (target, kinds) in by_target {
            if kinds.len() > 1 {
                out.push((
                    id.clone(),
                    target.to_string(),
                    kinds.into_iter().map(str::to_string).collect(),
                ));
            }
        }
    }
    out.sort();
    out
}

/// Tasks that are a subtask of more than one parent: `(child, parent ids)`. A
/// task may have at most one parent.
pub fn multi_parent_tasks<S: BuildHasher>(
    state: &HashMap<String, TaskState, S>,
    hierarchy: &BTreeSet<String>,
) -> Vec<(String, Vec<String>)> {
    let mut parents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (id, task) in state {
        for htype in hierarchy {
            for child in task.relationships.get(htype).into_iter().flatten() {
                parents.entry(child).or_default().insert(id);
            }
        }
    }
    parents
        .into_iter()
        .filter(|(_, ps)| ps.len() > 1)
        .map(|(child, ps)| {
            (
                child.to_string(),
                ps.into_iter().map(str::to_string).collect(),
            )
        })
        .collect()
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
    let index = IdIndex::new(state);
    let n = index.ids.len();
    // Precompute not-done per interned task, so the inner DFS is a Vec lookup.
    let not_done: Vec<bool> = index
        .ids
        .iter()
        .map(|&id| {
            state
                .get(id)
                .is_some_and(|t| !is_done(t, status_field, done_status))
        })
        .collect();
    // Integer adjacency to existing tasks: prerequisites (forward) and dependents
    // (reverse), as contiguous Vecs indexed by interned id.
    let mut prereqs: Vec<Vec<TaskId>> = vec![Vec::new(); n];
    let mut dependents: Vec<Vec<TaskId>> = vec![Vec::new(); n];
    for (i, &id) in index.ids.iter().enumerate() {
        if let Some(task) = state.get(id) {
            for (dep, _) in blocker_edges(task, blockers) {
                if let Some(j) = index.ix(dep) {
                    prereqs[i].push(j);
                    dependents[j].push(i);
                }
            }
        }
    }
    // A monotonically increasing stamp marks nodes seen in the current DFS, so
    // `seen` is reused across all 2·n traversals without an O(n) reset each time.
    let mut seen = vec![0u64; n];
    let mut stamp = 0u64;
    (0..n)
        .map(|i| {
            stamp += 1;
            let blocked_by = reach_not_done(i, &prereqs, &not_done, &mut seen, stamp);
            stamp += 1;
            let unblocks = reach_not_done(i, &dependents, &not_done, &mut seen, stamp);
            (index.ids[i].to_string(), (unblocks, blocked_by))
        })
        .collect()
}

/// Count the distinct not-done tasks reachable from `start` over `adj` (the start
/// itself excluded). Traversal passes through done tasks but only not-done ones
/// are counted. `seen[m] == stamp` marks `m` visited in this call.
fn reach_not_done(
    start: TaskId,
    adj: &[Vec<TaskId>],
    not_done: &[bool],
    seen: &mut [u64],
    stamp: u64,
) -> usize {
    let mut stack = vec![start];
    let mut count = 0;
    while let Some(node) = stack.pop() {
        for &m in &adj[node] {
            if seen[m] != stamp {
                seen[m] = stamp;
                if not_done[m] {
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
    let index = IdIndex::new(state);
    let mut graph: DiGraphMap<TaskId, ()> = DiGraphMap::new();

    for (i, &id) in index.ids.iter().enumerate() {
        graph.add_node(i);
        if let Some(task) = state.get(id) {
            for (dep, _) in blocker_edges(task, blockers) {
                // Only wire edges to deps that actually exist; dangling deps are
                // reported separately rather than crashing the sort.
                if let Some(j) = index.ix(dep) {
                    graph.add_edge(j, i, ());
                }
            }
        }
    }

    if petgraph::algo::is_cyclic_directed(&graph) {
        return Err("Cycle Error: Circular dependency detected in dependency graph.".to_string());
    }

    let sorted = petgraph::algo::toposort(&graph, None)
        .map_err(|_| "Topological cycle processing failure".to_string())?
        .into_iter()
        .map(|ix| index.ids[ix].to_string())
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
    let index = IdIndex::new(state);
    let mut graph: DiGraphMap<TaskId, ()> = DiGraphMap::new();
    let mut self_loops: Vec<String> = Vec::new();
    for (i, &id) in index.ids.iter().enumerate() {
        graph.add_node(i);
        if let Some(task) = state.get(id) {
            for (dep, _) in blocker_edges(task, blockers) {
                if let Some(j) = index.ix(dep) {
                    graph.add_edge(i, j, ());
                    if j == i {
                        self_loops.push(id.to_string());
                    }
                }
            }
        }
    }

    let mut cycles: Vec<Vec<String>> = Vec::new();
    for scc in petgraph::algo::tarjan_scc(&graph) {
        if scc.len() > 1 {
            let mut members: Vec<String> =
                scc.iter().map(|&ix| index.ids[ix].to_string()).collect();
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
