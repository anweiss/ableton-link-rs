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
    # Exactly one issue: the backlog. This workflow triages, it does not open per-item
    # issues. An extra issue here shares the `upstream-sync` label with the port
    # workflow's stranded-port detector, and used to halt porting entirely (#71 blocked
    # run 31667043733). The port workflow now checks tracker-id provenance rather than
    # the label alone, but there is no reason for this workflow to emit a second issue
    # in the first place.
    max: 1
    deduplicate-by-title: true
  add-comment:
    target: "*"
    max: 1
  update-issue:
    # The backlog body is this workflow's own output artifact, so it is allowed to
    # rewrite it wholesale — but only it. Without this, the workflow could comment and
    # never edit, so checkboxes were never ticked (20 unchecked / 0 checked) and the
    # drift header in the body went stale: it read `a729fd4c` while the real pin was
    # `e233c676`. Retired items also kept counting toward the 25-item saturation cut-off
    # below, which would eventually freeze triage over work that was already done.
    body:
    target: "*"
    max: 1
    # Both filters must hold, and together they match exactly one issue: the backlog.
    # The per-item issue #71 carries the same two labels, so the labels alone are not
    # enough — the title prefix is what excludes it.
    required-title-prefix: "[upstream-sync] Porting backlog"
    required-labels: [upstream-sync, automation]
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

- **If it exists**, do two things: rewrite its body to the current state of the world
  (see "Maintaining the backlog body"), and add a comment covering only upstream
  commits that are not already listed in it. Do not restate the whole backlog in the
  comment — the body is where the full picture lives. If every commit in
  `commits.txt` already appears in the issue and nothing retired since the last run,
  say so in one line and stop.
- **If it does not exist**, create it, titled exactly
  `Porting backlog: upstream Ableton Link`.

**Never open a second issue.** Not for a single commit, not for something urgent, not
for an item you think deserves its own tracking. Everything you produce goes in the
backlog issue or a comment on it. A second open issue carrying the `upstream-sync`
label is indistinguishable at a glance from a port whose push failed, and #71 — a
per-item issue for `5bf14d9` opened by this workflow — stalled the port workflow on run
31667043733. Your budget is one issue and it is spoken for.

## How to triage

Work through `/tmp/gh-aw/agent/upstream/commits.txt` oldest first. For each commit,
use `git -C vendor/ableton-link show --stat <sha>` to see what it touched, then read
the actual diff for anything that lands in a mapped path. Put each commit in exactly
one bucket:

- **Port** — changes behavior, wire format, timing, or the public API in a module the
  Rust port has. This is the bucket the port workflow consumes. `link_audio/**` now
  falls here too — the subsystem has been ported to `src/link_audio/` behind the
  `audio` feature, so bucket its commits like any other mapped path.
- **Not applicable** — confined to the "deliberately not ported" paths (ASIO, Catch2
  tests, CMake, C++ examples, DI plumbing). The watermark can move straight past these.
- **Needs a decision** — a new subsystem or an architectural change where the right
  Rust answer is not obvious. Say what the open question is; do not guess at an
  answer. LinkAudio *used* to be the standing example and no longer is: it is ported.
  Do not re-raise it, and do not park a `link_audio/**` commit here just because it is
  audio-related — reserve this bucket for changes that genuinely have no clear Rust
  answer, such as one that would need a new dependency or an `unsafe` construct that
  `src/link_audio/`'s `#![forbid(unsafe_code)]` rules out.

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

## Maintaining the backlog body

The body is the canonical state of the backlog; comments are just the running
narrative of what each triage run noticed. So rewrite the body every run with
`update_issue` (`operation: "replace"`, `issue_number` = the backlog issue), rather
than only commenting and letting the body rot.

Three things have to be true of the body you write:

1. **Retired items are checked off, not deleted.** `commits.txt` only lists commits
   ahead of the pin, so a `Port` item whose SHAs have *all* disappeared from it is
   already behind the watermark — it either shipped or was skipped as not applicable.
   Mark it `- [x]` and append ` — retired at pin <short-sha>`. Keep it in the body:
   the checklist doubles as the paper trail for which upstream commits this port has
   accounted for, and deleting an entry destroys that.
2. **The drift header matches this run.** Take pinned SHA, upstream SHA and commit
   count from `summary.md` every time. This is the line that went stale before: the
   body advertised pin `a729fd4c` for a week after the real pin had moved to
   `e233c676`, so anyone reading the issue got a wrong answer about what was left.
3. **Unfinished work keeps its original wording.** Do not re-triage or re-phrase a
   `Port` item that is still live just because you are rewriting the body. Carry it
   across verbatim, ancestry order intact. The port workflow reads these; churn in
   their wording is noise at best and a reordering bug at worst.

Compute step 1 rather than eyeballing it — the same discipline as the coverage check:

```bash
# Every SHA the backlog currently claims is outstanding
grep -oE '`[0-9a-f]{7,40}`' body.md | tr -d '`' | while read sha; do
  grep -qiF "${sha:0:7}" /tmp/gh-aw/agent/upstream/commits.txt || echo "RETIRED $sha"
done
```

Note that ticking a checkbox is bookkeeping for humans, not a signal to the port
workflow: it selects work by whether a SHA is still in `commits.txt`, not by checkbox
state, and is explicitly told to skip an item that is behind the watermark whether or
not the box got ticked. That independence is deliberate — do not introduce logic that
depends on the boxes being correct.

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
- Flag anything touching `src/discovery/messages.rs`, `src/link/payload.rs`,
  `src/link/audio_endpoint.rs`, `src/encoding.rs`, or
  `src/link_audio/{messages,payload,encoding,codec}.rs` as `risk: wire-format`. Those
  change bytes on the network and need a human before they ship.
- If the backlog issue has more than 25 **outstanding** `Port` items, stop adding to
  it. Comment saying the backlog is saturated and that porting needs to catch up
  first. Count only items still live by the rule above — unchecked *and* still present
  in `commits.txt`. Do not count retired ones: five of the twenty items in the backlog
  were already behind the watermark when this check was last relevant, so counting raw
  checklist lines overstates the queue and can freeze triage over finished work.

## Watch the watermark itself

The backlog issue records the pinned SHA it was written against. Compare that to
`pinned.txt` on this run.

If the pin moved but no `[upstream-sync]` pull request merged to explain it, say so
loudly at the top of your comment and list the SHAs that were jumped over. A submodule
bump landing through an unrelated PR marks upstream commits as reconciled when nobody
ported them, and it is silent — the drift report will simply never mention them again.
Recovering that range depends on somebody noticing here.

Separately, if `commits.txt` contains a security fix or a crash fix (upstream subjects
like "Fix a rare crash during initialization"), it still goes in the backlog checklist
like everything else — put it **first** and mark it `risk: behavior` with a leading
`**URGENT:**`, and call it out at the top of your comment so it is visible without
opening the backlog. Do not file it as its own issue. This paragraph used to say the
opposite, directly contradicting "Never open a second issue" above, and that
contradiction is what produced #71 (`5bf14d9`) on run 31417683862 and stalled the port
workflow on run 31667043733.
