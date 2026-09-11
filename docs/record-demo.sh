#!/usr/bin/env bash
# Record the README demo end to end: throwaway repo -> scripted session -> GIF.
#
#   bash docs/record-demo.sh                # record docs/demo.gif
#   bash docs/record-demo.sh --preview      # watch it run, record nothing
#   bash docs/record-demo.sh --keep         # leave the throwaway repo in place
#   bash docs/record-demo.sh --cols 100     # a wider recording
#   bash docs/record-demo.sh --social       # a short, feed-sized cut
#   bash docs/record-demo.sh --from-cast docs/demo.cast   # re-render, don't re-record
#
# Everything happens in a fresh `mktemp -d` repo that is deleted afterwards, so
# this never touches the taska store you are actually working in.
#
# FRAME COUNT. The typing is simulated keystroke by keystroke, so a GIF frame is
# roughly one typed CHARACTER - the full demo lands near 520 frames, and LinkedIn
# rejects a GIF over 400. --speed and --fps-cap merge keystrokes into shared
# frames; measured against the current cast:
#
#     speed  fps-cap   frames   length
#       1       30       534      66s    (defaults - README only)
#       1        6       320      66s
#     1.5       10       353      44s
#       2       10       270      33s    (--social)
#
# Re-rendering is cheap and needs no recording: --from-cast takes an existing
# .cast, so a cast captured once can be rendered for the README and for a feed.
set -euo pipefail

COLS=80
ROWS=24
PREVIEW=0
KEEP=0
OUT=""
FROM_CAST=""
SPEED=""
FPS_CAP=""
IDLE_LIMIT=""
SOCIAL=0

die() { printf 'record-demo: %s\n' "$*" >&2; exit 1; }
note() { printf '\033[36m==>\033[0m %s\n' "$*" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --preview) PREVIEW=1; shift ;;
    --keep)    KEEP=1; shift ;;
    --cols)    COLS="${2:?--cols needs a number}"; shift 2 ;;
    --rows)    ROWS="${2:?--rows needs a number}"; shift 2 ;;
    --out)     OUT="${2:?--out needs a path}"; shift 2 ;;
    --from-cast) FROM_CAST="${2:?--from-cast needs a path}"; shift 2 ;;
    --speed)   SPEED="${2:?--speed needs a number}"; shift 2 ;;
    --fps-cap) FPS_CAP="${2:?--fps-cap needs a number}"; shift 2 ;;
    # A feed-sized cut. GIF frames are roughly one per typed CHARACTER, because
    # the typing is simulated keystroke by keystroke - which is why the full demo
    # lands around 520 frames and LinkedIn rejects anything over 400. Speeding up
    # and capping the frame rate merges those keystrokes into shared frames; the
    # result is ~270 frames and about half the length, which suits a feed anyway.
    --social)  SOCIAL=1; shift ;;
    -h|--help) awk 'NR>1 && /^#/ {sub(/^# ?/, ""); print; next} NR>1 {exit}' "$0"; exit 0 ;;
    *)         die "unknown option '$1' (try --help)" ;;
  esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$REPO/docs/demo.script"
SETUP="$REPO/docs/demo-setup.sh"
if [ "$SOCIAL" -eq 1 ]; then
  OUT="${OUT:-$REPO/docs/demo-social.gif}"
  SPEED="${SPEED:-2}"
  FPS_CAP="${FPS_CAP:-10}"
  IDLE_LIMIT="${IDLE_LIMIT:-2}"
fi
OUT="${OUT:-$REPO/docs/demo.gif}"
CAST="${FROM_CAST:-${OUT%.gif}.cast}"

# Rendering is the same whether we just recorded the cast or were handed one.
# --cols re-renders at the pinned width even when the cast was captured in a
# wider window, which is how a recording made in someone's 115-column terminal
# stops carrying a third of a frame in dead space.
render() {
  command -v agg >/dev/null 2>&1 || die "agg not found (renders the cast to a GIF) - install with:
    cargo install --git https://github.com/asciinema/agg"
  [ -f "$CAST" ] || die "no cast at $CAST - record one first, or pass --from-cast <path>"
  local args=(--cols "$COLS")
  if [ -n "$SPEED" ];      then args+=(--speed "$SPEED"); fi
  if [ -n "$FPS_CAP" ];    then args+=(--fps-cap "$FPS_CAP"); fi
  if [ -n "$IDLE_LIMIT" ]; then args+=(--idle-time-limit "$IDLE_LIMIT"); fi
  note "rendering $OUT  (agg ${args[*]})"
  agg "${args[@]}" "$CAST" "$OUT"
  note "done: $OUT ($(du -h "$OUT" | cut -f1))"
}

# Re-render only: no repo, no shell, no recording - just the cast we were given.
if [ -n "$FROM_CAST" ]; then
  render
  exit 0
fi

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

# The narration is grey, which needs the `\e` string escape. A scriptty without
# it renders the codes as literal text, so probe for it rather than trusting the
# version: run a one-line script and look for a real ESC byte in the output.
esc_probe="$(mktemp "${TMPDIR:-/tmp}/scriptty-esc.XXXXXX")"
printf 'show "\\e[90mprobe\\e[0m"\n' > "$esc_probe"
esc_ok=0
"$SCRIPTTY_BIN" --script "$esc_probe" --command true 2>/dev/null | grep -q "$(printf '\033')\[90m" && esc_ok=1
rm -f "$esc_probe"
[ "$esc_ok" -eq 1 ] || die "this scriptty ($SCRIPTTY_BIN) does not support the \`\\e\` string
escape, so the demo's grey narration would render as literal text. Install one
that does:
    cargo install --git https://github.com/justpresident/scriptty"

if [ "$PREVIEW" -eq 0 ]; then
  command -v asciinema >/dev/null 2>&1 || die "asciinema not found - install it, use --preview to just
watch, or --from-cast <path> to re-render a cast you already have"
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

render
