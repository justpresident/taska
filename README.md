![CI](https://github.com/justpresident/rcypher/actions/workflows/ci.yml/badge.svg)
![Coverage](https://raw.githubusercontent.com/justpresident/taska/main/.github/badges/coverage.svg)
# taska

[![Crates.io](https://img.shields.io/crates/v/taska.svg)](https://crates.io/crates/taska)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A local-first, **git-native** task & dependency tracker for human and agent workflows. Tasks live in an append-only event log inside your repository, and concurrent edits on different branches are reconciled **automatically** by a custom git merge driver — no database, no daemon, no manual sync step.

The binary is `ta`; the crate is `taska`.

```console
$ ta create migrate-db title="Run DB migration" status=open   # ids and fields are yours
$ ta create deploy-api title="Deploy the API" status=open
$ ta block deploy-api migrate-db        # deploy-api depends on migrate-db
$ ta ready                              # only what's actionable, as a configurable table
ID          TITLE             STATUS  DEPS
migrate-db  Run DB migration  open
```

## Why a log, not a snapshot

Most task trackers store the **current state** of each task — a row in a database, a line in a YAML file — and *overwrite* it on every change. taska stores the opposite: an **append-only log of every change** (`create`, `update`, `delete`, …) in `.taska/mutations.jsonl`. The state you see is *replayed* from that log on demand; it is never written down.

That single choice is the whole point, because it is what makes git work *for* you instead of against you:

- **Branches actually merge.** Two people (or two agents) on separate branches each *append* their events, so merging is just unioning two lists — which taska's git merge driver does cleanly and **per-field**. Two overwritten *snapshots*, by contrast, can only collide. The log is the reason concurrent edits reconcile instead of clobbering each other: no database to keep in sync, no manual sync step, no tasks silently dropped or "resurrected" after deletion.
- **Full history, for free.** Every change is in the log, so you can see exactly how a task reached its current state — and a delete is just another event, so it stays deleted.
- **Schema-agnostic.** A task is an id plus arbitrary `key=value` fields; taska defines no fixed schema, so you grow your own conventions.
- **Non-intrusive.** No git hooks, no edits to your `AGENTS.md`/`CLAUDE.md`, no forced remote. It works entirely offline — prototype locally, review an agent's branch before merging, push when *you* decide.

## Install

```console
$ cargo install taska        # installs the `ta` binary
```

Or from source:

```console
$ git clone https://github.com/justpresident/taska
$ cargo install --path taska
```

## Quick start

Run `ta init` once per clone (inside a git repository) to create the `.taska/` store and register the merge driver in your **local** git config.

In the session below, only the lowercase verbs (`create`, `block`, `ready`, …) are literal taska syntax. Everything else is yours — task ids like `migrate-db` and fields like `status=open priority=3` are arbitrary, and taska defines none of them:

```console
$ git init && ta init

# Create two tasks. The ids (migrate-db) and fields (title=…, status=…) are all yours.
$ ta create migrate-db title="Run DB migration" status=open
$ ta create deploy-api title="Deploy the API" status=open

# deploy-api shouldn't start until migrate-db is finished:
$ ta block deploy-api migrate-db

# Default output is an aligned table of configurable columns:
$ ta list
ID          TITLE             STATUS  DEPS
deploy-api  Deploy the API    open    migrate-db
migrate-db  Run DB migration  open

# `ready` shows only not-done tasks whose dependencies are all done:
$ ta ready
ID          TITLE             STATUS  DEPS
migrate-db  Run DB migration  open

# Close the migration, and deploy-api unblocks:
$ ta update migrate-db status=closed
$ ta ready
ID          TITLE           STATUS  DEPS
deploy-api  Deploy the API  open    migrate-db

# For agents (or jq), --format json emits the same fields as a JSON array:
$ ta list --format json
[
  {"id":"deploy-api","title":"Deploy the API","status":"open","deps":["migrate-db"]},
  {"id":"migrate-db","title":"Run DB migration","status":"closed","deps":[]}
]
```

Commit `.taska/` and `.gitattributes` along with your code — they travel with the repo.

## How it works

Every command appends an immutable event (`Create`, `Update`, `Delete`, `AddDep`, `RemoveDep`) to `.taska/mutations.jsonl`. The current state of every task is **materialized** by replaying that log in order; nothing is mutated in place.

Each event carries a store-minted, strictly increasing `seq`. That sequence — not the wall clock — is the authoritative order, which keeps replay deterministic even after branches with interleaved timestamps are merged.

**Dependencies** form a DAG. taska validates against cycles and `ta ready` returns the not-yet-done tasks whose dependencies are all satisfied, in topological order.

**Compaction** (`ta compact`) folds old events into a `baseline.jsonl` snapshot to keep the log small, while retaining recent history so concurrent branches can still be reconciled (see configuration below).

## Collaboration & merging

Because the log is plain git-tracked JSONL, two people (or two agent branches) can edit tasks independently. When their branches merge, git invokes taska's merge driver, which:

1. Replays each branch's events since the fork.
2. Lets **non-overlapping** changes through untouched — different tasks, different fields, even different fields of the *same* task all merge cleanly.
3. Resolves a genuine conflict — both branches setting the *same* field to different values, a delete racing an edit, or an add/remove of the same dependency — **per field**, according to your `on_conflict` policy.

Each resolution is written as an explicit event carrying `_meta` provenance (the strategy used and the candidate values), so the merge decision is auditable in the log — and invisible to task state.

```toml
[merge]
# surface — stop and let a human resolve it (`ta resolve`)
# latest  — keep the most recently written value (by timestamp)
# ours    — keep the branch being merged INTO
# theirs  — keep the branch being merged IN
on_conflict = "surface"
```

With `on_conflict = "surface"` (the default), a real conflict pauses the merge and writes a marker; review it with `ta resolve`, then `git add` and commit.

> The merge driver is registered in **local** git config (per-clone, never committed), so every fresh clone must run `ta init` once to wire it up. The matching `.gitattributes` entry *is* committed and travels with the repo.

## Configuration

`ta init` writes a documented `.taska/config.toml`. Every key falls back to the default shown, so a partial file is fine.

```toml
[compaction]
# Keep at least this many of the most recent events (minimum 100); also the
# minimum log size before `ta compact` does anything.
keep_events = 1000
# Also keep every event from at least this many days back (0 disables).
keep_days = 30

[workflow]
# The field that records status, and the value that means "done".
# `ta ready` treats a dependency as satisfied once it reaches done_status.
status_field = "status"
done_status = "closed"

[merge]
on_conflict = "surface"

[display]
# Columns for list/search/ready (and the field order used by --format json).
# "id" and "deps" are built-ins; any other name is a task field. Override per
# command with --columns id,status or --full.
columns = ["id", "title", "status", "deps"]
# Truncate long human cell values to this many characters (0 = no limit).
max_width = 40
```

## Commands

| Command | Description |
|---|---|
| `ta init` | Create the store and register the git merge drivers (idempotent; run once per clone) |
| `ta create <id> [field=value ...]` | Create a task with arbitrary fields |
| `ta update <id> [field=value ...]` | Set fields on an existing task |
| `ta block <task> <depends_on>` | Add a dependency edge |
| `ta unblock <task> <depends_on>` | Remove a dependency edge |
| `ta delete <id>` | Delete a task |
| `ta list` | List all tasks |
| `ta search <key> <value>` | List tasks whose field equals a value |
| `ta show <id>` | Show a single task with all of its fields |
| `ta ready` | Not-done tasks whose dependencies are all done |
| `ta undo [--count N] [--remove] [--force]` | Reverse the last N events: truncate uncommitted ones, append compensating events for committed ones (`--remove` to force truncation) |
| `ta compact` | Fold old events into the baseline snapshot |
| `ta resolve` | Review and clear a surfaced merge conflict |

Field values are parsed as JSON when possible (`priority=3` is a number, `status=open` a string). The keys `seq`, `timestamp`, `op`, `task_id`, and `_meta` are reserved.

`list`, `search`, and `ready` share display flags: `--format human|json` (json is a parseable array, ideal for agents and `jq`), `--full` to show every field, and `--columns id,status,…` to pick the columns for one run. The defaults and `max_width` live in `[display]`.

## Storage layout

```
.taska/
  mutations.jsonl   # the append-only event log
  baseline.jsonl    # compacted snapshot (state of events folded so far)
  config.toml       # configuration
  .gitignore        # ignores the transient merge-conflict marker
.gitattributes      # registers the merge driver for the log files
```

## Status

taska is early (`0.x`) and the on-disk format may still evolve before `1.0`. The event log and merge model are the stable core; planned work includes optional schema validation, task archiving, richer queries, and built-in grooming prompts for agents.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work shall be licensed as above, without any additional terms or conditions.
