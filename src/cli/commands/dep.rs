//! `ta block`/`ta unblock` (legacy, untyped) and the `ta dep` command group —
//! add or remove typed relationship edges.

use std::collections::BTreeMap;

use clap::Subcommand;
use serde_json::{Map, Value};

use crate::config::RelationshipDef;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType};
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
    let (task, edges, op, verb) = match action {
        DepAction::Add { task, edges } => (task, edges, OpType::AddDep, "Added"),
        DepAction::Remove { task, edges } => (task, edges, OpType::RemoveDep, "Removed"),
    };
    let mut events = Vec::with_capacity(edges.len());
    for edge in &edges {
        let (rel_type, target) = edge
            .split_once('=')
            .filter(|(t, v)| !t.is_empty() && !v.is_empty())
            .ok_or_else(|| format!("invalid edge `{edge}` (expected type=target)"))?;
        if !types.contains_key(rel_type) {
            let declared: Vec<&str> = types.keys().map(String::as_str).collect();
            return Err(format!(
                "unknown relationship type `{rel_type}`; declared types: {}",
                declared.join(", ")
            )
            .into());
        }
        let mut payload = Map::new();
        payload.insert("dep".to_string(), Value::String(target.to_string()));
        if rel_type != "depends_on" {
            payload.insert("type".to_string(), Value::String(rel_type.to_string()));
        }
        events.push(MutationEvent::new(op.clone(), task.clone(), payload));
    }
    store.append_events(&events)?;
    println!("{verb} {} edge(s) on `{task}`", events.len());
    Ok(())
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
