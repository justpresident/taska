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
cargo clippy --all --all-features --all-targets -- -D warnings   # lint (CI fails on any warning; --all-targets also lints #[cfg(test)] code)
cargo fmt --all                                      # format; CI runs `cargo fmt --all -- --check`
```

CI (`.github/workflows/ci.yml`) runs test + clippy + fmt-check, and a separate coverage job via `cargo tarpaulin`.

**The pre-commit gate.** CI fails on any clippy warning or fmt diff, so before each commit run clippy (with `--all-targets`, as above), `cargo fmt --all`, and `cargo test --all --all-features`. `--all-targets` is the easy-to-miss part: a plain `cargo build`/`clippy` skips `#[cfg(test)]` code, so a test broken by a model/signature change stays hidden until you lint with it. Build commit messages with `git commit -F` (a heredoc-in-`$()` mangles newlines); one task per commit, and keep the suite green at every commit.

**Clippy is strict.** `lib.rs`/`main.rs` enable `clippy::pedantic`, `nursery`, and `cargo`, and deny `unwrap_used`, `panic`, and `dbg_macro` in non-test code — write accordingly. Test modules opt back in with `#![allow(clippy::unwrap_used)]`; `unwrap` is the conventional assertion style there.

**`pub` vs `pub(crate)`.** `clippy::redundant_pub_crate` is denied and shapes every new file: inside a **private** module (`cli/commands/*`, `test_support`) use plain `pub` — the private parent already caps visibility to the crate, so `pub(crate)` is rejected as redundant; inside a **public** module (`cli`, `format`) use `pub(crate)` for cross-module-but-internal items.

## Architecture

The whole program lives in the **library crate** (`src/lib.rs`); the `ta` binary (`src/main.rs`) is a thin wrapper around `cli::run()`. Modules are layered by dependency inversion — lower layers know nothing of higher ones:

- **`model.rs`** — pure data, no I/O. `MutationEvent` (one log record), `OpType` (`Create`/`Update`/`Append`/`Delete`/`AddDep`/`RemoveDep`; `Append` accumulates text per-field and commutes on merge), and the materialized `TaskState` (which also carries the computed `create_time`/`update_time`/`close_time`). Also `verify_seq_order` and `is_done` (the shared "status equals done_status" predicate, used by engine/graph/status).
- **`engine.rs`** — pure replay. `Engine::materialize_report` folds a mutation log over a baseline into the current task map **and** reports *orphaned* events (an `Update`/`Append`/`AddDep`/`RemoveDep`/`Delete` that applied to no task); `materialize_state` is a thin wrapper that discards the orphans. `retention_split` decides what compaction folds. No storage dependency, so it's trivially testable.
- **`storage.rs`** — the `EventStore` *trait* (what a store can do, including `config()`) and `FileStore` (fd-locked JSONL on disk). Everything above depends on the trait, so tests substitute an in-memory fake (`src/test_support.rs`'s `InMemoryStore`, shared by the command tests).
- **`graph.rs`** — dependency DAG over `petgraph`: cycle detection, topological sort, and `ready_tasks` (not-done tasks whose deps are all done). The graph is built from **blocker edges** (`graph::blocker_edges`): the `depends_on` field plus any relationship whose type is `blocker` (per `RelationshipConfig::blocker_types`). Informational (`info`) relationships never gate readiness/cycles/`dep tree`. Callers pass the blocker-name set in, so `graph.rs` stays free of `config`.
- **`merge.rs`** — the git merge drivers (the most intricate module; see below).
- **`config.rs`** — `Config` and the `default_toml()` template `ta init` writes. The rendered template round-trips to `Config::default()` (a test enforces this).
- **`git.rs`** — registers the merge drivers in *local* git config and writes `.gitattributes` lines. Idempotent.
- **`format.rs`** — presentation: the `DisplayArgs`/`OutputFormat` flags, column resolution (`full_columns`, canonical order), `--sort` ordering, per-column truncation, and the `human`/`json`/`jsonl` renderers. `cell_value` is the single source of truth for "this column's value", shared by all three. JSON/JSONL omit absent fields rather than emitting `null`.
- **`cli/`** — the CLI. `cli/mod.rs` is clap parsing + `run()`/dispatch plus the cross-cutting helpers handlers share (`state_of`/`replay`, `parse_field_ops`, `confirm`); each subcommand handler is its own file under `cli/commands/`. Handlers take `&impl EventStore`, not `FileStore`. Beyond the README's commands there is `show` (one task; human output is a vertical `field: value` record via `format::render_record`, untruncated — `--format json`/`jsonl` go the normal route), `status`, `config get/set/list/validate` (`validate` and `set` run `Config::validate_against(state)` — the cheap struct-only `Config::validate()` still runs on every other store command so a graph problem can't lock you out of the commands that fix it), `undo` (reverse the last N events — see invariants), the `dep` group (`add`/`remove`/`tree`/`cycles`/`plan` — typed relationship edges validated against `[relationships]`, via `cmd_dep_group`/`DepAction`; `dep` replaced the old top-level `block`/`unblock`; a task's relationships are surfaced by `show`, not a separate `dep list`), and `resolve` (which also prunes orphaned events). The display flag for "every field" is `--full` (not `--all`).
- **`error.rs`** — `DynError = Box<dyn Error>`; this is a print-and-exit CLI with no need for typed errors.

### Core invariants — do not break these

- **`seq` is the authoritative order, not the wall clock.** Each event carries a store-minted, strictly-increasing `seq`; replay, compaction, and merge all key off it. `timestamp` is informational (and used only as a *tiebreaker* by the `latest` merge strategy). The log must stay strictly increasing by `seq` — `verify_seq_order` *surfaces* a violation as corruption rather than silently sorting it.
- **`seq` is strictly increasing but need not be contiguous — gaps are allowed by design.** A `git revert` of a commit that added events removes those lines, leaving holes in the sequence; that is normal, not corruption (`verify_seq_order` rejects only out-of-order or duplicate seqs). Every seq computation is extremum-/position-based — `max(seq)+1` minting, `fork = max(ancestor seq)`, the `seq ≷ fork` filters, the renumber-to-contiguous tail in `assemble`, and the `min(seq)-1` watermark — so gaps reconcile correctly and the common revert (the reverted event still present in both logs) converges either merge direction. **Known limitation:** the merge's removal-detection (`removed_seqs`) only inspects ancestor events *above* a branch's `min(seq)-1` watermark, so a revert of a branch's *earliest* events — or one that compaction later folds past — is invisible and can resurrect the event or diverge by merge direction. The merge *warns* about the rewrites it **can** see — `rewritten_shared_seqs` flags a shared event present on one branch but reverted on the other, comparing only the region *above both* branches' watermarks (so ordinary compaction never trips it). The residual blind spot is a revert *below* the higher watermark (a branch's earliest events, or one compacted-past), which stays undetectable from the logs alone; documented in `protocol-doc`.
- **Writes are append-only.** `append_events` never rewrites existing lines (that is what keeps the log git-merge-friendly). `seq` is minted under an `fd_lock` write lock, as `max(seq)+1`, so concurrent writers can't collide. Only `compact` rewrites the log, and it holds the lock across the baseline swap.
- **Compaction never empties the log.** `retention_split` is clamped to always keep the last event, so the watermark `min(seq)-1` stays derivable.
- **`keep_events` has a floor** (`MIN_KEEP_EVENTS = 300`, see `config.rs`): retaining too few events would fold away history a concurrent branch still needs to merge. `Config::validate()` enforces it unconditionally on every store-backed command (there is no override; tests that exercise compaction stay above the floor and instead generate *more* events than `keep_events`).
- **`undo` preserves the append-only invariant.** Undoing events that are still local (uncommitted), or with `--remove`, physically truncates the log; but undoing events already git-committed *appends compensating events* to walk state back rather than rewriting committed history. See `cmd_undo` in `cli/commands/undo.rs`.
- **Reserved keys & null-unset.** `seq`, `timestamp`, `op`, `task_id`, and `_meta` cannot be used as task field names (`RESERVED_FIELD_KEYS` in `cli/mod.rs`). `_meta` holds merge provenance and is deliberately *not* materialized into task state. A field written as JSON `null` is the **unset convention** — replay removes the field rather than storing null, so it never reaches state, output, list filtering, or the baseline.
- **Field-value input (`parse_field_ops`/`field_value`).** `key=value` sets a field, `key+=value` appends to it — `parse_field_ops` splits tokens into a *set* map (emitted as `Update`) and an *append* map (emitted as `Append`), so one `update` can do both (two events). Values coerce JSON-then-string; `key=@PATH` reads the value from a file and `key=@-` from stdin (verbatim string, one trailing newline trimmed — the agent-friendly way to pass long/multiline notes without argv quoting); `key=@@x` is the literal `@x`.
- **Orphaned events are non-fatal.** An event applying to a non-existent task is counted, never errored; replay continues. Commands warn when orphans are present, and `ta resolve` can prune them (dropping a no-op orphan can't change materialized state).

### The merge model

Merging two diverged logs is a **rebase**, not a CRDT union: keep our events, restack the other branch's concurrent events (those with `seq > fork`, where `fork = max(seq)` in the common ancestor) on top, renumber them into a fresh contiguous tail, and settle genuine contradictions with explicit appended **resolution events** that carry `_meta` provenance. Resolution is **per-field** — only a field/dep/whole-task that *both* branches changed incompatibly is a conflict; everything else merges untouched. **Removals are unioned**: an event present in the ancestor but dropped on a branch (a revert or hand-removal) is removed from the merge result regardless of which side dropped it, so a revert on either branch converges (`removed_seqs` in `merge.rs`). The `[merge] on_conflict` policy picks the winner for genuine conflicts: `surface` (default — writes a tentative ours-merge, flags it, and fails so git marks the path unmerged; reviewed via `ta resolve`), `latest`, `ours`, or `theirs`. The baseline has a separate keep-ours driver.

⚠️ The lowercase `serde` names of `Strategy`, `Side`, `TaskOutcome`, `EdgeOutcome`, and the `_meta`/conflict-marker field names are an **on-disk serialization contract** (search `merge.rs` for "SERIALIZATION CONTRACT"). Renaming a variant without a migration breaks existing logs.

### Computed timestamps work by read-time injection

`create_time`/`update_time`/`close_time` are computed onto `TaskState` during replay (so they survive compaction via the baseline), but they're surfaced by **injecting them as ordinary RFC 3339 string fields in `state_of`**, under their configurable `[timestamps]` names. That read-time injection — not any per-feature plumbing — is why they "just work" as columns and in list filtering/`--sort`/`show`. Two consequences:

- `close_time` is the *most recent* close and is **cleared on reopen** (a deliberate product choice, not a bug).
- A test asserting an **exact** column/field set must disable timestamps for that store (`test_support::store_without_timestamps()`, or `[timestamps]` names set to `""`), or the injected times leak in.

The same injection pattern powers the computed graph columns `unblocks`/`blocked_by` (transitive not-done dependents / prerequisites over the blocker edges, from `graph::reachability_counts`), but **conditionally**: `cli::inject_reachability_columns` runs only in `list` and only when `format::referenced_columns` shows the display actually names one of them (a column or the `--sort` key). That keeps default/`--full`/json output unchanged, so — unlike timestamps — they don't leak into exact-field-set tests.

### Adding a config option: backfill the local config

`ta init` never overwrites an existing `config.toml`, so a new option is invisible in the dogfood store unless you **also add it (with its default + comment) to the repo's `.taska/config.toml`**, not just `default_toml()`. (TOML ordering bites too: scalar keys must precede any sub-table within a section.) Everything else is automatic — `Config` is `#[serde(default)]`, so partial/old files load and `ta config get/set/list` reflects the struct.

### Before adding a CLI command: prefer a flag, reuse what exists

A new subcommand is the last resort, not the first. Before adding one, work through this in order and record the reasoning (in the task notes / PR):

1. **Find the neighbours.** List the existing commands that do something similar (`ta list`, `ta show`, the `dep` group, …). `search` folded into `list --open`, `ready` into `list --ready`; `block`/`unblock` folded into `dep`. A new capability usually belongs *next to* one of these.
2. **Default to a flag on an existing command.** If the new behavior is "the same view/data, filtered or ordered differently," it's a flag, not a verb — e.g. `list --ready` over a separate `ready`, `tree --plan`/`--all` over a new command. Only add a verb when the *output shape* or *primary noun* genuinely differs (e.g. `dep plan`'s flat dedup'd order vs `dep tree`'s nested structure — a confirmed split, see git log).
3. **If a flag, name it from the existing vocabulary** and make it compose with the flags already there (`--open`, `--full`, `--columns`, `--sort`, `--reverse`, `--format`). Don't invent a second way to express something a flag already covers.
4. **Generalize, don't fork.** Reuse the shared implementations rather than copy-pasting: `format::cell_value` for any column's value, `parse_field_ops` for `key=value`/`key+=value`, `graph::blocker_edges`/`validate_and_sort_dependencies` for traversal, `state_of` for the materialized map, `confirm` for prompts. If two surfaces need the same logic, lift it into the shared helper (`format`/`graph`/`cli`) and call it from both — extend the primitive, don't grow a parallel one.

**Report before you build — mandatory.** Do not start implementing a command or a command-flag until the approach is confirmed. Surface the analysis first — the similar commands you found, whether it should be a flag or a verb, which flags make sense, and which shared primitives you'll reuse/generalize — propose the option(s), and get explicit confirmation. Implementation comes only after the user signs off.

The bar for a brand-new verb is: it can't be expressed as a flag, it doesn't duplicate an existing surface, and its core logic is built from the shared primitives above.

## Testing approach

`tests/e2e.rs` drives the real compiled `ta` binary (path from `CARGO_BIN_EXE_ta`) against throwaway git repos. Each test runs in its own dir under the **system** temp dir, *not* `CARGO_TARGET_TMPDIR` — that is deliberate, so `ta`'s walk-up store discovery can't climb into the repo's own `.taska` store. Merge-driver tests prepend the binary's directory to `PATH` so git's `ta git-merge ...` resolves to the binary under test. Compaction tests stay at or above the `keep_events` floor and simply generate more events than they retain. Prefer adding coverage here over ad-hoc manual scripts.

`tutorials/` holds runnable bash walkthroughs (`NN-*.sh`, driven by `lib.sh`; `run-all.sh` runs them in order) that double as UX validation and learning material. Each spins up its own throwaway repo outside the checkout. Run unattended with `TUTORIAL_NONINTERACTIVE=1`; they need `ta` on `PATH`.

Two gotchas when writing tests:

- **Default `--sort` is `create_time`** (oldest-first), not `id` — a test asserting row *order* must account for it or pass `--sort id`.
- When a test result contradicts how something was described, **trust the test and investigate** — it's often surfacing a real decision (e.g. an unset field's *name* reappearing as `null` was a genuine product question, not a flaky test).

## Release process / Publishing a new version of a crate
Described in docs/crate_release_process.md - use it only when user asks to release a new version.

## This repo dogfoods taska

The repo tracks its own work in `.taska/`, so:

- **Mutate that store only through the `ta` binary** (`ta create`, `ta update <id> status=closed`, …) — never hand-edit `mutations.jsonl`/`baseline.jsonl`, and never `git restore` it out from under in-flight work; either corrupts the append-only log.
- **Commit the eventlog change with the code it describes** — closing a task (`status=closed`; `done_status` is `closed`) goes in the same commit as the feature, and a pending eventlog change is flushed before the next task starts.
- `config.toml` is plain config, not the eventlog — edit it directly (and you must when adding a config option; see above).
- The roadmap lives in the store: `ta list` / `ta show <id>` carry the open tasks and the design questions in their notes.
- **File a roadmap task** with `ta create <kebab-id> title="…" notes="…"` — one task per feature; put the design questions in `notes` (read untruncated with `ta show <id> --full`). Don't pass `status=`: `ta create` stamps the `[workflow] default_status` (`todo`) automatically.
