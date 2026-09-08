#!/usr/bin/env bash
# Prepares a throwaway repo for `vhs docs/demo.tape`. Not part of the test suite;
# the tape calls it inside a Hide block so setup never appears in the recording.
#
#   TA=/path/to/ta bash docs/demo-setup.sh [dir]     # defaults to /tmp/taska-demo
set -euo pipefail

DEMO_DIR="${1:-/tmp/taska-demo}"
TA="${TA:-ta}"

rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"

git init -q -b main
git config user.email "demo@example.com"
git config user.name "taska demo"

# `ta init` creates the store, registers the merge driver and commits both.
"$TA" init >/dev/null
