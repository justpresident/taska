//! `undo` action: reverse the last N events.
//!
//! `plan` computes what would change and the exact log that would result —
//! truncating the tail (local events, or `--remove`) or, when committed history
//! is involved, keeping it and appending compensating events — without writing.
//! `apply` writes that log. Takes a concrete [`FileStore`]: it inspects git
//! (`committed_mutation_count`) and rewrites the log via `replace_mutations`.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::action::materialize;
use crate::error::DynError;
use crate::model::{MutationEvent, OpType, TaskState, REL_KEY, TARGET_KEY};
use crate::storage::{EventStore, FileStore};

/// One event that would be undone.
pub struct UndoneEvent {
    pub seq: u64,
    pub op: OpType,
    pub task_id: String,
}

/// An affected task's before/after state across the undo.
pub struct UndoChange {
    pub id: String,
    pub before: Option<TaskState>,
    pub after: Option<TaskState>,
}

/// A computed undo: what changes, and (privately) the log that applying writes.
pub struct UndoPlan {
    pub count: usize,
    pub undone: Vec<UndoneEvent>,
    pub changes: Vec<UndoChange>,
    /// Whether applying rewrites committed git history (`--remove` past a commit).
    pub rewrites_committed_history: bool,
    /// The log to write on [`apply`] — the truncated tail, or the committed
    /// prefix plus compensating events.
    new_log: Vec<MutationEvent>,
}

/// Plan the undo of the last `count` event(s). `None` when there's nothing to
/// undo (empty log or `count == 0`). No writes.
///
/// Two strategies, chosen by whether any undone event is already git-committed:
/// truncate the tail (safe for local events; `--remove` extends it to committed
/// ones, rewriting shared history), or keep committed history and append
/// compensating events that walk the state back to the target.
pub fn plan(store: &FileStore, count: usize, remove: bool) -> Result<Option<UndoPlan>, DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    let n = mutations.len();
    if count == 0 || n == 0 {
        return Ok(None);
    }
    let count = count.min(n);
    let keep = n - count;
    let undone_slice = &mutations[keep..];

    let current = materialize(store.config(), &baseline, &mutations);
    let target = materialize(store.config(), &baseline, &mutations[..keep]);

    // The tasks any undone event touched, sorted for stable output.
    let mut affected: Vec<String> = undone_slice.iter().map(|e| e.task_id.clone()).collect();
    affected.sort();
    affected.dedup();

    let undone = undone_slice
        .iter()
        .map(|e| UndoneEvent {
            seq: e.seq,
            op: e.op.clone(),
            task_id: e.task_id.clone(),
        })
        .collect();
    let changes = affected
        .iter()
        .map(|id| UndoChange {
            id: id.clone(),
            before: current.get(id).cloned(),
            after: target.get(id).cloned(),
        })
        .collect();

    // How many of the log's events are already committed to git.
    let committed_count = committed_mutation_count(store);
    let any_committed = keep < committed_count;

    let (new_log, rewrites_committed_history) = if remove || !any_committed {
        // Truncate the tail. `--remove` past a commit rewrites shared history.
        (mutations[..keep].to_vec(), remove && any_committed)
    } else {
        // Keep committed history; append compensating events from the committed
        // prefix's state toward the target.
        let truncate_to = committed_count;
        let post = materialize(store.config(), &baseline, &mutations[..truncate_to]);
        let comps = compensate(&post, &target, &affected);

        let next = mutations[..truncate_to]
            .iter()
            .map(|e| e.seq)
            .max()
            .map_or(1, |m| m + 1);
        let mut new_log = mutations[..truncate_to].to_vec();
        for (seq, mut comp) in (next..).zip(comps) {
            comp.seq = seq;
            new_log.push(comp);
        }
        (new_log, false)
    };

    Ok(Some(UndoPlan {
        count,
        undone,
        changes,
        rewrites_committed_history,
        new_log,
    }))
}

/// Apply a planned undo: rewrite the log to the planned result.
pub fn apply(store: &FileStore, plan: &UndoPlan) -> Result<(), DynError> {
    store.replace_mutations(&plan.new_log)?;
    Ok(())
}

/// Count non-empty lines in the git-committed `mutations.jsonl` (`HEAD:` blob).
/// Returns 0 when the file is not committed yet or there is no `HEAD`, which the
/// caller treats as "nothing committed", so every event is safe to truncate.
/// The `./` prefix makes the blob path relative to the store's parent (`-C`)
/// rather than the repo root, so a store NESTED below the root counts its
/// committed events instead of reading as all-uncommitted (which would make
/// undo truncate shared history).
fn committed_mutation_count(store: &FileStore) -> usize {
    let Some(repo_root) = store.repo_root() else {
        return 0;
    };
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", "HEAD:./.taska/mutations.jsonl"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        _ => 0,
    }
}

/// Produce DRAFT events (seq 0; the caller assigns real seqs) that transform the
/// `from` state into the `to` state for the `affected` tasks, using the existing
/// op vocabulary plus the null-unset convention:
///
/// - in `from` but not `to` -> `Delete`.
/// - in `to` but not `from` -> `Create` carrying its fields, then one typed
///   `AddEdge` per relationship edge.
/// - in both -> a single `Update` that sets each field whose value differs and
///   unsets (sets to null) each field present in `from` but gone in `to`; skipped
///   when that payload is empty. Then typed `AddEdge`/`RemoveEdge` to reconcile
///   the relationships map, per `(type, target)` edge.
/// - in neither -> nothing.
fn compensate(
    from: &HashMap<String, TaskState>,
    to: &HashMap<String, TaskState>,
    affected: &[String],
) -> Vec<MutationEvent> {
    let mut events = Vec::new();
    for id in affected {
        match (from.get(id), to.get(id)) {
            (Some(_), None) => {
                events.push(MutationEvent::new(OpType::Delete, id.clone(), Map::new()));
            }
            (None, Some(t)) => {
                events.push(MutationEvent::new(
                    OpType::Create,
                    id.clone(),
                    t.custom_fields.clone(),
                ));
                for (rel, targets) in &t.relationships {
                    for dep in targets {
                        events.push(dep_event(OpType::AddEdge, id, rel, dep));
                    }
                }
            }
            (Some(f), Some(t)) => {
                let mut payload = Map::new();
                // Set every field that differs (present-and-changed or newly added).
                for (k, v) in &t.custom_fields {
                    if f.custom_fields.get(k) != Some(v) {
                        payload.insert(k.clone(), v.clone());
                    }
                }
                // Unset (null) every field that existed in `from` but not in `to`.
                for k in f.custom_fields.keys() {
                    if !t.custom_fields.contains_key(k) {
                        payload.insert(k.clone(), Value::Null);
                    }
                }
                if !payload.is_empty() {
                    events.push(MutationEvent::new(OpType::Update, id.clone(), payload));
                }
                // Reconcile every relationship type: an edge is identified by its
                // `(type, target)` pair, so the same target under another type is
                // a different edge.
                let has_edge = |s: &TaskState, rel: &str, dep: &String| {
                    s.relationships.get(rel).is_some_and(|d| d.contains(dep))
                };
                for (rel, targets) in &t.relationships {
                    for dep in targets {
                        if !has_edge(f, rel, dep) {
                            events.push(dep_event(OpType::AddEdge, id, rel, dep));
                        }
                    }
                }
                for (rel, targets) in &f.relationships {
                    for dep in targets {
                        if !has_edge(t, rel, dep) {
                            events.push(dep_event(OpType::RemoveEdge, id, rel, dep));
                        }
                    }
                }
            }
            (None, None) => {}
        }
    }
    events
}

/// A dependency draft event with the `{ "target": <id>, "rel": <rel> }` payload
/// shape `dep add` writes and the engine's `AddEdge`/`RemoveEdge` replay expects.
/// Every edge carries an explicit `rel`; replay skips one that doesn't.
fn dep_event(op: OpType, task_id: &str, rel_type: &str, dep: &str) -> MutationEvent {
    let mut payload = Map::new();
    payload.insert(TARGET_KEY.to_string(), Value::String(dep.to_string()));
    payload.insert(REL_KEY.to_string(), Value::String(rel_type.to_string()));
    MutationEvent::new(op, task_id, payload)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::{state, task};

    #[test]
    fn compensate_unsets_a_removed_field_with_null() {
        // `from` has the field, `to` does not: the compensating Update must set
        // the field to JSON null so the engine's unset convention drops it.
        let from = state(&[task("a", &[], &[("owner", serde_json::json!("bob"))])]);
        let to = state(&[task("a", &[], &[])]);
        let events = compensate(&from, &to, &["a".to_string()]);
        assert_eq!(events.len(), 1, "one Update: {events:?}");
        assert_eq!(events[0].op, OpType::Update);
        assert_eq!(
            events[0].payload.get("owner"),
            Some(&Value::Null),
            "removed field unset via null: {:?}",
            events[0].payload
        );
    }

    #[test]
    fn compensate_handles_create_delete_and_field_change() {
        // a: present in `from`, absent in `to` -> Delete.
        // b: absent in `from`, present in `to` with a dep -> Create + AddEdge.
        // c: changed field value -> Update with just the changed key.
        let from = state(&[
            task("a", &[], &[("x", serde_json::json!(1))]),
            task("c", &[], &[("status", serde_json::json!("open"))]),
        ]);
        let to = state(&[
            task("b", &["dep1"], &[("y", serde_json::json!(2))]),
            task("c", &[], &[("status", serde_json::json!("closed"))]),
        ]);
        let affected = ["a".to_string(), "b".to_string(), "c".to_string()];
        let events = compensate(&from, &to, &affected);

        // a -> Delete
        let a: Vec<_> = events.iter().filter(|e| e.task_id == "a").collect();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].op, OpType::Delete);

        // b -> Create (carrying y) then a typed AddEdge dep1
        let b: Vec<_> = events.iter().filter(|e| e.task_id == "b").collect();
        assert_eq!(b.len(), 2, "create + adddep: {b:?}");
        assert_eq!(b[0].op, OpType::Create);
        assert_eq!(b[0].payload.get("y"), Some(&serde_json::json!(2)));
        assert_eq!(b[1].op, OpType::AddEdge);
        assert_eq!(b[1].payload.get("target"), Some(&serde_json::json!("dep1")));
        assert_eq!(
            b[1].payload.get("rel"),
            Some(&serde_json::json!("depends_on")),
            "compensating dep events carry their type: {b:?}"
        );

        // c -> Update setting only the changed status
        let c: Vec<_> = events.iter().filter(|e| e.task_id == "c").collect();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].op, OpType::Update);
        assert_eq!(
            c[0].payload.get("status"),
            Some(&serde_json::json!("closed"))
        );
    }

    #[test]
    fn compensate_reconciles_dependencies() {
        // from depends on x; to depends on y -> RemoveEdge x, AddEdge y, no Update.
        let from = state(&[task("a", &["x"], &[])]);
        let to = state(&[task("a", &["y"], &[])]);
        let events = compensate(&from, &to, &["a".to_string()]);
        assert!(
            !events.iter().any(|e| e.op == OpType::Update),
            "no field changes -> no Update: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.op == OpType::AddEdge
                && e.payload.get("target") == Some(&serde_json::json!("y"))),
            "adds y: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.op == OpType::RemoveEdge
                && e.payload.get("target") == Some(&serde_json::json!("x"))),
            "removes x: {events:?}"
        );
    }

    #[test]
    fn compensate_reconciles_typed_edges_per_type_and_target() {
        // The SAME target moves from relates_to to has_subtask, and a relates_to
        // edge to y disappears: each `(type, target)` pair is its own edge, so
        // the compensation is AddEdge has_subtask=x, RemoveEdge relates_to=x,
        // RemoveEdge relates_to=y — all typed.
        let mut f = task("a", &[], &[]);
        f.relationships.insert(
            "relates_to".to_string(),
            vec!["x".to_string(), "y".to_string()],
        );
        let mut t = task("a", &[], &[]);
        t.relationships
            .insert("has_subtask".to_string(), vec!["x".to_string()]);
        let events = compensate(&state(&[f]), &state(&[t]), &["a".to_string()]);

        let pair = |e: &MutationEvent| {
            (
                e.op.clone(),
                e.payload.get("rel").cloned(),
                e.payload.get("target").cloned(),
            )
        };
        let got: Vec<_> = events.iter().map(pair).collect();
        assert!(
            got.contains(&(
                OpType::AddEdge,
                Some(serde_json::json!("has_subtask")),
                Some(serde_json::json!("x"))
            )),
            "adds the new typed edge: {got:?}"
        );
        assert!(
            got.contains(&(
                OpType::RemoveEdge,
                Some(serde_json::json!("relates_to")),
                Some(serde_json::json!("x"))
            )),
            "removes the old type's edge to the same target: {got:?}"
        );
        assert!(
            got.contains(&(
                OpType::RemoveEdge,
                Some(serde_json::json!("relates_to")),
                Some(serde_json::json!("y"))
            )),
            "removes the dropped edge: {got:?}"
        );
        assert_eq!(events.len(), 3, "no Update, nothing extra: {got:?}");
    }
}
