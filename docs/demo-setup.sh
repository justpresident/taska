#!/usr/bin/env bash
# Prepares a throwaway repo for `scriptty --script docs/demo.script`. Not part of
# the test suite. Also writes the rcfile the demo shell starts from, so the
# recording shows a bare `$ ` prompt and no setup commands.
#
#   TA=/path/to/ta bash docs/demo-setup.sh [dir]     # defaults to /tmp/taska-demo
set -euo pipefail

DEMO_DIR="${1:-/tmp/taska-demo}"
TA="${TA:-ta}"

# Resolve `ta` to an absolute path now, so the demo shell finds the intended
# binary rather than whatever a login shell would put on PATH.
TA_BIN="$(command -v "$TA")" || { echo "no \`$TA\` on PATH - build it first" >&2; exit 1; }
TA_DIR="$(cd "$(dirname "$TA_BIN")" && pwd)"

rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR"
cd "$DEMO_DIR"

git init -q -b main
git config user.email "demo@example.com"
git config user.name "taska demo"

# `ta init` creates the store, registers the merge driver and commits both.
"$TA_BIN" init >/dev/null

# The shell the demo runs in: bare prompt, no history clutter, already in the repo.
cat > "$DEMO_DIR/.demo-bashrc" <<RC
PS1='\$ '
unset PROMPT_COMMAND
# A minimal PATH with exactly ONE \`ta\` on it. Inheriting the recorder's PATH
# risks a second, older \`ta\` (a cargo-installed one, say), and taska warns
# about shadowing binaries on every single command - which buries the demo.
export PATH="$TA_DIR:/usr/local/bin:/usr/bin:/bin"
export GIT_PAGER=cat
# Bracketed paste writes \`\\e[?2004h\` around every prompt; harmless on screen
# but noise in a recording.
bind 'set enable-bracketed-paste off' 2>/dev/null
cd "$DEMO_DIR"
clear
RC
