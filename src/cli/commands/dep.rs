//! `ta block`/`ta unblock` (legacy, untyped) and the `ta dep` command group —
//! add or remove typed relationship edges.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use clap::Subcommand;
use serde_json::{Map, Value};

use crate::cli::state_of;
use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType, TaskState};
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
}

/// Add or remove typed dependency edges. Each `type=target` edge's type is
/// validated against the declared relationship types; an `AddDep`/`RemoveDep`
/// event is appended per edge (the `depends_on` type omits an explicit `type` to
/// stay legacy-shaped, so `ta dep add x depends_on=y` matches `ta block x y`).
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

pub fn cmd_dep(
    store: &impl EventStore,
    task_id: &str,
    depends_on: &str,
    op: OpType,
) -> Result<(), DynError> {
    let mut payload = Map::new();
    payload.insert("dep".to_string(), Value::String(depends_on.to_string()));
    let is_add = matches!(op, OpType::AddDep);
    store.append_events(&[MutationEvent::new(op, task_id, payload)])?;
    if is_add {
        println!("`{task_id}` now depends on `{depends_on}`");
    } else {
        println!("`{task_id}` no longer depends on `{depends_on}`");
    }
    Ok(())
}
