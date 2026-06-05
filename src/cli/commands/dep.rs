//! The `ta dep` command group — add, remove, list, and inspect typed
//! relationship edges between tasks.

use std::cmp::Ordering;
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
    /// ASCII dependency tree: `ta dep tree [<task> …]` (roots default to tasks
    /// nothing depends on)
    Tree {
        /// Root tasks (default: every task nothing depends on)
        tasks: Vec<String>,
        /// Prune fully-resolved branches (no open task); done tasks that still
        /// lead to open work stay, so the graph is never spliced
        #[arg(long)]
        open: bool,
        /// Order siblings/roots by this column (default: [display].sort)
        #[arg(long)]
        sort: Option<String>,
        /// Reverse the sibling/root order
        #[arg(long)]
        reverse: bool,
        /// Disable ANSI color (also auto-disabled when stdout is not a TTY)
        #[arg(long)]
        no_color: bool,
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
        DepAction::Tree {
            tasks,
            open,
            sort,
            reverse,
            no_color,
        } => dep_tree(store, &tasks, open, sort, reverse, no_color),
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
    let mut resolved: Vec<(String, String, String)> = Vec::new();
    for edge in edges {
        let (name, target) = edge
            .split_once('=')
            .filter(|(t, v)| !t.is_empty() && !v.is_empty())
            .ok_or_else(|| format!("invalid edge `{edge}` (expected type=target)"))?;
        resolved.extend(resolve_edge(name, task, target, types, removing)?);
    }
    if !removing {
        validate_blocker_additions(store, &resolved)?;
    }
    let mut events = Vec::with_capacity(resolved.len());
    for (owner, rel_type, dep) in resolved {
        let mut payload = Map::new();
        payload.insert("dep".to_string(), Value::String(dep));
        // `depends_on` omits the type to stay legacy-shaped.
        if rel_type != "depends_on" {
            payload.insert("type".to_string(), Value::String(rel_type));
        }
        events.push(MutationEvent::new(op.clone(), owner, payload));
    }
    store.append_events(&events)?;
    println!("{verb} {} edge(s) on `{task}`", edges.len());
    Ok(())
}

/// Reject blocker-edge additions that would break the structural invariants:
/// (1) at most one blocking relationship between two tasks, and (2) a task may
/// have at most one parent (one incoming `hierarchy` edge). Checked incrementally
/// against the current state *plus* the edges added earlier in this command, so a
/// pre-existing violation elsewhere never blocks an unrelated add.
fn validate_blocker_additions(
    store: &impl EventStore,
    resolved: &[(String, String, String)],
) -> Result<(), DynError> {
    let blockers = store.config().relationships.blocker_types();
    if !resolved
        .iter()
        .any(|(_, t, _)| blockers.contains(t.as_str()))
    {
        return Ok(());
    }
    let hierarchy = store.config().relationships.hierarchy_types();
    let state = state_of(store)?;

    // Seed the would-be view from current state: each owner's blocker target→type,
    // and each child's parent.
    let mut blocker_to: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for (id, task) in &state {
        for (target, kind) in crate::graph::blocker_edges(task, &blockers) {
            blocker_to
                .entry(id.clone())
                .or_default()
                .insert(target.to_string(), kind.to_string());
        }
        for htype in &hierarchy {
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

/// A shortened title is truncated to this many characters in the tree.
const TREE_TITLE_MAX: usize = 50;

/// Shared, read-only context for rendering a `dep tree`.
struct TreeCtx<'a> {
    state: &'a HashMap<String, TaskState>,
    blockers: &'a BTreeSet<String>,
    hierarchy: &'a BTreeSet<String>,
    status_field: &'a str,
    done_status: &'a str,
    column: &'a str,
    reverse: bool,
    open: bool,
    color: bool,
    /// Tasks whose blocker-subtree contains at least one open task.
    open_subtrees: &'a HashSet<String>,
}

/// `ta dep tree` — ASCII tree of the blocker graph (the `depends_on` field plus
/// any `blocker`- or `hierarchy`-typed relationship), children nested under their
/// dependents. Shows the exact graph by default — done tasks are dimmed and marked
/// `✓`, never spliced out — so the structure is faithful; `--open` prunes only
/// fully-resolved branches (those with no open task). Roots default to tasks
/// nothing depends on, ordered by `--sort`/`[display].sort` (`--reverse` flips).
/// Subtask edges are tagged `[subtask]` with a `[subtasks d/t]` parent rollup;
/// other non-`depends_on` blocker edges are labelled with their type.
fn dep_tree(
    store: &impl EventStore,
    tasks: &[String],
    open: bool,
    sort: Option<String>,
    reverse: bool,
    no_color: bool,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let blockers = store.config().relationships.blocker_types();
    let hierarchy = store.config().relationships.hierarchy_types();
    let wf = store.config().workflow.clone();
    let column = sort.unwrap_or_else(|| store.config().display.sort.clone());
    let color = crate::format::want_color(no_color);
    let open_subtrees = compute_open_subtrees(&state, &blockers, &wf.status_field, &wf.done_status);

    let mut roots = if tasks.is_empty() {
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
        r
    } else {
        for t in tasks {
            if !state.contains_key(t) {
                return Err(format!("no task `{t}`").into());
            }
        }
        tasks.to_vec()
    };
    sort_ids(&mut roots, &state, &column, reverse);
    if open {
        roots.retain(|r| open_subtrees.contains(r));
    }
    if roots.is_empty() {
        println!("{}", if open { "(nothing open)" } else { "(no tasks)" });
        return Ok(());
    }

    let ctx = TreeCtx {
        state: &state,
        blockers: &blockers,
        hierarchy: &hierarchy,
        status_field: &wf.status_field,
        done_status: &wf.done_status,
        column: &column,
        reverse,
        open,
        color,
        open_subtrees: &open_subtrees,
    };
    let mut out = String::new();
    let mut expanded: HashSet<String> = HashSet::new();
    for root in &roots {
        out.push_str(&render_node(&ctx, root, None));
        out.push('\n');
        expanded.insert(root.clone());
        let mut path = vec![root.clone()];
        push_subtree(&ctx, root, "", &mut out, &mut path, &mut expanded);
    }
    print!("{out}");
    Ok(())
}

/// The colored label for one node: id, a shortened `title`, the edge tag
/// (`[subtask]` for a hierarchy edge, `[type]` for another blocker type, nothing
/// for `depends_on` or a root via `kind = None`), and its own `[subtasks d/t]`
/// rollup. A done task is dimmed and prefixed `✓`; an open task gets a cyan id,
/// magenta `[subtask]`, and a yellow rollup. Connectors and the position markers
/// (`(cycle)`/`(missing)`/`…`) are added by the caller.
fn render_node(ctx: &TreeCtx, id: &str, kind: Option<&str>) -> String {
    let done = ctx
        .state
        .get(id)
        .is_some_and(|t| is_done(t, ctx.status_field, ctx.done_status));
    let title = ctx
        .state
        .get(id)
        .and_then(|t| t.custom_fields.get("title"))
        .and_then(Value::as_str)
        .map(|s| crate::format::truncate(s, TREE_TITLE_MAX))
        .unwrap_or_default();
    let is_subtask = kind.is_some_and(|k| ctx.hierarchy.contains(k));
    let tag = match kind {
        Some(_) if is_subtask => Some("subtask".to_string()),
        Some(k) if k != "depends_on" => Some(k.to_string()),
        _ => None,
    };
    let rollup = {
        let (d, total) = ctx.state.get(id).map_or((0, 0), |t| {
            crate::graph::subtask_counts(
                t,
                ctx.state,
                ctx.hierarchy,
                ctx.status_field,
                ctx.done_status,
            )
        });
        if total == 0 {
            String::new()
        } else {
            format!(" [subtasks {d}/{total}]")
        }
    };
    let title_part = if title.is_empty() {
        String::new()
    } else {
        format!("  {title}")
    };
    if done {
        // Done: the whole node dimmed and check-marked.
        let mut s = format!("✓ {id}{title_part}");
        if let Some(t) = &tag {
            s.push_str(" [");
            s.push_str(t);
            s.push(']');
        }
        s.push_str(&rollup);
        crate::format::sgr(&s, "2", ctx.color)
    } else {
        let mut s = crate::format::sgr(id, "36", ctx.color); // cyan id
        s.push_str(&title_part);
        if let Some(t) = &tag {
            let tag_str = format!(" [{t}]");
            s.push_str(&if is_subtask {
                crate::format::sgr(&tag_str, "35", ctx.color) // magenta
            } else {
                tag_str
            });
        }
        if !rollup.is_empty() {
            s.push_str(&crate::format::sgr(&rollup, "33", ctx.color)); // yellow
        }
        s
    }
}

/// Append `id`'s blocker children to `out`, ordered by the chosen sort column;
/// `--open` prunes children whose subtree is fully resolved. `path` (ancestors)
/// breaks cycles; `expanded` collapses a node already shown in full elsewhere.
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
    let mut children = crate::graph::blocker_edges(task, ctx.blockers);
    children.sort_by(|a, b| child_cmp(ctx, a.0, b.0));
    if ctx.reverse {
        children.reverse();
    }
    if ctx.open {
        children.retain(|(c, _)| ctx.open_subtrees.contains(*c));
    }
    let n = children.len();
    for (i, &(child, kind)) in children.iter().enumerate() {
        let last = i + 1 == n;
        out.push_str(prefix);
        out.push_str(if last { "└─ " } else { "├─ " });
        if !ctx.state.contains_key(child) {
            out.push_str(child);
            out.push_str(" (missing)\n");
            continue;
        }
        out.push_str(&render_node(ctx, child, Some(kind)));
        let has_subtree = ctx
            .state
            .get(child)
            .is_some_and(|t| !crate::graph::blocker_edges(t, ctx.blockers).is_empty());
        if path.iter().any(|p| p.as_str() == child) {
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

/// Order two task ids by the tree's sort column (missing tasks last, id tiebreak).
fn child_cmp(ctx: &TreeCtx, a: &str, b: &str) -> Ordering {
    match (ctx.state.get(a), ctx.state.get(b)) {
        (Some(ta), Some(tb)) => crate::format::task_cmp(ta, tb, ctx.column),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

/// Sort task ids in place by `column` (missing tasks last), flipped by `reverse`.
fn sort_ids(ids: &mut [String], state: &HashMap<String, TaskState>, column: &str, reverse: bool) {
    ids.sort_by(|a, b| match (state.get(a), state.get(b)) {
        (Some(ta), Some(tb)) => crate::format::task_cmp(ta, tb, column),
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
        for (child, _) in crate::graph::blocker_edges(task, blockers) {
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
