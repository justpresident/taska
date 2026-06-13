//! `resolve` action: inspect (plan) and clear (apply) a surfaced merge conflict
//! and the log's orphaned events.
//!
//! This one takes a concrete [`FileStore`]: the merge marker is a file under
//! `base_dir` and pruning rewrites the log via `replace_mutations`, neither of
//! which the [`EventStore`] trait exposes. `plan` only reads; `apply` performs
//! the side effects a frontend has decided on (clear the marker, drop the
//! confirmed orphans).

use std::collections::HashSet;

use serde_json::Value;

use crate::engine::Engine;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType, TASK_ID_KEY};
use crate::storage::{EventStore, FileStore};

/// One tentatively-merged field conflict, as recorded in the merge marker.
pub struct ConflictItem {
    pub task_id: String,
    pub field: Option<String>,
    pub ours: Value,
    pub theirs: Value,
    pub kept: String,
}

/// An orphaned event - one that applies to no existing task during replay.
pub struct OrphanEvent {
    pub seq: u64,
    pub op: OpType,
    pub task_id: String,
}

/// What a `resolve` would act on. `conflicts` is `None` when there's no merge
/// marker, `Some(list)` when one is present (the list may be empty - a marker
/// that names no conflicts).
pub struct ResolvePlan {
    pub conflicts: Option<Vec<ConflictItem>>,
    pub orphans: Vec<OrphanEvent>,
}

/// Inspect the store - parse the merge marker (if any) and compute the orphaned
/// events - without writing anything.
pub fn plan(store: &FileStore) -> Result<ResolvePlan, DynError> {
    let conflicts = read_marker(store)?;

    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    let (_, orphan_seqs) = Engine::materialize_report(
        baseline,
        mutations.clone(),
        &store.config().workflow.done_status,
    );
    let drop: HashSet<u64> = orphan_seqs.iter().copied().collect();
    let orphans = mutations
        .iter()
        .filter(|e| drop.contains(&e.seq))
        .map(|e| OrphanEvent {
            seq: e.seq,
            op: e.op.clone(),
            task_id: e.task_id.clone(),
        })
        .collect();
    Ok(ResolvePlan { conflicts, orphans })
}

/// Clear the merge marker (when one was present) and, if `drop_orphans`, prune
/// the planned orphans from the log. Returns `(cleared_marker, dropped_count)`.
///
/// Clearing the marker only acknowledges the already-written tentative merge, so
/// it isn't gated on the orphan decision. Dropping an orphan is state-neutral (it
/// applies to nothing), so it's safe once the frontend has confirmed.
pub fn apply(
    store: &FileStore,
    plan: &ResolvePlan,
    drop_orphans: bool,
) -> Result<(bool, usize), DynError> {
    let cleared = if plan.conflicts.is_some() {
        std::fs::remove_file(store.base_dir.join("merge-conflict.json"))?;
        true
    } else {
        false
    };

    let dropped = if drop_orphans && !plan.orphans.is_empty() {
        let drop: HashSet<u64> = plan.orphans.iter().map(|o| o.seq).collect();
        let kept: Vec<MutationEvent> = store
            .load_mutations()?
            .into_iter()
            .filter(|e| !drop.contains(&e.seq))
            .collect();
        store.replace_mutations(&kept)?;
        plan.orphans.len()
    } else {
        0
    };
    Ok((cleared, dropped))
}

/// Parse the merge-conflict marker, if present. `Some(items)` (possibly empty)
/// when the file exists, `None` when it doesn't.
fn read_marker(store: &FileStore) -> Result<Option<Vec<ConflictItem>>, DynError> {
    let marker = store.base_dir.join("merge-conflict.json");
    if !marker.exists() {
        return Ok(None);
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&marker)?)?;
    let items = doc
        .get("conflicts")
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| ConflictItem {
                    task_id: item
                        .get(TASK_ID_KEY)
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    field: item
                        .get("field")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    ours: item.get("ours").cloned().unwrap_or(Value::Null),
                    theirs: item.get("theirs").cloned().unwrap_or(Value::Null),
                    kept: item
                        .get("kept")
                        .and_then(|v| v.as_str())
                        .unwrap_or("ours")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(items))
}
