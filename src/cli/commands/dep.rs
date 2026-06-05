//! The `ta dep` command group — add, remove, list, and inspect typed
//! relationship edges between tasks.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use clap::Subcommand;
use serde_json::{Map, Value};

use crate::cli::state_of;
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::model::{is_done, MutationEvent, OpType, TaskState};
use crate::storage::EventStore;

/// `ta dep` subcommands. Edges are `type=target` tokens; `type` must be declared
/// in `[relationships]`.
#[derive(Subcommand)]
pub enum DepAction {
    /// Add typed edge(s): `ta dep add <task> depends_on=<other> [relates_to=<x> …]`
    Add {
        task: String,
        /// `type=target` pairs (each `type` must be a declared relationship type)
        #[arg(required = true)]
        edges: Vec<String>,
    },
    /// Remove typed edge(s): `ta dep remove <task> depends_on=<other> …`
    Remove {
        task: String,
        /// `type=target` pairs to remove
        #[arg(required = true)]
        edges: Vec<String>,
    },
    /// List a task's edges, forward and inverse: `ta dep list [<task> …]`
    List {
        /// Tasks to list (default: every task)
        tasks: Vec<String>,
    },
    /// ASCII dependency tree: `ta dep tree [<task> …]` (roots default to tasks
    /// nothing depends on)
    Tree {
        /// Root tasks (default: every task nothing depends on)
        tasks: Vec<String>,
    },
    /// Report dependency cycles in the blocker graph: `ta dep cycles`
    Cycles,
    /// Ordered remaining prerequisites of a goal: `ta dep plan <goal> …`
    Plan {
        /// Goal task(s) to plan toward
        #[arg(required = true)]
        goals: Vec<String>,
        /// Show only the critical path: the longest chain of incomplete prerequisites
        #[arg(long)]
        critical: bool,
    },
}

/// Add or remove typed dependency edges. Each `type=target` edge's type is
/// validated against the declared relationship types; an `AddDep`/`RemoveDep`
/// event is appended per edge (the `depends_on` type omits an explicit `type` to
/// stay legacy-shaped on disk — it's stored in the dedicated `depends_on` field).
pub fn cmd_dep_group(
    store: &impl EventStore,
    action: DepAction,
    types: &BTreeMap<String, RelationshipDef>,
) -> Result<(), DynError> {
    match action {
        DepAction::Add { task, edges } => {
            dep_write(store, &task, &edges, &OpType::AddDep, "Added", types)
        }
        DepAction::Remove { task, edges } => {
            dep_write(store, &task, &edges, &OpType::RemoveDep, "Removed", types)
        }
        DepAction::List { tasks } => dep_list(store, &tasks, types),
        DepAction::Tree { tasks } => dep_tree(store, &tasks),
        DepAction::Cycles => dep_cycles(store),
        DepAction::Plan { goals, critical } => dep_plan(store, &goals, critical),
    }
}

/// Add or remove the `name=target` edges. Each `name` resolves to one or more
/// canonical stored edges (see [`resolve_edge`]) — typically one, but removing a
/// symmetric edge clears both sides — and one event is appended per resolved
/// edge.
fn dep_write(
    store: &impl EventStore,
    task: &str,
    edges: &[String],
    op: &OpType,
    verb: &str,
    types: &BTreeMap<String, RelationshipDef>,
) -> Result<(), DynError> {
    let removing = matches!(op, OpType::RemoveDep);
    let mut events = Vec::with_capacity(edges.len());
    for edge in edges {
        let (name, target) = edge
            .split_once('=')
            .filter(|(t, v)| !t.is_empty() && !v.is_empty())
            .ok_or_else(|| format!("invalid edge `{edge}` (expected type=target)"))?;
        for (owner, rel_type, dep) in resolve_edge(name, task, target, types, removing)? {
            let mut payload = Map::new();
            payload.insert("dep".to_string(), Value::String(dep));
            // `depends_on` omits the type to stay legacy-shaped.
            if rel_type != "depends_on" {
                payload.insert("type".to_string(), Value::String(rel_type));
            }
            events.push(MutationEvent::new(op.clone(), owner, payload));
        }
    }
    store.append_events(&events)?;
    println!("{verb} {} edge(s) on `{task}`", edges.len());
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

/// `ta dep list` — each task's edges, forward (its own) and inverse (other tasks'
/// edges pointing here, shown under the configured `inverse` name).
fn dep_list(
    store: &impl EventStore,
    tasks: &[String],
    types: &BTreeMap<String, RelationshipDef>,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let mut ids: Vec<&String> = if tasks.is_empty() {
        state.keys().collect()
    } else {
        for t in tasks {
            if !state.contains_key(t) {
                return Err(format!("no task `{t}`").into());
            }
        }
        tasks.iter().collect()
    };
    ids.sort();
    for id in ids {
        let edges = relationship_edges(&state, id, types);
        if edges.is_empty() {
            println!("{id}: (no relationships)");
        } else {
            println!("{id}:");
            for (rel, targets) in &edges {
                let joined: Vec<&str> = targets.iter().map(String::as_str).collect();
                println!("  {rel}: {}", joined.join(", "));
            }
        }
    }
    Ok(())
}

/// A task's relationship edges for display: its forward edges (the `depends_on`
/// field + the typed map) plus inverse edges — for every OTHER task with an edge
/// pointing here, that edge's configured `inverse` name (an empty inverse is
/// one-way and not surfaced). Keyed by display name → sorted target ids.
fn relationship_edges(
    state: &HashMap<String, TaskState>,
    id: &str,
    types: &BTreeMap<String, RelationshipDef>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut display: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(task) = state.get(id) {
        if !task.depends_on.is_empty() {
            display
                .entry("depends_on".to_string())
                .or_default()
                .extend(task.depends_on.iter().cloned());
        }
        for (rel, targets) in &task.relationships {
            display
                .entry(rel.clone())
                .or_default()
                .extend(targets.iter().cloned());
        }
    }
    for (other_id, other) in state {
        if other_id == id {
            continue;
        }
        let mut hit_types: Vec<&str> = Vec::new();
        if other.depends_on.iter().any(|t| t == id) {
            hit_types.push("depends_on");
        }
        for (rel_type, targets) in &other.relationships {
            if targets.iter().any(|t| t == id) {
                hit_types.push(rel_type);
            }
        }
        for rel_type in hit_types {
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

/// Shared, read-only context for rendering a `dep tree`.
struct TreeCtx<'a> {
    state: &'a HashMap<String, TaskState>,
    blockers: &'a BTreeSet<String>,
    hierarchy: &'a BTreeSet<String>,
    status_field: &'a str,
    done_status: &'a str,
}

/// `ta dep tree` — ASCII tree of the blocker graph (the `depends_on` field plus
/// any `blocker`- or `hierarchy`-typed relationship), children nested under their
/// dependents. Roots default to tasks nothing depends on (the top-level goals); if
/// every task has a dependent (e.g. a pure cycle), all tasks are used as roots so
/// nothing is hidden. Hierarchy (subtask) edges are tagged `[subtask]` and a
/// parent rolls up its child completion as `[subtasks done/total]`; other
/// non-`depends_on` blocker edges are labelled with their type.
fn dep_tree(store: &impl EventStore, tasks: &[String]) -> Result<(), DynError> {
    let state = state_of(store)?;
    let blockers = store.config().relationships.blocker_types();
    let hierarchy = store.config().relationships.hierarchy_types();
    let wf = store.config().workflow.clone();
    let roots = if tasks.is_empty() {
        let depended: BTreeSet<&str> = state
            .values()
            .flat_map(|t| {
                crate::graph::blocker_edges(t, &blockers)
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
        r.sort();
        r
    } else {
        for t in tasks {
            if !state.contains_key(t) {
                return Err(format!("no task `{t}`").into());
            }
        }
        tasks.to_vec()
    };
    if roots.is_empty() {
        println!("(no tasks)");
        return Ok(());
    }
    let ctx = TreeCtx {
        state: &state,
        blockers: &blockers,
        hierarchy: &hierarchy,
        status_field: &wf.status_field,
        done_status: &wf.done_status,
    };
    let mut out = String::new();
    let mut expanded: HashSet<String> = HashSet::new();
    for root in &roots {
        out.push_str(root);
        out.push_str(&rollup_suffix(&ctx, root));
        out.push('\n');
        expanded.insert(root.clone());
        let mut path = vec![root.clone()];
        push_subtree(&ctx, root, "", &mut out, &mut path, &mut expanded);
    }
    print!("{out}");
    Ok(())
}

/// A parent's subtask-completion suffix, ` [subtasks done/total]`, over its direct
/// hierarchy children; empty for a task with none.
fn rollup_suffix(ctx: &TreeCtx, id: &str) -> String {
    let Some(task) = ctx.state.get(id) else {
        return String::new();
    };
    let (mut done, mut total) = (0usize, 0usize);
    for htype in ctx.hierarchy {
        for child in task.relationships.get(htype).into_iter().flatten() {
            total += 1;
            if ctx
                .state
                .get(child)
                .is_some_and(|t| is_done(t, ctx.status_field, ctx.done_status))
            {
                done += 1;
            }
        }
    }
    if total == 0 {
        String::new()
    } else {
        format!(" [subtasks {done}/{total}]")
    }
}

/// Append `id`'s blocker children to `out` with box-drawing connectors: hierarchy
/// edges tagged `[subtask]`, other non-`depends_on` edges labelled with their
/// type, and each node's own subtask rollup appended. `path` (ancestors) breaks
/// cycles; `expanded` collapses a node already shown in full elsewhere to `…`.
fn push_subtree(
    ctx: &TreeCtx,
    id: &str,
    prefix: &str,
    out: &mut String,
    path: &mut Vec<String>,
    expanded: &mut HashSet<String>,
) {
    let Some(task) = ctx.state.get(id) else {
        return;
    };
    let children = crate::graph::blocker_edges(task, ctx.blockers);
    let n = children.len();
    for (i, &(child, kind)) in children.iter().enumerate() {
        let last = i + 1 == n;
        out.push_str(prefix);
        out.push_str(if last { "└─ " } else { "├─ " });
        out.push_str(child);
        if ctx.hierarchy.contains(kind) {
            out.push_str(" [subtask]");
        } else if kind != "depends_on" {
            out.push_str(" [");
            out.push_str(kind);
            out.push(']');
        }
        out.push_str(&rollup_suffix(ctx, child));
        let has_subtree = ctx
            .state
            .get(child)
            .is_some_and(|t| !crate::graph::blocker_edges(t, ctx.blockers).is_empty());
        if !ctx.state.contains_key(child) {
            out.push_str(" (missing)\n");
        } else if path.iter().any(|p| p.as_str() == child) {
            out.push_str(" (cycle)\n");
        } else if expanded.contains(child) && has_subtree {
            out.push_str(" …\n");
        } else {
            out.push('\n');
            expanded.insert(child.to_string());
            path.push(child.to_string());
            let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
            push_subtree(ctx, child, &child_prefix, out, path, expanded);
            path.pop();
        }
    }
}

/// `ta dep cycles` — report any cycles in the blocker graph.
fn dep_cycles(store: &impl EventStore) -> Result<(), DynError> {
    let state = state_of(store)?;
    let blockers = store.config().relationships.blocker_types();
    let cycles = crate::graph::dependency_cycles(&state, &blockers);
    if cycles.is_empty() {
        println!("No dependency cycles.");
        return Ok(());
    }
    println!("{} dependency cycle(s):", cycles.len());
    for cycle in &cycles {
        if cycle.len() == 1 {
            println!("  {} (depends on itself)", cycle[0]);
        } else {
            println!("  {}", cycle.join(" ↔ "));
        }
    }
    Ok(())
}

/// `ta dep plan <goal> …` — the not-done transitive prerequisites of the goal(s)
/// (the goals included), in dependency order: do exactly these, in this order.
/// Prerequisites are the blocker edges (the `depends_on` field plus any
/// `blocker`-typed relationship); already-done ones are dropped as satisfied.
/// `--critical` narrows the list to the longest single chain of incomplete
/// prerequisites — the sequence that sets the minimum remaining duration.
fn dep_plan(store: &impl EventStore, goals: &[String], critical: bool) -> Result<(), DynError> {
    let state = state_of(store)?;
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
            for (dep, _) in crate::graph::blocker_edges(task, &blockers) {
                if state.contains_key(dep) {
                    stack.push(dep.to_string());
                }
            }
        }
    }

    // Order just that subgraph (prerequisites before dependents); a cycle within
    // it is surfaced as an error, like `ta list --ready`.
    let sub: HashMap<String, TaskState> = want
        .iter()
        .filter_map(|id| state.get(id).map(|t| (id.clone(), t.clone())))
        .collect();
    let order = crate::graph::validate_and_sort_dependencies(&sub, &blockers)?;

    let wf = store.config().workflow.clone();
    let remaining: Vec<&String> = order
        .iter()
        .filter(|id| {
            sub.get(id.as_str())
                .is_some_and(|t| !is_done(t, &wf.status_field, &wf.done_status))
        })
        .collect();
    if remaining.is_empty() {
        println!("Nothing to do — every prerequisite is already done.");
        return Ok(());
    }

    let total = remaining.len();
    let to_print: Vec<String> = if critical {
        critical_path(&remaining, &sub, &blockers)
    } else {
        remaining.iter().map(|id| (*id).clone()).collect()
    };

    let width = to_print.iter().map(String::len).max().unwrap_or(0);
    for (i, id) in to_print.iter().enumerate() {
        let status = sub
            .get(id.as_str())
            .and_then(|t| t.custom_fields.get(&wf.status_field))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        println!("{:>2}. {id:<width$}  {status}", i + 1);
    }
    if critical {
        println!(
            "(critical path: {} of {total} remaining task(s))",
            to_print.len()
        );
    } else {
        println!("({total} task(s) remaining, in order)");
    }
    Ok(())
}

/// The longest chain of incomplete prerequisites within `remaining` (already in
/// topological order, prerequisites first). A DP over that order — `depth(t) =
/// 1 + max(depth(p))` across `t`'s not-done blocker prerequisites — then a
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
            for (dep, _) in crate::graph::blocker_edges(task, blockers) {
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
