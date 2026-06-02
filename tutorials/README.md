# taska tutorials

Runnable, interactive bash tutorials that walk real `taska` scenarios end to end.
They serve a dual purpose:

1. **UX validation** — the maintainer can run them to sanity-check how `ta`
   behaves across scenarios, on a fresh store, in one pass.
2. **Learning by doing** — each script narrates what it's about to do, runs the
   real `ta` command, and shows the real output, so you learn common usage
   patterns by watching them happen.

Every script is **self-contained**: it spins up a throwaway git repo in a temp
directory (via `mktemp`, *outside* this checkout) with its own initialized taska
store, so nothing here touches your real tasks or repo.

## Requirements

The `ta` binary must be on your `PATH`. From the project root:

```console
$ cargo build
$ export PATH="$PWD/target/debug:$PATH"
```

`ta` must be reachable not just for the scripts themselves but for git: the merge
tutorial relies on taska's git merge driver, which shells out to `ta git-merge`.
If `ta` is missing, every script stops immediately with a clear error.

## Running

Run a single scenario:

```console
$ bash tutorials/01-basics.sh
```

Run all of them in order:

```console
$ bash tutorials/run-all.sh
```

When run from a terminal, each script **pauses between sections** (press Enter to
continue) so you can read the output before moving on.

## Unattended / CI runs

Set `TUTORIAL_NONINTERACTIVE=1` to skip every pause and run straight through —
useful in tests or CI. (Pauses are also skipped automatically when stdin isn't a
terminal, e.g. when piping.)

```console
$ TUTORIAL_NONINTERACTIVE=1 bash tutorials/run-all.sh
```

## The scenarios

| Script | What it teaches |
|---|---|
| `01-basics.sh` | Create tasks with arbitrary fields; `list` (aligned table), `--columns`, `--full`, `--format json`; `search`; `show <id>`. |
| `02-dependencies.sh` | `block` to add dependency edges; `ready` showing only unblocked tasks; closing a dependency to unblock the dependent. |
| `03-merge.sh` | Two branches editing concurrently: a clean auto-merge (different fields), a same-field conflict under `surface` (`ta resolve`), and the same conflict resolved silently under `theirs` / `latest`. |
| `04-undo.sh` | `undo` of an uncommitted action (truncates) vs. a committed one (compensating event, with a before→after preview); `--count`, `--remove` (with its DANGER warning), and the confirmation prompt. |
| `05-revert-convergence.sh` | `git revert` a commit of task events, then merge in both directions — the result is identical (reverts converge). |
| `06-orphans.sh` | Manufacture an orphaned event, see the stderr warning on a read, then drop it with `ta resolve`. |
| `07-compaction.sh` | Create more events than `keep_events`, `compact`, and watch the baseline grow while the log shrinks (task state unchanged). |
| `08-null-unset.sh` | Set a field, then `update <id> field=null` to unset it; confirm it's gone from `list` / `show` / `search`. |

## How the helpers work (`lib.sh`)

Every script sources `lib.sh`, which provides:

- `say <text>` — bold narration, prefixed with `## `.
- `run <cmd...>` — echoes the command (prefixed `$ `), runs it for real, then a
  blank line, so you see the command and its actual output together.
- `pause` — waits for Enter, but returns immediately when non-interactive.
- `fresh_repo` — makes a throwaway `mktemp` repo outside this tree, `git init`s
  it, sets a git identity, and runs `ta init`.
