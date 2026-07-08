//! `undo` action: walk back through genuine history, skipping prior undos.
//!
//! `plan` selects the original event(s) to reverse - newest-first, or from a
//! given `--seq` - skipping events that are themselves compensations and
//! originals already undone, so repeated `undo` peels back real history instead
//! of bouncing on its own output. It then computes the resulting log without
//! writing: the clean uncommitted trailing run of targets is truncated, while
//! committed or buried targets are reversed by appended compensations tagged
//! `_undoes=<seq>` (the mark that records an original as undone). `apply` writes
//! that log. Takes a concrete [`FileStore`]: it inspects git
//! (`committed_mutation_count`) and rewrites the log via `replace_mutations`.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::action::materialize;
use crate::config::Config;
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
    /// The log to write on [`apply`] - the truncated tail, or the committed
    /// prefix plus compensating events.
    new_log: Vec<MutationEvent>,
}

impl UndoPlan {
    /// The sequence number of the last mutation in the resulting log (0 if empty).
    pub fn last_seq(&self) -> u64 {
        self.new_log.last().map_or(0, |e| e.seq)
    }
}

/// Plan an undo, walking back through real history.
///
/// With `seq = None` it targets the most recent *undoable* event and walks older
/// for `count` of them; with `seq = Some(s)` it starts at `s` (inclusive) and
/// walks older. Selection skips events that are themselves undo compensations and
/// originals already undone, so repeated `undo` peels back genuine history
/// instead of bouncing on its own output. `None` when there's nothing to undo
/// (empty log, `count == 0`, or no undoable event left). No writes.
///
/// Applying is hybrid: the maximal trailing run of selected events that is still
/// uncommitted (or any committed run when `remove`) is truncated outright; every
/// other selected event - committed, or buried under newer kept events - is
/// reversed by an *appended* compensation tagged `_undoes=<its seq>` (see
/// [`MutationEvent::set_undoes`]), which is what records it as undone.
pub fn plan(
    store: &FileStore,
    seq: Option<u64>,
    count: usize,
    remove: bool,
) -> Result<Option<UndoPlan>, DynError> {
    let baseline = store.load_baseline()?;
    let mutations = store.load_mutations()?;
    let n = mutations.len();
    if count == 0 || n == 0 {
        return Ok(None);
    }

    // The original events this undo targets, newest-first.
    let selected = select_targets(&mutations, seq, count)?;
    if selected.is_empty() {
        return Ok(None);
    }
    let selected_set: HashSet<u64> = selected.iter().map(|&i| mutations[i].seq).collect();
    let config = store.config();

    // Each selected event touches exactly its own task, and no event ever touches
    // a task it doesn't name - so undo only needs those tasks' own state. Scope
    // the baseline + log to them ONCE up front and fold only that subset in every
    // materialization below: a busy store with hundreds of tasks then replays a
    // handful of events per pass instead of the whole log. The truncation/seq
    // bookkeeping below still works on the full `mutations` (it rewrites the real
    // log, all tasks); only state computation is scoped.
    let mut affected: Vec<String> = selected
        .iter()
        .map(|&i| mutations[i].task_id.clone())
        .collect();
    affected.sort();
    affected.dedup();
    let (scoped_baseline, scoped_muts) = scope_to_tasks(&baseline, &mutations, &affected);

    // The seqs some existing compensation already undoes. Reversing an original
    // means replaying AS IF that original (and every already-undone one) never
    // happened - so target states are computed over the surviving ORIGINAL events
    // (compensations dropped), never the raw log (whose compensations would
    // otherwise pin the state and mask the removal).
    let already_undone: HashSet<u64> = scoped_muts
        .iter()
        .filter_map(MutationEvent::undoes)
        .collect();
    let originals_without = |removed: &HashSet<u64>| -> Vec<MutationEvent> {
        scoped_muts
            .iter()
            .filter(|e| e.undoes().is_none() && !removed.contains(&e.seq))
            .cloned()
            .collect()
    };

    // Preview state: current vs the originals with every selected one also removed.
    let current = materialize(config, &scoped_baseline, &scoped_muts);
    let mut removed_final = already_undone.clone();
    removed_final.extend(selected_set.iter().copied());
    let target = materialize(config, &scoped_baseline, &originals_without(&removed_final));

    let undone = selected
        .iter()
        .map(|&i| UndoneEvent {
            seq: mutations[i].seq,
            op: mutations[i].op.clone(),
            task_id: mutations[i].task_id.clone(),
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

    // Truncate the maximal trailing run of selected events within the writable
    // region; compensate the rest.
    let committed_count = committed_mutation_count(store);
    let mut p = n;
    while p > 0 && selected_set.contains(&mutations[p - 1].seq) {
        p -= 1;
    }
    let trunc_floor = if remove { 0 } else { committed_count };
    let trunc_p = p.max(trunc_floor);
    let rewrites_committed_history = remove && p < committed_count;

    let mut new_log = mutations[..trunc_p].to_vec();
    // Selected events still in the kept prefix (committed, or buried under newer
    // kept events) can't be truncated - reverse them with appended, marked
    // compensations.
    let buried: Vec<usize> = selected.iter().copied().filter(|&i| i < trunc_p).collect();
    if !buried.is_empty() {
        // Seed the suppressed set with the truncated tail originals (already gone
        // from `new_log`) plus the already-undone - the state the compensations
        // walk forward from.
        let mut seed = already_undone;
        seed.extend(mutations[trunc_p..].iter().map(|e| e.seq));
        let comps = buried_compensations(
            config,
            &scoped_baseline,
            &scoped_muts,
            &mutations,
            &buried,
            seed,
        );
        let next = new_log.iter().map(|e| e.seq).max().map_or(1, |m| m + 1);
        for (assigned, mut comp) in (next..).zip(comps) {
            comp.seq = assigned;
            new_log.push(comp);
        }
    }

    Ok(Some(UndoPlan {
        count: selected.len(),
        undone,
        changes,
        rewrites_committed_history,
        new_log,
    }))
}

/// The compensating events reversing the `buried` originals (indices into
/// `mutations`), each tagged with the seq it undoes.
///
/// Walking newest-first, each step removes one more original from the surviving
/// set and diffs the affected task's state, so a single original maps to one
/// tagged compensation; a state-neutral (shadowed) reversal still yields an inert
/// marker so repeated `undo` makes progress. `seed` is the set already removed
/// before the first step (the truncated tail plus anything already undone). Runs
/// on the task-scoped baseline/log, so each replay folds only the affected tasks.
fn buried_compensations(
    config: &Config,
    scoped_baseline: &[TaskState],
    scoped_muts: &[MutationEvent],
    mutations: &[MutationEvent],
    buried: &[usize],
    seed: HashSet<u64>,
) -> Vec<MutationEvent> {
    let originals_without = |removed: &HashSet<u64>| -> Vec<MutationEvent> {
        scoped_muts
            .iter()
            .filter(|e| e.undoes().is_none() && !removed.contains(&e.seq))
            .cloned()
            .collect()
    };
    let mut removed_running = seed;
    let mut prev = materialize(
        config,
        scoped_baseline,
        &originals_without(&removed_running),
    );
    let mut comps_all = Vec::new();
    for &i in buried {
        let s = mutations[i].seq;
        removed_running.insert(s);
        let step = materialize(
            config,
            scoped_baseline,
            &originals_without(&removed_running),
        );
        let id = mutations[i].task_id.clone();
        let mut comps = compensate(&prev, &step, std::slice::from_ref(&id));
        if comps.is_empty() {
            // The event was shadowed by a later kept write, so reversing it changes
            // no state - still record the undo with an inert marker event so a
            // repeated `undo` moves past it.
            comps.push(MutationEvent::new(OpType::Update, id, Map::new()));
        }
        for comp in &mut comps {
            comp.set_undoes(s);
        }
        comps_all.extend(comps);
        prev = step;
    }
    comps_all
}

/// The baseline records and log events that belong to `affected` - undo's state
/// computation only ever needs these tasks, so every materialization folds this
/// subset instead of the whole store.
fn scope_to_tasks(
    baseline: &[TaskState],
    mutations: &[MutationEvent],
    affected: &[String],
) -> (Vec<TaskState>, Vec<MutationEvent>) {
    let ids: HashSet<&str> = affected.iter().map(String::as_str).collect();
    (
        baseline
            .iter()
            .filter(|t| ids.contains(t.id.as_str()))
            .cloned()
            .collect(),
        mutations
            .iter()
            .filter(|e| ids.contains(e.task_id.as_str()))
            .cloned()
            .collect(),
    )
}

/// Choose the original events to undo, newest-first (returned as indices into
/// `mutations`).
///
/// A *candidate* is an event that is neither a compensation (carries no
/// `_undoes`) nor already undone (no other event points `_undoes` at it). With
/// `seq = None` the walk starts at the newest event; with `seq = Some(s)` it
/// starts at `s`, which must exist and be undoable - an unknown, already-undone,
/// or undo-marker seq is a hard error, since the user named it explicitly. From
/// the start it walks older, collecting up to `count` candidates and silently
/// skipping non-candidates along the way.
fn select_targets(
    mutations: &[MutationEvent],
    seq: Option<u64>,
    count: usize,
) -> Result<Vec<usize>, DynError> {
    if mutations.is_empty() {
        return Ok(Vec::new());
    }
    let undone: HashSet<u64> = mutations.iter().filter_map(MutationEvent::undoes).collect();
    let is_candidate = |e: &MutationEvent| e.undoes().is_none() && !undone.contains(&e.seq);

    let start = match seq {
        Some(s) => {
            let idx = mutations
                .iter()
                .position(|e| e.seq == s)
                .ok_or_else(|| format!("no event with seq {s} in the log"))?;
            if mutations[idx].undoes().is_some() {
                return Err(format!("seq {s} is itself an undo event; it can't be undone").into());
            }
            if undone.contains(&s) {
                return Err(format!("seq {s} is already undone").into());
            }
            idx
        }
        None => mutations.len().saturating_sub(1),
    };

    let mut selected = Vec::new();
    for i in (0..=start).rev() {
        if selected.len() >= count {
            break;
        }
        if is_candidate(&mutations[i]) {
            selected.push(i);
        }
    }
    Ok(selected)
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
    use crate::test_support::names::*;
    use crate::test_support::{state, task, task_rel};

    fn at(op: OpType, id: &str, seq: u64) -> MutationEvent {
        let mut e = MutationEvent::new(op, id, Map::new());
        e.seq = seq;
        e
    }

    #[test]
    fn scope_to_tasks_keeps_only_the_named_tasks_baseline_and_events() {
        let baseline = vec![
            task("a", &[], &[]),
            task("b", &[], &[]),
            task("c", &[], &[]),
        ];
        let muts = vec![
            at(OpType::Create, "a", 1),
            at(OpType::Update, "b", 2),
            at(OpType::Update, "a", 3),
            at(OpType::Create, "c", 4),
        ];
        let (scoped_baseline, scoped_muts) = scope_to_tasks(&baseline, &muts, &["a".to_string()]);
        assert_eq!(
            scoped_baseline
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a"],
            "only the named task's baseline survives"
        );
        assert_eq!(
            scoped_muts.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 3],
            "only the named task's events survive, in order"
        );
    }

    #[test]
    fn select_targets_walks_back_skipping_undos_and_already_undone() {
        // Originals at seq 1..=3, plus a compensation (seq 4) undoing seq 3.
        let mut comp = at(OpType::Update, "a", 4);
        comp.set_undoes(3);
        let log = vec![
            at(OpType::Create, "a", 1),
            at(OpType::Update, "a", 2),
            at(OpType::Update, "a", 3),
            comp,
        ];
        let seqs = |sel: Vec<usize>| sel.iter().map(|&i| log[i].seq).collect::<Vec<_>>();

        // Newest candidate is seq 2: seq 4 is a compensation, seq 3 is already undone.
        assert_eq!(seqs(select_targets(&log, None, 1).unwrap()), vec![2]);
        // Walking further back yields seq 2 then seq 1, never the comp or the undone.
        assert_eq!(seqs(select_targets(&log, None, 9).unwrap()), vec![2, 1]);
        // An explicit, valid original is selected on its own.
        assert_eq!(seqs(select_targets(&log, Some(1), 1).unwrap()), vec![1]);

        // Naming an already-undone, a compensation, or a missing seq is an error.
        assert!(
            select_targets(&log, Some(3), 1).is_err(),
            "seq 3 already undone"
        );
        assert!(
            select_targets(&log, Some(4), 1).is_err(),
            "seq 4 is an undo event"
        );
        assert!(select_targets(&log, Some(99), 1).is_err(), "no such seq");
    }

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
            task("c", &[], &[(STATUS_FIELD, serde_json::json!("open"))]),
        ]);
        let to = state(&[
            task_rel("b", BLOCKER, &["dep1"], &[("y", serde_json::json!(2))]),
            task("c", &[], &[(STATUS_FIELD, serde_json::json!("closed"))]),
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
        assert_eq!(
            b[1].payload.get(TARGET_KEY),
            Some(&serde_json::json!("dep1"))
        );
        assert_eq!(
            b[1].payload.get(REL_KEY),
            Some(&serde_json::json!(BLOCKER)),
            "compensating dep events carry their type: {b:?}"
        );

        // c -> Update setting only the changed STATUS_FIELD
        let c: Vec<_> = events.iter().filter(|e| e.task_id == "c").collect();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].op, OpType::Update);
        assert_eq!(
            c[0].payload.get(STATUS_FIELD),
            Some(&serde_json::json!("closed"))
        );
    }

    #[test]
    fn compensate_reconciles_dependencies() {
        // from has BLOCKER edge to x; to has BLOCKER edge to y ->
        // RemoveEdge x, AddEdge y, no Update.
        let from = state(&[task_rel("a", BLOCKER, &["x"], &[])]);
        let to = state(&[task_rel("a", BLOCKER, &["y"], &[])]);
        let events = compensate(&from, &to, &["a".to_string()]);
        assert!(
            !events.iter().any(|e| e.op == OpType::Update),
            "no field changes -> no Update: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.op == OpType::AddEdge
                && e.payload.get(TARGET_KEY) == Some(&serde_json::json!("y"))),
            "adds y: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.op == OpType::RemoveEdge
                && e.payload.get(TARGET_KEY) == Some(&serde_json::json!("x"))),
            "removes x: {events:?}"
        );
    }

    #[test]
    fn compensate_reconciles_typed_edges_per_type_and_target() {
        // The SAME target moves from INFO to HIER, and an INFO edge to y disappears:
        // each `(type, target)` pair is its own edge, so the compensation is
        // AddEdge HIER=x, RemoveEdge INFO=x, RemoveEdge INFO=y - all typed.
        let mut f = task("a", &[], &[]);
        f.relationships
            .insert(INFO.to_string(), vec!["x".to_string(), "y".to_string()]);
        let mut t = task("a", &[], &[]);
        t.relationships
            .insert(HIER.to_string(), vec!["x".to_string()]);
        let events = compensate(&state(&[f]), &state(&[t]), &["a".to_string()]);

        let pair = |e: &MutationEvent| {
            (
                e.op.clone(),
                e.payload.get(REL_KEY).cloned(),
                e.payload.get(TARGET_KEY).cloned(),
            )
        };
        let got: Vec<_> = events.iter().map(pair).collect();
        assert!(
            got.contains(&(
                OpType::AddEdge,
                Some(serde_json::json!(HIER)),
                Some(serde_json::json!("x"))
            )),
            "adds the new typed edge: {got:?}"
        );
        assert!(
            got.contains(&(
                OpType::RemoveEdge,
                Some(serde_json::json!(INFO)),
                Some(serde_json::json!("x"))
            )),
            "removes the old type's edge to the same target: {got:?}"
        );
        assert!(
            got.contains(&(
                OpType::RemoveEdge,
                Some(serde_json::json!(INFO)),
                Some(serde_json::json!("y"))
            )),
            "removes the dropped edge: {got:?}"
        );
        assert_eq!(events.len(), 3, "no Update, nothing extra: {got:?}");
    }
}
