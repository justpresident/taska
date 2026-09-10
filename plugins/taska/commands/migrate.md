---
description: Migrate an existing tracker (beads, TODO.md, GitHub issues) into a taska store.
argument-hint: "[source, e.g. .beads/issues.jsonl or TODO.md]"
---

Migrate `$ARGUMENTS` into this repository's taska store.

The authoritative instructions are the migration guide, which is kept with the
tool and is more current than this command. Fetch and follow it:

https://raw.githubusercontent.com/justpresident/taska/master/docs/MIGRATING.md

If it cannot be fetched, migrate from first principles - the guide's substance is
that taska has no importer on purpose, because a mechanical one has to guess at
which status means done and which links really gate work, and you can read this
repository instead. Take the project's own conventions into account, and carry
these four hazards with you:

1. **Read the source before deciding anything.** Which fields are actually
   populated, which status value means done, which links truly block versus merely
   relate, and whether there is a parent/child structure.
2. **`workflow.default_status` is stamped at create.** If you declare a status enum
   that does not contain it, *every* `ta create` omitting a status fails schema
   validation. `done_status` must be in the enum too. Align them first, and run
   `ta config validate`.
3. **An empty value means unset.** `notes=""` stores nothing and registers no field
   name, even with `--new-field` - so the next task using that field is still
   rejected. Seed fields with real values or declare them in the schema.
4. **Descriptions contain newlines.** Never drive the migration from a `while read`
   shell loop over a flattened export: it splits one issue across several records
   and invents tasks named after comment fragments, and the count looks plausible.
   Iterate in a real language and pass bodies over stdin (`notes=@-`).

Create every task before any edge - `ta dep add` rejects an edge whose target does
not exist yet. Mind direction: "X blocks Y" becomes `ta dep add Y depends_on=X`.

Finish by verifying with `ta status`, `ta dep cycles` and `ta list --ready`, then
report explicitly what you could not map: statuses with no clear equivalent, links
you dropped or demoted, fields you left behind. A migration that names its gaps is
finished; one that hides them is not.
