# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`taska` is a local-first, git-native task & dependency tracker. The crate is `taska`; the binary is `ta`. Tasks live in an **append-only event log** (`.taska/mutations.jsonl`); the state you see is *replayed* from that log, never stored as a snapshot. A custom git merge driver reconciles concurrent edits on different branches per-field. See `README.md` for the user-facing model.

## Commands

```bash
cargo build                                          # build
cargo run -- <subcommand> [args]                     # run `ta` from source, e.g. cargo run -- list
cargo test --all --all-features                      # all tests (unit + e2e)
cargo test --lib                                     # unit tests only (in-crate #[cfg(test)] modules)
cargo test --test e2e                                # end-to-end tests only
cargo test <name>                                    # a single test, e.g. cargo test crud_search_and_ready_workflow
cargo clippy --all --all-features -- -D warnings     # lint (CI fails on any warning)
cargo fmt --all                                      # format; CI runs `cargo fmt --all -- --check`
```

CI (`.github/workflows/ci.yml`) runs test + clippy + fmt-check, and a separate coverage job via `cargo tarpaulin`.

**Clippy is strict.** `lib.rs`/`main.rs` enable `clippy::pedantic`, `nursery`, and `cargo`, and deny `unwrap_used`, `panic`, and `dbg_macro` in non-test code — write accordingly. Test modules opt back in with `#![allow(clippy::unwrap_used)]`; `unwrap` is the conventional assertion style there.

## Architecture

The whole program lives in the **library crate** (`src/lib.rs`); the `ta` binary (`src/main.rs`) is a thin wrapper around `cli::run()`. Modules are layered by dependency inversion — lower layers know nothing of higher ones:

- **`model.rs`** — pure data, no I/O. `MutationEvent` (one log record), `OpType` (`Create`/`Update`/`Delete`/`AddDep`/`RemoveDep`), and the materialized `TaskState`. Also `verify_seq_order`.
- **`engine.rs`** — pure replay. `Engine::materialize_report` folds a mutation log over a baseline into the current task map **and** reports *orphaned* events (an `Update`/`AddDep`/`RemoveDep`/`Delete` that applied to no task); `materialize_state` is a thin wrapper that discards the orphans. `retention_split` decides what compaction folds. No storage dependency, so it's trivially testable.
- **`storage.rs`** — the `EventStore` *trait* (what a store can do) and `FileStore` (fd-locked JSONL on disk). Everything above depends on the trait, so tests substitute an in-memory fake (`cli.rs` tests do exactly this).
- **`graph.rs`** — dependency DAG over `petgraph`: cycle detection, topological sort, and `ready_tasks` (not-done tasks whose deps are all done).
- **`merge.rs`** — the git merge drivers (the most intricate module; see below).
- **`config.rs`** — `Config` and the `default_toml()` template `ta init` writes. The rendered template round-trips to `Config::default()` (a test enforces this).
- **`git.rs`** — registers the merge drivers in *local* git config and writes `.gitattributes` lines. Idempotent.
- **`cli.rs`** — clap parsing, command dispatch, and table/JSON rendering. Handlers take `&impl EventStore`, not `FileStore`. Beyond the README's commands it also has `show` (one task, all fields), `undo` (reverse the last N events — see invariants), and `resolve` (which now also prunes orphaned events). The display flag for "every field" is `--full` (not `--all`).
- **`error.rs`** — `DynError = Box<dyn Error>`; this is a print-and-exit CLI with no need for typed errors.

### Core invariants — do not break these

- **`seq` is the authoritative order, not the wall clock.** Each event carries a store-minted, strictly-increasing `seq`; replay, compaction, and merge all key off it. `timestamp` is informational (and used only as a *tiebreaker* by the `latest` merge strategy). The log must stay strictly increasing by `seq` — `verify_seq_order` *surfaces* a violation as corruption rather than silently sorting it.
- **Writes are append-only.** `append_events` never rewrites existing lines (that is what keeps the log git-merge-friendly). `seq` is minted under an `fd_lock` write lock, as `max(seq)+1`, so concurrent writers can't collide. Only `compact` rewrites the log, and it holds the lock across the baseline swap.
- **Compaction never empties the log.** `retention_split` is clamped to always keep the last event, so the watermark `min(seq)-1` stays derivable.
- **`keep_events` has a floor** (`MIN_KEEP_EVENTS = 100`, see `config.rs`): retaining too few events would fold away history a concurrent branch still needs to merge. `Config::validate()` enforces it unconditionally on every store-backed command (there is no override; tests that exercise compaction stay above the floor and instead generate *more* events than `keep_events`).
- **`undo` preserves the append-only invariant.** Undoing events that are still local (uncommitted), or with `--remove`, physically truncates the log; but undoing events already git-committed *appends compensating events* to walk state back rather than rewriting committed history. See `cmd_undo` in `cli.rs`.
- **Reserved keys & null-unset.** `seq`, `timestamp`, `op`, `task_id`, and `_meta` cannot be used as task field names (`RESERVED_FIELD_KEYS` in `cli.rs`). `_meta` holds merge provenance and is deliberately *not* materialized into task state. A field written as JSON `null` is the **unset convention** — replay removes the field rather than storing null, so it never reaches state, output, search, or the baseline.
- **Orphaned events are non-fatal.** An event applying to a non-existent task is counted, never errored; replay continues. Commands warn when orphans are present, and `ta resolve` can prune them (dropping a no-op orphan can't change materialized state).

### The merge model

Merging two diverged logs is a **rebase**, not a CRDT union: keep our events, restack the other branch's concurrent events (those with `seq > fork`, where `fork = max(seq)` in the common ancestor) on top, renumber them into a fresh contiguous tail, and settle genuine contradictions with explicit appended **resolution events** that carry `_meta` provenance. Resolution is **per-field** — only a field/dep/whole-task that *both* branches changed incompatibly is a conflict; everything else merges untouched. **Removals are unioned**: an event present in the ancestor but dropped on a branch (a revert or hand-removal) is removed from the merge result regardless of which side dropped it, so a revert on either branch converges (`removed_seqs` in `merge.rs`). The `[merge] on_conflict` policy picks the winner for genuine conflicts: `surface` (default — writes a tentative ours-merge, flags it, and fails so git marks the path unmerged; reviewed via `ta resolve`), `latest`, `ours`, or `theirs`. The baseline has a separate keep-ours driver.

⚠️ The lowercase `serde` names of `Strategy`, `Side`, `TaskOutcome`, `EdgeOutcome`, and the `_meta`/conflict-marker field names are an **on-disk serialization contract** (search `merge.rs` for "SERIALIZATION CONTRACT"). Renaming a variant without a migration breaks existing logs.

## Testing approach

`tests/e2e.rs` drives the real compiled `ta` binary (path from `CARGO_BIN_EXE_ta`) against throwaway git repos. Each test runs in its own dir under the **system** temp dir, *not* `CARGO_TARGET_TMPDIR` — that is deliberate, so `ta`'s walk-up store discovery can't climb into the repo's own `.taska` store. Merge-driver tests prepend the binary's directory to `PATH` so git's `ta git-merge ...` resolves to the binary under test. Compaction tests stay at or above the `keep_events` floor and simply generate more events than they retain. Prefer adding coverage here over ad-hoc manual scripts.

`tutorials/` holds runnable bash walkthroughs (`NN-*.sh`, driven by `lib.sh`; `run-all.sh` runs them in order) that double as UX validation and learning material. Each spins up its own throwaway repo outside the checkout. Run unattended with `TUTORIAL_NONINTERACTIVE=1`; they need `ta` on `PATH`.

## This repo dogfoods taska

The repo tracks its own work in `.taska/`. **Mutate that store only through the `ta` binary — never hand-edit `mutations.jsonl`/`baseline.jsonl`**, and never `git restore` it out from under in-flight work. Commit `.taska/` changes alongside the code they describe.
