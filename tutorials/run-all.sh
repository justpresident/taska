#!/usr/bin/env bash
# run-all.sh — run every tutorial script in order.
#
# Interactivity passes straight through: by default each script pauses between
# sections (when run from a terminal). Set TUTORIAL_NONINTERACTIVE=1 to run the
# whole sequence unattended (e.g. in CI), in which case the pauses are skipped.
#
# Requires the `ta` binary on PATH (see README.md).
set -u

DIR="$(cd "$(dirname "$0")" && pwd)"

# Run the numbered scenario scripts in order. Globbing in sorted order gives
# 01..09; lib.sh / run-all.sh / README are excluded by the numeric prefix.
scripts=("$DIR"/[0-9][0-9]-*.sh)

for script in "${scripts[@]}"; do
  printf '\n========================================================================\n'
  printf '  %s\n' "$(basename "$script")"
  printf '========================================================================\n\n'
  # Run in a subshell (`bash <script>`) so each scenario's cd into its throwaway
  # repo and its shell options can't leak into the next one.
  bash "$script"
  status=$?
  if [ "$status" -ne 0 ]; then
    printf '\n!! %s exited with status %s\n' "$(basename "$script")" "$status" >&2
    exit "$status"
  fi
done

printf '\nAll tutorials completed.\n'
