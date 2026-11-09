#!/usr/bin/env bash
# Fail a release run whose binary does not report the version its tag names.
#
#   out=$("$bin" --version) && bash scripts/check-tag-version.sh "$out"
#
# kaibo's version comes from Cargo.toml, not from the tag, so a tag pushed without
# the version bump would otherwise ship binaries (and an image) that report the
# previous release. On a v* tag run, the `--version` output must be exactly
# `kaibo ${GITHUB_REF_NAME#v}`. On any other ref (a branch dispatch) there
# is no tag to compare against, so it says so and passes.
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 'kaibo X.Y.Z' (one argument: the binary's --version output)" >&2; exit 2; }
got="${1%$'\r'}" # defensive: strip one trailing CR if a runner ever adds one

# Same predicate as release.yml's publish gates: only a v* tag ref publishes.
case "${GITHUB_REF:-}" in
  refs/tags/v*) : ;;
  *)
    echo "not a v* tag run (ref '${GITHUB_REF:-unset}'): version check skipped; binary reports '$got'"
    exit 0
    ;;
esac

want="kaibo ${GITHUB_REF_NAME#v}"
if [ "$got" != "$want" ]; then
  echo "::error::binary reports '$got' but tag '${GITHUB_REF_NAME}' needs '$want'. Bump \`version\` in Cargo.toml to match the tag, then tag that commit again." >&2
  exit 1
fi
echo "version check passed: '$got' matches tag '${GITHUB_REF_NAME}'"
