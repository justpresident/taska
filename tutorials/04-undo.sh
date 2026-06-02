#!/usr/bin/env bash
# 04-undo.sh — reversing the last event(s) safely, committed vs uncommitted.
#
# 'ta undo' has two paths, chosen by whether the undone events are git-committed:
#   - uncommitted  -> truncate the log's tail (they were never shared).
#   - committed    -> append a COMPENSATING event (keeps shared history intact).
# Plus --count N, the dangerous --remove, and the y/N confirmation prompt.
source "$(dirname "$0")/lib.sh"

fresh_repo

# -------------------------------------------------------------------------
# Undo of an UNCOMMITTED action -> truncation.
# -------------------------------------------------------------------------
say "Create a task and commit it, so the Create event is part of shared history."
run ta create deploy title="Deploy service" status=open
run git add .taska .gitattributes
run git commit -q -m "track deploy"

say "Now make an UNCOMMITTED change."
run ta update deploy status=closed
run ta show deploy
pause

say "Undo it. Because that last event was never committed, undo simply TRUNCATES it."
say "(--force skips the y/N confirmation, so this runs unattended.)"
run ta undo --force
say "status is back to open — the uncommitted Update is gone:"
run ta show deploy
pause

# -------------------------------------------------------------------------
# Undo of a COMMITTED action -> compensating event (with before->after preview).
# -------------------------------------------------------------------------
say "This time, COMMIT the change first."
run ta update deploy status=done
run git commit -q -am "mark deploy done"

say "Undo a COMMITTED event. taska keeps history intact and APPENDS a compensating"
say "event instead of rewriting it. The preview shows the before->after transition:"
run ta undo --force
run ta show deploy
say "The log grew (a new compensating event) rather than shrinking:"
run tail -n 2 .taska/mutations.jsonl
pause

# -------------------------------------------------------------------------
# --count N: undo several events at once.
# -------------------------------------------------------------------------
say "'--count N' undoes the last N events in one shot. Make two quick edits..."
run ta update deploy status=staging
run ta update deploy status=production
run ta show deploy
say "...then undo BOTH with --count 2."
run ta undo --count 2 --force
run ta show deploy
pause

# -------------------------------------------------------------------------
# The confirmation prompt (run WITHOUT --force).
# -------------------------------------------------------------------------
say "Without --force, undo asks for confirmation (y/N) and defaults to NO."
say "Here we pipe 'n' to decline, so it changes nothing (and won't hang unattended)."
run ta update deploy status=open
# Feed 'n' so the prompt is answered even with no TTY; the decline is expected.
echo n | run ta undo || true
say "Declined — the status=open edit is still in place:"
run ta show deploy
pause

# -------------------------------------------------------------------------
# --remove: the DANGEROUS truncate-committed path.
# -------------------------------------------------------------------------
say "'--remove' forces truncation even of COMMITTED events — rewriting shared history."
say "Commit first so the event is committed, then watch the loud DANGER warning."
run git commit -q -am "commit before --remove"
run ta undo --remove --force || true
say "--remove rewrote committed history; only ever do this for events never shared."
