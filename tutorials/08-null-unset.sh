#!/usr/bin/env bash
# 08-null-unset.sh — removing a field entirely with the null convention.
#
# 'ta update <id> field=null' UNSETS a field (vs. setting it to some value). The
# field then disappears from list / show / search — it's gone, not blanked.
source "$(dirname "$0")/lib.sh"

fresh_repo

say "Create a task that has an 'owner' field."
run ta create ticket title="Investigate outage" status=open owner=alice
run ta show ticket
pause

say "'ta search owner=alice' finds it while the field is set."
run ta search owner=alice
pause

say "Unset the field with the null convention: 'ta update <id> owner=null'."
say "(null is JSON null, not the string \"null\" — it removes the key entirely.)"
run ta update ticket owner=null
pause

say "'ta show' confirms the owner column is gone — the field no longer exists on the task."
run ta show ticket
pause

say "It's gone from 'ta list --full' too (no OWNER column, since no task has the field)."
run ta list --full
pause

say "And 'ta search owner=alice' now finds nothing — the field is truly unset, not blank."
run ta search owner=alice
say "field=null is how you delete a field; everything else replays as if it were never set."
