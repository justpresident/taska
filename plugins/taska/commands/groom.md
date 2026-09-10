---
description: Review the taska backlog for gaps - thin tasks, missing dependencies, duplicates, staleness.
argument-hint: "[optional filter, e.g. owner=alice]"
---

Groom this repository's taska backlog. Find the problems a backlog accumulates
quietly, propose fixes as concrete commands, and apply nothing without asking.

Start with `ta prime` to learn this store's vocabulary, then survey the open work
with `ta list --open $ARGUMENTS` and read candidates with `ta show <id> --full`.

Look for, in rough order of how much damage each does:

1. **Tasks nobody else could pick up.** Notes that state a goal but no approach, no
   code pointers, and no open questions. This is the most common defect and the
   most expensive: it silently makes a task un-delegatable.
2. **Missing dependencies.** Two tasks that clearly cannot proceed in either order
   but have no edge between them. Use the code, not the titles, to judge it.
3. **Tasks that are really several.** A task whose notes contain a list of
   independently-completable things wants splitting, with edges between the parts.
4. **Duplicates and overlaps** - two tasks that would touch the same code for the
   same reason. Propose which survives, and link the other with the store's
   informational relationship type rather than deleting history.
5. **Staleness.** Tasks referencing files, functions or flags that no longer exist.
   Check before claiming this - `ta list --format json` plus a grep of the repo.
6. **Wrong shape.** Something open that is really done, or done that is really
   still open, judged against the code rather than the status field.

Report findings grouped by the categories above, worst first, each with the exact
`ta` command that would fix it. Then ask which to apply. Say plainly if the
backlog is in good shape - a grooming pass that invents work to justify itself is
worse than one that reports nothing.
