# taska

![CI](https://github.com/justpresident/taska/actions/workflows/ci.yml/badge.svg?branch=master)
![Coverage](https://raw.githubusercontent.com/justpresident/taska/master/.github/badges/coverage.svg)
[![Crates.io](https://img.shields.io/crates/v/taska.svg)](https://crates.io/crates/taska)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A local-first, **git-native** task & dependency tracker for human and agent workflows. Tasks live in an append-only event log inside your repository, and concurrent edits on different branches are reconciled **automatically** by a custom git merge driver — no database, no daemon, no manual sync step.

## What it does better than other in-repo task managers

### Everything is configurable!

No assumptions about your workflow. Only a bare minimum of fields is fixed — `id` and `deps`. Everything else is arbitrary `key=value` fields that you define yourself. Even the `status` field, which carries a lot of meaning for dependency tracking, can be renamed in the config. You can also configure the value that signals a task is finished — name it `closed`, `done`, or whatever you're used to; taska defines no fixed schema, so you grow your own conventions. That also keeps the filtering and manipulation commands simple and flexible: no dozens of custom flags pinned to a reserved field name.

And when you do want guarantees, declare per-task-type schemas in `[task_types]`: typed fields (`uint`, `enum`, `datetime`, `array<T>`, `set<T>`, …), required fields, and closed types — enforced on every write, with every violation reported in one error. Tasks stay schema-agnostic until you declare a type.

### An append-only log, not a snapshot

The append-only log is inspired by how high-performance KV stores like Cassandra absorb concurrent updates from tens of thousands of clients. Raw speed isn't the goal for a task tracker, but the same design earns its keep here for a different reason. Most trackers store the **current state** of each task — a row in a database, a line in a YAML file — and *overwrite* it on every change; taska stores the opposite: an **append-only log of every change** (`create`, `update`, `delete`, …) in `.taska/mutations.jsonl`. The state you see is *replayed* from that log on demand; it is never written down.

That single choice is the whole point, because it is what makes git work *for* you instead of against you:

- **One-pass reconstruction.** The whole dependency graph is rebuilt in a single sweep of the event log — no separate load-then-resolve step, and friendlier to cache than a two-pass walk over a full graph.
- **Branches actually merge.** Two people (or two agents) on separate branches each *append* their events, so merging is just unioning two lists — which taska's git merge driver does cleanly and **per-field**. Two overwritten *snapshots*, by contrast, can only collide. The log is the reason concurrent edits reconcile instead of clobbering each other: no database to keep in sync, no manual sync step, no tasks silently dropped or "resurrected" after deletion.
- **Full history, for free.** Every change is in the log, so you can see exactly how a task reached its current state — and a delete is just another event, so it stays deleted.

When the event log eventually grows large enough that replaying it gets slow, you can compact it — the same move Cassandra makes with its SSTables. Compaction folds the old prefix of the log into `baseline.jsonl`, a snapshot of the dependency graph in its final state. That file never produces merge conflicts, because it is built only from old, settled events. The smaller `mutations.jsonl` holds the recent events and is the one the merge driver reconciles.

See [docs/MERGE.md](docs/MERGE.md) for the detailed design: the event log and `seq` model, the merge/rebase algorithm, revert handling, per-field conflict resolution, and compaction.

TODO: compaction will support not only jsonl, but also more compact and fast binary indexes for when your task counts are measured in millions.

### Non-intrusive

No git hooks, no agent session hooks, no mandatory `AGENTS.md`/`CLAUDE.md` edits, and no remote required. It works entirely offline — prototype locally, review an agent's branch before merging, and push when *you* decide.

The default setup installs a custom merge driver — via `.gitattributes` — for **only** the two files taska manages: `.taska/mutations.jsonl` (the event log) and `.taska/baseline.jsonl` (the checkpoint). Unlike a git hook, it runs only for those files; taska never touches anything else during your merge conflict resolutions.


## Install

The quick way — a prebuilt `ta` for Linux/macOS, no Rust toolchain needed:

```console
$ curl -fsSL https://raw.githubusercontent.com/justpresident/taska/master/scripts/install.sh | bash
```

It picks the right binary for your OS/arch from the [latest release](https://github.com/justpresident/taska/releases/latest), verifies its checksum, and installs to `/usr/local/bin` (or `~/.local/bin` if that isn't writable). Override the location with `TASKA_INSTALL_DIR=…` or pin a tag with `TASKA_VERSION=v0.5.0`; on an unsupported platform it falls back to `cargo install`. Prefer to read before you pipe? It's [`scripts/install.sh`](scripts/install.sh).

Or, with Rust installed, from crates.io:

```console
$ cargo install taska        # installs the `ta` binary
```

Or grab a prebuilt binary — no Rust toolchain needed — from the [latest
release](https://github.com/justpresident/taska/releases/latest). Each release
ships a static Linux binary (runs on any distro) and both macOS variants:

```console
# macOS — pick your arch: aarch64 (Apple Silicon) or x86_64 (Intel)
$ TARGET=aarch64-apple-darwin   # Intel Macs: x86_64-apple-darwin
$ VERSION=v0.5.0                # see the releases page for the latest
$ curl -sSfL "https://github.com/justpresident/taska/releases/download/$VERSION/ta-$VERSION-$TARGET.tar.gz" | tar xz
$ sudo mv "ta-$VERSION-$TARGET/ta" /usr/local/bin/

# Linux (static, any distro)
$ TARGET=x86_64-unknown-linux-musl
$ curl -sSfL "https://github.com/justpresident/taska/releases/download/$VERSION/ta-$VERSION-$TARGET.tar.gz" | tar xz
$ sudo mv "ta-$VERSION-$TARGET/ta" /usr/local/bin/
```

Each archive has a sibling `.sha256` you can verify with `shasum -a 256 -c`. On
macOS, if Gatekeeper quarantines the unsigned binary, clear it with
`xattr -d com.apple.quarantine /usr/local/bin/ta`.

Or build from source:

```console
$ git clone https://github.com/justpresident/taska
$ cargo install --path taska
```

## Quick start

Run `ta init` once per clone (inside a git repository) to create the `.taska/` store and register the merge driver in your **local** git config. Run from a subdirectory, it creates the store at the repo root (an existing store anywhere up the tree is reused).

In the session below, only the lowercase verbs (`create`, `dep`, `list`, …) are literal taska syntax. Everything else is yours — task ids like `migrate-db` and fields like `status=open priority=3` are arbitrary, and taska defines none of them:

```console
$ git init && ta init

# Create two tasks. The ids (migrate-db) and fields (title=…, status=…) are all yours.
$ ta create migrate-db title="Run DB migration" status=open
$ ta create deploy-api title="Deploy the API" status=open

# deploy-api shouldn't start until migrate-db is finished:
$ ta dep add deploy-api depends_on=migrate-db

# Default output is an aligned table of configurable columns:
$ ta list
ID          TITLE             STATUS  DEPS
deploy-api  Deploy the API    open    depends_on: migrate-db
migrate-db  Run DB migration  open

# `list --ready` shows only not-done tasks whose dependencies are all done:
$ ta list --ready
ID          TITLE             STATUS  DEPS
migrate-db  Run DB migration  open

# Close the migration, and deploy-api unblocks:
$ ta update migrate-db status=closed
$ ta list --ready
ID          TITLE           STATUS  DEPS
deploy-api  Deploy the API  open    depends_on: migrate-db

# For agents (or jq), --format json emits the same fields as a JSON array:
$ ta list --format json
[
  {"id":"deploy-api","title":"Deploy the API","status":"open","deps":{"depends_on":["migrate-db"]}},
  {"id":"migrate-db","title":"Run DB migration","status":"closed","deps":{}}
]

# --format jsonl is NDJSON — one object per line, ideal for streaming/grep:
$ ta list --format jsonl
{"id":"deploy-api","title":"Deploy the API","status":"open","deps":{"depends_on":["migrate-db"]}}
{"id":"migrate-db","title":"Run DB migration","status":"closed","deps":{}}
```

Commit `.taska/` and `.gitattributes` along with your code — they travel with the repo. The merge-driver *definitions*, however, live in per-clone local git config, so run `ta init` once in every fresh clone (and after a late `git init`); until you do, every `ta` command warns on stderr that the clone's merge protection is incomplete.

## How it works

Every command appends an immutable event (`Create`, `Update`, `Append`, `Add`, `Remove`, `Delete`, `AddEdge`, `RemoveEdge`) to `.taska/mutations.jsonl`. The current state of every task is **materialized** by replaying that log in order; nothing is mutated in place. On the keyboard the field operators are just `=`, `+=`, and `-=`: `+=` appends text to string fields, adds numerically to declared numeric fields, and inserts elements into declared `set<…>` fields; `-=` subtracts or removes elements. The accumulating events commute, so concurrent `points+=2` and `points+=3` on different branches merge to `+5` — no conflict.

Each event carries a store-minted, strictly increasing `seq`. That sequence — not the wall clock — is the authoritative order, which keeps replay deterministic even after branches with interleaved timestamps are merged.

**Dependencies** form a DAG. taska validates against cycles and `ta list --ready` returns the not-yet-done tasks whose dependencies are all satisfied.

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
# The DISPLAY name of the status field, and the value that means "done" —
# storage always uses the canonical key `status`, so renaming this is free.
# `ta list --ready` treats a dependency as satisfied once it reaches done_status.
status_field = "status"
done_status = "closed"
# Status stamped onto a new task when `ta create` doesn't set one ("" = off).
default_status = "todo"
# DISPLAY name of the task-type discriminator that selects a [task_types]
# schema (stored canonically as `task_type`; renaming is free too).
type_field = "type"
# Read commands warn ONCE when tasks don't conform to their schema (old data
# stays readable by design; any write to such a task must fix it). false silences.
warn_nonconforming = true
# Tasks WITHOUT a type while schemas are declared: "allow" (sanctioned, silent),
# "warn" (tolerated, reported), "deny" (type mandatory — writes to untyped tasks
# are rejected). Long migrations typically walk allow -> warn -> deny.
untyped_tasks = "deny"

[merge]
on_conflict = "surface"

# Typed relationship edges, one inline table per declared type, usable with
# `ta dep add/remove`. `kind` is its semantics: "blocker" gates readiness and
# cycle detection, "hierarchy" is a parent/child subtask edge (gates like a blocker
# but renders distinctly), "info" is informational only. `inverse` names the reverse
# edge `show` surfaces (omit it for a one-way type). At least one "blocker" type
# must be declared; `depends_on` is the default config's (there is no implicit blocker).
[relationships]
depends_on  = { kind = "blocker", inverse = "blocks" }
has_subtask = { kind = "hierarchy", inverse = "subtask_of" }
relates_to  = { kind = "info", inverse = "relates_to" }   # symmetric

# Per-type task schemas — OFF while no [task_types.<name>] is declared (the
# store stays fully schema-agnostic). Once declared, the `type` field selects a
# task's schema, enforced on every create/update (whole-task). Field kinds:
# string, bool, int, uint, float, datetime, enum, any, array<T>, set<T>.
# config.toml is TOML 1.1, so `fields` reads best as one multi-line inline
# table (trailing comma allowed), one line per field; `default` is stamped at
# create, healed onto writes, substituted at read for old data.
[task_types.bug]
closed = true                       # no fields beyond the declared ones
fields = {
  points   = "uint",                # shorthand: just the kind
  tags     = "array<string>",
  severity = { type = "enum", values = ["low", "medium", "high"], required = true, default = "low" },
  estimate = { type = "uint", min = 1, max = 13 },
}

[display]
# Columns for list/ready (and the field order used by --format json).
# "id" and "deps" are built-ins; any other name is a task field. Override per
# command with --columns id,status or --full. The all-fields views (--full and
# `show`) use a canonical order: these configured columns first, then every
# remaining field alphabetically — identical across human and json output.
columns = ["id", "title", "status", "deps"]
# Truncate long human cell values to this many characters (0 = no limit). The
# global fallback for any column not overridden below.
max_width = 40
# Default column to sort list/ready by (ascending; --sort overrides,
# --reverse flips). Any field, "id", or "deps"; empty/unknown falls back to id.
sort = "create_time"
# Default layout per command: "table" (aligned columns) or "list" (a vertical
# field: value record per task). --layout overrides for one run.
list_layout = "table"
show_layout = "list"

# Per-column truncation overrides: a column listed here is truncated to its own
# width instead of max_width (0 = no limit). --full ignores these entirely.
column_max_width = { title = 80 }

[timestamps]
# Computed (never user-set) timestamp fields materialized onto every task from
# the event log. These name the columns they surface under; "" disables one.
# They behave like ordinary string fields — usable with --columns/--full/show,
# list filtering, and --sort.
create_time = "create_time"   # the Create event's time
update_time = "update_time"   # the latest touching event's time
close_time  = "close_time"    # most recent time status hit done_status (cleared on reopen)
```

Because the times are folded into the baseline at compaction, they survive even after their source events are compacted away. They are best-effort: event timestamps are informational (`seq` is the authoritative order), so after a cross-branch merge they can be non-monotonic.

## Commands

| Command | Description |
|---|---|
| `ta init` | Create the store and register the git merge drivers (idempotent; run once per clone) |
| `ta create <id> [field=value ...]` | Create a task with arbitrary fields |
| `ta update <id> <field=value \| field+=value ...>` | `=` sets a field; `+=` appends to a text field (one entry per line). Mix both in one command. Appends merge conflict-free (concurrent appends accumulate) |
| `ta dep add <task> <type>=<target> …` | Add typed relationship edge(s); each `type` must be declared in `[relationships]` (e.g. `ta dep add api depends_on=db relates_to=ui`). A `hierarchy` type like `has_subtask` makes a parent/child edge that gates like a blocker but renders distinctly. Rejects a second blocking edge between the same pair, or a second parent for a task |
| `ta dep remove <task> <type>=<target> …` | Remove typed edge(s); a type's configured `inverse` name works too (`ta dep remove db blocks=api` removes `api depends_on db`) |
| `ta dep tree [<task> …]` | ASCII dependency tree with a shortened title per node, colored on a TTY (done tasks dimmed + `✓`). Shows the exact graph by default — never spliced; `--open` prunes fully-resolved branches. `--sort`/`--reverse` order siblings. Subtasks are tagged `[subtask]`, a parent rolls up `[subtasks done/total]`, shared nodes collapse, cycles are flagged |
| `ta dep cycles` | Report cycles in the blocker graph (`depends_on` plus any `blocker`/`hierarchy` relationship edges) |
| `ta dep plan <goal> …` | A goal's not-done transitive prerequisites in dependency order — "do exactly these, in this order". `--critical` narrows to the longest single chain (the critical path) |
| `ta delete <id>` | Delete a task |
| `ta list [criteria...] [--open] [--ready]` | List tasks, optionally filtered by AND-combined criteria: `field=value` (exact), `field=~regex`, `field!=value`, `field!~regex`, or a comparison `field>value`/`>=`/`<`/`<=` (numbers numerically, strings/dates lexicographically — quote them so the shell keeps `>`/`<`: `ta list 'unblocks>0' 'priority>=4'`); `field` may be a task field, `id`, `deps` (a target under any relationship type), a relationship type or inverse name (`depends_on=db`, `subtask_of=epic`, `blocks=api`), or a computed column (`unblocks=0`); a multi-valued field (set/array, `deps`, relationship type) matches if any element does (see [Filtering](#filtering)). `--open` limits to not-done tasks; `--ready` to not-done tasks whose dependencies are all done. With no criteria, lists everything |
| `ta show <id>` | Show one task as a readable vertical record — every field, untruncated, one `field: value` line each, plus the inverse edges pointing at it (`blocks`, `subtask_of`, …) as their own fields (`--format json`/`jsonl` for machine output) |
| `ta status` | Summary counts: total, per-status (discovered from the data), blocked, ready, and closed (`--format json`/`jsonl` for a machine-readable object) |
| `ta undo [--count N] [--remove] [--force]` | Reverse the last N events: truncate uncommitted ones, append compensating events for committed ones (`--remove` to force truncation) |
| `ta compact` | Fold old events into the baseline snapshot |
| `ta config get\|set\|list [key] [value]` | View or change `.taska/config.toml` by dotted key (e.g. `ta config set compaction.keep_events 500`); `set` validates and preserves comments |
| `ta config validate` | Check the config against the task graph: every relationship edge uses a declared type, blocker edges are acyclic, inverse names don't collide, at most one blocking relationship exists per task pair, and a task has at most one parent. Run after hand-editing `config.toml` (`set` runs the same check) |
| `ta resolve` | Review and clear a surfaced merge conflict |
| `ta repair [--migrate] [--schema] [--set-type-if-none TYPE] [--rename NEW=OLD]` | The store fixer — the one command that rewrites existing records (no prompt; review with `git diff`, revert with `git restore` before committing). `--migrate` updates the on-disk format (a stale store is refused on read until migrated). `--schema` applies every lossless data fix toward the `[task_types]` schemas and lists the ambiguous remainder with suggested commands — never guessing, never writing what a schema would reject. `--set-type-if-none TYPE` explicitly types every untyped task. `--rename severity=sev` moves a column under its declared name (coerced); `--rename type=category` adopts a de-facto type column, converting only values that name a declared type. All idempotent |

Field values are parsed as JSON when possible (`priority=3` is a number, `status=open` a string). A value of `@PATH` is read from that file and `@-` from stdin — taken verbatim as a string (one trailing newline trimmed); this is the way to pass long or shell-hostile text (notes, descriptions) without fighting argv quoting, e.g. `ta update api notes=@notes.md` or `… notes=@-`. Write a literal `@` with `@@` (`owner=@@alice`). The keys `seq`, `timestamp`, `op`, `task_id`, and `_meta` are reserved.

Mutations are **verified before they're logged** (atomically, under the store lock). Rejected: creating a task that already exists; updating, deleting, or adding a dependency to a task that doesn't; a dependency on itself; `+=` on the single-valued status field; and setting a reserved or *computed* field name (`id`, `deps`, the timestamp/graph columns, or a relationship type name — their value is derived, so a user value would be invisible). Setting a field to the value it already has, or re-adding an edge that already exists, writes nothing rather than bloating the log.

`list` and `show` share display flags: `--format human|json|jsonl` (`json` is a parseable array; `jsonl` is NDJSON — one object per line — for streaming, `grep`, and agents; both omit a field a task lacks rather than emitting `null`), `--full` to show every field, `--columns id,status,…` to pick the columns for one run (its `--help` lists the always-available computed columns), `--sort <column>` / `--reverse` to order the rows (`list`; default column from `[display].sort`), and `--layout table|list` to switch between an aligned table and a vertical `field: value` record per task. The per-command layout default lives in `[display]` (`list_layout` defaults to `table`, `show_layout` to `list`), alongside the columns and `max_width`.

Every command that prints data — `list`, `show`, `status`, and `dep tree`/`plan`/`cycles` — accepts the same `--format human|json|jsonl` and `--no-color`, so machine output is available and consistent everywhere (`dep tree` json is the nested tree; `dep plan` an ordered array; `dep cycles` an array of cycles; `status` a summary object).

Human output is **colored** when stdout is a terminal, and **consistently across every command** (`list`, `show`, `dep tree`, and any future one) — they share one styling rule: `id` is cyan, the configured status column green, headers bold, and the `deps` type groups styled by kind (readiness-gating types bold, informational ones dim). A **done/closed task greys out** wholesale (dim), overriding those colors, so finished work recedes in `list` rows and `dep tree` nodes alike. It uses the terminal's named 16-color palette, so it adapts to your light/dark theme. Color auto-disables when output isn't a TTY (pipes, redirects) and for `--format json`/`jsonl`, so machine output and `grep` stay clean; `--no-color` or the `NO_COLOR` env var turns it off explicitly.

`list` also offers a few **computed** columns for triage — `unblocks` (how many still-open tasks this one transitively unblocks — "finish it to free up N"), `blocked_by` (how many still-open prerequisites it's waiting on), and `subtasks` (a parent's `done/total` child completion). The first two behave like numeric fields, so `--sort unblocks --reverse` surfaces the highest-leverage work and `--sort blocked_by` the most-stuck. They're opt-in: computed only when named in `--columns`/`--sort` or the configured columns, so default and `--full`/json output are untouched.

### Filtering

`ta list` takes any number of positional `field<op>value` criteria, AND-combined — a task must satisfy all of them:

| Criterion | Matches |
|---|---|
| `field=value` | exact equality (value JSON-coerced, so `priority=3` is the number 3) |
| `field!=value` | not equal |
| `field=~regex` | regex over the value's string form (perl/bash spelling) |
| `field!~regex` | regex that does not match |
| `field>value` · `>=` · `<` · `<=` | ordering — numbers numerically, strings/dates lexicographically; a cross-type compare never matches |

`field` may be a task field, `id`, `deps` (a target under any relationship type), a relationship type or inverse name (`depends_on=db`, `subtask_of=epic`, `blocks=api`), or a computed column (`unblocks`, `blocked_by`, `subtasks`).

**Single- vs multi-valued fields.** Most fields hold one value. A `set`/`array` field, `deps`, and relationship types are *multi-valued*, and a criterion tests **each element**: the positive operators (`=`, `~`, `>`, …) match if **any** element does, and `!=`/`!~` hold when **none** does (so also when the field is empty or absent). So `tags=urgent` is set membership, `scores>=5` is "some score ≥ 5", and `tags!=wip` is "wip is not a member". (An `enum` field is single-valued — filter it with a plain `severity=high`.)

Dates work for free: the computed timestamps are RFC 3339 strings, so a lexicographic range *is* a chronological one — `ta list 'create_time>=2026-06-01'`. Quote any comparison so the shell doesn't read `>`/`<` as redirection (`ta list 'unblocks>0' 'priority>=4'`). `--open` limits to not-done tasks; `--ready` to not-done tasks whose dependencies are all done.

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
