# Changelog

All notable changes to `taska` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/). History before 0.3.0 lives in the
git log.

## [Unreleased]

## [1.2.0] - 2026-07-27

This release adds coordination primitives for agent and human workflows: a
blocking watch command, atomic conditional writes, explicit mutation cursors,
Mercurial/Sapling merge-driver support, and clearer automation-facing failures.

### Added
- **`ta watch`** streams task mutations: `ta watch [criteria] --since SEQ` blocks
  until a task matching the list-grammar filter is created, updated, or deleted
  past `SEQ`, then prints only the changed `-`/`+` lines and exits 0. `--holdout`
  batches bursts, `--timeout` bounds the wait, and `--format json` emits
  structured removed/added deltas.
- **Atomic conditional writes with `--if`.** `ta update` and `ta delete` now accept
  repeatable `--if COND` guards using the same criteria grammar as `ta list`.
  Guards are checked under the store lock, so racing agents can claim work safely;
  an unmet guard exits 3 without appending anything.
- **Mutation cursors in CLI output.** `ta create`, `ta update`, `ta edit`,
  `ta delete`, and `ta undo` print `[seq:N]` for the resulting high-water event,
  and `ta status --current` prints the current cursor directly (`{"seq":N}` with
  `--format json`).
- **Mercurial and Sapling merge-driver support.** `ta init` registers managed hg
  merge tools in `.hg/hgrc`, the hidden `hg-merge` entrypoints share the same
  event-log merge core as git, and undo now detects committed history through the
  active SCM.
- **Automation-friendly exit codes.** General errors exit 1, schema/typo-guard
  rejections exit 2, and failed `--if` preconditions exit 3.

### Changed
- **Empty values clear optional fields.** `field=` now unsets optional or
  undeclared fields, and `ta list field=` matches absent or empty values; required
  schema fields keep the literal empty string.
- **`ta undo` preview is a line-level diff.** Instead of dumping the whole before
  and after record, it shows only the lines that change. The diff renderer is
  shared with `ta watch`.
- **Structured output rendering is lazy and consistent.** `status`, `prime`, and
  `dep` subcommands now build only the selected `--format` representation while
  preserving the existing JSON/JSONL contracts.

### Fixed
- `ta init` no longer creates `.gitattributes` when no supported SCM is present.
- `ta watch` handles very large `--since` values without overflowing during the
  retained-log cursor check.

## [1.1.0] - 2026-06-27

Quality-of-life work on top of 1.0: a git-style `-C` flag for driving a store
from elsewhere, an `ta init` that version-controls the store for you, a reworked
`ta undo` that walks real history, `ta show` over multiple ids, and ARM64 Linux
binaries.

### Added
- **Global `-C` / `--directory` flag** (git's `-C` semantics): run any command as
  if `ta` were started in `<DIR>`, so e.g. a worktree can drive the main
  checkout's `.taska` store as a shared board. Store discovery, relative `@FILE`
  paths, and `init`'s repo-root search all resolve from there; it's honored during
  shell completion too.
- **`ta init` commits the store.** After creating `.taska/`, registering the merge
  drivers, and writing the agent block, `init` makes one path-scoped commit of the
  store, `.gitattributes`, and the block it wrote - so the store is
  version-controlled from the first command. `--no-commit` opts out; a user's
  unrelated changes are left untouched, gitignored paths are skipped, a non-git
  directory is handled gracefully, and a no-op re-init makes no empty commit.
- **ARM64 Linux binaries.** Prebuilt static-musl `aarch64-unknown-linux-musl` `ta`
  binaries now ship on each GitHub release; the installer and `ta self-update`
  pick the right one automatically.

### Changed
- **`ta show` accepts multiple ids** - `ta show a b c` renders each task in full
  (duplicate ids shown once, in first-occurrence order); an unknown id errors.
- **`ta undo` walks back through real history.** Repeated `undo` peels back
  genuine events newest-first, skipping its own compensations and already-undone
  events instead of bouncing on its own output; `--seq` / `--count` pick the
  starting event and how many to walk older.
- **Merge drivers auto-register on a fresh clone.** When the committed
  `.gitattributes` already declares the taska merge drivers, the next `ta` command
  registers them in your local git config silently instead of warning - it warns
  only if the `.gitattributes` entries themselves are missing. (The registered
  command is a taska-owned constant, so this never runs anything the repo chose.)

## [1.0.0] - 2026-06-14

First **stable** release. It rounds out the agent-facing surface (a config
primer, an editor round-trip, shell completion, a self-updater), ships prebuilt
binaries and a one-line installer, adds a soft typo guard for field names, and
drops every pre-1.0 on-disk compatibility shim - so `taska` now reads only
v1.0+ stores.

### Added
- **`ta prime`** - a config-tailored primer for an AI agent driving the store:
  this store's actual status field/values, declared task types and
  relationships, the core commands in that vocabulary, and a count summary -
  generated live from the config so it never goes stale (`--format json` for the
  raw facts).
- **Agent integration on `ta init`:** writes a config-agnostic, marker-delimited
  task-tracking block into `AGENTS.md`/`CLAUDE.md` (created if absent, updated
  idempotently), pointing agents at `ta prime` and `ta <command> --help`.
- **`ta edit` / `ed`** - round-trip a task's fields through `$EDITOR` as TOML (or
  `--json`); the saved diff funnels through the same write gate as `ta update`,
  with a re-edit loop on any error.
- **`ta self-update`** - download this platform's prebuilt binary from the latest
  GitHub release and replace the running executable in place (`--check` to report
  only, `--force` to reinstall); warns if another `ta` still shadows it on PATH.
- **Shell completion** - `ta completions <shell>` for bash/zsh/fish/powershell/
  elvish, **dynamic and store-aware** (completes live task ids, `list` filter
  fields, and column names), with `--install [user|system]` to set it up (sudo
  fallback for system paths). `ta init` and the installer offer it interactively.
- **Prebuilt binaries and a `curl | bash` installer.** A tag-triggered workflow
  ships static-musl Linux and macOS (Intel + Apple Silicon) `ta` binaries on the
  GitHub release; `install.sh` downloads, checksum-verifies, and installs the
  right one (or falls back to `cargo install`) and puts it on PATH.
- **Soft typo guard.** A field name no task uses yet is rejected - with a
  did-you-mean suggestion - unless you pass `--new-field`, so a misspelling
  (`titel`, `pirority`) can't silently create a phantom column. Schemaless stores
  stay schemaless; the first task on an empty store seeds the vocabulary.
- **Richer `ta list` filters:** comparison operators `field>value` / `>=` / `<` /
  `<=` (numeric or lexicographic, so RFC 3339 dates compare chronologically), and
  element-wise matching on multi-valued fields - `tags=urgent` matches a member,
  `scores>=5` matches if any element does.
- **Shadowed-binary warning.** When more than one `ta` is on PATH, every command
  warns, probes each copy's version, and prints the exact `rm` that keeps only
  the newest - so an update can't land on a copy you never run.

### Changed
- **Output is plain ASCII** (no Unicode typography); `dep tree` keeps its
  box-drawing connectors.
- **`dep tree` rendering:** subtasks show a done-state checkbox (`[x]` / `[ ]`)
  with connectors aligned under it, a parent rolls up `[subtasks done/total]`, a
  collapsed shared node points to where it's expanded (`expanded above` /
  `below`), and `--reverse` flips sibling order.
- **Consistent coloring across every command.** One styling rule drives `list`,
  `show`, and `dep tree` (id cyan, the status column green, a done task's whole
  row/record dimmed, deps colored by kind); informational relationships render
  plain rather than dim.
- **`ta undo` preview is a colored per-field `-`/`+` diff** of just the columns
  that change, styled like `ta show`.
- **`ta list` regex operators are `=~` / `!~`** (the bare `~` / `!=~` spellings
  are gone).
- **Frontend-agnostic core** (for library consumers): command logic moved into an
  `action` layer and the write gate / schema law into a `schema` module, both
  depending only on the storage trait, so a non-CLI frontend can drive the same
  functionality through one verified write choreography.

### Fixed
- Repeated `+=` / `-=` to the same field in one command now accumulates **all**
  operands (previously all but the last were dropped).
- `install.sh` works on macOS (bash 3.2 + BSD tools) and no longer trips an
  unbound-variable error during cleanup.

### Removed
- **All pre-1.0 on-disk compatibility (breaking).** This binary reads only v1.0+
  stores: the legacy edge spellings (`AddDep`/`RemoveDep`, `type`/`dep` payload
  keys), untyped edges, and the top-level `depends_on` baseline field are no
  longer accepted. A pre-1.0 store is detected and refused on read - migrate it
  with `ta repair --migrate` on the **last 0.x release** first, then upgrade.

## [0.5.0] - 2026-06-07

This release adds opt-in **per-task-type schemas** - typed fields, constraints,
and defaults enforced on every write - a **`ta repair`** command for store
migrations and data fixes, and a cleaner on-disk event vocabulary (migrated
automatically). Stores remain fully schema-agnostic until a schema is declared.

### Added
- **Per-task-type schemas** (`[task_types.<name>]` in config). A task's `type`
  field selects its schema; field kinds are `string`, `bool`, `int`, `uint`,
  `float`, `datetime`, `enum`, `any`, `array<T>`, `set<T>` - declared shorthand
  (`points = "uint"`) or long form with constraints: `required`, `default`,
  `min`/`max` (numbers, datetimes, strings), `pattern`, `min_len`/`max_len`,
  `min_items`/`max_items`. `closed = true` rejects undeclared field names.
  Enforcement is **whole-task on every create/update**, with every violation
  reported in one error; retyping a task revalidates against the new type.
- **Schema-aware value shaping.** Values coerce toward their declared kind at
  write time: a declared string keeps its verbatim token (`version=3.10` stores
  `"3.10"`, not `3.1`), numeric strings parse, a bare scalar lifts into a
  declared array/set, and sets canonicalize to a sorted, deduped form so
  concurrent inserts converge bytewise on merge.
- **`+=` / `-=` on numbers and sets.** Declared numeric fields add/subtract and
  `set<T>` fields insert/remove elements via new commutative `Add`/`Remove`
  events - like text appends, they merge conflict-free across branches. Plain
  text `+=` remains for strings and undeclared fields.
- **Field defaults with a full life-cycle:** stamped at create, healed onto any
  write that leaves the field absent, substituted at read for missing/invalid
  stored values, and stamped by `ta repair --schema` - so `required` + `default`
  never blocks a write.
- **Read tolerance.** Reads never fail on non-conforming (grandfathered) data:
  read commands print one warning (silence with
  `[workflow] warn_nonconforming = false`), `ta config validate` lists every
  violation, and writes to such a task must bring it into conformance. The
  `[workflow] untyped_tasks = "allow" | "warn" | "deny"` knob is the migration
  ladder for legacy untyped stores.
- **`ta repair`** - the store fixer, and the one sanctioned command that
  rewrites existing records (no prompt: review with `git diff`, revert with
  `git restore` before committing). `--migrate` applies on-disk format
  migrations; `--schema` applies every lossless fix toward the declared schemas
  (value coercions, datetime normalization, required-default stamping) and
  lists the ambiguous remainder with suggested commands; `--rename NEW=OLD`
  moves a column under its declared name - including adopting a de-facto type
  column, converting only values that name a declared type;
  `--set-type-if-none TYPE` explicitly types every untyped task. All
  idempotent.
- **`ta list` filters by relationship names:** a declared type or inverse name
  is a criterion field (`depends_on=db`, `blocks=api`, `subtask_of=epic`),
  `deps=<id>` matches any edge, and computed columns work in criteria
  (`unblocks=0`).
- **Configurable `status`/`type` display names.** `[workflow] status_field` and
  `type_field` rename what you see and type; storage always uses the canonical
  keys (`status`, `task_type`), so renaming is free - no data migration - and
  clones with different display configs merge cleanly.
- **TOML 1.1 config.** `config.toml` accepts multi-line inline tables and
  trailing commas; the template and docs now style a type's `fields`, the
  relationship defs, and `column_max_width` as inline tables, and
  `ta config set` edits inside them while preserving comments and formatting.

### Changed
- **On-disk event vocabulary** (breaking, migrated by `ta repair --migrate`; a
  legacy store is detected and refused on read with instructions): edge ops are
  `AddEdge`/`RemoveEdge` (were `AddDep`/`RemoveDep`) with payload keys
  `rel`/`target` (were `type`/`dep`), and the type discriminator is stored as
  `task_type`. The parser keeps accepting the legacy spellings until v1, so old
  logs remain readable for migration and cross-branch merging.
- **The `deps` column carries the whole typed relationship map**, grouped by
  type in every format - labeled groups in human output (gating types bold,
  informational dim), a `{type: [targets...]}` object in json/jsonl - instead of
  a flat `depends_on` list.
- `[relationships]` declarations use `kind = "blocker" | "hierarchy" | "info"`
  (was `type =`; the old key is still accepted until v1).
- **`ta init` works from anywhere in the repo:** a new store is created at the
  SCM root (not the invocation directory), nested `.git`/`.taska` layouts are
  fully supported, running outside any git repo prints one actionable warning
  instead of raw git errors, and every store command warns when the clone's
  merge-driver protection is missing or incomplete (pointing at `ta init`).
- `ta undo` compensates **all** typed relationship edges, not just
  `depends_on`.
- Config validation enforces one namespace across every configured name -
  field, relationship, timestamp, and computed-column names can't collide.
- Upgraded `toml` to 1.1.2 and `toml_edit` to 0.25 (TOML spec 1.1).

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
  inverse-mirrored) as fields - the way to inspect edges now that `dep list` is
  gone.
- **Write-time validation.** Mutations are verified before they're logged,
  atomically under the store lock: creating a task that already exists,
  mutating/deleting a missing one, a dependency on itself or on a missing task,
  `+=` on the single-valued status field, and setting a reserved/computed field
  name are all rejected; setting a field to its current value - or re-adding an
  edge that exists - writes nothing instead of bloating the log.
- **`ta dep tree` rework:** a shortened title per node, color on a TTY (done
  tasks dimmed + a check mark), the exact graph by default with `--open` to prune resolved
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
  parent per task - enforced on `ta dep add` and by `ta config validate`.
- Graph traversal interns task ids to integers for the run, making readiness,
  topological sort, and reachability markedly faster on large stores.
- `ta dep cycles` reports cycles over the whole blocker graph (`depends_on` plus
  any `blocker`/`hierarchy` edges), not just `depends_on`.
- `seq` minting now refuses to write over an unparseable log line instead of
  silently skipping it (which could mint a duplicate `seq` and corrupt the log) -
  typically a stale `ta` binary predating a newer event type.

### Removed
- **`ta dep list`** - a task's relationships are shown by `ta show`.

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
  the config against the materialized task graph - every edge uses a declared
  type, blocker edges are acyclic, and no inverse name collides with another
  type.
- **File / stdin field input:** `key=@FILE` reads a value from a file, `key=@-`
  from stdin, and `key=@@x` writes a literal `@x` - the quoting-free way to pass
  long or multiline values.
- **Append operator:** `key+=value` accumulates onto a text field via a new
  conflict-free `Append` event (concurrent appends merge without conflict).
- **`[workflow] default_status`** - the status stamped onto a task created
  without one.
- **`ta show`** renders a single task as a readable vertical `field: value`
  record.
- **`docs/MERGE.md`** - the merge/revert/conflict protocol, linked from the
  README.

### Changed
- Readiness, cycle detection, and `dep tree` now operate over all
  **blocker-typed** relationships, not just the `depends_on` field.
- Compaction retention raised: `keep_events` defaults to 5000 (floor 300).
- Upgraded `petgraph` 0.6 -> 0.8.
- The crate/CLI description is now "task & dependency tracker" (was "engine").

### Removed
- `ta block` / `ta unblock` - use `ta dep add`/`ta dep remove` with
  `depends_on=<target>`.
- `ta search` - folded into `ta list` with `field<op>value` criteria
  (`=`, `~`, `!=`, `!~`) plus `--open`.
- `ta ready` - folded into `ta list --ready`.

### Fixed
- `ta --version` reported `0.1.0` regardless of the crate version; it now
  reflects the real version.
- Writing to a closed pipe (e.g. `ta list | head`) exits cleanly via `SIGPIPE`
  instead of panicking.
- The merge driver now warns when one branch reverts a shared event, and the
  sequence-gap / revert behavior is documented and tested.
