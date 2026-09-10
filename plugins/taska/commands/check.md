---
description: Validate a taska store - dependency cycles, orphaned events, schema conformance, suspicious readiness.
allowed-tools: Bash(ta prime:*), Bash(ta list:*), Bash(ta show:*), Bash(ta status:*), Bash(ta dep:*), Bash(ta config:*)
---

Check this repository's taska store for structural problems. Read-only: report,
do not repair.

Run each of these and interpret the result, rather than just echoing it:

```bash
ta prime                 # this store's schema, so the rest can be read correctly
ta status                # totals, and the per-status split
ta dep cycles            # must report none: a cycle means nothing in it can be ready
ta config validate       # config against the actual graph
ta list --open --columns id,title,status,blocked_by,unblocks
```

Then judge the shape, which the commands above will not tell you:

- **Is everything ready?** That usually means dependency edges were never added,
  not that the project has no dependencies.
- **Is nothing ready?** Usually a cycle, or edges pointing the wrong way -
  `ta dep add A depends_on=B` means A waits for B, and an inverted import makes
  every task wait on its dependents.
- **Warnings on read** about tasks not conforming to their declared type mean old
  data predates a schema change. `ta repair --schema` lists the lossless fixes and
  the ambiguous remainder; report it, do not run it.
- **Orphaned events** are reported by read commands and pruned by `ta resolve`.
  They are non-fatal but worth surfacing.
- **A task blocked by something already done** is a sign a status was set outside
  the workflow.

Report what is wrong, what it implies about how the store got that way, and the
command that would fix each. If the store is sound, say so in one line.
