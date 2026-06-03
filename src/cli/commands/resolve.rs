//! `ta resolve` — clear a surfaced merge conflict and prune orphaned events.

use std::collections::HashSet;

use serde_json::Value;

use crate::cli::{confirm, replay_report};
use crate::error::DynError;
use crate::model::MutationEvent;
use crate::storage::{EventStore, FileStore};

/// Clean up after a merge or a divergent history: report and clear a surfaced
/// merge conflict, and drop any orphaned events the log has accumulated.
///
/// The deterministic merge is already written to the log by the driver, so the
/// conflict step only acknowledges the conflicts and removes the marker; per-field
/// resolution is future work. The orphan step prunes events that apply to nothing
/// (a dropped `Create` left their target missing). Dropping a no-op event is
/// state-neutral, so it needs no confirmation. With neither a marker nor an
/// orphan, there is nothing to do.
pub fn cmd_resolve(store: &FileStore, force: bool) -> Result<(), DynError> {
    let cleared_marker = resolve_merge_marker(store)?;
    let dropped_orphans = resolve_orphans(store, force)?;
    if !cleared_marker && dropped_orphans == 0 {
        println!("Nothing to resolve (no merge conflicts and no orphaned events).");
    }
    Ok(())
}

/// Report and clear a surfaced merge-conflict marker, if present. Returns whether
/// a marker was found and cleared.
fn resolve_merge_marker(store: &FileStore) -> Result<bool, DynError> {
    let marker = store.base_dir.join("merge-conflict.json");
    if !marker.exists() {
        return Ok(false);
    }

    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&marker)?)?;
    let conflicts = doc.get("conflicts").and_then(|c| c.as_array());
    match conflicts {
        Some(items) if !items.is_empty() => {
            println!(
                "{} field conflict(s) were merged tentatively (keeping ours):",
                items.len()
            );
            for item in items {
                let task = item.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
                let field = item.get("field").and_then(|v| v.as_str());
                let ours = item.get("ours").cloned().unwrap_or(Value::Null);
                let theirs = item.get("theirs").cloned().unwrap_or(Value::Null);
                let kept = item.get("kept").and_then(|v| v.as_str()).unwrap_or("ours");
                match field {
                    Some(f) => {
                        println!("  - `{task}`.{f}: ours={ours} / theirs={theirs} -> kept {kept}");
                    }
                    None => println!(
                        "  - `{task}` (whole task): ours={ours} / theirs={theirs} -> kept {kept}"
                    ),
                }
            }
            println!(
                "\nThe tentative merge is already written to the log. To accept it, `git add` \
                 the files and commit; to pick differently, edit the log or re-merge with a \
                 different `on_conflict` strategy."
            );
        }
        _ => println!("Merge marker present but lists no conflicts."),
    }

    std::fs::remove_file(&marker)?;
    Ok(true)
}

/// Prune orphaned events — those that apply to nothing during replay — from the
/// log, rewriting it without them. Returns how many were dropped. Because an
/// orphan is by definition a no-op, removing it can't change materialized state.
fn resolve_orphans(store: &FileStore, force: bool) -> Result<usize, DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    let (_, orphans) = replay_report(store, baseline, mutations.clone());
    if orphans.is_empty() {
        return Ok(0);
    }

    let drop: HashSet<u64> = orphans.iter().copied().collect();
    // Verbose: name every event that would be dropped before touching the log.
    println!(
        "{} orphaned event(s) apply to no existing task and would be dropped:",
        orphans.len()
    );
    for event in mutations.iter().filter(|e| drop.contains(&e.seq)) {
        println!("  - seq {}: {:?} `{}`", event.seq, event.op, event.task_id);
    }
    if !confirm("Drop these orphaned events from the log?", force)? {
        println!("Aborted; the log is unchanged.");
        return Ok(0);
    }

    let kept: Vec<MutationEvent> = mutations
        .into_iter()
        .filter(|e| !drop.contains(&e.seq))
        .collect();
    store.replace_mutations(&kept)?;
    println!("Dropped {} orphaned event(s) from the log.", orphans.len());
    Ok(orphans.len())
}
