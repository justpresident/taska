---
description: Pick the next taska task to work on, and explain why that one.
argument-hint: "[optional filter, e.g. owner=alice]"
allowed-tools: Bash(ta prime:*), Bash(ta list:*), Bash(ta show:*), Bash(ta dep tree:*), Bash(ta status:*)
---

Recommend what to work on next in this repository's taska store, and justify the
choice. Do not start the work - this is a decision, not an implementation.

1. Run `ta prime` first: this store's field names, statuses and relationship types
   are its own, and everything below has to be phrased in its vocabulary.
2. `ta list --ready $ARGUMENTS` is the candidate set - not done, every dependency
   satisfied. Anything outside it is blocked and not a real option.
3. Rank the candidates by leverage rather than by position in the list:
   - `ta list --ready --columns id,title,unblocks,blocked_by --sort unblocks --reverse`
     puts the tasks that free up the most other work first. This is the column
     most people never look at, and it is usually the whole answer.
   - Prefer a task that unblocks others over an equally-sized leaf task.
   - Respect the store's own priority field if it has one, but say so when it
     disagrees with the leverage ordering.
4. Read the top candidates properly with `ta show <id> --full` before recommending
   one. A task whose notes are too thin to act on is not ready in practice - say
   that, and recommend specifying it as the next step instead.

Report: the task you recommend, why it beats the runners-up, what it unblocks, and
anything in its notes that is unresolved. If the ready set is empty, say what is
blocking the backlog and which task would unblock the most if finished.
