//! Git custom merge drivers for the event log and its baseline.
//!
//! The log is a *sequence*, not a timestamp-keyed CRDT. Merging two diverged
//! branches is a **rebase**: keep our events, restack the other branch's
//! concurrent events on top, and then settle any genuine contradictions with an
//! explicit, appended **resolution event** so the decision is visible in the
//! history rather than hidden in an ordering trick.
//!
//! Resolution is **per-field**: only a field (or dependency edge, or whole-task
//! delete) that *both* branches changed to incompatible values is a conflict;
//! everything else merges untouched. Each conflict is settled by the
//! `[merge] on_conflict` policy - `surface` (stop for a human), or one of three
//! predictable strategies: `latest` (newest timestamp wins), `ours`, `theirs`.
//!
//! The baseline gets its own keep-ours driver: two branches that both compacted
//! folded the same shared history to different depths, so our own baseline plus
//! the (separately reconciled) log already reconstructs the state.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::OnConflict;
use crate::error::DynError;
use crate::model::{edge_rel, edge_target, MutationEvent, OpType, TaskState, REL_KEY, TARGET_KEY};

/// `ta git-merge %O %A %B` - reconcile diverged mutation logs into `current`.
///
/// (Git's `%A` = "ours"; `incoming` is `%B` = "theirs".) `marker_path`, when
/// known, is where a surfaced conflict is recorded for `ta resolve`.
pub fn execute_git_merge(
    ancestor: &str,
    current: &str,
    incoming: &str,
    on_conflict: OnConflict,
    marker_path: Option<&Path>,
) -> Result<(), DynError> {
    let anc = read_log(ancestor)?;
    let ours = read_log(current)?;
    let theirs = read_log(incoming)?;

    // The last sequence number both branches share. Everything above it on each
    // side is that branch's concurrent work since the fork.
    let fork = anc.iter().map(|e| e.seq).max().unwrap_or(0);

    // Removals are symmetric. An ancestor event a branch no longer carries -
    // above that branch's compaction watermark, so it wasn't merely folded into
    // the baseline - was reverted or hand-removed on that branch. The merge
    // honors BOTH sides' removals (a union), so a revert converges to the same
    // result regardless of merge direction. Ours' shared tail already lacks ours'
    // removals; here we also drop the events theirs removed.
    let removed_by_theirs = removed_seqs(&anc, &theirs);
    let shared_tail: Vec<&MutationEvent> = ours
        .iter()
        .filter(|e| e.seq <= fork && !removed_by_theirs.contains(&e.seq))
        .collect();
    let ours_concurrent: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq > fork).collect();
    let theirs_concurrent: Vec<&MutationEvent> = theirs.iter().filter(|e| e.seq > fork).collect();

    // A shared seq with *different* content on different sides is not a clean
    // revert - it's reuse of a freed seq (undo of a shared event) or a hand-edit,
    // and the merge can't trust it. Warn loudly; git surfaces driver stderr.
    let mismatches = content_mismatches(fork, &anc, &ours, &theirs);
    if mismatches > 0 {
        eprintln!(
            "taska: warning: {mismatches} event(s) reuse a shared seq with different content \
             (undo/seq-reuse or a hand-edit) - the merge may be unreliable; inspect the log."
        );
    }

    // A shared event present on one side but reverted on the other (compared only
    // above BOTH branches' watermarks, so a baseline fold is never mistaken for a
    // revert) means the shared prefix was rewritten - usually a `git revert`. The
    // removal-union above already reconciles it convergently by honoring the
    // removal, but that silently drops events the other branch still had, so we
    // surface it. (A revert BELOW the higher watermark stays invisible - see
    // `removed_seqs` and the `revert_below_the_watermark_is_a_known_limitation` test.)
    let rewritten = rewritten_shared_seqs(fork, &ours, &theirs);
    if !rewritten.is_empty() {
        eprintln!(
            "taska: warning: {} shared event(s) were reverted on one branch but kept on the \
             other (seq {:?}); the merge honored the removal. Review if that drop was unexpected.",
            rewritten.len(),
            rewritten
        );
    }

    // `surface` still writes a valid, deterministic file (resolving tentatively
    // as `ours`) so it's never broken, then flags the conflicts and fails so Git
    // marks the path unmerged.
    let strategy = Strategy::for_policy(on_conflict);
    let plan = resolve(
        &summarize(&ours_concurrent),
        &summarize(&theirs_concurrent),
        strategy,
    );

    let merged = assemble(
        &shared_tail,
        &ours_concurrent,
        &theirs_concurrent,
        &plan,
        fork,
    );
    write_log(current, &merged)?;

    if on_conflict == OnConflict::Surface && !plan.conflicts.is_empty() {
        if let Some(path) = marker_path {
            write_conflict_marker(path, &plan.conflicts)?;
        }
        eprintln!(
            "taska: {} event-log merge conflict(s); a tentative merge (keeping ours) was \
             written but flagged. Review with `ta resolve`, then `git add` and commit.",
            plan.conflicts.len()
        );
        return Err(format!("{} unresolved merge conflict(s)", plan.conflicts.len()).into());
    }

    Ok(())
}

/// Ancestor seqs a branch deliberately dropped: above the branch's compaction
/// watermark (so not merely folded into its baseline) yet absent from its log -
/// i.e. reverted or hand-removed. Unioning both sides' removals makes a revert
/// converge regardless of merge direction.
///
/// LIMITATION: this only sees absences *above* the branch's watermark. A revert
/// *below* it - of the branch's earliest events, or one that compaction later
/// folded past - is indistinguishable from a baseline fold using the log alone, so
/// the removal is missed and the event can resurrect or the merge diverge by
/// direction. `rewritten_shared_seqs` *warns* about the above-watermark rewrites it
/// can see; the below-watermark case stays a blind spot (the
/// `revert_below_the_watermark_is_a_known_limitation` test).
fn removed_seqs(anc: &[MutationEvent], branch: &[MutationEvent]) -> HashSet<u64> {
    let watermark = branch
        .iter()
        .map(|e| e.seq)
        .min()
        .map_or(0, |m| m.saturating_sub(1));
    let present: HashSet<u64> = branch.iter().map(|e| e.seq).collect();
    anc.iter()
        .filter(|e| e.seq > watermark && !present.contains(&e.seq))
        .map(|e| e.seq)
        .collect()
}

/// Shared-region seqs (`<= fork`) present on exactly one side, compared only
/// *above both branches' compaction watermarks* - the region both still hold in
/// their logs. A mismatch there means the shared prefix was rewritten on one
/// branch (a `git revert` of an event the other kept). Restricting to above both
/// watermarks is what keeps this sound: anything either side folded into its
/// baseline sits below the higher watermark and is excluded, so ordinary
/// compaction never trips it. The blind spot is a revert *below* that watermark
/// (see `removed_seqs`).
fn rewritten_shared_seqs(fork: u64, ours: &[MutationEvent], theirs: &[MutationEvent]) -> Vec<u64> {
    let watermark = |log: &[MutationEvent]| {
        log.iter()
            .map(|e| e.seq)
            .min()
            .map_or(0, |m| m.saturating_sub(1))
    };
    let lo = watermark(ours).max(watermark(theirs));
    let in_window = |log: &[MutationEvent]| -> HashSet<u64> {
        log.iter()
            .map(|e| e.seq)
            .filter(|&s| s > lo && s <= fork)
            .collect()
    };
    let mut diff: Vec<u64> = in_window(ours)
        .symmetric_difference(&in_window(theirs))
        .copied()
        .collect();
    diff.sort_unstable();
    diff
}

/// Count shared-region (`seq <= fork`) seqs that carry *different* content on
/// different sides - the dangerous "same seq, different event" case (undo
/// seq-reuse or a hand-edit), as opposed to a clean revert.
fn content_mismatches(
    fork: u64,
    anc: &[MutationEvent],
    ours: &[MutationEvent],
    theirs: &[MutationEvent],
) -> usize {
    let mut seen: HashMap<u64, String> = HashMap::new();
    let mut bad: HashSet<u64> = HashSet::new();
    for log in [anc, ours, theirs] {
        for event in log.iter().filter(|e| e.seq <= fork) {
            let content = serde_json::to_string(event).unwrap_or_default();
            match seen.get(&event.seq).cloned() {
                Some(prev) if prev != content => {
                    bad.insert(event.seq);
                }
                None => {
                    seen.insert(event.seq, content);
                }
                _ => {}
            }
        }
    }
    bad.len()
}

/// `ta git-merge-baseline %O %A %B` - keep our baseline.
///
/// `current` (`%A`) already holds our version, so we leave it untouched and only
/// sanity-check for a sign that someone compacted past their fork (which
/// `keep_events` is meant to prevent): a task unchanged from the ancestor on our
/// side but different on theirs.
pub fn execute_git_merge_baseline(
    ancestor: &str,
    current: &str,
    incoming: &str,
) -> Result<(), DynError> {
    let base = index_baseline(read_baseline(ancestor)?);
    let ours = index_baseline(read_baseline(current)?);
    let theirs = index_baseline(read_baseline(incoming)?);

    for (id, base_task) in &base {
        if let (Some(our_task), Some(their_task)) = (ours.get(id), theirs.get(id)) {
            if our_task == base_task && their_task != base_task {
                eprintln!(
                    "taska: warning: baseline task `{id}` diverged across branches; a compaction \
                     may have folded events past its fork (consider raising keep_events)."
                );
            }
        }
    }

    // Leaving `current` as-is keeps ours.
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-branch summary
// ---------------------------------------------------------------------------

/// What one branch did to a single task since the fork: the last value written
/// to each field, the last add/remove of each dependency edge, and whether it
/// net-deleted the task. Timestamps are carried so `latest` can compare writes.
#[derive(Default)]
struct Delta {
    fields: HashMap<String, FieldWrite>,
    /// Keyed by `(relationship type, target id)` - a `depends_on` edge and a
    /// `relates_to` edge to the same task are distinct relationships.
    deps: HashMap<(String, String), DepWrite>,
    deleted: Option<DateTime<Utc>>,
    last_change: Option<DateTime<Utc>>,
}

struct FieldWrite {
    value: Value,
    ts: DateTime<Utc>,
}

struct DepWrite {
    added: bool,
    ts: DateTime<Utc>,
}

/// Collapse a branch's concurrent events into one [`Delta`] per task.
fn summarize(events: &[&MutationEvent]) -> HashMap<String, Delta> {
    let mut deltas: HashMap<String, Delta> = HashMap::new();
    for event in events {
        let delta = deltas.entry(event.task_id.clone()).or_default();
        match event.op {
            OpType::Create | OpType::Update => {
                if matches!(event.op, OpType::Create) {
                    delta.deleted = None; // a (re)create undoes an earlier delete
                }
                for (key, value) in &event.payload {
                    delta.fields.insert(
                        key.clone(),
                        FieldWrite {
                            value: value.clone(),
                            ts: event.timestamp,
                        },
                    );
                }
                delta.last_change = Some(max_ts(delta.last_change, event.timestamp));
            }
            OpType::Append | OpType::Add | OpType::Remove => {
                // Accumulating ops commute - two concurrent appends (text),
                // adds (numbers/set inserts), or removes to the same field
                // accumulate at replay rather than contending - so they are NOT
                // recorded as field writes (which would flag a false conflict
                // and let a resolution event overwrite the accumulated value).
                // They still count as a change for the delete-vs-change check.
                delta.last_change = Some(max_ts(delta.last_change, event.timestamp));
            }
            OpType::Delete => delta.deleted = Some(event.timestamp),
            OpType::AddEdge | OpType::RemoveEdge => {
                // A well-formed edge carries both `target` and `rel`; one missing
                // either is malformed (a pre-1.0 untyped event) and ignored.
                if let (Some(dep), Some(dep_type)) =
                    (edge_target(&event.payload), edge_rel(&event.payload))
                {
                    delta.deps.insert(
                        (dep_type.to_string(), dep.to_string()),
                        DepWrite {
                            added: matches!(event.op, OpType::AddEdge),
                            ts: event.timestamp,
                        },
                    );
                    delta.last_change = Some(max_ts(delta.last_change, event.timestamp));
                }
            }
        }
    }
    deltas
}

fn max_ts(current: Option<DateTime<Utc>>, ts: DateTime<Utc>) -> DateTime<Utc> {
    current.map_or(ts, |c| c.max(ts))
}

// ---------------------------------------------------------------------------
// Conflict resolution
// ---------------------------------------------------------------------------

/// How `auto` settles a contradiction.
///
/// SERIALIZATION CONTRACT: the lowercase names are written into each resolution
/// event's `_meta.strategy` in the persisted log - do not rename without a
/// migration. (`surface` maps to `ours` here; see [`Strategy::for_policy`].)
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Strategy {
    Latest,
    Ours,
    Theirs,
}

impl Strategy {
    /// `surface` resolves tentatively as `ours` (and is flagged by the caller).
    const fn for_policy(policy: OnConflict) -> Self {
        match policy {
            OnConflict::Surface | OnConflict::Ours => Self::Ours,
            OnConflict::Latest => Self::Latest,
            OnConflict::Theirs => Self::Theirs,
        }
    }

    fn pick(self, ours_ts: DateTime<Utc>, theirs_ts: DateTime<Utc>) -> Side {
        match self {
            Self::Theirs => Side::Theirs,
            // Tie goes to ours, so the choice is total and deterministic.
            Self::Latest if theirs_ts > ours_ts => Side::Theirs,
            Self::Ours | Self::Latest => Side::Ours,
        }
    }
}

/// Which branch a pick came from.
///
/// SERIALIZATION CONTRACT: the lowercase names below are written into every
/// resolution event's `_meta` and into the conflict marker. They are part of the
/// on-disk log format - do not rename a variant without migrating existing logs.
#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Side {
    Ours,
    Theirs,
}

/// A branch's effect in a whole-task delete-vs-change conflict, recorded as the
/// candidate "value" for each side.
///
/// SERIALIZATION CONTRACT: written into `_meta`/the marker - keep the names.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum TaskOutcome {
    Deleted,
    Changed,
}

/// A branch's effect on a dependency edge in an add/remove conflict.
///
/// SERIALIZATION CONTRACT: written into `_meta`/the marker - keep the names.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum EdgeOutcome {
    Added,
    Removed,
}

impl EdgeOutcome {
    const fn of(added: bool) -> Self {
        if added {
            Self::Added
        } else {
            Self::Removed
        }
    }
}

/// Provenance recorded on a resolution event's `_meta`: which strategy ran and,
/// per resolved item, the two candidate values and the side kept.
///
/// SERIALIZATION CONTRACT: these field names and the enum strings inside are part
/// of the on-disk `_meta` format - keep them stable.
#[derive(Serialize)]
struct ResolutionMeta {
    strategy: Strategy,
    resolved: Vec<ResolvedItem>,
}

/// One resolved contradiction. Exactly one of `field`/`dep`/`task` is set, naming
/// what was resolved.
#[derive(Serialize)]
struct ResolvedItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dep: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<String>,
    ours: Value,
    theirs: Value,
    kept: Side,
}

/// Serialize provenance for an event's `_meta` slot.
fn provenance(strategy: Strategy, resolved: Vec<ResolvedItem>) -> Value {
    serde_json::to_value(ResolutionMeta { strategy, resolved })
        .expect("resolution provenance always serializes")
}

/// Serialize a small typed outcome into the heterogeneous `ours`/`theirs` slot.
fn as_value<T: Serialize>(outcome: T) -> Value {
    serde_json::to_value(outcome).expect("merge outcome always serializes")
}

/// A single resolved contradiction, recorded for `ta resolve` / the marker.
///
/// SERIALIZATION CONTRACT: serialized verbatim into the conflict marker file
/// (`merge-conflict.json`), which `ta resolve` reads back.
#[derive(Serialize)]
struct Conflict {
    task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    reason: &'static str,
    ours: Value,
    theirs: Value,
    kept: Side,
}

/// The synthetic events to append and the raw deletes to drop, plus the list of
/// conflicts that were resolved.
#[derive(Default)]
struct Plan {
    resolutions: Vec<MutationEvent>,
    /// `(is_ours, task_id)` of Delete events to omit from the merged stream.
    drop_deletes: HashSet<(bool, String)>,
    conflicts: Vec<Conflict>,
}

/// Compare the two branches' deltas task-by-task and decide each contradiction.
fn resolve(
    ours: &HashMap<String, Delta>,
    theirs: &HashMap<String, Delta>,
    strategy: Strategy,
) -> Plan {
    let mut plan = Plan::default();

    // Only a task touched by BOTH branches can contain a contradiction.
    let mut tasks: Vec<&String> = ours.keys().filter(|t| theirs.contains_key(*t)).collect();
    tasks.sort(); // stable output regardless of map iteration order

    for task in tasks {
        let (od, td) = (&ours[task], &theirs[task]);

        // Whole-task: one branch deleted while the other kept changing it.
        if resolve_delete(task, od, td, strategy, &mut plan) {
            continue; // the task's existence is decided; skip field/dep resolution
        }

        resolve_fields(task, od, td, strategy, &mut plan);
        resolve_deps(task, od, td, strategy, &mut plan);
    }

    plan
}

/// Resolve a delete-vs-change contradiction (one branch deleted a task while the
/// other kept editing it); returns whether one was found.
///
/// This is decided at the WHOLE-TASK level, not per field - a deleted task has no
/// fields to merge into, so the only question is whether it survives. The active
/// strategy decides exactly as for a field conflict: for `latest`, the delete's
/// timestamp races the other branch's most recent change.
///
/// Deliberate choice: a delete-vs-change is treated like any other conflict, so
/// even `surface` resolves it tentatively (as `ours`) rather than always halting
/// on it. If we later decide structural conflicts should always surface
/// regardless of strategy, this function is the single place to branch on it.
fn resolve_delete(task: &str, od: &Delta, td: &Delta, strategy: Strategy, plan: &mut Plan) -> bool {
    // A real conflict only when exactly one side deleted and the other changed.
    // Track which side deleted as a `Side` so the winner check reads directly.
    let (delete_side, delete_ts, change_ts) = match (od.deleted, td.deleted) {
        (Some(d), None) => match td.last_change {
            Some(change_ts) => (Side::Ours, d, change_ts),
            None => return false,
        },
        (None, Some(d)) => match od.last_change {
            Some(change_ts) => (Side::Theirs, d, change_ts),
            None => return false,
        },
        _ => return false,
    };

    // Order the timestamps as (ours, theirs) for the strategy; the delete wins
    // iff the strategy picked the side that deleted.
    let (ours_ts, theirs_ts) = match delete_side {
        Side::Ours => (delete_ts, change_ts),
        Side::Theirs => (change_ts, delete_ts),
    };
    let winner = strategy.pick(ours_ts, theirs_ts);
    let delete_wins = winner == delete_side;

    let (ours_outcome, theirs_outcome) = match delete_side {
        Side::Ours => (TaskOutcome::Deleted, TaskOutcome::Changed),
        Side::Theirs => (TaskOutcome::Changed, TaskOutcome::Deleted),
    };

    if delete_wins {
        // Force deletion last; the other side's changes replay first, then vanish.
        let item = ResolvedItem {
            field: None,
            dep: None,
            task: Some(task.to_string()),
            ours: as_value(ours_outcome),
            theirs: as_value(theirs_outcome),
            kept: winner,
        };
        plan.resolutions.push(event(
            OpType::Delete,
            task,
            Map::new(),
            delete_ts,
            Some(provenance(strategy, vec![item])),
        ));
    } else {
        // Keep the changes: drop the losing Delete so it can't remove the task.
        // (The change events are the user's own, so they carry their own history.)
        plan.drop_deletes
            .insert((delete_side == Side::Ours, task.to_string()));
    }

    plan.conflicts.push(Conflict {
        task_id: task.to_string(),
        field: None,
        reason: "one branch deleted a task the other changed",
        ours: as_value(ours_outcome),
        theirs: as_value(theirs_outcome),
        kept: winner,
    });
    true
}

fn resolve_fields(task: &str, od: &Delta, td: &Delta, strategy: Strategy, plan: &mut Plan) {
    let mut winners = Map::new();
    let mut latest_ts: Option<DateTime<Utc>> = None;
    let mut items = Vec::new();

    let mut fields: Vec<&String> = od
        .fields
        .keys()
        .filter(|f| td.fields.contains_key(*f))
        .collect();
    fields.sort();

    for field in fields {
        let (ow, tw) = (&od.fields[field], &td.fields[field]);
        if ow.value == tw.value {
            continue; // both wrote the same value - no contradiction
        }
        let winner = strategy.pick(ow.ts, tw.ts);
        let (value, ts) = match winner {
            Side::Ours => (ow.value.clone(), ow.ts),
            Side::Theirs => (tw.value.clone(), tw.ts),
        };
        latest_ts = Some(max_ts(latest_ts, ts));
        winners.insert(field.clone(), value);
        items.push(ResolvedItem {
            field: Some(field.clone()),
            dep: None,
            task: None,
            ours: ow.value.clone(),
            theirs: tw.value.clone(),
            kept: winner,
        });
        plan.conflicts.push(Conflict {
            task_id: task.to_string(),
            field: Some(field.clone()),
            reason: "both branches set the same field to different values",
            ours: ow.value.clone(),
            theirs: tw.value.clone(),
            kept: winner,
        });
    }

    // `latest_ts` is `Some` iff at least one field was contested, which is also
    // exactly when `winners` is non-empty - so matching on it both gates the push
    // and yields the timestamp without an unwrap.
    if let Some(ts) = latest_ts {
        // One Update carrying every per-field winner for this task, annotated with
        // the provenance of each pick.
        plan.resolutions.push(event(
            OpType::Update,
            task,
            winners,
            ts,
            Some(provenance(strategy, items)),
        ));
    }
}

fn resolve_deps(task: &str, od: &Delta, td: &Delta, strategy: Strategy, plan: &mut Plan) {
    let mut edges: Vec<&(String, String)> = od
        .deps
        .keys()
        .filter(|d| td.deps.contains_key(*d))
        .collect();
    edges.sort();

    for edge in edges {
        let (rel_type, target) = edge;
        let (ow, tw) = (&od.deps[edge], &td.deps[edge]);
        if ow.added == tw.added {
            continue; // both added or both removed - no contradiction
        }
        let winner = strategy.pick(ow.ts, tw.ts);
        let (added, ts) = match winner {
            Side::Ours => (ow.added, ow.ts),
            Side::Theirs => (tw.added, tw.ts),
        };
        let op = if added {
            OpType::AddEdge
        } else {
            OpType::RemoveEdge
        };
        let mut payload = Map::new();
        payload.insert(TARGET_KEY.to_string(), Value::String(target.clone()));
        payload.insert(REL_KEY.to_string(), Value::String(rel_type.clone()));
        // Provenance labels every edge by its type uniformly - `type:target` -
        // so the `_meta` record names which typed edge was resolved.
        let label = format!("{rel_type}:{target}");
        let item = ResolvedItem {
            field: None,
            dep: Some(label.clone()),
            task: None,
            ours: as_value(EdgeOutcome::of(ow.added)),
            theirs: as_value(EdgeOutcome::of(tw.added)),
            kept: winner,
        };
        plan.resolutions.push(event(
            op,
            task,
            payload,
            ts,
            Some(provenance(strategy, vec![item])),
        ));

        plan.conflicts.push(Conflict {
            task_id: task.to_string(),
            field: Some(format!("dep:{label}")),
            reason: "one branch added a dependency the other removed",
            ours: as_value(EdgeOutcome::of(ow.added)),
            theirs: as_value(EdgeOutcome::of(tw.added)),
            kept: winner,
        });
    }
}

fn event(
    op: OpType,
    task: &str,
    payload: Map<String, Value>,
    ts: DateTime<Utc>,
    meta: Option<Value>,
) -> MutationEvent {
    MutationEvent {
        seq: 0,
        timestamp: ts,
        op,
        task_id: task.to_string(),
        meta,
        payload,
    }
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Build the merged log: shared history, then both branches' concurrent events
/// (minus any Delete the resolution dropped), then the resolution events - all
/// above the fork renumbered onto a fresh contiguous tail.
fn assemble(
    shared_tail: &[&MutationEvent],
    ours_concurrent: &[&MutationEvent],
    theirs_concurrent: &[&MutationEvent],
    plan: &Plan,
    fork: u64,
) -> Vec<MutationEvent> {
    let mut merged: Vec<MutationEvent> = shared_tail.iter().map(|e| (*e).clone()).collect();

    let kept = |events: &[&MutationEvent], is_ours: bool, out: &mut Vec<MutationEvent>| {
        for event in events {
            let dropped = matches!(event.op, OpType::Delete)
                && plan
                    .drop_deletes
                    .contains(&(is_ours, event.task_id.clone()));
            if !dropped {
                out.push((*event).clone());
            }
        }
    };
    kept(ours_concurrent, true, &mut merged);
    kept(theirs_concurrent, false, &mut merged);
    merged.extend(plan.resolutions.iter().cloned());

    // Renumber everything above the fork into a contiguous, strictly-increasing
    // tail so the log keeps its core invariant.
    for (seq, event) in (fork + 1..).zip(merged.iter_mut().skip(shared_tail.len())) {
        event.seq = seq;
    }
    merged
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

fn write_conflict_marker(path: &Path, conflicts: &[Conflict]) -> Result<(), DynError> {
    #[derive(Serialize)]
    struct Marker<'a> {
        conflicts: &'a [Conflict],
    }
    std::fs::write(path, serde_json::to_string_pretty(&Marker { conflicts })?)?;
    Ok(())
}

fn read_log(path: &str) -> Result<Vec<MutationEvent>, DynError> {
    let mut events = Vec::new();
    if Path::new(path).exists() {
        let file = File::open(path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                events.push(serde_json::from_str::<MutationEvent>(&line)?);
            }
        }
    }
    // Each input is a committed log version, which is always seq-ordered; a
    // violation means corruption, so fail the merge rather than reorder.
    crate::model::verify_seq_order(&events)?;
    Ok(events)
}

fn write_log(path: &str, events: &[MutationEvent]) -> Result<(), DynError> {
    let mut file = File::create(path)?;
    for event in events {
        writeln!(file, "{}", serde_json::to_string(event)?)?;
    }
    file.flush()?;
    Ok(())
}

fn read_baseline(path: &str) -> Result<Vec<TaskState>, DynError> {
    let mut out = Vec::new();
    if Path::new(path).exists() {
        let file = File::open(path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                out.push(serde_json::from_str::<TaskState>(&line)?);
            }
        }
    }
    Ok(out)
}

fn index_baseline(tasks: Vec<TaskState>) -> HashMap<String, TaskState> {
    tasks.into_iter().map(|t| (t.id.clone(), t)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::model::STATUS_KEY;
    use crate::test_support::names::*;
    use serde_json::json;

    fn ev(seq: u64, mins: i64, op: OpType, task: &str, payload: &[(&str, Value)]) -> MutationEvent {
        let base = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let map: Map<String, Value> = payload
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        MutationEvent {
            seq,
            timestamp: base + chrono::Duration::minutes(mins),
            op,
            task_id: task.to_string(),
            meta: None,
            payload: map,
        }
    }

    /// Merge two concurrent logs that share the `anc` prefix, returning the
    /// final materialized fields of `task`.
    fn merge_to_fields(
        anc: &[MutationEvent],
        ours: &[MutationEvent],
        theirs: &[MutationEvent],
        policy: OnConflict,
        task: &str,
    ) -> Map<String, Value> {
        let fork = anc.iter().map(|e| e.seq).max().unwrap_or(0);
        let shared: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq <= fork).collect();
        let oc: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq > fork).collect();
        let tc: Vec<&MutationEvent> = theirs.iter().filter(|e| e.seq > fork).collect();
        let plan = resolve(
            &summarize(&oc),
            &summarize(&tc),
            Strategy::for_policy(policy),
        );
        let merged = assemble(&shared, &oc, &tc, &plan, fork);
        let state = Engine::materialize_state(Vec::new(), merged, DONE_STATUS);
        state
            .get(task)
            .map(|t| t.custom_fields.clone())
            .unwrap_or_default()
    }

    /// Merge two diverged logs the way the driver does - including the symmetric
    /// removal union - and return the sorted ids of surviving tasks.
    fn merged_task_ids(
        anc: &[MutationEvent],
        ours: &[MutationEvent],
        theirs: &[MutationEvent],
    ) -> Vec<String> {
        let fork = anc.iter().map(|e| e.seq).max().unwrap_or(0);
        let removed = removed_seqs(anc, theirs);
        let shared: Vec<&MutationEvent> = ours
            .iter()
            .filter(|e| e.seq <= fork && !removed.contains(&e.seq))
            .collect();
        let oc: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq > fork).collect();
        let tc: Vec<&MutationEvent> = theirs.iter().filter(|e| e.seq > fork).collect();
        let plan = resolve(&summarize(&oc), &summarize(&tc), Strategy::Ours);
        let merged = assemble(&shared, &oc, &tc, &plan, fork);
        let mut ids: Vec<String> = Engine::materialize_state(Vec::new(), merged, DONE_STATUS)
            .into_keys()
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn revert_converges_regardless_of_merge_direction() {
        // Ancestor has a, b, c (seq 1,2,3). One branch reverts b's create
        // (removes seq 2 -> a gap); the other keeps all three and adds d (seq 4).
        let anc = vec![
            ev(1, 0, OpType::Create, "a", &[]),
            ev(2, 0, OpType::Create, "b", &[]),
            ev(3, 0, OpType::Create, "c", &[]),
        ];
        let reverted = vec![anc[0].clone(), anc[2].clone()]; // b removed
        let kept = vec![
            anc[0].clone(),
            anc[1].clone(),
            anc[2].clone(),
            ev(4, 0, OpType::Create, "d", &[]),
        ];

        let one = merged_task_ids(&anc, &reverted, &kept);
        let two = merged_task_ids(&anc, &kept, &reverted);
        assert_eq!(one, two, "revert must converge regardless of direction");
        assert_eq!(
            one,
            vec!["a".to_string(), "c".to_string(), "d".to_string()],
            "b stays reverted, a/c/d present"
        );
    }

    #[test]
    fn multiple_reverts_converge_with_several_gaps() {
        // Ancestor a..e (seq 1..5). One branch reverts b and d (seq 2 and 4 - two
        // separate gaps); the other keeps all five and adds f (seq 6). Both gaps
        // must reconcile and converge either direction.
        let anc = vec![
            ev(1, 0, OpType::Create, "a", &[]),
            ev(2, 0, OpType::Create, "b", &[]),
            ev(3, 0, OpType::Create, "c", &[]),
            ev(4, 0, OpType::Create, "d", &[]),
            ev(5, 0, OpType::Create, "e", &[]),
        ];
        let reverted = vec![anc[0].clone(), anc[2].clone(), anc[4].clone()]; // b, d gone
        let kept = vec![
            anc[0].clone(),
            anc[1].clone(),
            anc[2].clone(),
            anc[3].clone(),
            anc[4].clone(),
            ev(6, 0, OpType::Create, "f", &[]),
        ];

        let one = merged_task_ids(&anc, &reverted, &kept);
        let two = merged_task_ids(&anc, &kept, &reverted);
        assert_eq!(
            one, two,
            "two-gap revert must converge regardless of direction"
        );
        assert_eq!(
            one,
            vec![
                "a".to_string(),
                "c".to_string(),
                "e".to_string(),
                "f".to_string()
            ],
            "both reverted tasks gone, the rest present"
        );
    }

    #[test]
    fn revert_of_the_fork_event_converges() {
        // Reverting the HIGHEST ancestor seq - the fork event itself - still
        // converges, and the dropped top event stays gone.
        let anc = vec![
            ev(1, 0, OpType::Create, "a", &[]),
            ev(2, 0, OpType::Create, "b", &[]),
            ev(3, 0, OpType::Create, "c", &[]),
        ];
        let reverted = vec![anc[0].clone(), anc[1].clone()]; // c (the fork) removed
        let kept = vec![
            anc[0].clone(),
            anc[1].clone(),
            anc[2].clone(),
            ev(4, 0, OpType::Create, "d", &[]),
        ];

        let one = merged_task_ids(&anc, &reverted, &kept);
        let two = merged_task_ids(&anc, &kept, &reverted);
        assert_eq!(one, two, "reverting the fork event must converge");
        assert_eq!(
            one,
            vec!["a".to_string(), "b".to_string(), "d".to_string()],
            "c (the reverted fork event) stays gone"
        );
    }

    #[test]
    fn merge_against_an_emptied_log_converges() {
        // A branch that reverted its entire shared history has an empty log:
        // `min(seq)` is None, so the watermark falls to 0 and every ancestor event
        // is correctly seen as removed. Exercises the empty-branch / None path.
        let anc = vec![
            ev(1, 0, OpType::Create, "a", &[]),
            ev(2, 0, OpType::Create, "b", &[]),
        ];
        let emptied: Vec<MutationEvent> = Vec::new();
        let kept = vec![
            anc[0].clone(),
            anc[1].clone(),
            ev(3, 0, OpType::Create, "c", &[]),
        ];

        let one = merged_task_ids(&anc, &emptied, &kept); // ours emptied
        let two = merged_task_ids(&anc, &kept, &emptied); // theirs emptied
        assert_eq!(one, two, "an emptied branch converges both ways");
        assert_eq!(
            one,
            vec!["c".to_string()],
            "a and b were reverted away; only the surviving branch's c remains"
        );
    }

    #[test]
    fn reverting_a_create_above_the_watermark_orphans_its_kept_updates() {
        // x(seq1), then Create a(seq2) and Update a(seq3). One branch reverts only
        // a's Create (seq2) while keeping x and a's Update. Because seq2 sits ABOVE
        // the branch's min (x=seq1 remains), its removal IS detected - and the kept
        // Update, now applying to no task, surfaces as an orphan on replay.
        let anc = vec![
            ev(1, 0, OpType::Create, "x", &[]),
            ev(2, 0, OpType::Create, "a", &[(STATUS_KEY, json!("open"))]),
            ev(3, 0, OpType::Update, "a", &[(STATUS_KEY, json!("done"))]),
        ];
        let reverted = vec![anc[0].clone(), anc[2].clone()]; // Create a (seq2) gone
        let kept = [anc[0].clone(), anc[1].clone(), anc[2].clone()];

        let removed = removed_seqs(&anc, &reverted);
        assert!(
            removed.contains(&2),
            "the reverted Create sits above the watermark, so it is dropped"
        );

        // Reconstruct the merged log (ours = kept) and replay it for the orphan.
        let fork = anc.iter().map(|e| e.seq).max().unwrap_or(0);
        let shared: Vec<&MutationEvent> = kept
            .iter()
            .filter(|e| e.seq <= fork && !removed.contains(&e.seq))
            .collect();
        let oc: Vec<&MutationEvent> = Vec::new();
        let tc: Vec<&MutationEvent> = Vec::new();
        let plan = resolve(&summarize(&oc), &summarize(&tc), Strategy::Ours);
        let merged = assemble(&shared, &oc, &tc, &plan, fork);

        let (state, orphans) = Engine::materialize_report(Vec::new(), merged, DONE_STATUS);
        assert!(
            !state.contains_key("a"),
            "task a does not materialize - its Create was reverted"
        );
        assert_eq!(orphans.len(), 1, "a's kept Update is reported as an orphan");
    }

    #[test]
    fn revert_below_the_watermark_is_a_known_limitation() {
        // KNOWN LIMITATION - the BELOW-watermark blind spot of the revert checks.
        // `removed_seqs` and `rewritten_shared_seqs` only see reverts ABOVE the
        // branch's min-seq watermark. A revert of the EARLIEST event raises that
        // branch's min - the same shape compaction-past-a-revert produces - so it
        // falls below the watermark and is invisible: the event resurrects and the
        // merge DIVERGES by direction. This pins that residual behavior.
        let anc = vec![
            ev(1, 0, OpType::Create, "a", &[]),
            ev(2, 0, OpType::Create, "b", &[]),
            ev(3, 0, OpType::Create, "c", &[]),
        ];
        // Drop the earliest event (a, seq1), keeping b, c - min becomes 2, so the
        // watermark (1) hides seq1's removal.
        let reverted = vec![anc[1].clone(), anc[2].clone()];
        let kept = vec![anc[0].clone(), anc[1].clone(), anc[2].clone()];

        assert!(
            !removed_seqs(&anc, &reverted).contains(&1),
            "today the watermark hides the below-min removal of seq1"
        );

        let into_kept = merged_task_ids(&anc, &kept, &reverted); // ours kept a
        let into_reverted = merged_task_ids(&anc, &reverted, &kept); // ours dropped a
        assert!(
            into_kept.contains(&"a".to_string()),
            "a resurrects when the side that kept it is `ours`"
        );
        assert!(
            !into_reverted.contains(&"a".to_string()),
            "a stays gone when the side that reverted it is `ours`"
        );
        assert_ne!(
            into_kept, into_reverted,
            "so the merge diverges by direction - the below-watermark blind spot"
        );
        // And the above-watermark detector cannot see it either (it is below the
        // higher watermark), which is exactly why it stays a limitation.
        assert!(
            rewritten_shared_seqs(3, &kept, &reverted).is_empty(),
            "the below-watermark revert is outside the detector's window"
        );
    }

    #[test]
    fn rewritten_shared_seqs_flags_a_reverted_shared_event() {
        // Ancestor 1..5; one branch reverts seq 3 (a still-shared event, above both
        // watermarks), the other keeps it. The detector flags seq 3, either order.
        let anc = vec![
            ev(1, 0, OpType::Create, "a", &[]),
            ev(2, 0, OpType::Create, "b", &[]),
            ev(3, 0, OpType::Create, "c", &[]),
            ev(4, 0, OpType::Create, "d", &[]),
            ev(5, 0, OpType::Create, "e", &[]),
        ];
        let reverted = vec![
            anc[0].clone(),
            anc[1].clone(),
            anc[3].clone(),
            anc[4].clone(),
        ]; // c (seq 3) gone
        let kept = anc;
        let fork = 5;
        assert_eq!(rewritten_shared_seqs(fork, &kept, &reverted), vec![3]);
        assert_eq!(
            rewritten_shared_seqs(fork, &reverted, &kept),
            vec![3],
            "symmetric - flagged regardless of side"
        );
    }

    #[test]
    fn rewritten_shared_seqs_ignores_legitimate_compaction() {
        // The key soundness case: two branches that compacted to DIFFERENT depths.
        // ours folded 1,2 (log 3..5); theirs folded 1,2,3 (log 4..6). Nothing was
        // reverted, so the detector must stay silent - the differing folded prefix
        // is below the higher watermark and excluded from the comparison.
        let ours = vec![
            ev(3, 0, OpType::Create, "c", &[]),
            ev(4, 0, OpType::Create, "d", &[]),
            ev(5, 0, OpType::Create, "e", &[]),
        ];
        let theirs = vec![
            ev(4, 0, OpType::Create, "d", &[]),
            ev(5, 0, OpType::Create, "e", &[]),
            ev(6, 0, OpType::Create, "f", &[]),
        ];
        assert!(
            rewritten_shared_seqs(5, &ours, &theirs).is_empty(),
            "compaction to different depths must not be flagged as a revert"
        );
    }

    #[test]
    fn concurrent_appends_accumulate_without_conflict() {
        // Both branches append to `notes` since the fork. Under `surface` - which
        // FAILS on a genuine conflict - the merge still resolves and BOTH appends
        // survive, because appends commute and are never summarized as field
        // writes that could contend.
        let anc = vec![ev(1, 0, OpType::Create, "X", &[("notes", json!("base"))])];
        let ours = vec![
            anc[0].clone(),
            ev(2, 0, OpType::Append, "X", &[("notes", json!("from-ours"))]),
        ];
        let theirs = vec![
            anc[0].clone(),
            ev(
                2,
                0,
                OpType::Append,
                "X",
                &[("notes", json!("from-theirs"))],
            ),
        ];
        let fields = merge_to_fields(&anc, &ours, &theirs, OnConflict::Surface, "X");
        let notes = fields["notes"].as_str().unwrap();
        assert!(notes.contains("base"), "base preserved: {notes}");
        assert!(
            notes.contains("from-ours") && notes.contains("from-theirs"),
            "both concurrent appends survive: {notes}"
        );
    }

    #[test]
    fn typed_dep_edges_do_not_collide_across_types() {
        // Concurrent: ours adds `X BLOCKER Y`; theirs adds `X INFO Y`.
        // Distinct typed edges to the same target - both survive, no conflict.
        let anc = vec![ev(1, 0, OpType::Create, "X", &[])];
        let ours = [
            anc[0].clone(),
            ev(
                2,
                0,
                OpType::AddEdge,
                "X",
                &[("target", json!("Y")), ("rel", json!(BLOCKER))],
            ),
        ];
        let theirs = vec![
            anc[0].clone(),
            ev(
                2,
                0,
                OpType::AddEdge,
                "X",
                &[("target", json!("Y")), ("rel", json!(INFO))],
            ),
        ];
        let fork = 1;
        let oc: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq > fork).collect();
        let tc: Vec<&MutationEvent> = theirs.iter().filter(|e| e.seq > fork).collect();
        let plan = resolve(&summarize(&oc), &summarize(&tc), Strategy::Ours);
        assert!(
            plan.conflicts.is_empty(),
            "distinct typed edges to the same task must not conflict"
        );
        let removed = removed_seqs(&anc, &theirs);
        let shared: Vec<&MutationEvent> = ours
            .iter()
            .filter(|e| e.seq <= fork && !removed.contains(&e.seq))
            .collect();
        let merged = assemble(&shared, &oc, &tc, &plan, fork);
        let state = Engine::materialize_state(Vec::new(), merged, DONE_STATUS);
        assert_eq!(
            state["X"].relationships[BLOCKER],
            vec!["Y".to_string()],
            "BLOCKER edge"
        );
        assert_eq!(
            state["X"].relationships[INFO],
            vec!["Y".to_string()],
            "INFO edge"
        );
    }

    #[test]
    fn content_mismatch_detects_seq_reuse() {
        let anc = vec![ev(1, 0, OpType::Create, "a", &[])];
        // Same seq 1 but a different event (a reused freed seq).
        let reused = vec![ev(1, 0, OpType::Create, "different", &[])];
        assert_eq!(content_mismatches(1, &anc, &reused, &anc), 1);
        // Identical content across all sides is fine.
        assert_eq!(content_mismatches(1, &anc, &anc, &anc), 0);
    }

    #[test]
    fn non_overlapping_fields_all_survive_and_conflicts_resolve_per_field() {
        // The four-field example: STATUS_KEY & owner conflict; scope & priority don't.
        let anc = vec![ev(1, 0, OpType::Create, "X", &[])];
        let ours = vec![
            anc[0].clone(),
            ev(
                2,
                0,
                OpType::Update,
                "X",
                &[
                    (STATUS_KEY, json!("done")),
                    ("owner", json!("alice")),
                    ("scope", json!("project")),
                ],
            ),
        ];
        let theirs = vec![
            anc[0].clone(),
            ev(
                2,
                0,
                OpType::Update,
                "X",
                &[
                    (STATUS_KEY, json!("open")),
                    ("owner", json!("bob")),
                    ("priority", json!(3)),
                ],
            ),
        ];

        // `theirs` wins the two conflicting fields; the disjoint ones both stay.
        let fields = merge_to_fields(&anc, &ours, &theirs, OnConflict::Theirs, "X");
        assert_eq!(fields[STATUS_KEY], json!("open"), "theirs wins STATUS_KEY");
        assert_eq!(fields["owner"], json!("bob"), "theirs wins owner");
        assert_eq!(
            fields["scope"],
            json!("project"),
            "ours-only field survives"
        );
        assert_eq!(fields["priority"], json!(3), "theirs-only field survives");
    }

    #[test]
    fn latest_resolves_each_field_by_its_own_timestamp() {
        let anc = vec![ev(1, 0, OpType::Create, "X", &[])];
        // ours: STATUS_KEY newer (t=10), owner older (t=1).
        let ours = vec![
            anc[0].clone(),
            ev(2, 10, OpType::Update, "X", &[(STATUS_KEY, json!("ours"))]),
            ev(3, 1, OpType::Update, "X", &[("owner", json!("ours"))]),
        ];
        // theirs: STATUS_KEY older (t=5), owner newer (t=20).
        let theirs = vec![
            anc[0].clone(),
            ev(2, 5, OpType::Update, "X", &[(STATUS_KEY, json!("theirs"))]),
            ev(3, 20, OpType::Update, "X", &[("owner", json!("theirs"))]),
        ];

        let fields = merge_to_fields(&anc, &ours, &theirs, OnConflict::Latest, "X");
        assert_eq!(
            fields[STATUS_KEY],
            json!("ours"),
            "ours' STATUS_KEY is newer"
        );
        assert_eq!(fields["owner"], json!("theirs"), "theirs' owner is newer");
    }

    #[test]
    fn delete_versus_change_follows_strategy() {
        let anc = vec![ev(1, 0, OpType::Create, "X", &[(STATUS_KEY, json!("a"))])];
        let ours = vec![anc[0].clone(), ev(2, 0, OpType::Delete, "X", &[])];
        let theirs = vec![
            anc[0].clone(),
            ev(2, 0, OpType::Update, "X", &[(STATUS_KEY, json!("changed"))]),
        ];

        // ours deleted -> with `ours`, the task is gone.
        let fork = 1;
        let oc: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq > fork).collect();
        let tc: Vec<&MutationEvent> = theirs.iter().filter(|e| e.seq > fork).collect();
        let plan = resolve(&summarize(&oc), &summarize(&tc), Strategy::Ours);
        let merged = assemble(
            &ours.iter().filter(|e| e.seq <= fork).collect::<Vec<_>>(),
            &oc,
            &tc,
            &plan,
            fork,
        );
        let state = Engine::materialize_state(Vec::new(), merged, DONE_STATUS);
        assert!(
            !state.contains_key("X"),
            "ours deleted, so the task is gone"
        );

        // With `theirs`, the change wins and the task survives.
        let fields = merge_to_fields(&anc, &ours, &theirs, OnConflict::Theirs, "X");
        assert_eq!(
            fields[STATUS_KEY],
            json!("changed"),
            "theirs' change is kept"
        );
    }

    #[test]
    fn resolution_event_carries_provenance_but_state_does_not() {
        let anc = [ev(1, 0, OpType::Create, "X", &[])];
        let ours = [
            anc[0].clone(),
            ev(2, 0, OpType::Update, "X", &[(STATUS_KEY, json!("a"))]),
        ];
        let theirs = [
            anc[0].clone(),
            ev(2, 0, OpType::Update, "X", &[(STATUS_KEY, json!("b"))]),
        ];
        let fork = 1;
        let shared: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq <= fork).collect();
        let oc: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq > fork).collect();
        let tc: Vec<&MutationEvent> = theirs.iter().filter(|e| e.seq > fork).collect();
        let plan = resolve(&summarize(&oc), &summarize(&tc), Strategy::Theirs);
        let merged = assemble(&shared, &oc, &tc, &plan, fork);

        // The resolution Update carries `_meta` explaining the pick.
        let res = merged
            .iter()
            .find(|e| e.meta.is_some())
            .expect("a resolution event");
        let meta = res.meta.as_ref().unwrap();
        assert_eq!(meta["strategy"], json!("theirs"));
        assert_eq!(meta["resolved"][0]["field"], json!(STATUS_KEY));
        assert_eq!(meta["resolved"][0]["ours"], json!("a"));
        assert_eq!(meta["resolved"][0]["kept"], json!("theirs"));

        // But replay ignores it: the task has no `_meta` field, just the winner.
        let state = Engine::materialize_state(Vec::new(), merged, DONE_STATUS);
        assert!(
            !state["X"].custom_fields.contains_key("_meta"),
            "provenance stays out of state"
        );
        assert_eq!(state["X"].custom_fields[STATUS_KEY], json!("b"));
    }

    #[test]
    fn disjoint_tasks_produce_no_conflicts() {
        let anc = [ev(1, 0, OpType::Create, "base", &[])];
        let ours = [anc[0].clone(), ev(2, 0, OpType::Create, "a", &[])];
        let theirs = [anc[0].clone(), ev(2, 0, OpType::Create, "b", &[])];
        let oc: Vec<&MutationEvent> = ours.iter().filter(|e| e.seq > 1).collect();
        let tc: Vec<&MutationEvent> = theirs.iter().filter(|e| e.seq > 1).collect();
        let plan = resolve(&summarize(&oc), &summarize(&tc), Strategy::Latest);
        assert!(plan.conflicts.is_empty());
    }
}
