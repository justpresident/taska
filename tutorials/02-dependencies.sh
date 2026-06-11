#!/usr/bin/env bash
# 02-dependencies.sh — dependency edges, 'ta list --ready', and unblocking.
source "$(dirname "$0")/lib.sh"

fresh_repo

say "Create three tasks. deploy-api and smoke-test both depend on migrate-db finishing."
run ta create migrate-db title="Run DB migration" status=open
run ta create deploy-api title="Deploy the API" status=open
run ta create smoke-test title="Smoke-test prod" status=open

say "'ta dep add <task> depends_on=<target>' adds a dependency edge."
run ta dep add deploy-api depends_on=migrate-db
run ta dep add smoke-test depends_on=deploy-api

say "Now 'ta list' shows the DEPS column wired up, and 'ta dep tree' draws the graph."
run ta list
run ta dep tree

say "'ta show' surfaces a task's relationships both ways: deploy-api depends on migrate-db, and (inverse) blocks smoke-test."
run ta show deploy-api

say "'ta list --ready' shows only NOT-done tasks whose dependencies are all done."
say "Right now only migrate-db is actionable — the others are blocked."
run ta list --ready

say "Close the migration (status=closed is the configured 'done' value)."
run ta update migrate-db status=closed

say "deploy-api now unblocks — its only dependency is done. smoke-test still waits on deploy-api."
run ta list --ready

say "Dependencies form a DAG; 'ta list --ready' walks it and surfaces exactly what you can start now."
