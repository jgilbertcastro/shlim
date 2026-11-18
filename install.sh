#!/bin/sh
# shlim installer for macOS and Linux. Needs only curl (or wget), tar and a POSIX shell:
# no Rust, no Node.
#
#   curl -fsSL https://raw.githubusercontent.com/jgilbertcastro/shlim/main/install.sh | sh
#
# Environment:
#   SHLIM_VERSION      release tag to install (default: latest), e.g. v0.1.0
#   SHLIM_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   SHLIM_REPO         GitHub repository (default: jgilbertcastro/shlim)
#   SHLIM_BASE_URL     download from this URL instead of GitHub (testing, mirrors)
set -eu

REPO="${SHLIM_REPO:-jgilbertcastro/shlim}"
VERSION="${SHLIM_VERSION:-latest}"
DIR="${SHLIM_INSTALL_DIR:-$HOME/.local/bin}"
BASE="${SHLIM_BASE_URL:-}"

say() { printf '%s\n' "$*"; }
die() { printf 'shlim installer: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux)  os="unknown-linux-musl" ;;
  *) die "unsupported system $(uname -s). shlim runs on macOS and Linux." ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  arch="x86_64" ;;
  arm64|aarch64) arch="aarch64" ;;
  *) die "unsupported architecture $(uname -m)." ;;
esac
target="$arch-$os"

if [ -z "$BASE" ]; then
  if [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
  else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
  fi
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  die "need curl or wget."
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "need sha256sum or shasum to verify the download."
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

archive="shlim-$target.tar.gz"
say "Downloading shlim ($target)..."
if ! fetch "$BASE/$archive" "$tmp/$archive"; then
  if [ -z "${SHLIM_BASE_URL:-}" ] && [ "$VERSION" = "latest" ]; then
    die "could not download $BASE/$archive
There may be no stable release yet. Pick a version from https://github.com/$REPO/releases and run:
  curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | SHLIM_VERSION=<tag> sh"
  fi
  die "could not download $BASE/$archive"
fi
fetch "$BASE/$archive.sha256" "$tmp/$archive.sha256" || die "could not download the checksum"

expected="$(cut -d' ' -f1 "$tmp/$archive.sha256")"
actual="$(sha "$tmp/$archive")"
[ "$expected" = "$actual" ] || die "checksum mismatch (expected $expected, got $actual). Nothing was installed."

tar -xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/shlim" ] || die "the archive does not contain a shlim binary."

mkdir -p "$DIR"
upgrading=""
[ -x "$DIR/shlim" ] && upgrading="yes"
cp "$tmp/shlim" "$DIR/shlim.new"
chmod 755 "$DIR/shlim.new"
mv -f "$DIR/shlim.new" "$DIR/shlim"

say "Installed: $DIR/shlim"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *)
    say ""
    say "$DIR is not in your PATH. Add this line to your ~/.zshrc or ~/.bashrc:"
    say "  export PATH=\"$DIR:\$PATH\""
    ;;
esac

if [ -n "$upgrading" ]; then
  say ""
  say "Upgrade: a running daemon keeps the old version. Run 'shlim stop' (it ends every running session) and the next tool call starts the new one."
fi

say ""
say "Next, connect your agent:"
found=""
if command -v claude >/dev/null 2>&1; then say "  shlim install claude"; found="yes"; fi
if command -v codex  >/dev/null 2>&1; then say "  shlim install codex";  found="yes"; fi
if [ -z "$found" ]; then
  say "  shlim install claude     # Claude Code"
  say "  shlim install codex      # Codex"
  say "  (any other MCP client: use the command 'shlim mcp')"
fi
say "Then restart the agent and run: shlim doctor"
