//! The `ta dep` command group — add, remove, and inspect (tree/cycles/plan)
//! typed relationship edges between tasks.

use std::collections::BTreeMap;

use clap::Subcommand;
use serde_json::Value;

use crate::action::dep::{Kids, Node};
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::format::OutputArgs;
use crate::model::{OpType, DEPS_KEY, ID_KEY, SUBTASKS_KEY};
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
/// event carrying an explicit `target`+`rel` is appended per edge (every type,
/// `depends_on` included, is stored uniformly in the `relationships` map).
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

/// Add or remove the `name=target` edges via [`crate::action::dep::apply_edges`],
/// reporting how many stored edges changed.
fn dep_write(
    store: &impl EventStore,
    task: &str,
    edges: &[String],
    op: &OpType,
    verb: &str,
    types: &BTreeMap<String, RelationshipDef>,
) -> Result<(), DynError> {
    let written = crate::action::dep::apply_edges(store, task, edges, op, types)?;
    if written == 0 {
        println!("no changes on `{task}`");
    } else {
        println!("{verb} {written} edge(s) on `{task}`");
    }
    Ok(())
}

/// A shortened title is truncated to this many characters in the tree.
const TREE_TITLE_MAX: usize = 50;

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
    // The action builds and orders the forest itself; resolve the sort column
    // here (default `[display].sort`) and hand it the name.
    let column = sort.unwrap_or_else(|| store.config().display.sort.clone());
    // The per-node columns are the configured display columns minus `id` (the node
    // itself) and `deps` (the tree). No field name is hardcoded.
    let columns: Vec<String> = store
        .config()
        .display
        .columns
        .iter()
        .filter(|c| c.as_str() != ID_KEY && c.as_str() != DEPS_KEY)
        .cloned()
        .collect();
    let outcome = crate::action::dep::tree(
        store,
        &crate::action::dep::TreeQuery {
            roots: tasks,
            open,
            reverse,
            columns: &columns,
            sort: &column,
        },
    )?;
    crate::cli::print_warnings(&outcome.warnings);
    let color = crate::format::want_color(output.no_color);
    if outcome.forest.is_empty() {
        let human = if open { "(nothing open)" } else { "(no tasks)" };
        crate::format::emit(output, human, &Value::Array(Vec::new()));
        return Ok(());
    }
    // Render the built-once forest to BOTH human and JSON.
    let value = Value::Array(outcome.forest.iter().map(node_json).collect());
    let human = render_human_forest(&outcome.forest, color);
    crate::format::emit(output, &human, &value);
    Ok(())
}

/// The colored label for one node: id, then each requested column's value
/// (truncated), the edge tag (`[subtask]` magenta, `[type]` plain, nothing for
/// `depends_on`/root) and its `[subtasks d/t]` rollup. A done node is dimmed and
/// prefixed `✓`. The connectors and position markers (`(cycle)`/`(missing)`/`…`)
/// are added by the caller.
fn node_label(node: &Node, color: bool) -> String {
    let mut cells = String::new();
    for (_, v) in &node.cells {
        cells.push_str("  ");
        cells.push_str(&crate::format::truncate(&render_cell(v), TREE_TITLE_MAX));
    }
    let rollup = node
        .rollup
        .map_or(String::new(), |(d, t)| format!(" [subtasks {d}/{t}]"));
    if node.done {
        let mut s = format!("✓ {}{cells}", node.id);
        if let Some(e) = &node.edge {
            s.push_str(" [");
            s.push_str(e);
            s.push(']');
        }
        s.push_str(&rollup);
        crate::format::sgr(&s, "2", color)
    } else {
        let mut s = crate::format::sgr(&node.id, "36", color);
        s.push_str(&cells);
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

/// A column value as a one-line human string: the raw string for a JSON string,
/// elements joined by `", "` for an array, else its compact JSON.
fn render_cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(render_cell).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
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
    // Each requested column becomes a key, full value (json isn't truncated).
    for (name, value) in &node.cells {
        o.insert(name.clone(), value.clone());
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

/// `ta dep cycles` — report any cycles in the blocker graph. JSON is an array of
/// cycles (each an array of member ids); human is one cycle per line.
fn dep_cycles(store: &impl EventStore, output: &OutputArgs) -> Result<(), DynError> {
    let outcome = crate::action::dep::cycles(store)?;
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
    let outcome = crate::action::dep::plan(store, goals, critical)?;
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
