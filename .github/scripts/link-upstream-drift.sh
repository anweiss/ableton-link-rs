#!/usr/bin/env bash
#
# Compute how far the vendored Ableton Link submodule has drifted from upstream
# master, and write a report the agentic sync workflows consume.
#
# The submodule pin at vendor/ableton-link is the port watermark: everything at or
# before it is considered reconciled with the Rust port, everything after it is the
# backlog. This script never moves the pin — it only reports.
#
# Output: $OUT_DIR (default /tmp/gh-aw/agent/upstream)
#   pinned.txt    pinned upstream SHA
#   upstream.txt  Ableton/link@master HEAD SHA
#   commits.txt   <sha>\t<iso-date>\t<subject>, oldest first
#   files.txt     <added>\t<deleted>\t<path> numstat over the range
#   summary.md    human-readable rollup
#
set -euo pipefail

SUBMODULE="${SUBMODULE:-vendor/ableton-link}"
UPSTREAM_REF="${UPSTREAM_REF:-master}"
OUT_DIR="${OUT_DIR:-/tmp/gh-aw/agent/upstream}"

mkdir -p "$OUT_DIR"

if [ ! -e "$SUBMODULE/.git" ]; then
  echo "error: $SUBMODULE is not checked out; the workflow needs submodules: recursive" >&2
  exit 1
fi

PINNED="$(git -C "$SUBMODULE" rev-parse HEAD)"

# The submodule may be a shallow clone. Deepen it so the agent can inspect any
# commit in the range offline, without needing network access from the sandbox.
if ! git -C "$SUBMODULE" fetch --quiet --unshallow origin "$UPSTREAM_REF" 2>/dev/null; then
  git -C "$SUBMODULE" fetch --quiet origin "$UPSTREAM_REF"
fi

UPSTREAM="$(git -C "$SUBMODULE" rev-parse FETCH_HEAD)"

# Everything downstream assumes the pin is a real ancestor of upstream master. If it
# is not, the range is meaningless and reporting an empty drift would be worse than
# failing: the workflows would conclude there is nothing to port.
if ! git -C "$SUBMODULE" cat-file -e "$PINNED^{commit}" 2>/dev/null; then
  echo "error: pinned commit $PINNED is not present in $SUBMODULE" >&2
  exit 1
fi

if ! git -C "$SUBMODULE" merge-base --is-ancestor "$PINNED" "$UPSTREAM"; then
  echo "error: pinned commit $PINNED is not an ancestor of $UPSTREAM_REF ($UPSTREAM)." >&2
  echo "       Upstream history was rewritten, or the pin points off $UPSTREAM_REF." >&2
  echo "       Refusing to report a drift range that would be wrong." >&2
  exit 1
fi

if [ "$(git -C "$SUBMODULE" rev-parse --is-shallow-repository)" = "true" ]; then
  echo "error: $SUBMODULE is still shallow after fetch; the drift range would be incomplete" >&2
  exit 1
fi

printf '%s\n' "$PINNED" > "$OUT_DIR/pinned.txt"
printf '%s\n' "$UPSTREAM" > "$OUT_DIR/upstream.txt"

# Oldest first: the backlog is worked front to back so the watermark advances
# monotonically and each port lands against the state upstream expected.
# No `|| true` here — these succeed with empty output when there is no drift, so a
# non-zero exit is a real failure and must not be swallowed.
git -C "$SUBMODULE" log --reverse --no-merges \
  --pretty=format:'%H%x09%cI%x09%s' "$PINNED..$UPSTREAM" > "$OUT_DIR/commits.txt"
printf '\n' >> "$OUT_DIR/commits.txt"

git -C "$SUBMODULE" diff --numstat "$PINNED..$UPSTREAM" > "$OUT_DIR/files.txt"

# grep -c exits 1 on zero matches, which is a legitimate "no drift" result here.
COMMIT_COUNT="$(grep -c . "$OUT_DIR/commits.txt" || true)"
FILE_COUNT="$(grep -c . "$OUT_DIR/files.txt" || true)"

{
  echo "# Upstream Ableton Link drift"
  echo
  echo "- Pinned (port watermark): \`$PINNED\`"
  echo "- Upstream \`$UPSTREAM_REF\`: \`$UPSTREAM\`"
  echo "- Commits behind: **$COMMIT_COUNT**"
  echo "- Files changed: **$FILE_COUNT**"
  echo

  if [ "$PINNED" = "$UPSTREAM" ]; then
    echo "The port is level with upstream. There is nothing to do."
  else
    echo "## Changed paths by volume"
    echo
    echo '```'
    # Sort by total churn so the agent sees the substantive files first rather
    # than whatever happens to sort alphabetically.
    #
    # Truncation is done with awk, not `head`. Under `set -o pipefail`, `head`
    # exiting early closes the pipe, the upstream `sort` takes SIGPIPE, and the
    # whole script dies with exit 141 and no output. Whether it happens at all
    # depends on how the write races the pipe buffer, so it passes locally and
    # fails on a runner. `awk 'NR<=N'` drains its input instead of exiting early.
    awk -F'\t' 'NF==3 && $1 != "-" { printf "%6d  %s\n", $1 + $2, $3 }' "$OUT_DIR/files.txt" \
      | sort -rn | awk 'NR<=60'
    echo '```'
    if [ "$FILE_COUNT" -gt 60 ]; then
      echo
      echo "_Showing the 60 highest-churn paths of $FILE_COUNT. Full list: \`files.txt\`._"
    fi
    echo
    echo "## Commits, oldest first"
    echo
    # Never truncate this listing. The triage agent classifies commits from what it
    # can see here, and a silently short list reads as a complete one: capping at 80
    # of 135 left 12 commits untriaged in issue #56, including "Truncate the peer
    # name to avoid buffer overruns on serialization". Every commit in the range has
    # to be accounted for, so every commit in the range gets printed.
    echo '```'
    awk -F'\t' 'NF>=3 { printf "%s  %s\n", substr($1,1,12), $3 }' "$OUT_DIR/commits.txt"
    echo '```'
    echo
    echo "_All $COMMIT_COUNT commits in \`$PINNED..$UPSTREAM\` are listed above._"
  fi
} > "$OUT_DIR/summary.md"

# The workflows treat these four files as authoritative. If any of them is missing or
# empty the agent would reason from a blank slate and conclude there is nothing to
# port, so fail loudly instead.
for f in pinned.txt upstream.txt summary.md; do
  if [ ! -s "$OUT_DIR/$f" ]; then
    echo "error: $OUT_DIR/$f is missing or empty; the drift report is incomplete" >&2
    exit 1
  fi
done

echo "wrote drift report to $OUT_DIR ($COMMIT_COUNT commits, $FILE_COUNT files)"
