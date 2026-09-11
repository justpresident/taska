#!/usr/bin/env bash
# Prepares a throwaway repo for `scriptty --script docs/demo.script`. Not part of
# the test suite. Also writes the rcfile the demo shell starts from, so the
# recording shows a bare `$ ` prompt and no setup commands.
#
#   TA=/path/to/ta bash docs/demo-setup.sh [dir]     # defaults to /tmp/taska-demo
#
# With --rc-only <cwd>, no repo or store is created: it writes just the demo
# shell's rcfile, pointing at an existing directory. That is how the read-only
# demos run against taska's OWN store - the whole point of those being that the
# tasks on screen are real ones.
set -euo pipefail

RC_ONLY=""
if [ "${1:-}" = "--rc-only" ]; then
  RC_ONLY="${2:?--rc-only needs the directory the demo shell should start in}"
  shift 2
fi

DEMO_DIR="${1:-/tmp/taska-demo}"
TA="${TA:-ta}"

# Resolve `ta` to an absolute path now, so the demo shell finds the intended
# binary rather than whatever a login shell would put on PATH.
TA_BIN="$(command -v "$TA")" || { echo "no \`$TA\` on PATH - build it first" >&2; exit 1; }
TA_DIR="$(cd "$(dirname "$TA_BIN")" && pwd)"

if [ -n "$RC_ONLY" ]; then
  mkdir -p "$DEMO_DIR"
  START_DIR="$RC_ONLY"
else
  rm -rf "$DEMO_DIR"
  mkdir -p "$DEMO_DIR"
  cd "$DEMO_DIR"

  git init -q -b main
  git config user.email "demo@example.com"
  git config user.name "taska demo"

  # `ta init` creates the store, registers the merge driver and commits both.
  "$TA_BIN" init >/dev/null
  START_DIR="$DEMO_DIR"
fi

# The shell the demo runs in: bare prompt, no history clutter, already in the repo.
cat > "$DEMO_DIR/.demo-bashrc" <<RC
# Which branch we are on is half the story this demo tells, so the prompt shows
# it. Evaluated per prompt via \$(...), and silent outside a repo. Yellow is the
# conventional colour for it and collides with nothing taska paints (cyan ids,
# green status, grey cursor).
_branch() {
  local b
  b=\$(git rev-parse --abbrev-ref HEAD 2>/dev/null) || return 0
  printf '(%s) ' "\$b"
}
# The trailing space of the prompt stays INSIDE the green span: with the reset
# in between, the literal "\$ " is no longer contiguous and every
# expect "\$ " in docs/demo.script stops matching.
PS1='\[\e[33m\]\$(_branch)\[\e[0m\]\[\e[1;32m\]\$ \[\e[0m\]'
unset PROMPT_COMMAND
# A minimal PATH with exactly ONE \`ta\` on it. Inheriting the recorder's PATH
# risks a second, older \`ta\` (a cargo-installed one, say), and taska warns
# about shadowing binaries on every single command - which buries the demo.
export PATH="$TA_DIR:/usr/local/bin:/usr/bin:/bin"
export GIT_PAGER=cat
# Bracketed paste writes \`\\e[?2004h\` around every prompt; harmless on screen
# but noise in a recording.
bind 'set enable-bracketed-paste off' 2>/dev/null
cd "$START_DIR"
clear
RC
