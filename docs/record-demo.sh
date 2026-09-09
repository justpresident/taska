#!/usr/bin/env bash
# Record the README demo end to end: throwaway repo -> scripted session -> GIF.
#
#   bash docs/record-demo.sh                # record docs/demo.gif
#   bash docs/record-demo.sh --preview      # watch it run, record nothing
#   bash docs/record-demo.sh --keep         # leave the throwaway repo in place
#   bash docs/record-demo.sh --cols 100     # a wider recording
#
# Everything happens in a fresh `mktemp -d` repo that is deleted afterwards, so
# this never touches the taska store you are actually working in.
set -euo pipefail

COLS=80
ROWS=24
PREVIEW=0
KEEP=0
OUT=""

die() { printf 'record-demo: %s\n' "$*" >&2; exit 1; }
note() { printf '\033[36m==>\033[0m %s\n' "$*" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --preview) PREVIEW=1; shift ;;
    --keep)    KEEP=1; shift ;;
    --cols)    COLS="${2:?--cols needs a number}"; shift 2 ;;
    --rows)    ROWS="${2:?--rows needs a number}"; shift 2 ;;
    --out)     OUT="${2:?--out needs a path}"; shift 2 ;;
    -h|--help) awk 'NR>1 && /^#/ {sub(/^# ?/, ""); print; next} NR>1 {exit}' "$0"; exit 0 ;;
    *)         die "unknown option '$1' (try --help)" ;;
  esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$REPO/docs/demo.script"
SETUP="$REPO/docs/demo-setup.sh"
OUT="${OUT:-$REPO/docs/demo.gif}"
CAST="${OUT%.gif}.cast"

[ -f "$SCRIPT" ] || die "missing $SCRIPT"
[ -f "$SETUP" ]  || die "missing $SETUP"

# A candidate binary counts only if it actually runs. A build left over from an
# older toolchain still looks executable but dies on a missing libc symbol, and
# resolving to one would silently record the wrong thing - or nothing.
runs_ok() { [ -x "$1" ] && "$1" --version >/dev/null 2>&1; }

# --- resolve `ta`: prefer this checkout's build over whatever is on PATH, so the
# --- demo can never record a stale installed binary.
resolve_ta() {
  local c
  for c in "$REPO/target/release/ta" "$REPO/target/debug/ta" "$(command -v ta 2>/dev/null || true)"; do
    [ -n "$c" ] && runs_ok "$c" && { printf '%s\n' "$c"; return 0; }
  done
  return 1
}

if [ -n "${TA:-}" ]; then
  # An explicit TA is an instruction, not a hint: fail rather than quietly
  # recording some other binary.
  TA_BIN="$(command -v "$TA" 2>/dev/null || printf '%s' "$TA")"
  runs_ok "$TA_BIN" || die "TA=$TA does not run (stale build, or wrong path)"
else
  TA_BIN="$(resolve_ta)" || die "no working \`ta\` found - run \`cargo build\` in $REPO first
(a build from an older toolchain can exist but fail to run; rebuilding fixes it)"
fi

# --- resolve scriptty: PATH, else a sibling checkout.
resolve_scriptty() {
  local c
  for c in "$(command -v scriptty 2>/dev/null || true)" \
           "$REPO/../scriptty/target/release/scriptty" \
           "$REPO/../scriptty/target/debug/scriptty"; do
    [ -n "$c" ] && runs_ok "$c" && { printf '%s\n' "$c"; return 0; }
  done
  return 1
}

if [ -n "${SCRIPTTY:-}" ]; then
  SCRIPTTY_BIN="$(command -v "$SCRIPTTY" 2>/dev/null || printf '%s' "$SCRIPTTY")"
  runs_ok "$SCRIPTTY_BIN" || die "SCRIPTTY=$SCRIPTTY does not run (stale build, or wrong path)"
else
  SCRIPTTY_BIN="$(resolve_scriptty)" || die "no working \`scriptty\` found - install it with:
    cargo install --git https://github.com/justpresident/scriptty"
fi

# The PTY-sizing fix is what keeps long lines from overwriting themselves; a
# scriptty without --cols pins every PTY to 80x24 no matter the real terminal.
"$SCRIPTTY_BIN" --help 2>&1 | grep -q -- '--cols' || die "this scriptty ($SCRIPTTY_BIN) has no --cols, so it
cannot size the PTY. Install one that does:
    cargo install --git https://github.com/justpresident/scriptty"

if [ "$PREVIEW" -eq 0 ]; then
  command -v asciinema >/dev/null 2>&1 || die "asciinema not found - install it, or use --preview to just watch"
  command -v agg >/dev/null 2>&1 || die "agg not found (renders the cast to a GIF) - install with:
    cargo install --git https://github.com/asciinema/agg"
fi

# Pinning the PTY WIDER than the real terminal is the case that corrupts output,
# so warn rather than silently record a broken demo.
term_cols="$(tput cols 2>/dev/null || echo 0)"
if [ "$term_cols" -gt 0 ] && [ "$term_cols" -lt "$COLS" ]; then
  note "warning: your terminal is ${term_cols} columns but the demo records at ${COLS}."
  note "         Widen it, or pass --cols ${term_cols}, or lines will redraw over each other."
fi

DEMO_DIR="$(mktemp -d "${TMPDIR:-/tmp}/taska-demo.XXXXXX")"
cleanup() { [ "$KEEP" -eq 1 ] && note "kept $DEMO_DIR" || rm -rf "$DEMO_DIR"; }
trap cleanup EXIT

note "using ta:       $TA_BIN ($("$TA_BIN" --version))"
note "using scriptty: $SCRIPTTY_BIN"
note "demo repo:      $DEMO_DIR"

TA="$TA_BIN" bash "$SETUP" "$DEMO_DIR" >/dev/null

run_demo=("$SCRIPTTY_BIN" --script "$SCRIPT" --cols "$COLS" --rows "$ROWS"
          --command bash -- --rcfile "$DEMO_DIR/.demo-bashrc" -i)

if [ "$PREVIEW" -eq 1 ]; then
  note "preview only - nothing will be written"
  "${run_demo[@]}"
  exit 0
fi

note "recording to $CAST"
rm -f "$CAST"
asciinema rec "$CAST" --overwrite --cols "$COLS" --rows "$ROWS" \
  --command "$(printf '%q ' "${run_demo[@]}")"

note "rendering $OUT"
agg "$CAST" "$OUT"

note "done: $OUT ($(du -h "$OUT" | cut -f1))"
note "add it to the README by replacing the DEMO SLOT comment with:"
note "    ![demo](docs/${OUT##*/})"
