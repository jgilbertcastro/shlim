#!/bin/sh
# Prints the Homebrew formula for a shlim release.
#   scripts/gen-formula.sh v0.1.0 path/to/dist > Formula/shlim.rb
# `dist` holds the release files: shlim-<target>.tar.gz.sha256 for every target.
set -eu

tag="${1:?usage: gen-formula.sh <tag, e.g. v0.1.0> <dist-dir>}"
dist="${2:?usage: gen-formula.sh <tag> <dist-dir>}"
repo="${SHLIM_REPO:-jgilbertcastro/shlim}"
version="${tag#v}"

sum() {
  f="$dist/shlim-$1.tar.gz.sha256"
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
  cut -d' ' -f1 "$f"
}
# Computed up front: a missing checksum must stop the script before anything is printed.
mac_arm=$(sum aarch64-apple-darwin)
mac_x86=$(sum x86_64-apple-darwin)
lin_arm=$(sum aarch64-unknown-linux-musl)
lin_x86=$(sum x86_64-unknown-linux-musl)
url() { printf 'https://github.com/%s/releases/download/%s/shlim-%s.tar.gz' "$repo" "$tag" "$1"; }

cat <<RUBY
class Shlim < Formula
  desc "Persistent, token-lean shell for AI coding agents, over MCP"
  homepage "https://github.com/$repo"
  version "$version"
  license "MIT"

  on_macos do
    on_arm do
      url "$(url aarch64-apple-darwin)"
      sha256 "$mac_arm"
    end
    on_intel do
      url "$(url x86_64-apple-darwin)"
      sha256 "$mac_x86"
    end
  end

  on_linux do
    on_arm do
      url "$(url aarch64-unknown-linux-musl)"
      sha256 "$lin_arm"
    end
    on_intel do
      url "$(url x86_64-unknown-linux-musl)"
      sha256 "$lin_x86"
    end
  end

  def install
    bin.install "shlim"
  end

  def caveats
    <<~EOS
      Connect your agent, then restart it:
        shlim install claude
        shlim install codex
      After an upgrade, run \`shlim stop\` so the daemon restarts on the new version.
    EOS
  end

  test do
    ENV["SHLIM_HOME"] = "/tmp/shlim-brew-#{Process.pid}"
    assert_match "hello", shell_output("#{bin}/shlim run 'echo hello'")
  ensure
    system bin/"shlim", "stop"
  end
end
RUBY
