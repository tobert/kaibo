#!/usr/bin/env bash
# Bump the Homebrew tap (tobert/homebrew-kaibo) to a published kaibo release.
#
# Run this AFTER you push a vX.Y.Z tag and the release workflow has finished
# publishing assets. It uses your own `gh`/git auth to read the release and push
# the tap — no CI secret, nothing to rotate. Because you cut releases by hand,
# this one command is the last step of that ritual.
#
#   scripts/bump-tap.sh v0.2.0-rc.6      # leading v optional
#
# It rewrites ONLY the formula's `version` line and the four per-target sha256s,
# from the same `.sha256` sidecars the release attests — no second hashing to
# drift. Everything else in the formula (desc/install/test) stays put.
set -euo pipefail

REPO="tobert/kaibo"
TAP="tobert/homebrew-kaibo"

TAG="${1:-}"
[ -n "$TAG" ] || { echo "usage: $0 vX.Y.Z" >&2; exit 2; }
case "$TAG" in v*) : ;; *) TAG="v$TAG" ;; esac
VER="${TAG#v}"
command -v gh >/dev/null || { echo "need the gh CLI on PATH" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "==> fetching checksums for $TAG"
gh release download "$TAG" --repo "$REPO" --dir "$work" -p '*.tar.gz.sha256'

# Read a target's sha and REQUIRE exactly 64 hex chars. An empty/short value
# would otherwise sail through a naive presence grep (grep -q "" matches every
# line) and write sha256 "" — crash over that.
sha() {
  local v
  v="$(awk '{print $1}' "$work/kaibo-${TAG}-$1.tar.gz.sha256")"
  case "$v" in *[!0-9a-f]* | "") echo "bad sha for $1: '$v'" >&2; exit 1 ;; esac
  [ "${#v}" -eq 64 ] || { echo "sha for $1 not 64 chars: '$v'" >&2; exit 1; }
  printf '%s' "$v"
}
MAC_ARM="$(sha aarch64-apple-darwin)"
MAC_X86="$(sha x86_64-apple-darwin)"
LIN_ARM="$(sha aarch64-unknown-linux-musl)"
LIN_X86="$(sha x86_64-unknown-linux-musl)"

echo "==> cloning $TAP"
gh repo clone "$TAP" "$work/tap" -- --depth 1 >/dev/null 2>&1
f="$work/tap/Formula/kaibo.rb"
[ -f "$f" ] || { echo "formula not found at $f" >&2; exit 1; }

# Match each sha256 line by the target named on the url line just above it. A
# sha256 line with no preceding known target is left UNTOUCHED (no silent
# default) — the context-anchored checks below then catch any miss.
awk -v ver="$VER" -v a="$MAC_ARM" -v b="$MAC_X86" -v c="$LIN_ARM" -v d="$LIN_X86" '
  /^  version "/                         { sub(/"[^"]*"/, "\"" ver "\"") }
  /aarch64-apple-darwin\.tar\.gz"/       { t="a" }
  /x86_64-apple-darwin\.tar\.gz"/        { t="b" }
  /aarch64-unknown-linux-musl\.tar\.gz"/ { t="c" }
  /x86_64-unknown-linux-musl\.tar\.gz"/  { t="d" }
  /sha256 "/ {
    s = ""
    if (t=="a") s=a; else if (t=="b") s=b; else if (t=="c") s=c; else if (t=="d") s=d
    if (s != "") sub(/"[0-9a-f]*"/, "\"" s "\"")
    t=""
  }
  { print }
' "$f" > "$f.new" && mv "$f.new" "$f"

# Validate by CONTEXT: each sha must sit on the line right after its own target
# url, so a mis-assignment (not just a missing sha) fails loudly.
grep -q "version \"${VER}\"" "$f" || { echo "version not patched" >&2; exit 1; }
check() { # <target> <sha>
  grep -A1 -- "-$1\.tar\.gz\"" "$f" | grep -q "\"$2\"" \
    || { echo "sha for $1 not placed correctly" >&2; exit 1; }
}
check aarch64-apple-darwin       "$MAC_ARM"
check x86_64-apple-darwin        "$MAC_X86"
check aarch64-unknown-linux-musl "$LIN_ARM"
check x86_64-unknown-linux-musl  "$LIN_X86"

cd "$work/tap"
if git diff --quiet -- Formula/kaibo.rb; then
  echo "==> tap already at ${VER}; nothing to do"
  exit 0
fi
git add Formula/kaibo.rb
git commit -m "kaibo ${VER}"
git push
echo "==> tap bumped to ${VER}"
