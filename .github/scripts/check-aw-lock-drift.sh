#!/usr/bin/env bash
# Fails when the compiled gh-aw lock files are not exactly what `gh aw compile`
# produces from their .md sources. Lock files are compiler output: a direct edit
# (for example a Dependabot bump of a `uses:` pin) is silently reverted by the
# next compile and desynchronises the pin from the gh-aw-manifest header.
#
# GH_AW selects the compiler command; it must be gh-aw v0.87.10, the version the
# committed lock files were compiled with. Defaults to the `gh aw` extension.
set -euo pipefail

required_version="v0.87.10"
read -r -a gh_aw <<< "${GH_AW:-gh aw}"

# `gh-aw version` prints to stderr.
actual_version="$("${gh_aw[@]}" version 2>&1 | awk '{print $NF}')"
if [ "$actual_version" != "$required_version" ]; then
  echo "::error::gh-aw $required_version is required to check lock drift, found '$actual_version'."
  exit 1
fi

"${gh_aw[@]}" compile --no-check-update

status=0
git diff --exit-code -- '.github/workflows/*.lock.yml' .github/aw/actions-lock.json || status=$?
untracked="$(git ls-files --others --exclude-standard -- .github/workflows .github/aw)"
if [ "$status" -ne 0 ] || [ -n "$untracked" ]; then
  [ -n "$untracked" ] && printf 'Uncommitted compiler output:\n%s\n' "$untracked"
  cat <<MSG
::error::gh-aw lock files are out of date with their .md sources. Edit the .md source (never the .lock.yml), then run 'gh aw compile --no-check-update' with gh-aw $required_version and commit the regenerated .lock.yml files and .github/aw/actions-lock.json. If this is a Dependabot PR that bumped an action inside a .lock.yml, apply the same pin to the .md source and recompile instead of merging the lock-only edit.
MSG
  exit 1
fi
echo "gh-aw lock files match their sources (gh-aw $required_version)."
