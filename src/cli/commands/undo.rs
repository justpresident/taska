//! `ta undo` — reverse the last N events (truncate local, compensate committed).

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::cli::{confirm, replay};
use crate::error::DynError;
use crate::model::{MutationEvent, OpType, TaskState, REL_KEY, TARGET_KEY};
use crate::storage::{EventStore, FileStore};

/// Undo the last `count` event(s) in the log.
///
/// Two paths, chosen by whether any undone event is already git-committed:
/// - All undone events are still local (uncommitted), or `--remove` is set:
///   truncate the log's tail. This is safe for local events because they were
///   never shared; `--remove` extends it to committed events at the cost of
///   rewriting shared history (it prints a loud warning).
/// - Some undone events are already committed (the default for that case): keep
///   committed history intact and instead *append* compensating events that
///   transform the current state back to the target (pre-undo prefix) state.
///   Staying append-only avoids cross-branch seq collisions on merge.
///
/// Only events still in the log can be undone; anything folded into the baseline
/// by compaction is out of reach.
pub fn cmd_undo(
    store: &FileStore,
    count: usize,
    force: bool,
    remove: bool,
) -> Result<(), DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    let n = mutations.len();
    if count == 0 || n == 0 {
        println!("Nothing to undo.");
        return Ok(());
    }

    let count = count.min(n);
    let keep = n - count;
    let undone = &mutations[keep..];

    let current = replay(store, baseline.clone(), mutations.clone());
    let target = replay(store, baseline.clone(), mutations[..keep].to_vec());

    // The tasks any undone event touched, sorted for stable output.
    let mut affected: Vec<String> = undone.iter().map(|e| e.task_id.clone()).collect();
    affected.sort();
    affected.dedup();

    // PREVIEW: name each undone event, then show each affected task's before/after.
    println!("Undoing {count} event(s):");
    for event in undone {
        println!("  seq {}: {:?} `{}`", event.seq, event.op, event.task_id);
    }
    for id in &affected {
        println!(
            "  - {id}: {} -> {}",
            describe(current.get(id)),
            describe(target.get(id))
        );
    }

    // How many of the log's events are already committed to git. If the file was
    // never committed (or there is no HEAD yet), nothing is committed.
    let committed_count = committed_mutation_count(store);
    let any_committed = keep < committed_count;

    if !confirm("Apply this undo?", force)? {
        println!("Aborted; nothing changed.");
        return Ok(());
    }

    if remove || !any_committed {
        // Truncate the tail. Safe for local events; for committed ones `--remove`
        // rewrites shared history, so warn loudly.
        store.replace_mutations(&mutations[..keep])?;
        if remove && any_committed {
            eprintln!(
                "DANGER: --remove deleted committed event(s), rewriting shared history. \
                 Other branches will see a removal on merge; only do this if you are sure \
                 the removed events were never pushed or pulled elsewhere."
            );
        }
    } else {
        // Default committed path: keep committed history, append compensating
        // events. Build them from the committed prefix's state toward the target.
        let truncate_to = committed_count;
        let post = replay(store, baseline, mutations[..truncate_to].to_vec());
        let comps = compensate(&post, &target, &affected);

        // Continue the seq sequence past the highest committed seq we keep.
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
        store.replace_mutations(&new_log)?;
    }

    println!("Undone.");
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

/// Render a task's salient state for the before/after preview: `(absent)` for a
/// missing task, else the JSON of its custom fields plus `deps=<typed map>` when
/// it has any edge — the same `{type: [targets…]}` shape the `deps` column
/// shows. Mirrors the field-centric framing of the other task views.
fn describe(task: Option<&TaskState>) -> String {
    task.map_or_else(
        || "(absent)".to_string(),
        |t| {
            let fields =
                serde_json::to_string(&t.custom_fields).unwrap_or_else(|_| "{}".to_string());
            if t.relationships.is_empty() {
                fields
            } else {
                let deps =
                    serde_json::to_string(&t.relationships).unwrap_or_else(|_| "{}".to_string());
                format!("{fields} deps={deps}")
            }
        },
    )
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
/// shape `cmd_dep_group` writes and the engine's `AddEdge`/`RemoveEdge` replay
/// expects. The type is always explicit — an untyped event is the legacy shape
/// `ta repair --migrate` exists to rewrite.
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

    #[test]
    fn describe_renders_absent_fields_and_typed_deps() {
        assert_eq!(describe(None), "(absent)");
        let mut t = task("a", &["d1"], &[("status", serde_json::json!("open"))]);
        t.relationships
            .insert("relates_to".to_string(), vec!["d2".to_string()]);
        let out = describe(Some(&t));
        assert!(out.contains(r#""status":"open""#), "fields: {out}");
        assert!(
            out.contains(r#"deps={"depends_on":["d1"],"relates_to":["d2"]}"#),
            "typed deps map shown: {out}"
        );

        let no_deps = task("b", &[], &[]);
        assert_eq!(describe(Some(&no_deps)), "{}", "empty fields, no deps");
    }
}
