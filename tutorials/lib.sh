# tutorials/lib.sh — shared helpers for the taska tutorial scripts.
#
# Source this from every scenario script:
#
#   source "$(dirname "$0")/lib.sh"
#
# It requires the `ta` binary on PATH, and provides four helpers the scripts
# narrate with: `say` (narration), `run` (echo + run a command), `pause` (wait
# for Enter, skipped when non-interactive), and `fresh_repo` (a throwaway git
# repo with an initialized taska store, OUTSIDE this project tree).
#
# Set TUTORIAL_NONINTERACTIVE=1 (or pipe stdin from a non-TTY) to run unattended:
# every `pause` returns immediately so the whole script runs end to end in CI.

# Be strict, but DON'T set -e: several scenarios intentionally run commands that
# fail (a surfaced merge conflict, a declined confirmation), and the narration
# must continue past them. Each script handles those cases explicitly.
set -u

# --- colors (disabled when stdout is not a terminal) -----------------------
if [ -t 1 ]; then
  _C_BOLD=$'\033[1m'
  _C_CYAN=$'\033[36m'
  _C_DIM=$'\033[2m'
  _C_RESET=$'\033[0m'
else
  _C_BOLD=""
  _C_CYAN=""
  _C_DIM=""
  _C_RESET=""
fi

# Require `ta` on PATH up front with a clear, actionable message.
if ! command -v ta >/dev/null 2>&1; then
  printf '%s\n' "error: the \`ta\` binary is not on your PATH." >&2
  printf '%s\n' "Build it and add it to PATH, e.g.:" >&2
  printf '%s\n' "    cargo build && export PATH=\"\$PWD/target/debug:\$PATH\"" >&2
  printf '%s\n' "then re-run this tutorial." >&2
  exit 1
fi

# say <text...> — print a line of narration in a distinct bold style.
say() {
  printf '%s## %s%s\n' "$_C_BOLD$_C_CYAN" "$*" "$_C_RESET"
}

# run <cmd...> — echo the command prefixed with `$ `, run it for real so the
# learner sees the actual output, then print a trailing blank line. The command's
# own exit status is preserved (returned), so a caller can react to a failure.
run() {
  printf '%s$ %s%s\n' "$_C_DIM" "$*" "$_C_RESET"
  "$@"
  local status=$?
  printf '\n'
  return $status
}

# pause — wait for the learner to press Enter before the next section. Skipped
# (returns immediately) when stdin is not a TTY or TUTORIAL_NONINTERACTIVE=1, so
# the scripts run unattended in tests/CI.
pause() {
  if [ "${TUTORIAL_NONINTERACTIVE:-0}" = "1" ] || [ ! -t 0 ]; then
    return 0
  fi
  printf '%s(press Enter to continue)%s ' "$_C_DIM" "$_C_RESET"
  read -r _ || true
}

# fresh_repo — create a throwaway repo OUTSIDE the project tree and cd into it.
#
# Using mktemp -d (which lives under $TMPDIR, not this checkout) is deliberate:
# `ta` discovers its store by walking UP the directory tree, so running inside
# the project would find the project's own `.taska/` instead of a clean one.
#
# Sets up: an empty git repo on `main`, a throwaway git identity (so commits
# work without touching the user's global config), and an initialized taska store.
fresh_repo() {
  local dir
  dir=$(mktemp -d "${TMPDIR:-/tmp}/taska-tutorial.XXXXXX")
  cd "$dir" || exit 1
  git init -q -b main
  git config user.email "tutorial@example.com"
  git config user.name "Taska Tutorial"
  # Quietly initialize the store + register the git merge driver locally.
  ta init >/dev/null
  say "Working in a throwaway repo: $dir"
  printf '\n'
}
