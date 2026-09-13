#!/usr/bin/env bash
# Print the files a push changed, one per line; nothing for other events.
# Diffs the whole push, not just its tip commit, so needs fetch-depth: 0.
# Usage: ci-changed-files.sh   Env: EVENT_NAME, GITHUB_SHA, PUSH_BEFORE
set -euo pipefail

[[ "${EVENT_NAME:-}" == "push" ]] || exit 0

BASE="${PUSH_BEFORE:-}"
if [[ -z "${BASE}" || "${BASE}" =~ ^0+$ ]] || ! git cat-file -e "${BASE}^{commit}" 2>/dev/null; then
  BASE="${GITHUB_SHA}~1"
fi

git diff --name-only "${BASE}" "${GITHUB_SHA}"
