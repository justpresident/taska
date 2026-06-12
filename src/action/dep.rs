//! `dep` group actions: add/remove typed edges, and the read views
//! (`cycles`/`plan`/`tree`).
//!
//! The write side resolves user-facing `type=target` edges to canonical stored
//! edges, enforces the structural blocker invariants, and appends under the lock;
//! the read side returns typed graph data (cycle lists, ordered prerequisites,
//! the tree forest with the requested columns per node) plus warnings. No field
//! name is hardcoded - `tree` fetches whatever columns the caller asks for.
//! Nothing here prints.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Map, Value};

use crate::action::{inject_computed_columns, materialize, read, Warning};
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::graph;
use crate::model::{is_done, task_cmp, MutationEvent, OpType, TaskState, REL_KEY, TARGET_KEY};
use crate::schema::vet_events;
use crate::storage::EventStore;

/// Add or remove the `name=target` edges on `task`, returning the number of
/// stored edges actually written (0 = every edge was already present/absent).
///
/// Each `name` resolves to one or more canonical stored edges (typically one,
/// but removing a symmetric edge clears both sides). The structural checks (one
/// blocker per pair, one parent), existence/self-reference checks, and the no-op
/// drop all run against freshly-read state under the store lock, so none can race
/// a concurrent writer.
pub fn apply_edges(
    store: &impl EventStore,
    task: &str,
    edges: &[String],
    op: &OpType,
    types: &BTreeMap<String, RelationshipDef>,
) -> Result<usize, DynError> {
    let removing = matches!(op, OpType::RemoveEdge);
    let mut resolved: Vec<(String, String, String)> = Vec::new();
    for edge in edges {
        let (name, target) = edge
            .split_once('=')
            .filter(|(t, v)| !t.is_empty() && !v.is_empty())
            .ok_or_else(|| format!("invalid edge `{edge}` (expected type=target)"))?;
        resolved.extend(resolve_edge(name, task, target, types, removing)?);
    }
    let events: Vec<MutationEvent> = resolved
        .iter()
        .map(|(owner, rel_type, dep)| {
            let mut payload = Map::new();
            payload.insert(TARGET_KEY.to_string(), Value::String(dep.clone()));
            // Every edge carries an explicit type now - no implicit `depends_on`.
            payload.insert(REL_KEY.to_string(), Value::String(rel_type.clone()));
            MutationEvent::new(op.clone(), owner.clone(), payload)
        })
        .collect();

    let blockers = store.config().relationships.blocker_types();
    let hierarchy = store.config().relationships.hierarchy_types();
    let config = store.config().clone();
    let written = store.append_checked(&|baseline, log| {
        let state = materialize(&config, baseline, log);
        if !removing {
            validate_blocker_additions(&resolved, &state, &blockers, &hierarchy)?;
        }
        vet_events(&events, &state, &config)
    })?;
    Ok(written.len())
}

/// Reject blocker-edge additions that would break the structural invariants:
/// (1) at most one blocking relationship between two tasks, and (2) a task may
/// have at most one parent (one incoming `hierarchy` edge). Checked incrementally
/// against the current state *plus* the edges added earlier in this command, so a
/// pre-existing violation elsewhere never blocks an unrelated add.
fn validate_blocker_additions(
    resolved: &[(String, String, String)],
    state: &HashMap<String, TaskState>,
    blockers: &BTreeSet<String>,
    hierarchy: &BTreeSet<String>,
) -> Result<(), DynError> {
    if !resolved
        .iter()
        .any(|(_, t, _)| blockers.contains(t.as_str()))
    {
        return Ok(());
    }

    // Seed the would-be view from current state: each owner's blocker target->type,
    // and each child's parent.
    let mut blocker_to: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for (id, task) in state {
        for (target, kind) in graph::blocker_edges(task, blockers) {
            blocker_to
                .entry(id.clone())
                .or_default()
                .insert(target.to_string(), kind.to_string());
        }
        for htype in hierarchy {
            for child in task.relationships.get(htype).into_iter().flatten() {
                parent_of.insert(child.clone(), id.clone());
            }
        }
    }

    for (owner, rel_type, target) in resolved {
        if !blockers.contains(rel_type.as_str()) {
            continue;
        }
        let owner_map = blocker_to.entry(owner.clone()).or_default();
        match owner_map.get(target).cloned() {
            Some(existing) if existing != *rel_type => {
                return Err(format!(
                    "`{owner}` already has a `{existing}` relationship to `{target}`; only one \
                     blocking relationship is allowed between two tasks"
                )
                .into());
            }
            Some(_) => {} // same type, idempotent
            None => {
                owner_map.insert(target.clone(), rel_type.clone());
            }
        }
        if hierarchy.contains(rel_type.as_str()) {
            match parent_of.get(target).cloned() {
                Some(parent) if parent != *owner => {
                    return Err(format!(
                        "`{target}` is already a subtask of `{parent}`; a task can have only one \
                         parent"
                    )
                    .into());
                }
                Some(_) => {}
                None => {
                    parent_of.insert(target.clone(), owner.clone());
                }
            }
        }
    }
    Ok(())
}

/// Resolve a user-facing `name=target` edge on `task` into canonical stored
/// edges `(owner, forward_type, target)`. `name` may be a declared relationship
/// type (a forward edge stored on `task`) or the configured `inverse` of one (in
/// which case the stored edge lives on the *other* task). Removal resolves to
/// every matching location so the edge clears regardless of which side stores
/// it; add resolves to a single canonical edge (declared type preferred).
fn resolve_edge(
    name: &str,
    task: &str,
    target: &str,
    types: &BTreeMap<String, RelationshipDef>,
    removing: bool,
) -> Result<Vec<(String, String, String)>, DynError> {
    let mut edges = Vec::new();
    if types.contains_key(name) {
        edges.push((task.to_string(), name.to_string(), target.to_string()));
    }
    // An inverse name (or, when removing, the inverse side of a symmetric edge)
    // points at the forward edge stored on the other task.
    if removing || edges.is_empty() {
        for (fwd, def) in types {
            if def.inverse == name {
                edges.push((target.to_string(), fwd.clone(), task.to_string()));
            }
        }
    }
    edges.sort();
    edges.dedup();
    if edges.is_empty() {
        let mut accepted: Vec<&str> = types.keys().map(String::as_str).collect();
        for def in types.values() {
            if !def.inverse.is_empty() {
                accepted.push(def.inverse.as_str());
            }
        }
        accepted.sort_unstable();
        accepted.dedup();
        return Err(format!(
            "unknown relationship type `{name}`; accepted: {}",
            accepted.join(", ")
        )
        .into());
    }
    Ok(edges)
}

/// A `cycles` read.
///
/// The blocker-graph cycles (each a list of member ids) plus any read warnings.
pub struct CyclesOutcome {
    pub cycles: Vec<Vec<String>>,
    pub warnings: Vec<Warning>,
}

/// Report the cycles in the blocker graph.
pub fn cycles(store: &impl EventStore) -> Result<CyclesOutcome, DynError> {
    let session = read(store)?;
    let blockers = store.config().relationships.blocker_types();
    let cycles = graph::dependency_cycles(&session.state, &blockers);
    Ok(CyclesOutcome {
        cycles,
        warnings: session.warnings,
    })
}

/// One step of a `plan`: a remaining prerequisite and its current status.
pub struct PlanStep {
    pub id: String,
    pub status: String,
}

/// A `plan` read: the ordered remaining prerequisites, the total remaining count
/// (before any `critical` narrowing), whether the critical path was requested,
/// and any read warnings.
pub struct PlanOutcome {
    pub steps: Vec<PlanStep>,
    pub total: usize,
    pub critical: bool,
    pub warnings: Vec<Warning>,
}

/// Plan toward `goals`.
///
/// The not-done transitive prerequisites (goals included) in dependency order.
/// `critical` narrows the list to the longest single chain of incomplete
/// prerequisites. A cycle within the relevant subgraph is an error.
pub fn plan(
    store: &impl EventStore,
    goals: &[String],
    critical: bool,
) -> Result<PlanOutcome, DynError> {
    let session = read(store)?;
    let state = session.state;
    for g in goals {
        if !state.contains_key(g) {
            return Err(format!("no task `{g}`").into());
        }
    }
    let blockers = store.config().relationships.blocker_types();

    // Transitive prerequisite closure, the goals included.
    let mut want: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = goals.to_vec();
    while let Some(id) = stack.pop() {
        if !want.insert(id.clone()) {
            continue;
        }
        if let Some(task) = state.get(&id) {
            for (dep, _) in graph::blocker_edges(task, &blockers) {
                if state.contains_key(dep) {
                    stack.push(dep.to_string());
                }
            }
        }
    }

    // Order just that subgraph (prerequisites before dependents); a cycle within
    // it is surfaced as an error, like `list --ready`.
    let sub: HashMap<String, TaskState> = want
        .iter()
        .filter_map(|id| state.get(id).map(|t| (id.clone(), t.clone())))
        .collect();
    let order = graph::validate_and_sort_dependencies(&sub, &blockers)?;

    let workflow = &store.config().workflow;
    let remaining: Vec<&String> = order
        .iter()
        .filter(|id| {
            sub.get(id.as_str())
                .is_some_and(|t| !is_done(t, &workflow.status_field, &workflow.done_status))
        })
        .collect();
    let total = remaining.len();
    let to_print: Vec<String> = if remaining.is_empty() {
        Vec::new()
    } else if critical {
        critical_path(&remaining, &sub, &blockers)
    } else {
        remaining.iter().map(|id| (*id).clone()).collect()
    };

    let status_of = |id: &str| -> String {
        sub.get(id)
            .and_then(|t| t.custom_fields.get(&workflow.status_field))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    let steps = to_print
        .iter()
        .map(|id| PlanStep {
            id: id.clone(),
            status: status_of(id),
        })
        .collect();
    Ok(PlanOutcome {
        steps,
        total,
        critical,
        warnings: session.warnings,
    })
}

/// The longest chain of incomplete prerequisites within `remaining` (already in
/// topological order, prerequisites first). A DP over that order - `depth(t) =
/// 1 + max(depth(p))` across `t`'s not-done blocker prerequisites - then a
/// backtrack from the deepest task. Ties break on the smaller id so the chosen
/// path is deterministic.
fn critical_path(
    remaining: &[&String],
    sub: &HashMap<String, TaskState>,
    blockers: &BTreeSet<String>,
) -> Vec<String> {
    let in_rem: BTreeSet<&str> = remaining.iter().map(|s| s.as_str()).collect();
    let mut depth: HashMap<&str, usize> = HashMap::new();
    let mut pred: HashMap<&str, Option<&str>> = HashMap::new();
    for id in remaining {
        let id = id.as_str();
        // Best (deepest) not-done prerequisite, smaller id winning ties.
        let mut best: Option<(usize, &str)> = None;
        if let Some(task) = sub.get(id) {
            for (dep, _) in graph::blocker_edges(task, blockers) {
                if in_rem.contains(dep) {
                    let d = depth.get(dep).copied().unwrap_or(0);
                    let keep = best.is_some_and(|b| b.0 > d || (b.0 == d && b.1 < dep));
                    if !keep {
                        best = Some((d, dep));
                    }
                }
            }
        }
        depth.insert(id, best.map_or(1, |b| b.0 + 1));
        pred.insert(id, best.map(|b| b.1));
    }

    // End at the deepest task (the goal, as the common sink), smaller id on ties.
    let end = remaining.iter().copied().max_by(|a, b| {
        let (da, db) = (
            depth.get(a.as_str()).copied().unwrap_or(0),
            depth.get(b.as_str()).copied().unwrap_or(0),
        );
        da.cmp(&db).then_with(|| b.as_str().cmp(a.as_str()))
    });

    let mut chain = Vec::new();
    let mut cur = end.map(String::as_str);
    while let Some(node) = cur {
        chain.push(node.to_string());
        cur = pred.get(node).copied().flatten();
    }
    chain.reverse();
    chain
}

/// One node of a dependency tree, built once for any frontend to render.
///
/// `cells` carries the FULL value of each requested column the task has (no
/// hardcoded field names - the caller chooses which columns); renderers truncate
/// to taste, so this layer stays free of display concerns.
pub struct Node {
    pub id: String,
    /// `(column name, value)` for each [`TreeQuery::columns`] the task has, in
    /// the order requested. The label the frontend shows is its own choice over
    /// these - the domain assumes nothing about a "title" field.
    pub cells: Vec<(String, Value)>,
    /// Whether the task is done - the load-bearing use of the status field, kept
    /// so the renderer can mark/dim done nodes.
    pub done: bool,
    /// Edge to the parent: `subtask` for a hierarchy edge, the type name for
    /// another non-`depends_on` blocker, `None` for `depends_on` or a root.
    pub edge: Option<String>,
    /// `(done, total)` subtask rollup, when the node has hierarchy children.
    pub rollup: Option<(usize, usize)>,
    pub kids: Kids,
}

/// A node's children, or why they're absent.
pub enum Kids {
    Children(Vec<Node>),
    /// Already shown in full elsewhere (a repeated DAG node).
    Collapsed,
    /// A back-edge to an ancestor.
    Cycle,
    /// The target id isn't a known task.
    Missing,
}

/// A `tree` query.
///
/// The root tasks (empty = every task nothing depends on), the `--open` prune,
/// sibling/root order reversal, and which columns to fetch per node (the
/// frontend's display columns minus `id`/`deps`).
pub struct TreeQuery<'a> {
    pub roots: &'a [String],
    pub open: bool,
    pub reverse: bool,
    pub columns: &'a [String],
    /// The column siblings/roots are ordered by, ascending (`--reverse` flips).
    pub sort: &'a str,
}

/// A `tree` read: the built-once forest plus any read warnings.
pub struct TreeOutcome {
    pub forest: Vec<Node>,
    pub warnings: Vec<Warning>,
}

/// Build the blocker-graph forest, children nested under their dependents.
///
/// Siblings and roots are ordered by `query.sort` ascending (via the neutral
/// [`task_cmp`]), with `reverse` and the missing-last rule applied on top.
pub fn tree(store: &impl EventStore, query: &TreeQuery) -> Result<TreeOutcome, DynError> {
    let session = read(store)?;
    let mut state = session.state;
    // Inject any graph-computed columns the query asked for (timestamps already
    // arrive from `read`), via the same shared primitive `list` uses.
    let wanted: Vec<&str> = query.columns.iter().map(String::as_str).collect();
    inject_computed_columns(store, &mut state, &wanted);

    let blockers = store.config().relationships.blocker_types();
    let hierarchy = store.config().relationships.hierarchy_types();
    let wf = &store.config().workflow;
    let open_subtrees = compute_open_subtrees(&state, &blockers, &wf.status_field, &wf.done_status);

    let mut roots = if query.roots.is_empty() {
        let depended: BTreeSet<&str> = state
            .values()
            .flat_map(|t| {
                graph::blocker_edges(t, &blockers)
                    .into_iter()
                    .map(|(target, _)| target)
            })
            .collect();
        let mut r: Vec<String> = state
            .keys()
            .filter(|id| !depended.contains(id.as_str()))
            .cloned()
            .collect();
        if r.is_empty() {
            r = state.keys().cloned().collect();
        }
        r
    } else {
        for t in query.roots {
            if !state.contains_key(t) {
                return Err(format!("no task `{t}`").into());
            }
        }
        query.roots.to_vec()
    };
    sort_ids(&mut roots, &state, query.sort, query.reverse);
    if query.open {
        roots.retain(|r| open_subtrees.contains(r));
    }
    if roots.is_empty() {
        return Ok(TreeOutcome {
            forest: Vec::new(),
            warnings: session.warnings,
        });
    }

    let ctx = TreeCtx {
        state: &state,
        blockers: &blockers,
        hierarchy: &hierarchy,
        status_field: &wf.status_field,
        done_status: &wf.done_status,
        default_blocker: store.config().relationships.default_blocker(),
        sort: query.sort,
        reverse: query.reverse,
        open: query.open,
        open_subtrees: &open_subtrees,
        columns: query.columns,
    };
    // Build the forest once; `build` marks a node expanded/on-path itself, so
    // roots start from empty state.
    let mut expanded: HashSet<String> = HashSet::new();
    let forest: Vec<Node> = roots
        .iter()
        .map(|root| {
            let mut path = Vec::new();
            build(&ctx, root, None, &mut path, &mut expanded)
        })
        .collect();
    Ok(TreeOutcome {
        forest,
        warnings: session.warnings,
    })
}

/// Shared, read-only context for building a `dep tree`.
struct TreeCtx<'a> {
    state: &'a HashMap<String, TaskState>,
    blockers: &'a BTreeSet<String>,
    hierarchy: &'a BTreeSet<String>,
    status_field: &'a str,
    done_status: &'a str,
    /// The default blocker type (the configured first `blocker`-kind type, e.g.
    /// `depends_on`). Its edge tag is hidden since it's the tree's implied default
    /// relation; every other type is tagged. `None` only for a store with no
    /// blocker type (which `Config::validate` rejects).
    default_blocker: Option<&'a str>,
    /// The column siblings are ordered by (via [`task_cmp`]).
    sort: &'a str,
    reverse: bool,
    open: bool,
    /// Tasks whose blocker-subtree contains at least one open task.
    open_subtrees: &'a HashSet<String>,
    /// The columns to fetch into each node's `cells`.
    columns: &'a [String],
}

/// Build the node for `id` (reached via `kind`), recursing over its sorted,
/// `--open`-pruned blocker children. `path` (ancestors) breaks cycles; `expanded`
/// collapses a node already shown in full elsewhere.
fn build(
    ctx: &TreeCtx,
    id: &str,
    kind: Option<&str>,
    path: &mut Vec<String>,
    expanded: &mut HashSet<String>,
) -> Node {
    let edge = match kind {
        Some(k) if ctx.hierarchy.contains(k) => Some("subtask".to_string()),
        // Tag every blocker type except the default (its name is the implied
        // default relation of the tree, so showing it would just be noise).
        Some(k) if ctx.default_blocker != Some(k) => Some(k.to_string()),
        _ => None,
    };
    let Some(task) = ctx.state.get(id) else {
        return Node {
            id: id.to_string(),
            cells: Vec::new(),
            done: false,
            edge: None,
            rollup: None,
            kids: Kids::Missing,
        };
    };
    let done = is_done(task, ctx.status_field, ctx.done_status);
    // The requested columns the task actually has, full values, in order - no
    // field name is hardcoded; the frontend chose the columns.
    let cells: Vec<(String, Value)> = ctx
        .columns
        .iter()
        .filter_map(|col| {
            task.custom_fields
                .get(col)
                .map(|v| (col.clone(), v.clone()))
        })
        .collect();
    let (rd, rt) = graph::subtask_counts(
        task,
        ctx.state,
        ctx.hierarchy,
        ctx.status_field,
        ctx.done_status,
    );
    let rollup = (rt > 0).then_some((rd, rt));

    let kids = if path.iter().any(|p| p == id) {
        Kids::Cycle
    } else if expanded.contains(id) && !graph::blocker_edges(task, ctx.blockers).is_empty() {
        Kids::Collapsed
    } else {
        expanded.insert(id.to_string());
        path.push(id.to_string());
        let mut children = graph::blocker_edges(task, ctx.blockers);
        children.sort_by(|a, b| child_cmp(ctx, a.0, b.0));
        if ctx.reverse {
            children.reverse();
        }
        if ctx.open {
            children.retain(|(c, _)| ctx.open_subtrees.contains(*c));
        }
        let nodes = children
            .iter()
            .map(|&(c, k)| build(ctx, c, Some(k), path, expanded))
            .collect();
        path.pop();
        Kids::Children(nodes)
    };
    Node {
        id: id.to_string(),
        cells,
        done,
        edge,
        rollup,
        kids,
    }
}

/// Order two task ids by the sort column (missing tasks last, id tiebreak).
fn child_cmp(ctx: &TreeCtx, a: &str, b: &str) -> Ordering {
    match (ctx.state.get(a), ctx.state.get(b)) {
        (Some(ta), Some(tb)) => task_cmp(ta, tb, ctx.sort),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

/// Sort task ids in place by the `sort` column (missing tasks last), flipped by
/// `reverse`.
fn sort_ids(ids: &mut [String], state: &HashMap<String, TaskState>, sort: &str, reverse: bool) {
    ids.sort_by(|a, b| match (state.get(a), state.get(b)) {
        (Some(ta), Some(tb)) => task_cmp(ta, tb, sort),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    });
    if reverse {
        ids.reverse();
    }
}

/// Tasks whose blocker-subtree (the task itself or any transitive prerequisite)
/// contains an open task. `--open` keeps exactly these, so a done task that still
/// leads to open work stays while fully-resolved branches are pruned.
fn compute_open_subtrees(
    state: &HashMap<String, TaskState>,
    blockers: &BTreeSet<String>,
    status_field: &str,
    done_status: &str,
) -> HashSet<String> {
    let mut memo: HashMap<String, bool> = HashMap::new();
    let mut on_path: HashSet<String> = HashSet::new();
    for id in state.keys() {
        subtree_open(
            id,
            state,
            blockers,
            status_field,
            done_status,
            &mut memo,
            &mut on_path,
        );
    }
    memo.into_iter()
        .filter_map(|(k, v)| v.then_some(k))
        .collect()
}

fn subtree_open(
    id: &str,
    state: &HashMap<String, TaskState>,
    blockers: &BTreeSet<String>,
    status_field: &str,
    done_status: &str,
    memo: &mut HashMap<String, bool>,
    on_path: &mut HashSet<String>,
) -> bool {
    if let Some(&v) = memo.get(id) {
        return v;
    }
    if !on_path.insert(id.to_string()) {
        return false; // cycle back-edge: don't recurse, the node's own status counts
    }
    let mut open = state
        .get(id)
        .is_some_and(|t| !is_done(t, status_field, done_status));
    if let Some(task) = state.get(id) {
        for (child, _) in graph::blocker_edges(task, blockers) {
            if subtree_open(
                child,
                state,
                blockers,
                status_field,
                done_status,
                memo,
                on_path,
            ) {
                open = true;
            }
        }
    }
    on_path.remove(id);
    memo.insert(id.to_string(), open);
    open
}
