#!/usr/bin/env bash
# 03-schemas.sh - per-type schemas: the write gate, constraints, defaults, and
# migrating a legacy untyped store into a schema with `ta repair`.
#
# taska starts schema-agnostic: any field name, any value. Declaring
# [task_types.<name>] turns on a per-type schema enforced on every write -
# whole-task, with EVERY violation reported in one error - plus constraints and
# a default life-cycle. This walks that arc, then adopts two legacy untyped tasks
# into the schema and ratchets the untyped policy allow -> warn -> deny.
source "$(dirname "$0")/lib.sh"

fresh_repo

say "A fresh store is schema-agnostic: any field name, any value is accepted."
run ta create parser title="Parser crashes on EOF" severity=high
run ta create login title="Login 500s under load" severity=low
say "Neither has a 'type' - these are our 'legacy' tasks for the migration later."

say "Declare a schema for a 'bug' type by adding [task_types.bug] to .taska/config.toml:"
SCHEMA='
[task_types.bug]
closed = true   # no fields beyond the declared ones
fields = {
  title    = { type = "string", required = true, min_len = 3 },
  severity = { type = "enum", values = ["low", "medium", "high"], required = true, default = "low" },
  points   = { type = "uint", min = 1, max = 13 },
  owner    = { type = "string", pattern = "^[a-z]+$" },
  tags     = "set<string>",
}'
printf '%s\n\n' "$SCHEMA"
printf '%s\n' "$SCHEMA" >>.taska/config.toml
say "Schemas default to untyped_tasks=deny; a careful migration starts in 'allow'"
say "so the existing untyped tasks keep working while we add typed ones."
run ta config set workflow.untyped_tasks allow

say "The write gate checks the WHOLE task on create and reports EVERY violation at"
say "once, not just the first. This create breaks four rules:"
run ta create timeout type=bug title=x severity=critical points=99 owner=Bob123 || true
say "title under min_len 3, severity outside the enum, points over max 13, owner"
say "failing the ^[a-z]+\$ pattern. Fix them all and the same create is accepted:"
run ta create timeout type=bug title="Request times out" severity=high points=8 owner=dana

say "Numeric and set fields take +=/-=, dispatched by the declared kind: points is"
say "a uint so += ADDS to it; tags is a set<string> so += INSERTS a member."
run ta update timeout points+=2 tags+=regression
run ta update timeout tags+=flaky
run ta update timeout tags-=flaky
run ta show timeout
say "points 8 -> 10, and tags settled at {regression} after adding then dropping flaky."

say "Defaults have a life-cycle. severity defaults to 'low' and is STAMPED at create"
say "when omitted, so a typed bug always has one:"
run ta create crash type=bug title="Segfault in handler"
run ta show crash
say "(severity: low, though we never set it.)"

say "Now the migration. The two legacy tasks (parser, login) are still untyped;"
say "in 'allow' mode they're sanctioned - never reported:"
run ta list --columns id,type,title --sort id

say "Step the policy up to 'warn': reads now flag the untyped tasks on stderr,"
say "without blocking anything."
run ta config set workflow.untyped_tasks warn
run ta list --columns id,type,title --sort id

say "Adopt them into the schema with one repair pass - type every untyped task as"
say "'bug'. Defaults are healed in; both already have title + severity, so they"
say "conform immediately."
run ta repair --schema --set-type-if-none bug
run ta list --columns id,type,title --sort id

say "Every task now has a conforming type, so close the ladder at 'deny':"
say "from here an untyped write is rejected outright."
run ta config set workflow.untyped_tasks deny
run ta list --columns id,type,title --sort id
say "Schema declared, data migrated, gate fully on - the store is self-describing."
