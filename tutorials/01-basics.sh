#!/usr/bin/env bash
# 01-basics.sh - creating tasks with arbitrary fields, viewing them, and unsetting fields.
source "$(dirname "$0")/lib.sh"

fresh_repo

say "taska tasks are just an id plus arbitrary key=value fields - no fixed schema."
say "Values parse as JSON when they can (priority=3 is a number; status=open a string)."
run ta create migrate-db title="Run DB migration" status=open priority=2
say "Fields are free-form, but a NAME no task uses yet needs --new-field (a typo guard,"
say "so 'titel' can't silently become a column). migrate-db seeded title/status/priority;"
say "'owner' is new here, so we opt in:"
run ta create deploy-api --new-field title="Deploy the API" status=open priority=1 owner=alice
run ta create write-docs title="Write the docs" status=todo

say "'ta list' renders an aligned table of the configured columns (id, title, status, deps)."
run ta list

say "Pick columns for one run with --columns (overrides the configured default)."
run ta list --columns id,priority,status

say "'--full' shows every field any task has - note 'owner' appears only where set."
run ta list --full

say "'--format json' emits the same fields as a parseable array - ideal for agents or jq."
run ta list --format json

say "'ta list' filters by AND-combined criteria: '=' exact, '=~' regex, '!=' / '!~' negated."
run ta list status=open
run ta list 'title=~API' status=open

say "'ta show <id>' shows a single task with ALL of its own fields."
run ta show deploy-api

say "'ta list owner=alice' finds deploy-api while the field is set."
run ta list owner=alice

say "Unset a field with the null convention: 'ta update <id> owner=null'."
say "(null is JSON null, not the string \"null\" - it removes the key entirely.)"
run ta update deploy-api owner=null

say "'ta show' confirms the owner field is gone - it no longer exists on the task."
run ta show deploy-api

say "It's gone from 'ta list --full' too (no OWNER column, since no task has the field)."
run ta list --full

say "And 'ta list owner=alice' now finds nothing - the field is truly unset, not blank."
run ta list owner=alice

say "That's the read surface: list / show, each with --columns / --full / --format, and field=null to delete a field."
