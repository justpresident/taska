#!/usr/bin/env bash
# 06-orphans.sh - detecting and cleaning up orphaned events ("no silent failures").
#
# An ORPHAN is an Update/AddDep/RemoveDep/Delete whose target task no longer
# exists, so it applies to nothing during replay. It's the symptom of a dropped
# Create (from a merge removal-union, a revert, or a manual edit). Read commands
# WARN about orphans on stderr; 'ta resolve' drops them (they're no-ops, so this
# can't change task state).
source "$(dirname "$0")/lib.sh"

fresh_repo

say "Orphans arise when a task's Create is DROPPED while an Update to it survives - a"
say "merge removal-union, a 'git revert', or a hand-edit. The write gate won't let you"
say "make one directly (an Update to a missing task is rejected up front), so we"
say "reproduce the end state: create temp, update it, then remove its Create line from"
say "the log (a stand-in for that revert/merge - never hand-edit a real store)."
run ta create keep title="Keep me" status=open
run ta create temp title="Temporary" status=open
run ta update temp status=closed
# Drop temp's Create line (simulating a revert/merge removal), orphaning the Update.
sed -i '/"op":"Create"[^}]*"task_id":"temp"/d' .taska/mutations.jsonl

say "Any READ command warns about orphans on STDERR (it never blocks the read)."
say "Watch for the 'taska: warning: ... orphaned event(s)' line:"
run ta list

say "'ta resolve --force' names each orphaned event and drops it from the log."
run ta resolve --force

say "The warning is gone - the log no longer carries the orphan."
run ta list
say "Orphans surface loudly instead of failing silently, and resolve cleans them up safely."
