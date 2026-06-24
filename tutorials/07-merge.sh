#!/usr/bin/env bash
# 07-merge.sh - concurrent edits on two branches, reconciled by the git merge driver.
#
# Shows three things:
#   1. a clean auto-merge (the branches touch DIFFERENT fields),
#   2. a real same-field conflict under on_conflict=surface (merge stops; ta resolve),
#   3. the same conflict resolved silently under theirs / latest.
#
# NOTE: in surface mode the `git merge` is SUPPOSED to fail (it stops for a human),
# so those calls are guarded with `|| true` and must not abort the script.
source "$(dirname "$0")/lib.sh"

# set_policy <surface|latest|ours|theirs> - rewrite [merge] on_conflict in config.
set_policy() {
  sed -i "s/^on_conflict = .*/on_conflict = \"$1\"/" .taska/config.toml
}

# ---------------------------------------------------------------------------
# 1. Clean auto-merge: two branches change DIFFERENT fields of the same task.
# ---------------------------------------------------------------------------
fresh_repo

say "Create a task and commit it, so both branches share a common ancestor."
run ta create feature-x title="Build feature X" status=open owner=alice
run git add .taska .gitattributes
run git commit -q -m "track feature-x"

say "Branch 'review' marks it done; meanwhile main reassigns the owner. DIFFERENT fields."
run git checkout -q -b review
run ta update feature-x status=closed
run git commit -q -am "review: mark done"
run git checkout -q main
run ta update feature-x owner=bob
run git commit -q -am "main: reassign owner"

say "Merge 'review' into main. Different fields don't collide - the driver merges cleanly."
run git merge review -m "merge review" || true

say "Both edits survived: status=closed AND owner=bob. No conflict, no human needed."
run ta show feature-x

# ---------------------------------------------------------------------------
# 2. Real same-field conflict under on_conflict=surface (the default).
# ---------------------------------------------------------------------------
fresh_repo

say "This time both branches set the SAME field (status) to different values."
say "The store is on the default policy: on_conflict = \"surface\"."
run grep on_conflict .taska/config.toml
run ta create release title="Cut the release" status=open
run git add .taska .gitattributes
run git commit -q -m "track release"

run git checkout -q -b qa
run ta update release status=in-review
run git commit -q -am "qa: in-review"
run git checkout -q main
run ta update release status=blocked
run git commit -q -am "main: blocked"

say "Under 'surface', a genuine conflict STOPS the merge for a human (this failure is expected)."
run git merge qa -m "merge qa" || true
say "git left the path unmerged - exactly as intended:"
run git status --short

say "'ta resolve' reports what was tentatively kept (ours) and points at how to finish."
run ta resolve --force || true

say "Accept the tentative merge: stage the reconciled log and commit."
run git add .taska/mutations.jsonl
run git commit -q --no-edit
run ta show release

# ---------------------------------------------------------------------------
# 3. The SAME conflict, resolved silently by a non-surface strategy.
# ---------------------------------------------------------------------------
for policy in theirs latest; do
  fresh_repo

  say "Same same-field conflict, but now on_conflict = \"$policy\" (set in .taska/config.toml)."
  set_policy "$policy"
  run grep on_conflict .taska/config.toml
  run ta create release title="Cut the release" status=open
  run git add .taska .gitattributes
  run git commit -q -m "track release"

  run git checkout -q -b qa
  run ta update release status=in-review
  run git commit -q -am "qa: in-review"
  run git checkout -q main
  run ta update release status=blocked
  run git commit -q -am "main: blocked"

  say "With '$policy', the driver resolves the field automatically and the merge SUCCEEDS."
  if [ "$policy" = "theirs" ]; then
    say "'theirs' keeps the branch merged IN (qa) -> expect status=in-review."
  else
    say "'latest' keeps the most recently written value (main was committed last) -> expect status=blocked."
  fi
  run git merge qa -m "merge qa"
  run ta show release
done

say "Recap: non-overlapping edits always merge; same-field conflicts follow on_conflict -"
say "surface (stop + ta resolve), or theirs / latest / ours to resolve automatically."
