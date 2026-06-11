#!/usr/bin/env bash
# 06-orphans.sh — detecting and cleaning up orphaned events ("no silent failures").
#
# An ORPHAN is an Update/AddDep/RemoveDep/Delete whose target task no longer
# exists, so it applies to nothing during replay. It's the symptom of a dropped
# Create (from a merge removal-union, a revert, or a manual edit). Read commands
# WARN about orphans on stderr; 'ta resolve' drops them (they're no-ops, so this
# can't change task state).
source "$(dirname "$0")/lib.sh"

fresh_repo

say "Manufacture an orphan: create a task, delete it, then update the (now-gone) task."
say "That trailing Update targets a task that no longer exists -> an orphaned event."
run ta create temp title="Temporary" status=open
run ta delete temp
run ta update temp status=closed

say "Any READ command warns about orphans on STDERR (it never blocks the read)."
say "Watch for the 'taska: warning: ... orphaned event(s)' line:"
run ta list

say "'ta resolve --force' names each orphaned event and drops it from the log."
run ta resolve --force

say "The warning is gone — the log no longer carries the orphan."
run ta list
say "Orphans surface loudly instead of failing silently, and resolve cleans them up safely."
