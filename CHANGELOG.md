# Changelog

All notable changes to `taska` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/) (pre-1.0: breaking changes bump the
minor). History before 0.3.0 lives in the git log.

## [0.4.0] - 2026-06-05

This release adds a parent/child **subtask** hierarchy, makes every mutation
**verified before it is logged**, reworks the dependency views with color and
consistent machine output, and speeds up graph traversal.

### Added
- **Subtask hierarchy.** A relationship `type = "hierarchy"` (e.g. `has_subtask`
  with inverse `subtask_of`) is a parent/child edge that gates readiness like a
  blocker but renders distinctly: `ta dep tree` tags subtasks and rolls up a
  parent's `[subtasks done/total]`, and `ta list` gains a `subtasks` completion
  column. A task may have at most one parent.
- **`ta show` surfaces a task's typed relationships** (forward and
  inverse-mirrored) as fields — the way to inspect edges now that `dep list` is
  gone.
- **Write-time validation.** Mutations are verified before they're logged,
  atomically under the store lock: creating a task that already exists,
  mutating/deleting a missing one, a dependency on itself or on a missing task,
  `+=` on the single-valued status field, and setting a reserved/computed field
  name are all rejected; setting a field to its current value — or re-adding an
  edge that exists — writes nothing instead of bloating the log.
- **`ta dep tree` rework:** a shortened title per node, color on a TTY (done
  tasks dimmed + ✓), the exact graph by default with `--open` to prune resolved
  branches, and `--sort`/`--reverse` for sibling order.
- **Theme-safe color** for human output (`id` cyan, `status` green, headers and
  `deps` bold) via the terminal's 16-color palette; auto-disabled off a TTY and
  for `--format json`/`jsonl`; `--no-color` / `NO_COLOR` force it off.
- **Consistent machine output everywhere:** `list`, `show`, `status`, and
  `dep tree`/`plan`/`cycles` all accept the same `--format human|json|jsonl` and
  `--no-color`.
- **`--layout table|list`** on `list`/`show`, with per-command defaults in
  `[display]` (`list_layout`, `show_layout`).
- A no-dependency **performance benchmark suite** (`cargo bench --bench perf`)
  and an empirical Performance section in `docs/MERGE.md`.
- The bundled **tutorials run end-to-end** as a `cargo test`.

### Changed
- At most one blocking relationship between a pair of tasks, and at most one
  parent per task — enforced on `ta dep add` and by `ta config validate`.
- Graph traversal interns task ids to integers for the run, making readiness,
  topological sort, and reachability markedly faster on large stores.
- `ta dep cycles` reports cycles over the whole blocker graph (`depends_on` plus
  any `blocker`/`hierarchy` edges), not just `depends_on`.
- `seq` minting now refuses to write over an unparseable log line instead of
  silently skipping it (which could mint a duplicate `seq` and corrupt the log) —
  typically a stale `ta` binary predating a newer event type.

### Removed
- **`ta dep list`** — a task's relationships are shown by `ta show`.

### Fixed
- Format/render tests no longer depend on whether stdout is a TTY.

## [0.3.0] - 2026-06-05

This release replaces the single untyped dependency edge with a full typed
**relationship** model and a `dep` command group, adds graph-traversal views for
planning, and folds several standalone commands into flags on `list`.

### Added
- **Typed relationships.** A `[relationships]` config section declares the edge
  types; each is `type = "blocker"` (gates readiness and cycle detection) or
  `type = "info"` (purely informational), with an optional `inverse` name
  (omitted = one-way, the type's own name = symmetric, any other name labels the
  reverse direction). Undeclared types are rejected.
- **`ta dep` command group:** `add` / `remove` (typed edges, addable or
  removable from either side by a type's inverse name), `list` (a task's edges,
  forward and inverse-mirrored), `tree` (ASCII blocker tree, shared nodes
  collapsed and cycles flagged), `cycles` (report blocker-graph cycles), and
  `plan <goal>` (the not-done transitive prerequisites in dependency order;
  `--critical` narrows to the longest chain).
- **Triage columns for `list`:** `unblocks` (how many still-open tasks this one
  transitively unblocks) and `blocked_by` (how many still-open prerequisites it
  waits on), usable as `--sort` keys. Computed only when referenced.
- **`ta config validate`** (and the same check inside `config set`): validates
  the config against the materialized task graph — every edge uses a declared
  type, blocker edges are acyclic, and no inverse name collides with another
  type.
- **File / stdin field input:** `key=@FILE` reads a value from a file, `key=@-`
  from stdin, and `key=@@x` writes a literal `@x` — the quoting-free way to pass
  long or multiline values.
- **Append operator:** `key+=value` accumulates onto a text field via a new
  conflict-free `Append` event (concurrent appends merge without conflict).
- **`[workflow] default_status`** — the status stamped onto a task created
  without one.
- **`ta show`** renders a single task as a readable vertical `field: value`
  record.
- **`docs/MERGE.md`** — the merge/revert/conflict protocol, linked from the
  README.

### Changed
- Readiness, cycle detection, and `dep tree` now operate over all
  **blocker-typed** relationships, not just the `depends_on` field.
- Compaction retention raised: `keep_events` defaults to 5000 (floor 300).
- Upgraded `petgraph` 0.6 → 0.8.
- The crate/CLI description is now "task & dependency tracker" (was "engine").

### Removed
- `ta block` / `ta unblock` — use `ta dep add`/`ta dep remove` with
  `depends_on=<target>`.
- `ta search` — folded into `ta list` with `field<op>value` criteria
  (`=`, `~`, `!=`, `!~`) plus `--open`.
- `ta ready` — folded into `ta list --ready`.

### Fixed
- `ta --version` reported `0.1.0` regardless of the crate version; it now
  reflects the real version.
- Writing to a closed pipe (e.g. `ta list | head`) exits cleanly via `SIGPIPE`
  instead of panicking.
- The merge driver now warns when one branch reverts a shared event, and the
  sequence-gap / revert behavior is documented and tested.
