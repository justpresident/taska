#!/usr/bin/env bash
# 01-basics.sh — creating tasks with arbitrary fields, and the ways to view them.
source "$(dirname "$0")/lib.sh"

fresh_repo

say "taska tasks are just an id plus arbitrary key=value fields — no fixed schema."
say "Values parse as JSON when they can (priority=3 is a number; status=open a string)."
run ta create migrate-db title="Run DB migration" status=open priority=2
run ta create deploy-api title="Deploy the API" status=open priority=1 owner=alice
run ta create write-docs title="Write the docs" status=todo
pause

say "'ta list' renders an aligned table of the configured columns (id, title, status, deps)."
run ta list
pause

say "Pick columns for one run with --columns (overrides the configured default)."
run ta list --columns id,priority,status
pause

say "'--full' shows every field any task has — note 'owner' appears only where set."
run ta list --full
pause

say "'--format json' emits the same fields as a parseable array — ideal for agents or jq."
run ta list --format json
pause

say "'ta list' filters by AND-combined criteria: '=' exact, '~' regex, '!=' / '!~' negated."
run ta list status=open
run ta list 'title~API' status=open
pause

say "'ta show <id>' shows a single task with ALL of its own fields."
run ta show deploy-api

say "That's the read surface: list / show, each with --columns / --full / --format."
