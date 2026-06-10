//! The `ta dep` command group — add, remove, and inspect (tree/cycles/plan)
//! typed relationship edges between tasks.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use clap::Subcommand;
use serde_json::Value;

use crate::cli::state_of;
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::format::OutputArgs;
use crate::model::{is_done, OpType, TaskState, DEPENDS_ON, ID_KEY, SUBTASKS_KEY};
use crate::storage::EventStore;

/// `ta dep` subcommands. Edges are `type=target` tokens; `type` must be declared
/// in `[relationships]`.
#[derive(Subcommand)]
pub enum DepAction {
    /// Add typed edge(s): `ta dep add <task> depends_on=<other> [relates_to=<x> …]`
    ///
    /// Both tasks must exist and a task can't reference itself; a duplicate edge
    /// is a no-op. At most one blocker between a pair, and one parent per task.
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
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Report dependency cycles in the blocker graph: `ta dep cycles`
    Cycles {
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Ordered remaining prerequisites of a goal: `ta dep plan <goal> …`
    Plan {
        /// Goal task(s) to plan toward
        #[arg(required = true)]
        goals: Vec<String>,
        /// Show only the critical path: the longest chain of incomplete prerequisites
        #[arg(long)]
        critical: bool,
        #[command(flatten)]
        output: OutputArgs,
    },
}

/// Add or remove typed dependency edges. Each `type=target` edge's type is
/// validated against the declared relationship types; an `AddEdge`/`RemoveEdge`
/// event is appended per edge (the `depends_on` type omits an explicit `type` to
/// stay legacy-shaped on disk — it's stored in the dedicated `depends_on` field).
pub fn cmd_dep_group(
    store: &impl EventStore,
    action: DepAction,
    types: &BTreeMap<String, RelationshipDef>,
) -> Result<(), DynError> {
    match action {
        DepAction::Add { task, edges } => {
            dep_write(store, &task, &edges, &OpType::AddEdge, "Added", types)
        }
        DepAction::Remove { task, edges } => {
            dep_write(store, &task, &edges, &OpType::RemoveEdge, "Removed", types)
        }
        DepAction::Tree {
            tasks,
            open,
            sort,
            reverse,
            output,
        } => dep_tree(store, &tasks, open, sort, reverse, &output),
        DepAction::Cycles { output } => dep_cycles(store, &output),
        DepAction::Plan {
            goals,
            critical,
            output,
        } => dep_plan(store, &goals, critical, &output),
    }
}

/// Add or remove the `name=target` edges via [`crate::action::apply_edges`],
/// reporting how many stored edges changed.
fn dep_write(
    store: &impl EventStore,
    task: &str,
    edges: &[String],
    op: &OpType,
    verb: &str,
    types: &BTreeMap<String, RelationshipDef>,
) -> Result<(), DynError> {
    let written = crate::action::apply_edges(store, task, edges, op, types)?;
    if written == 0 {
        println!("no changes on `{task}`");
    } else {
        println!("{verb} {written} edge(s) on `{task}`");
    }
    Ok(())
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
    output: &OutputArgs,
) -> Result<(), DynError> {
    let state = state_of(store)?;
    let blockers = store.config().relationships.blocker_types();
    let hierarchy = store.config().relationships.hierarchy_types();
    let wf = store.config().workflow.clone();
    let column = sort.unwrap_or_else(|| store.config().display.sort.clone());
    let color = crate::format::want_color(output.no_color);
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
        let human = if open { "(nothing open)" } else { "(no tasks)" };
        crate::format::emit(output, human, &Value::Array(Vec::new()));
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
        open_subtrees: &open_subtrees,
    };
    // Build the forest once, then render BOTH human and JSON from it. `build`
    // marks a node expanded/on-path itself, so roots start from empty state.
    let mut expanded: HashSet<String> = HashSet::new();
    let forest: Vec<Node> = roots
        .iter()
        .map(|root| {
            let mut path = Vec::new();
            build(&ctx, root, None, &mut path, &mut expanded)
        })
        .collect();

    let value = Value::Array(forest.iter().map(node_json).collect());
    let human = render_human_forest(&forest, color);
    crate::format::emit(output, &human, &value);
    Ok(())
}

/// One node of a rendered tree, built once and rendered to both human and JSON.
struct Node {
    id: String,
    title: String,
    status: String,
    done: bool,
    /// Edge to the parent: `subtask` for a hierarchy edge, the type name for
    /// another non-`depends_on` blocker, `None` for `depends_on` or a root.
    edge: Option<String>,
    /// `(done, total)` subtask rollup, when the node has hierarchy children.
    rollup: Option<(usize, usize)>,
    kids: Kids,
}

enum Kids {
    Children(Vec<Node>),
    /// Already shown in full elsewhere (a repeated DAG node).
    Collapsed,
    /// A back-edge to an ancestor.
    Cycle,
    /// The target id isn't a known task.
    Missing,
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
        Some(k) if k != DEPENDS_ON => Some(k.to_string()),
        _ => None,
    };
    let Some(task) = ctx.state.get(id) else {
        return Node {
            id: id.to_string(),
            title: String::new(),
            status: String::new(),
            done: false,
            edge: None,
            rollup: None,
            kids: Kids::Missing,
        };
    };
    let done = is_done(task, ctx.status_field, ctx.done_status);
    let title = task
        .custom_fields
        .get("title")
        .and_then(Value::as_str)
        .map(|s| crate::format::truncate(s, TREE_TITLE_MAX))
        .unwrap_or_default();
    let status = task
        .custom_fields
        .get(ctx.status_field)
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let (rd, rt) = crate::graph::subtask_counts(
        task,
        ctx.state,
        ctx.hierarchy,
        ctx.status_field,
        ctx.done_status,
    );
    let rollup = (rt > 0).then_some((rd, rt));

    let kids = if path.iter().any(|p| p == id) {
        Kids::Cycle
    } else if expanded.contains(id) && !crate::graph::blocker_edges(task, ctx.blockers).is_empty() {
        Kids::Collapsed
    } else {
        expanded.insert(id.to_string());
        path.push(id.to_string());
        let mut children = crate::graph::blocker_edges(task, ctx.blockers);
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
        title,
        status,
        done,
        edge,
        rollup,
        kids,
    }
}

/// The colored label for one node: id, a shortened `title`, the edge tag
/// (`[subtask]` magenta, `[type]` plain, nothing for `depends_on`/root) and its
/// `[subtasks d/t]` rollup. A done node is dimmed and prefixed `✓`. The connectors
/// and position markers (`(cycle)`/`(missing)`/`…`) are added by the caller.
fn node_label(node: &Node, color: bool) -> String {
    let title_part = if node.title.is_empty() {
        String::new()
    } else {
        format!("  {}", node.title)
    };
    let rollup = node
        .rollup
        .map_or(String::new(), |(d, t)| format!(" [subtasks {d}/{t}]"));
    if node.done {
        let mut s = format!("✓ {}{title_part}", node.id);
        if let Some(e) = &node.edge {
            s.push_str(" [");
            s.push_str(e);
            s.push(']');
        }
        s.push_str(&rollup);
        crate::format::sgr(&s, "2", color)
    } else {
        let mut s = crate::format::sgr(&node.id, "36", color);
        s.push_str(&title_part);
        if let Some(e) = &node.edge {
            let tag = format!(" [{e}]");
            s.push_str(&if e == "subtask" {
                crate::format::sgr(&tag, "35", color)
            } else {
                tag
            });
        }
        if !rollup.is_empty() {
            s.push_str(&crate::format::sgr(&rollup, "33", color));
        }
        s
    }
}

/// Render the forest to the ASCII tree with box-drawing connectors.
fn render_human_forest(forest: &[Node], color: bool) -> String {
    let mut out = String::new();
    for node in forest {
        out.push_str(&node_label(node, color));
        out.push('\n');
        push_kids(node, "", &mut out, color);
    }
    out.trim_end_matches('\n').to_string()
}

fn push_kids(node: &Node, prefix: &str, out: &mut String, color: bool) {
    let Kids::Children(kids) = &node.kids else {
        return;
    };
    let n = kids.len();
    for (i, kid) in kids.iter().enumerate() {
        let last = i + 1 == n;
        out.push_str(prefix);
        out.push_str(if last { "└─ " } else { "├─ " });
        out.push_str(&node_label(kid, color));
        match &kid.kids {
            Kids::Missing => out.push_str(" (missing)"),
            Kids::Cycle => out.push_str(" (cycle)"),
            Kids::Collapsed => out.push_str(" …"),
            Kids::Children(_) => {}
        }
        out.push('\n');
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
        push_kids(kid, &child_prefix, out, color);
    }
}

/// One tree node as JSON, recursing into children.
fn node_json(node: &Node) -> Value {
    let mut o = serde_json::Map::new();
    o.insert(ID_KEY.to_string(), Value::String(node.id.clone()));
    if !node.title.is_empty() {
        o.insert("title".to_string(), Value::String(node.title.clone()));
    }
    if !node.status.is_empty() {
        o.insert("status".to_string(), Value::String(node.status.clone()));
    }
    o.insert("done".to_string(), Value::Bool(node.done));
    if let Some(e) = &node.edge {
        o.insert("edge".to_string(), Value::String(e.clone()));
    }
    if let Some((d, t)) = node.rollup {
        o.insert(
            SUBTASKS_KEY.to_string(),
            serde_json::json!({"done": d, "total": t}),
        );
    }
    match &node.kids {
        Kids::Children(kids) if !kids.is_empty() => {
            o.insert(
                "children".to_string(),
                Value::Array(kids.iter().map(node_json).collect()),
            );
        }
        Kids::Children(_) => {}
        Kids::Cycle => {
            o.insert("cycle".to_string(), Value::Bool(true));
        }
        Kids::Collapsed => {
            o.insert("collapsed".to_string(), Value::Bool(true));
        }
        Kids::Missing => {
            o.insert("missing".to_string(), Value::Bool(true));
        }
    }
    Value::Object(o)
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

/// `ta dep cycles` — report any cycles in the blocker graph. JSON is an array of
/// cycles (each an array of member ids); human is one cycle per line.
fn dep_cycles(store: &impl EventStore, output: &OutputArgs) -> Result<(), DynError> {
    let outcome = crate::action::cycles(store)?;
    crate::cli::print_warnings(&outcome.warnings);
    let cycles = outcome.cycles;
    let color = crate::format::want_color(output.no_color);

    let value = Value::Array(
        cycles
            .iter()
            .map(|c| Value::Array(c.iter().cloned().map(Value::String).collect()))
            .collect(),
    );
    let human = if cycles.is_empty() {
        "No dependency cycles.".to_string()
    } else {
        let mut lines = vec![crate::format::sgr(
            &format!("{} dependency cycle(s):", cycles.len()),
            "1",
            color,
        )];
        for cycle in &cycles {
            if cycle.len() == 1 {
                lines.push(format!("  {} (depends on itself)", cycle[0]));
            } else {
                lines.push(format!("  {}", cycle.join(" ↔ ")));
            }
        }
        lines.join("\n")
    };
    crate::format::emit(output, &human, &value);
    Ok(())
}

/// `ta dep plan <goal> …` — the not-done transitive prerequisites of the goal(s)
/// (the goals included), in dependency order: do exactly these, in this order.
/// Prerequisites are the blocker edges (the `depends_on` field plus any
/// `blocker`-typed relationship); already-done ones are dropped as satisfied.
/// `--critical` narrows the list to the longest single chain of incomplete
/// prerequisites — the sequence that sets the minimum remaining duration.
fn dep_plan(
    store: &impl EventStore,
    goals: &[String],
    critical: bool,
    output: &OutputArgs,
) -> Result<(), DynError> {
    let outcome = crate::action::plan(store, goals, critical)?;
    crate::cli::print_warnings(&outcome.warnings);
    let steps = &outcome.steps;
    let total = outcome.total;
    let color = crate::format::want_color(output.no_color);

    let value = Value::Array(
        steps
            .iter()
            .map(|s| serde_json::json!({ "id": &s.id, "status": &s.status }))
            .collect(),
    );
    let human = if steps.is_empty() {
        "Nothing to do — every prerequisite is already done.".to_string()
    } else {
        let width = steps.iter().map(|s| s.id.len()).max().unwrap_or(0);
        let mut lines: Vec<String> = steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let id_c = crate::format::sgr(&format!("{:<width$}", s.id), "36", color);
                format!("{:>2}. {id_c}  {}", i + 1, s.status)
            })
            .collect();
        lines.push(if outcome.critical {
            format!(
                "(critical path: {} of {total} remaining task(s))",
                steps.len()
            )
        } else {
            format!("({total} task(s) remaining, in order)")
        });
        lines.join("\n")
    };
    crate::format::emit(output, &human, &value);
    Ok(())
}
