#!/usr/bin/env bash
# 08-revert-convergence.sh - `git revert` of task events converges either merge way.
#
# Because the merge driver UNIONS both sides' removals, a reverted commit stays
# reverted no matter which direction you merge. We prove it by merging the same
# divergence both ways and showing the materialized task set is identical.
source "$(dirname "$0")/lib.sh"

fresh_repo

say "Commit a baseline task, then commit two more (b and c) as one commit."
run ta create a title="Keep me" status=open
run git add .taska .gitattributes
run git commit -q -m "track a"
run ta create b title="Revert me" status=open
run ta create c title="Revert me too" status=open
run git commit -q -am "add b and c"
TARGET=$(git rev-parse HEAD)

say "'git revert' that commit. Reverting removes b and c's events, leaving a GAP in the log."
run git revert --no-edit "$TARGET"
say "Only 'a' remains:"
run ta list --columns id,title

say "Branch 'feature' from BEFORE the revert (so it still has b and c) and add a new task d."
run git checkout -q -b feature "$TARGET"
run ta create d title="New work" status=open
run git commit -q -am "add d on feature"

say "Direction 1: merge feature INTO main. The driver unions main's removal of b,c..."
run git checkout -q main
run git merge feature -m "merge feature into main" || true
RESULT_MAIN=$(ta list --columns id --format json)
run ta list --columns id,title

say "Direction 2: merge main INTO feature instead."
run git checkout -q feature
run git merge main -m "merge main into feature" || true
RESULT_FEATURE=$(ta list --columns id --format json)
run ta list --columns id,title

say "Compare the two results. Reverts CONVERGE: b,c stay gone, a and d survive - both ways."
if [ "$RESULT_MAIN" = "$RESULT_FEATURE" ]; then
  say "IDENTICAL. The merge direction did not matter."
else
  say "MISMATCH (unexpected!): main=$RESULT_MAIN feature=$RESULT_FEATURE"
fi
