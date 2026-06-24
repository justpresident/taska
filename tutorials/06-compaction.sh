#!/usr/bin/env bash
# 06-compaction.sh - folding old events into the baseline snapshot.
#
# The log is append-only, so it grows forever. 'ta compact' folds the OLD prefix
# into a baseline.jsonl snapshot, keeping only the most recent events in the log
# (enough that divergent branches can still be reconciled).
#
# keep_events has a production FLOOR of 300, so to demonstrate folding we create
# ~320 events and set keep_events accordingly. We also set keep_days=0: otherwise
# the time window would retain everything created today and nothing would fold.
source "$(dirname "$0")/lib.sh"

# Helpers used via `run` so the learner sees a clean command instead of a wall of
# nested quoting. They run in this shell, so they see the throwaway repo's cwd.
log_sizes() {
  printf 'mutations.jsonl: %s lines\n' "$(wc -l < .taska/mutations.jsonl)"
  printf 'baseline.jsonl:  %s lines\n' "$(wc -l < .taska/baseline.jsonl)"
}
task_count() {
  printf 'tasks visible: %s\n' "$(ta list --format json | grep -c '"id"')"
}

fresh_repo

say "Tune retention for the demo: keep_events=300 (the production floor) and keep_days=0."
say "(keep_days=0 disables the time window, which would otherwise retain today's events.)"
run sed -i 's/^keep_events = .*/keep_events = 300/; s/^keep_days = .*/keep_days = 0/' .taska/config.toml
run grep -E 'keep_events|keep_days' .taska/config.toml

say "Create 320 tasks -> 320 events in the append-only log, baseline still empty."
for i in $(seq 1 320); do ta create "task-$i" status=open >/dev/null; done
run log_sizes

say "'ta compact' folds everything but the most recent 300 events into the baseline."
run ta compact

say "Now the log holds the 300 retained events and the baseline holds the 20 folded ones."
run log_sizes

say "Task state is unchanged - compaction is invisible to what you see. All 320 are still here."
run task_count
say "Compaction keeps the log small while preserving every task and enough history to merge."
