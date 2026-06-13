//! The `ta dep` command group - add, remove, and inspect (tree/cycles/plan)
//! typed relationship edges between tasks.

use std::collections::{BTreeMap, HashMap};

use clap::Subcommand;
use clap_complete::engine::{ArgValueCandidates, ArgValueCompleter};
use serde_json::Value;

use crate::cli::complete;

use crate::action::dep::{Kids, Node};
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::format::{OutputArgs, RowStyle};
use crate::model::{OpType, DEPS_KEY, ID_KEY, STATUS_KEY, SUBTASKS_KEY};
use crate::storage::EventStore;

/// `ta dep` subcommands. Edges are `type=target` tokens; `type` must be declared
/// in `[relationships]`.
#[derive(Subcommand)]
pub enum DepAction {
    /// Add typed edge(s): `ta dep add <task> depends_on=<other> [relates_to=<x> ...]`
    ///
    /// Both tasks must exist and a task can't reference itself; a duplicate edge
    /// is a no-op. At most one blocker between a pair, and one parent per task.
    Add {
        #[arg(add = ArgValueCandidates::new(complete::task_ids))]
        task: String,
        /// `type=target` pairs (each `type` must be a declared relationship type)
        #[arg(required = true, add = ArgValueCompleter::new(complete::criteria))]
        edges: Vec<String>,
    },
    /// Remove typed edge(s): `ta dep remove <task> depends_on=<other> ...`
    Remove {
        #[arg(add = ArgValueCandidates::new(complete::task_ids))]
        task: String,
        /// `type=target` pairs to remove
        #[arg(required = true, add = ArgValueCompleter::new(complete::criteria))]
        edges: Vec<String>,
    },
    /// Dependency tree: `ta dep tree [<task> ...]` (roots default to tasks
    /// nothing depends on)
    Tree {
        /// Root tasks (default: every task nothing depends on)
        #[arg(add = ArgValueCandidates::new(complete::task_ids))]
        tasks: Vec<String>,
        /// Prune fully-resolved branches (no open task); done tasks that still
        /// lead to open work stay, so the graph is never spliced
        #[arg(long)]
        open: bool,
        /// Order siblings/roots by this column (default: [display].sort)
        #[arg(long, add = ArgValueCandidates::new(complete::columns))]
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
    /// Ordered remaining prerequisites of a goal: `ta dep plan <goal> ...`
    Plan {
        /// Goal task(s) to plan toward
        #[arg(required = true, add = ArgValueCandidates::new(complete::task_ids))]
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

/// `ta dep tree` - box-drawing tree of the blocker graph (the `depends_on` field
/// plus any `blocker`- or `hierarchy`-typed relationship), children nested under
/// their dependents. Shows the exact graph by default - done tasks are dimmed and
/// marked with a check mark, never spliced out - so the structure is faithful;
/// `--open` prunes only fully-resolved branches (those with no open task). Roots
/// default to tasks nothing depends on, ordered by `--sort`/`[display].sort`
/// (`--reverse` flips). Subtask edges show a `[x]`/`[ ]` done-state checkbox with
/// a `[subtasks d/t]` parent rollup; other non-`depends_on` blocker edges are
/// labelled with their type.
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
    // Render the built-once forest to BOTH human and JSON. The shared RowStyle
    // gives the tree the same per-column / done coloring as a `list` row.
    let value = Value::Array(outcome.forest.iter().map(node_json).collect());
    let workflow = &store.config().workflow;
    let style = crate::format::RowStyle {
        status_field: &workflow.status_field,
        done_status: &workflow.done_status,
    };
    let human = render_human_forest(&outcome.forest, color, style);
    crate::format::emit(output, &human, &value);
    Ok(())
}

/// The label for one node. A subtask edge LEADS with a done-state checkbox
/// (`[x]` done / `[ ]` open, magenta when open); every other edge leads with its
/// `[type]` tag (nothing for the default blocker / a root) and a done node is
/// then prefixed with a check mark. After the prefix come the id and each
/// requested column's value (truncated), then the `[subtasks d/t]` rollup. The
/// id/columns are colored by the shared [`RowStyle`], identical to a `list` row
/// (id cyan, the status column green); a done node greys whole (dim). The
/// connectors and position markers (`(cycle)`/`(missing)`/`(expanded above|below)`)
/// are added by the caller.
fn node_label(node: &Node, color: bool, style: RowStyle) -> String {
    let done = node.done;
    let paint = |text: &str, col: &str| crate::format::paint_cell(text, col, done, style, color);
    // A subtask edge renders as a checkbox encoding its done state - filled
    // (U+2713) when done, empty when open - dim when done, else magenta so pending
    // subtasks stand out. Every OTHER edge keeps its `[type]` tag (dim when done,
    // else plain), and a done node is then prefixed with the check mark.
    let mut s = if node.edge.as_deref() == Some("subtask") {
        let checkbox = if done { "[\u{2713}] " } else { "[ ] " };
        crate::format::sgr(checkbox, if done { "2" } else { "35" }, color)
    } else {
        let mut prefix = node.edge.as_deref().map_or(String::new(), |e| {
            let tag = format!("[{e}] ");
            if done {
                crate::format::sgr(&tag, "2", color)
            } else {
                tag
            }
        });
        if done {
            // A check mark (U+2713) prefixes a done node; written as an escape so
            // the source stays ASCII while the output shows the glyph.
            prefix.push_str(&crate::format::sgr("\u{2713} ", "2", color));
        }
        prefix
    };
    s.push_str(&paint(&node.id, ID_KEY));
    for (col, v) in &node.cells {
        s.push_str("  ");
        s.push_str(&paint(
            &crate::format::truncate(&render_cell(v), TREE_TITLE_MAX),
            col,
        ));
    }
    if let Some((d, t)) = node.rollup {
        let rollup = format!(" [subtasks {d}/{t}]");
        s.push_str(&crate::format::sgr(
            &rollup,
            if done { "2" } else { "33" },
            color,
        ));
    }
    s
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

/// Render the forest to a tree with Unicode box-drawing connectors (written as
/// `\u` escapes so the source stays ASCII while the output shows the glyphs).
fn render_human_forest(forest: &[Node], color: bool, style: RowStyle) -> String {
    // Pre-order line index of each id's *expanded* occurrence (the one drawn with
    // children), so a collapsed reference can say whether that occurrence sits
    // above or below it in the output.
    let mut expanded_at: HashMap<&str, usize> = HashMap::new();
    let mut idx = 0usize;
    index_expanded(forest, &mut idx, &mut expanded_at);

    let mut out = String::new();
    let mut line = 0usize;
    for node in forest {
        out.push_str(&node_label(node, color, style));
        out.push('\n');
        line += 1;
        push_kids(node, "", &mut out, color, style, &mut line, &expanded_at);
    }
    out.trim_end_matches('\n').to_string()
}

/// Record, in pre-order, the line index of each id's *expanded* occurrence (a node
/// drawn with children). Collapsed/cycle/missing occurrences carry no subtree, so
/// they aren't recorded. Walks in exactly [`render_human_forest`]'s emit order.
fn index_expanded<'a>(nodes: &'a [Node], idx: &mut usize, at: &mut HashMap<&'a str, usize>) {
    for node in nodes {
        let here = *idx;
        *idx += 1;
        if let Kids::Children(kids) = &node.kids {
            if !kids.is_empty() {
                at.insert(node.id.as_str(), here);
            }
            index_expanded(kids, idx, at);
        }
    }
}

fn push_kids(
    node: &Node,
    prefix: &str,
    out: &mut String,
    color: bool,
    style: RowStyle,
    line: &mut usize,
    expanded_at: &HashMap<&str, usize>,
) {
    let Kids::Children(kids) = &node.kids else {
        return;
    };
    // A subtask parent leads with a `[x]` checkbox, so its check mark sits one
    // column right of where a plain node's body starts. Indent its children by
    // that column, so a connector descends from under the check mark, not the `[`.
    let anchor = if node.edge.as_deref() == Some("subtask") {
        " "
    } else {
        ""
    };
    let n = kids.len();
    for (i, kid) in kids.iter().enumerate() {
        let last = i + 1 == n;
        let here = *line;
        out.push_str(prefix);
        out.push_str(anchor);
        out.push_str(if last {
            "\u{2514}\u{2500} "
        } else {
            "\u{251C}\u{2500} "
        });
        out.push_str(&node_label(kid, color, style));
        match &kid.kids {
            Kids::Missing => out.push_str(" (missing)"),
            Kids::Cycle => out.push_str(" (cycle)"),
            // Shown here, but its subtree is drawn at the node's expanded
            // occurrence; point to it (above/below depends on order, esp.
            // `--reverse`, so it's resolved from the recorded line index).
            Kids::Collapsed => out.push_str(match expanded_at.get(kid.id.as_str()) {
                Some(&e) if e < here => " (expanded above)",
                Some(_) => " (expanded below)",
                None => " (expanded elsewhere)",
            }),
            Kids::Children(_) => {}
        }
        out.push('\n');
        *line += 1;
        let child_prefix = format!(
            "{prefix}{anchor}{}",
            if last { "   " } else { "\u{2502}  " }
        );
        push_kids(kid, &child_prefix, out, color, style, line, expanded_at);
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

/// `ta dep cycles` - report any cycles in the blocker graph. JSON is an array of
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
                lines.push(format!("  {}", cycle.join(" <-> ")));
            }
        }
        lines.join("\n")
    };
    crate::format::emit(output, &human, &value);
    Ok(())
}

/// `ta dep plan <goal> ...` - the not-done transitive prerequisites of the goal(s)
/// (the goals included), in dependency order: do exactly these, in this order.
/// Prerequisites are the blocker edges (the `depends_on` field plus any
/// `blocker`-typed relationship); already-done ones are dropped as satisfied.
/// `--critical` narrows the list to the longest single chain of incomplete
/// prerequisites - the sequence that sets the minimum remaining duration.
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
            .map(|s| serde_json::json!({ ID_KEY: &s.id, STATUS_KEY: &s.status }))
            .collect(),
    );
    let human = if steps.is_empty() {
        "Nothing to do - every prerequisite is already done.".to_string()
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
