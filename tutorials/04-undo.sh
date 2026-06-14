#!/usr/bin/env bash
# 04-undo.sh - reversing the last event(s) safely, committed vs uncommitted.
#
# 'ta undo' has two paths, chosen by whether the undone events are git-committed:
#   - uncommitted  -> truncate the log's tail (they were never shared).
#   - committed    -> append a COMPENSATING event (keeps shared history intact).
# Plus --count N, the dangerous --remove, and the y/N confirmation prompt. Each
# example below reverses a DIFFERENT kind of edit (several fields at once, an
# append, a field + a dependency edge, an unset) - and the preview is a '-/+'
# diff of just the columns that change, colored like 'show'.
source "$(dirname "$0")/lib.sh"

fresh_repo

say "Create a task with a few fields and commit it, so its Create is shared history."
run ta create deploy title="Deploy service" status=open priority=low notes="Plan the rollout."
run git add .taska .gitattributes
run git commit -q -m "track deploy"

# -------------------------------------------------------------------------
# 1. Undo an UNCOMMITTED, MULTI-FIELD update -> truncation.
# -------------------------------------------------------------------------
say "Make ONE uncommitted update that changes SEVERAL fields at once (a single"
say "Update event): move the status, raise the priority, and add an owner ('owner' is a"
say "new field name, so --new-field opts it past the typo guard)."
run ta update deploy --new-field status=staging owner=alice priority=high
run ta show deploy

say "Undo it. The event was never committed, so undo TRUNCATES it. The preview is a"
say "'-/+' diff of every column that changes - status and priority revert, owner drops."
run ta undo --force
run ta show deploy

# -------------------------------------------------------------------------
# 2. Undo a COMMITTED APPEND -> compensating event.
# -------------------------------------------------------------------------
say "Now an APPEND, committed. '+=' ACCUMULATES onto a field instead of replacing it."
run ta update deploy "notes+= Roll back on error."
run git commit -q -am "note the rollback step"
run ta show deploy

say "Undo the committed append. taska keeps history intact and APPENDS a compensating"
say "event; the diff shows 'notes' reverting to its pre-append value:"
run ta undo --force
run ta show deploy
say "The log GREW (a compensating event) rather than shrinking:"
run tail -n 2 .taska/mutations.jsonl

# -------------------------------------------------------------------------
# 3. --count N: undo several edits of DIFFERENT kinds at once.
# -------------------------------------------------------------------------
say "'--count N' undoes the last N events. Make two edits of different kinds - a"
say "field set and a dependency edge (we add a second task to depend on)..."
run ta create db title="Database"
run ta update deploy --new-field region=us-east
run ta dep add deploy depends_on=db
run ta show deploy

say "...then undo BOTH with --count 2 (the edge AND the region set fall away)."
run ta undo --count 2 --force
run ta show deploy

# -------------------------------------------------------------------------
# 4. The confirmation prompt (run WITHOUT --force), on an UNSET.
# -------------------------------------------------------------------------
say "Unset a field with '=null' (the null-unset convention removes 'priority')."
run ta update deploy priority=null
say "Without --force, undo asks y/N and defaults to NO. The preview shows it WOULD"
say "re-add 'priority'; we pipe 'n' to decline, so nothing changes (and it won't hang)."
# Feed 'n' so the prompt is answered even with no TTY; the decline is expected.
echo n | run ta undo || true
say "Declined - priority is still unset:"
run ta show deploy

# -------------------------------------------------------------------------
# 5. --remove: the DANGEROUS truncate-committed path.
# -------------------------------------------------------------------------
say "'--remove' forces truncation even of COMMITTED events - rewriting shared history."
say "Commit first so the unset is committed, then watch the loud DANGER warning."
run git commit -q -am "commit before --remove"
run ta undo --remove --force || true
say "--remove rewrote committed history; only ever do this for events never shared."
