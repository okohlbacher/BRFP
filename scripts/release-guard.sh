#!/usr/bin/env bash
#
# release-guard.sh
#
# Fails if any forbidden artifact is tracked by git (in the index) and would
# therefore be packaged into a release archive. Forbidden artifacts include the
# proprietary Bruker SDK, raw-data directories, generated converter outputs, and
# build trees. None of these may ever be committed or redistributed.
#
# Usage:
#   bash scripts/release-guard.sh
#
# Exits 0 when the index is clean of forbidden artifacts, non-zero otherwise.

set -euo pipefail

# Forbidden path prefixes (directories that must never be tracked).
forbidden_prefixes='^(vendor/|data/|tmp/|fixtures/private/|target/|target-linux/)'

# Forbidden filename patterns (extensions that must never be tracked).
forbidden_globs='(\.zip|\.mzpeak)$'

# Collect all files git currently tracks. If this is not a git repo, fail loud.
if ! tracked="$(git ls-files)"; then
  echo "release-guard: not a git repository or git unavailable" >&2
  exit 2
fi

offenders=""
if [ -n "${tracked}" ]; then
  offenders="$(printf '%s\n' "${tracked}" \
    | grep -E "${forbidden_prefixes}|${forbidden_globs}" || true)"
fi

if [ -n "${offenders}" ]; then
  echo "release-guard: FAILED -- forbidden artifacts are tracked by git:" >&2
  printf '%s\n' "${offenders}" | sed 's/^/  - /' >&2
  echo "" >&2
  echo "These paths must never be committed or packaged into a release." >&2
  echo "Remove them from the index (e.g. 'git rm --cached <path>') and ensure" >&2
  echo "they are covered by .gitignore before releasing." >&2
  exit 1
fi

echo "release-guard: OK -- no forbidden artifacts are tracked by git."
exit 0
