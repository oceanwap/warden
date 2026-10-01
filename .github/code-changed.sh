#!/usr/bin/env bash
# Does this push or pull request change anything but documentation?
#
# Used by the CI and Service managers workflows to skip the expensive jobs
# (macOS minutes count ten times) for a commit that only touches docs, the
# README or the benchmark result files. Prints `code=true` or `code=false`
# (and appends it to $GITHUB_OUTPUT when that is set).
#
# Environment: EVENT (github.event_name), BEFORE (push: the previous tip),
# BASE (pull request: the base commit), SHA (the commit to compare; default
# HEAD). Anything not understood answers `true`: when in doubt, run the jobs.
# That is also the answer for a new branch or tag (BEFORE all zeros), a
# manual run, and a base commit this checkout doesn't have (a force push).
set -euo pipefail

event=${EVENT:-}
sha=${SHA:-HEAD}
case "$event" in
  push) base=${BEFORE:-} ;;
  pull_request) base=${BASE:-} ;;
  *) base= ;;
esac

# What counts as documentation. Everything else is code: sources, tests,
# Cargo files, the shim, contrib/ (the nginx test reads it), the example
# config, install.sh, and .github/ itself (a workflow change must be tested).
docs='^(docs/|bench/results/)|\.md$|^(LICENSE-MIT|LICENSE-APACHE)$'

code=true
if [[ -n "$base" && -n "${base//0/}" ]] && git cat-file -e "$base^{commit}" 2>/dev/null; then
  files=$(git diff --name-only "$base" "$sha")
  if [[ -n "$files" ]] && ! grep -qvE "$docs" <<<"$files"; then
    code=false
  fi
fi

echo "code=$code"
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "code=$code" >>"$GITHUB_OUTPUT"
fi
