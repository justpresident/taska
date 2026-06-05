# The taska event-log protocol: merge, revert, conflict, compaction

How taska stores tasks and reconciles concurrent edits. For *why* an event log
(vs. a state snapshot), see the [README](../README.md); this doc is the *how*.

## The log

Tasks are not stored as state. They live as an **append-only log of changes** in
`.taska/mutations.jsonl` — one JSON event per line. Each event records an
operation (`create`, `update`, `append`, `delete`, `add-dep`, `remove-dep`) on a task id,
plus a store-minted **`seq`** number. The state you see (`ta list`, `ta show`) is
*replayed* from the log on demand; it is never written back.

```
seq op      task   payload
1   create  api    {"title":"Build API","status":"open"}
2   create  db     {"status":"open"}
3   add-dep api     db                 # api depends on db
4   update  db     {"status":"closed"} # replay → db.status = closed
```

`seq` is the **authoritative order** — replay, compaction, and merge all key off
it; the wall-clock `timestamp` is informational (only a tiebreaker for the
`latest` merge strategy). New seqs are minted under a write lock as `max(seq)+1`,
so concurrent writers never collide.

`seq` is **strictly increasing but not necessarily contiguous**: a `git revert`
that drops committed events leaves gaps, which is normal and supported. Only an
*out-of-order* or *duplicate* seq is corruption — `verify_seq_order` surfaces
that loudly rather than silently re-sorting.

### Orphaned events

An `update`/`delete`/dep event whose target task no longer exists (e.g. its
`create` was reverted or merged away) is an **orphan**. Orphans are never fatal —
replay counts them, every read warns about them, and `ta resolve` prunes them
(dropping a no-op orphan can't change state).

## Merge

Merging two diverged logs is a **rebase, not a CRDT union**. Git invokes the
driver `ta git-merge %O %A %B` (registered in *local* git config + `.gitattributes`
by `ta init`). `%A` is *ours* (the branch merged into), `%B` is *theirs*.

1. **Fork.** `fork = max(seq in the common ancestor %O)`. Everything `≤ fork` is
   shared history; everything `> fork` is a branch's concurrent work.
2. **Keep ours' shared tail.** Take ours' events `≤ fork`.
3. **Restack theirs' concurrent events.** Take theirs' events `> fork` and append
   them, **renumbered** into a fresh contiguous tail starting at `fork+1`.
4. **Resolve contradictions per-field** with explicit appended resolution events
   (see below).

```
ancestor %O:  1 2 3                      fork = 3
ours    %A:   1 2 3   4(status=open)
theirs  %B:   1 2 3   4(owner=alice)
merged:       1 2 3   4(status=open) 5(owner=alice)   # theirs' 4 restacked to 5
```

Because only *concurrent* events are restacked and renumbered, two branches that
each appended converge to the same set regardless of merge direction.

### Conflicts and `on_conflict`

A **conflict** is a single field, dependency edge, or whole-task delete that
*both* branches changed to **incompatible** values. Everything else — disjoint
fields, commuting edits — merges untouched. The `[merge] on_conflict` policy
picks the winner per field:

- **`surface`** (default) — write a deterministic tentative merge (keeping ours),
  record the conflicts in `.taska/merge-conflict.json`, and **fail** so git marks
  the path unmerged. Review with `ta resolve`, then `git add` + commit.
- **`latest`** — keep the value with the newest `timestamp`.
- **`ours`** / **`theirs`** — keep that side's value.

Resolution events carry `_meta` provenance so the decision is visible in history.
(The lowercase serde names of the strategy/side/outcome enums and the `_meta`
field names are an on-disk serialization contract — don't rename without a
migration.)

## Revert

A `git revert` of a commit that *added* events removes those lines, leaving a gap.
taska treats reverts as first-class:

- **Removal-union.** When merging, an ancestor event a branch no longer carries —
  above that branch's compaction watermark — was reverted. The merge honors
  **both** sides' removals (a union), so a reverted event stays gone regardless of
  merge direction. A revert on either branch converges.
- **One-sided-revert warning.** When one branch reverts a shared event the other
  kept, the removal-union still drops it — but that silently discards data the
  other branch had, so the merge **warns** (`rewritten_shared_seqs`). The check
  compares presence only *above both* branches' watermarks, so ordinary
  compaction is never mistaken for a revert.
- **Seq reuse.** A revert frees its seq; the next `ta create` reuses it
  (`max(seq)+1`). If both branches then put *different* events at that seq, the
  merge warns about the content mismatch instead.

## Compaction and the baseline

To keep the log small, `ta compact` folds an old prefix into a snapshot,
`.taska/baseline.jsonl`; replay overlays the remaining log on top.

- **Retention.** Compaction keeps at least the most recent `keep_events` events
  (and everything within `keep_days`). `keep_events` has a floor
  (`MIN_KEEP_EVENTS = 300`; default `5000`): retaining too little would fold away
  history a concurrent branch still needs to reconcile. **Keep enough to cover
  your longest-lived branch.**
- **Never empties.** Compaction always leaves the last event, so the watermark
  `min(seq)−1` stays derivable.
- **Baseline merge.** The baseline has its own keep-ours driver
  (`ta git-merge-baseline`): two branches that compacted to different depths each
  keep their own baseline, and the (separately reconciled) log rebuilds the state.
  It warns if a task diverged in a way that suggests compaction folded past a
  fork.

## Performance

Replay rebuilds the whole dependency graph in a **single pass** over the log — no
load-then-resolve step, and friendlier to cache than a two-pass walk over a
materialized graph. Compaction bounds on-disk growth as history accumulates.

The numbers below come from `cargo bench --bench perf` (a dependency-free
`std::time` harness; release build, single core) over synthetic logs. Each log
is ~¼ `Create`s; the rest are `Update`s and random `AddDep`s, with the dep share
varied to probe how a denser graph behaves (the `create / update / dep` column
gives the actual mix). Treat them as orders of magnitude, not guarantees.

**Replay / materialize** scales linearly with log length, and a denser
dependency graph costs a little more — the per-task dedup on `AddDep` grows with
edges/task. The log is ~97 bytes/event on disk.

| events  | create / update / dep | log size | replay   |
|---------|-----------------------|----------|----------|
| 1,000   | 25% / 72% / 3%        | 94 KB    | 0.6 ms   |
| 1,000   | 25% / 34% / 41%       | 93 KB    | 0.7 ms   |
| 100,000 | 25% / 71% / 4%        | 9.6 MB   | 108.1 ms |
| 100,000 | 25% / 37% / 38%       | 9.6 MB   | 126.0 ms |
| 200,000 | 25% / 60% / 15%       | 19.3 MB  | 291.8 ms |
| 500,000 | 25% / 71% / 4%        | 48.6 MB  | 719.0 ms |
| 500,000 | 25% / 37% / 38%       | 49.0 MB  | 828.2 ms |

So even a 100k-event history — far past any hand-managed backlog — replays in
about a tenth of a second, and half a million events in under a second.

**Compaction** keeps that hot path cheap as history grows. Every command
re-materializes the current state from the baseline plus only the retained tail
(`keep_events`), never the whole log. Same logical state, two storage shapes:

| store                                  | on disk | replay   |
|----------------------------------------|---------|----------|
| 500,000-event log, uncompacted         | 48.7 MB | 787.8 ms |
| 125,000-task baseline + 5,000-event tail | 21.8 MB | 226.5 ms |

— a ~3.5× faster replay and ~2.2× smaller footprint. The win grows with how many
events accumulate per task: this synthetic log only averages ~4 events/task, so
the baseline still holds 125k task records; a real backlog of hundreds of tasks,
each churning through many updates, folds away far more of its history.

**Merge** of two branches diverged from a shared ancestor — 1,000 ancestor
events plus 100 concurrent, conflicting `owner` edits per branch (100 genuine
per-field conflicts to resolve) — completes in ~15 ms end to end, including
reading the three logs and writing the result.

## Known limitation: reverting very old changes

When you merge a branch that reverts task updates older than your retention window
(`keep_events` / `keep_days`), the merge can't tell the revert from
already-archived history, so that one change may reappear or be dropped (depending
on merge direction).

Worst case is that single change — a field, dependency, or task: no log
corruption, it's visible in `ta show` / history, and a forward edit fixes it. With
reasonable `keep_events` / `keep_days` values in your config, this is guaranteed
not to happen.

## At a glance

| Situation | What happens |
|-----------|--------------|
| Disjoint concurrent edits | Merge cleanly; theirs' events restacked above `fork` |
| Same field, both changed | Per-field conflict → `on_conflict` policy |
| Concurrent appends to a field (`ta update <id> field+=…`) | Accumulate in `seq` order — they commute, never a conflict |
| Revert above the watermark | Removal-union drops it convergently; merge warns if one-sided |
| Revert of an already-archived change | Rare — only when you merge a branch older than your `keep_events` window that still holds a change you reverted and archived. Worst case: that one change (a dep, field, or task) reappears or is dropped — no corruption |
| Two branches compacted | Keep-ours baseline + reconciled log rebuild full state |
| Event targets a missing task | Orphan — warned on read, pruned by `ta resolve` |
