---
name: Link Upstream Watch
description: Weekly triage of Ableton Link upstream commits that have landed since the vendored submodule pin, maintained as a porting backlog issue.
emoji: "🔭"
labels: [upstream-sync, automation]
tracker-id: link-upstream-watch

on:
  schedule:
    - cron: "weekly on monday"
  workflow_dispatch:

permissions:
  contents: read
  issues: read
  pull-requests: read
  actions: read

engine: copilot

concurrency:
  group: link-upstream-watch
  cancel-in-progress: false

network:
  allowed:
    - defaults

timeout-minutes: 20
max-turns: 120

checkout:
  fetch-depth: 0
  submodules: recursive

tools:
  github:
    mode: gh-proxy
    toolsets: [default]
  bash:
    - "git log:*"
    - "git show:*"
    - "git diff:*"
    - "git -C:*"
    - "cat:*"
    - "head:*"
    - "tail:*"
    - "grep:*"
    - "wc:*"
    - "sort:*"
    - "uniq:*"
    - "awk:*"
    - "sed:*"
    - "ls:*"
    - "rg:*"

safe-outputs:
  create-issue:
    title-prefix: "[upstream-sync] "
    labels: [upstream-sync, automation]
    max: 2
    deduplicate-by-title: true
  add-comment:
    target: "*"
    max: 1
  missing-tool:

imports:
  - shared/link-upstream-context.md

steps:
  - name: Compute upstream drift
    run: ./.github/scripts/link-upstream-drift.sh
---

# Upstream Link watch

You are triaging changes that have landed in the upstream C++ Ableton Link project
since this Rust port last reconciled with it. You are **not** porting anything in this
run — you are deciding what is worth porting, and recording that decision so the
`Link Upstream Port` workflow can act on it.

Read `/tmp/gh-aw/agent/upstream/summary.md` first. If it says the port is level with
upstream, stop and do nothing — do not create an issue saying there is nothing to do.

## What to produce

There is exactly one backlog issue for this repo. Find it:

```
gh issue list --state open --label upstream-sync --search "Porting backlog in:title"
```

- **If it exists**, add a comment covering only upstream commits that are not already
  listed in it. Do not restate the whole backlog. If every commit in
  `commits.txt` already appears in the issue, say so in one line and stop.
- **If it does not exist**, create it, titled exactly
  `Porting backlog: upstream Ableton Link`.

## How to triage

Work through `/tmp/gh-aw/agent/upstream/commits.txt` oldest first. For each commit,
use `git -C vendor/ableton-link show --stat <sha>` to see what it touched, then read
the actual diff for anything that lands in a mapped path. Put each commit in exactly
one bucket:

- **Port** — changes behavior, wire format, timing, or the public API in a module the
  Rust port has. This is the bucket the port workflow consumes.
- **Not applicable** — confined to the "deliberately not ported" paths (ASIO, Catch2
  tests, CMake, C++ examples, DI plumbing). The watermark can move straight past these.
- **Needs a decision** — a new subsystem or an architectural change where the right
  Rust answer is not obvious. LinkAudio is the standing example. Say what the open
  question is; do not guess at an answer.

Group aggressively. Upstream frequently splits one behavioral change across several
commits (`Add link_audio::Messages`, `Add link_audio::PeerInfo`, ...). A backlog item
should be one *idea*, listing every SHA that makes it up, not one line per commit.

## Backlog issue shape

Keep it a checklist so the port workflow can find the next item and a human can check
things off. For each **Port** item:

```markdown
- [ ] **<what changed, in plain terms>**
  - upstream: `<sha>` (+ any additional SHAs in the same change)
  - rust: `src/...` (from the module map)
  - why: <one line on the observable effect — protocol, timing, API, correctness>
  - risk: wire-format | behavior | api-break | internal
```

Then a short **Not applicable** section (one line per group, with SHAs, so the
watermark has a paper trail) and a **Needs a decision** section.

End the issue body with the drift header from `summary.md` — pinned SHA, upstream SHA,
commit count — so the numbers are checkable without re-running anything.

## Every commit gets a bucket

The port workflow advances the submodule pin, and once the pin moves past a commit
that commit is gone from the next drift report for good. A commit you never mention is
therefore not "deferred", it is deleted. So coverage is the property that matters most
here, ahead of how neatly the backlog reads.

Before you write the issue, check yourself: take every SHA in `commits.txt` and
confirm it appears somewhere in the body you are about to post — as a `Port` item, in
a **Not applicable** line, or in **Needs a decision**. Grouping is fine and encouraged,
but a group must name each SHA it covers rather than trailing off with "and related
commits". If a SHA is not accounted for, you have not finished triaging it.

**Compute this, do not eyeball it.** Write your draft issue body to a file and run the
set difference against `commits.txt`:

```bash
# body.md = the issue body you are about to post
cut -f1 /tmp/gh-aw/agent/upstream/commits.txt | grep . | while read sha; do
  grep -qiF "${sha:0:7}" body.md || echo "UNACCOUNTED $sha"
done
```

Every line that command prints is a commit you are about to drop on the floor. Go back
and classify it, then run the check again. Only post once the command prints nothing,
and take the `Coverage: N of N` line from what you actually counted rather than from
what you assume — a previous run asserted "135 of 135" while `f7bae98` was in fact
missing from the body, which is the exact failure this check exists to catch.

The first run of this workflow left 19 of 135 commits unmentioned, including
`d8a47ba` ("Truncate the peer name to avoid buffer overruns on serialization") and
`0fc58dc` ("Use int64_t consistently for time"). Both would have been silently retired
the first time the pin moved past them.

## Rules

- Every claim traces to a SHA you actually read. If you did not open the diff, do not
  characterize it.
- Order **Port** items by their **line order in `commits.txt`**, which is true ancestry
  order from `git log --reverse`. Do not order them by theme or by how related they
  feel — the port workflow takes the earliest one and moves a monotonic watermark, so
  a backlog whose order disagrees with ancestry causes ports to happen out of order.
- Flag anything touching `src/discovery/messages.rs`, `src/link/payload.rs`, or
  `src/encoding.rs` as `risk: wire-format`. Those change bytes on the network and need
  a human before they ship.
- If the backlog issue already has more than 25 open **Port** items, stop adding to it.
  Comment saying the backlog is saturated and that porting needs to catch up first.

## Watch the watermark itself

The backlog issue records the pinned SHA it was written against. Compare that to
`pinned.txt` on this run.

If the pin moved but no `[upstream-sync]` pull request merged to explain it, say so
loudly at the top of your comment and list the SHAs that were jumped over. A submodule
bump landing through an unrelated PR marks upstream commits as reconciled when nobody
ported them, and it is silent — the drift report will simply never mention them again.
Recovering that range depends on somebody noticing here.

Separately, if `commits.txt` contains a security fix or a crash fix (upstream subjects
like "Fix a rare crash during initialization"), file that as its own issue with a
`Port` classification rather than burying it in the backlog checklist.
