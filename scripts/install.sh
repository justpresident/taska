#!/usr/bin/env bash
#
# taska (`ta`) installer — download a prebuilt binary for this OS/arch from the
# latest GitHub release, verify its checksum, and drop it on your PATH.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/justpresident/taska/master/scripts/install.sh | bash
#
# This script must be EXECUTED, not SOURCED (it calls `exit` on errors).
#   ✅  curl -fsSL … | bash        ✅  bash install.sh        ❌  source install.sh
#
# Environment overrides:
#   TASKA_VERSION      release tag to install (default: the latest), e.g. v0.5.0
#   TASKA_INSTALL_DIR  directory to install `ta` into
#                      (default: /usr/local/bin if writable, else ~/.local/bin)
set -euo pipefail

REPO="justpresident/taska"
BIN="ta"      # the binary
CRATE="taska" # the crates.io package (the cargo fallback)

# --- logging (to stderr, so stdout stays clean for scripting) --------------
if [ -t 2 ]; then
  C_BLUE=$'\033[0;34m'; C_GREEN=$'\033[0;32m'; C_YELLOW=$'\033[1;33m'; C_RED=$'\033[0;31m'; C_OFF=$'\033[0m'
else
  C_BLUE=''; C_GREEN=''; C_YELLOW=''; C_RED=''; C_OFF=''
fi
info() { printf '%s==>%s %s\n' "$C_BLUE" "$C_OFF" "$1" >&2; }
ok()   { printf '%s==>%s %s\n' "$C_GREEN" "$C_OFF" "$1" >&2; }
warn() { printf '%s==>%s %s\n' "$C_YELLOW" "$C_OFF" "$1" >&2; }
die()  { printf '%sError:%s %s\n' "$C_RED" "$C_OFF" "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- download / fetch via curl or wget -------------------------------------
download() { # download <url> <out-file>
  if have curl; then curl -fsSL -o "$2" "$1"
  else wget -qO "$2" "$1"; fi
}
fetch() { # fetch <url> -> stdout
  if have curl; then curl -fsSL "$1"
  else wget -qO- "$1"; fi
}

# --- sha256 of a file, via whatever tool is present ------------------------
sha256() { # sha256 <file> -> hex on stdout, or non-zero if no tool
  if have sha256sum; then sha256sum "$1" | awk '{print $1}'
  elif have shasum;   then shasum -a 256 "$1" | awk '{print $1}'
  elif have openssl;  then openssl dgst -sha256 "$1" | awk '{print $NF}'
  else return 1; fi
}

# --- detect the release target triple --------------------------------------
# Sets TARGET on success; returns 1 when there's no prebuilt for this platform
# (the caller then falls back to `cargo install`); dies on a truly unsupported one.
detect_target() {
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      die "Windows is not covered by the prebuilt binaries — use WSL, or 'cargo install ${CRATE}'" ;;
  esac
  local os arch
  case "$(uname -s)" in
    Linux) os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *) die "unsupported OS '$(uname -s)' — prebuilt binaries cover Linux and macOS" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) die "unsupported architecture '$(uname -m)'" ;;
  esac
  # Releases ship x86_64 for Linux, and both arches for macOS.
  if [ "$os" = "unknown-linux-musl" ] && [ "$arch" != "x86_64" ]; then
    return 1
  fi
  TARGET="${arch}-${os}"
}

# --- resolve the version tag (latest, unless TASKA_VERSION is set) ----------
resolve_version() {
  if [ -n "${TASKA_VERSION:-}" ]; then VERSION="$TASKA_VERSION"; return; fi
  local json
  json="$(fetch "https://api.github.com/repos/$REPO/releases/latest")" \
    || die "couldn't reach the GitHub releases API for $REPO"
  VERSION="$(printf '%s' "$json" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$VERSION" ] || die "no published release found for $REPO (set TASKA_VERSION, or 'cargo install ${CRATE}')"
}

# --- fall back to building from crates.io ----------------------------------
fallback_cargo() {
  if have cargo; then
    warn "No prebuilt binary for this platform — installing from crates.io with cargo."
    cargo install "$CRATE" && { ok "Installed via 'cargo install ${CRATE}'."; exit 0; }
    die "'cargo install ${CRATE}' failed"
  fi
  die "no prebuilt binary for this platform, and cargo is not installed. Install Rust (https://rustup.rs), then: cargo install ${CRATE}"
}

main() {
  printf '\n  taska (%s) installer\n\n' "$BIN" >&2
  have tar || die "tar is required"
  have curl || have wget || die "need curl or wget"

  detect_target || fallback_cargo
  resolve_version
  info "Installing ${BIN} ${VERSION} (${TARGET})"

  local asset base tmp
  asset="${BIN}-${VERSION}-${TARGET}.tar.gz"
  base="https://github.com/${REPO}/releases/download/${VERSION}"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  info "Downloading ${asset}"
  download "${base}/${asset}" "${tmp}/${asset}" || { warn "download failed (${base}/${asset})"; fallback_cargo; }

  # Verify against the sibling .sha256, when both it and a hash tool exist.
  if download "${base}/${asset}.sha256" "${tmp}/${asset}.sha256" 2>/dev/null; then
    local want got
    want="$(awk '{print $1; exit}' "${tmp}/${asset}.sha256")"
    if got="$(sha256 "${tmp}/${asset}")"; then
      [ "$want" = "$got" ] || die "checksum mismatch for ${asset} — refusing to install"
      ok "Checksum verified"
    else
      warn "no sha256 tool (sha256sum/shasum/openssl) — skipping checksum verification"
    fi
  else
    warn "no checksum published for ${asset} — skipping verification"
  fi

  tar -xzf "${tmp}/${asset}" -C "$tmp" || die "failed to extract ${asset}"
  local src="${tmp}/${BIN}-${VERSION}-${TARGET}/${BIN}"
  [ -f "$src" ] || src="$(find "$tmp" -type f -name "$BIN" | head -n1)"
  [ -n "${src:-}" ] && [ -f "$src" ] || die "the archive did not contain the ${BIN} binary"

  local dir
  if [ -n "${TASKA_INSTALL_DIR:-}" ]; then dir="$TASKA_INSTALL_DIR"
  elif [ -w /usr/local/bin ]; then dir="/usr/local/bin"
  else dir="${HOME}/.local/bin"; fi
  mkdir -p "$dir" || die "couldn't create ${dir}"
  cp "$src" "${dir}/${BIN}" || die "couldn't write ${dir}/${BIN} — set TASKA_INSTALL_DIR to a writable directory"
  chmod 0755 "${dir}/${BIN}"

  # Clear the macOS Gatekeeper quarantine flag on the unsigned binary, if set.
  [ "$(uname -s)" = "Darwin" ] && xattr -d com.apple.quarantine "${dir}/${BIN}" 2>/dev/null || true

  ok "Installed ${BIN} to ${dir}/${BIN}"

  case ":${PATH:-}:" in
    *":${dir}:"*) ;;
    *)
      warn "${dir} is not on your PATH — add it to your shell profile (~/.bashrc, ~/.zshrc, …):"
      printf '    export PATH="%s:$PATH"\n' "$dir" >&2 ;;
  esac

  printf '\n' >&2
  "${dir}/${BIN}" --version >&2 2>/dev/null || true
  printf '\nGet started:\n  cd your-repo\n  %s init\n  %s create my-task title="My first task" status=open\n\n' "$BIN" "$BIN" >&2
}

main "$@"
