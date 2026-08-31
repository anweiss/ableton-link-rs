---
name: Link Upstream Watch
description: Weekly triage of Ableton Link upstream commits that have landed since the vendored submodule pin, proposed as edits to the versioned porting backlog file.
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
    - "python3:*"

safe-outputs:
  create-pull-request:
    # The backlog is now `.github/upstream-backlog.toml`, a file in this repository,
    # so triage output is a diff and not an issue body. That is the whole point of the
    # change: an issue body could be edited by anyone, in any direction, with no
    # review and no history that anybody reads, and the checkboxes in it drifted out
    # of agreement with reality within one run. A pull request against a tracked file
    # gets review, blame, and the `Upstream backlog check` required status.
    #
    # No title-prefix: `Validate PR title` is a required check on main and enforces
    # conventional commits, so a bracketed prefix makes the PR unmergeable.
    # NOT `upstream-sync`. The port workflow stops if any pull request carrying that
    # label is open, because a second port PR would branch from the same base and
    # conflict on the gitlink. A triage PR holds no port work and may sit awaiting
    # review for weeks, so labelling it `upstream-sync` would halt porting for weeks
    # — the #71 deadlock again, moved from issues to pull requests.
    labels: [upstream-triage, automation]
    reviewers: [anweiss]
    draft: false
    max: 1
    if-no-changes: ignore
    expires: 30
    # Exclusive allowlist. This workflow triages; it has no business touching source,
    # CI, or its own prompt. One file is all it needs.
    allowed-files:
      - ".github/upstream-backlog.toml"
    # Same protected-file problem as the port workflow: gh-aw protects every
    # top-level dot directory by default and that check ignores `allowed-files`,
    # so without this exclusion every triage pull request this workflow ever
    # produced would fall back to a review issue — the exact failure the move to
    # a file-based backlog was meant to eliminate. `allowed-files` below permits
    # nothing else under `.github/`.
    protected-files:
      policy: fallback-to-issue
      exclude:
        - ".github/"
  create-issue:
    title-prefix: "[upstream-sync] "
    labels: [upstream-sync, automation]
    # The escape hatch only, for when there is something to say that is not a backlog
    # edit. Per-item tracking issues are NOT opened here: they are reconciled from the
    # file by `.github/workflows/upstream-backlog-issues.yml`, deterministically, under
    # the separate `upstream-item` label. An extra issue carrying `upstream-sync` is
    # indistinguishable at a glance from a port whose push failed, and #71 — a per-item
    # issue opened by this workflow — stalled the port workflow on run 31667043733.
    max: 1
    deduplicate-by-title: true
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

The backlog is the file `.github/upstream-backlog.toml`, checked out in the repository
you are working in. Read it first. Your output is a **pull request that edits that
file** — nothing else.

- If triage changes nothing (every commit in `commits.txt` is already accounted for in
  the file, and nothing retired since the last run), say so in one line and stop. Do
  not open an empty pull request.
- Otherwise, edit the file and let `create-pull-request` propose it. Title it as a
  conventional commit, e.g. `chore: triage upstream drift through <short-sha>`.

**Do not open a per-item issue.** Each outstanding item does get its own tracking
issue, but those are created by `.github/workflows/upstream-backlog-issues.yml` from
this file, deterministically, under the `upstream-item` label. Your job is to get the
file right; the issues follow. An issue you open carrying `upstream-sync` looks exactly
like a port whose push failed, which is how #71 stalled the port workflow on run
31667043733.

## The file, and what is actually authoritative

```toml
schema_version = 1

[watermark]
pinned = "<40-char submodule pin>"
upstream = "<40-char upstream master head>"
commits_behind = 117

[[port]]
id = "kebab-case-stable-slug"
title = "what changed, in plain terms"
upstream = ["<40-char sha>", "..."]   # every SHA this one idea covers
rust = ["src/..."]                     # from the module map
risk = "wire-format" | "behavior" | "api-break" | "internal"
status = "outstanding" | "retired"
retired_at_pin = ""                    # the pin that retired it, or "" if outstanding
why = "one line on the observable effect"
note = "optional"                      # free text; nothing parses it
```

Plus `[[undecided]]` and `[[not_applicable]]` tables, which exist so every SHA has a
home. They are **not** the same shape as `[[port]]` — they carry no `id`, `status`,
`risk`, `rust` or `retired_at_pin`, and the validator ignores any you invent:

```toml
[[undecided]]
upstream = ["<sha>", "..."]
note = "what the open question is"    # optional, but say it anyway

[[not_applicable]]
upstream = ["<sha>", "..."]
reason = "why this can never need porting"   # required, and checked
```

Only `[[port]]` items get a tracking issue, because only they describe work.

**The submodule pin is still the truth, and this file is metadata about it.** A commit
is ported when the pin is past it, not when this file says `retired`. That is why the
file cannot be trusted on its own and why
`.github/scripts/validate-upstream-backlog.py` cross-checks it against the real drift
range on every push. Run it yourself before you finish:

```bash
python3 .github/scripts/validate-upstream-backlog.py
```

If it exits non-zero, fix the file until it does not. Do not open a pull request that
fails it — that check is required on `main`, so it will not merge anyway.

## How to triage

Work through `/tmp/gh-aw/agent/upstream/commits.txt` oldest first. For each commit,
use `git -C vendor/ableton-link show --stat <sha>` to see what it touched, then read
the actual diff for anything that lands in a mapped path. Put each commit in exactly
one bucket:

- **`[[port]]`** — changes behavior, wire format, timing, or the public API in a module
  the Rust port has. This is the bucket the port workflow consumes. `link_audio/**`
  falls here too — the subsystem has been ported to `src/link_audio/` behind the
  `audio` feature, so bucket its commits like any other mapped path.
- **`[[not_applicable]]`** — confined to the "deliberately not ported" paths (ASIO,
  Catch2 tests, CMake, C++ examples, DI plumbing). The watermark can move straight
  past these.
- **`[[undecided]]`** — a new subsystem or an architectural change where the right Rust
  answer is not obvious. Say what the open question is; do not guess at an answer.
  LinkAudio *used* to be the standing example and no longer is: it is ported. Do not
  re-raise it, and do not park a `link_audio/**` commit here just because it is
  audio-related — reserve this bucket for changes that genuinely have no clear Rust
  answer, such as one that would need a new dependency or an `unsafe` construct that
  `src/link_audio/`'s `#![forbid(unsafe_code)]` rules out.

Group aggressively. Upstream frequently splits one behavioral change across several
commits (`Add link_audio::Messages`, `Add link_audio::PeerInfo`, ...). An item should
be one *idea*, listing every SHA that makes it up, not one line per commit.

## Editing the file

Five things have to be true of the diff you propose.

1. **`id` is stable and never reused.** It is the join key between the file and the
   tracking issue, which carries `<!-- upstream-backlog-id: <id> -->` in its body.
   Rename an `id` and the reconcile workflow closes one issue and opens another,
   throwing away whatever discussion was on the first. Renaming a `title` is free;
   renaming an `id` is not. Never give a new item an `id` that has appeared before.
2. **Retired items are marked, not deleted.** `commits.txt` only lists commits ahead of
   the pin, so a `[[port]]` item whose SHAs have *all* disappeared from it is already
   behind the watermark. Set `status = "retired"` and `retired_at_pin` to the current
   pin. Keep the item: the file doubles as the paper trail for which upstream commits
   this port has accounted for, and deleting an entry destroys that. The validator
   rejects `retired` with an empty `retired_at_pin`, and vice versa.

   The reverse does not hold: an item may be `retired` while some of its SHAs are
   still ahead of the pin. `retired` means the work was ported, not that the pin has
   passed it, and upstream splits single ideas across commits that sit far apart. Do
   not "correct" such an item back to outstanding — that would make the port workflow
   re-port work already on `main`.
3. **`[watermark]` matches this run.** Take `pinned`, `upstream` and `commits_behind`
   from `summary.md` every time. The old issue body went stale here — it advertised
   pin `a729fd4c` for a week after the real pin had moved to `e233c676`. The validator
   now compares `pinned` against the actual submodule gitlink and fails on a mismatch, so
   this can no longer rot silently, but it is still your job to update it.
4. **Outstanding items stay in ancestry order.** The watermark is monotonic: the port
   workflow takes the earliest outstanding commit, and porting out of order means the
   pin either stalls or jumps work. Order outstanding `[[port]]` items by the position
   of their earliest SHA in `commits.txt`. The validator enforces this — it was added
   because the migrated backlog turned out to be in the order `74, 11, 56, 85, 83,
   111, 0, 3, 21, 29`, which would have ported a crash fix before the refactor it
   depends on.
5. **Live items keep their original wording.** Do not re-triage or re-phrase a
   `[[port]]` item that is still outstanding just because you are editing the file.
   Carry it across verbatim. Churn in the wording rewrites the tracking issue body for
   no reason and makes the diff unreviewable.

## Every commit gets a bucket

The port workflow advances the submodule pin, and once the pin moves past a commit that
commit is gone from the next drift report for good. A commit you never mention is
therefore not "deferred", it is deleted. So coverage is the property that matters most
here, ahead of how neatly the file reads.

The validator computes this for you, and fails the pull request if a commit in the
drift range appears nowhere in the file. Run it rather than eyeballing coverage. If
you want the check before you have finished editing:

```bash
cut -f1 /tmp/gh-aw/agent/upstream/commits.txt | grep . | while read sha; do
  grep -qiF "${sha:0:7}" .github/upstream-backlog.toml || echo "UNACCOUNTED $sha"
done
```

Every line that command prints is a commit you are about to drop on the floor. Go back
and classify it, then run it again.

A group must name each SHA it covers rather than trailing off with "and related
commits" — the validator matches on full SHAs, so an unnamed one reads as unaccounted.

This is not hypothetical. The first run of the old workflow left 19 of 135 commits
unmentioned, including `d8a47ba` ("Truncate the peer name to avoid buffer overruns on
serialization") and `0fc58dc` ("Use int64_t consistently for time"). Both would have
been silently retired the first time the pin moved past them. A later run asserted
"135 of 135" while `f7bae98` was in fact missing. Under the old design nothing caught
either; the validator catches both.

## Rules

- Every claim traces to a SHA you actually read. If you did not open the diff, do not
  characterize it.
- Order outstanding `[[port]]` items by their **line order in `commits.txt`**, which is
  true ancestry order from `git log --reverse`. Do not order them by theme or by how
  related they feel — the port workflow takes the earliest one and moves a monotonic
  watermark, so a backlog whose order disagrees with ancestry causes ports to happen
  out of order. Retired items sit above the outstanding ones and keep their historical
  order; the validator expects that shape.
- Flag anything touching `src/discovery/messages.rs`, `src/link/payload.rs`,
  `src/link/audio_endpoint.rs`, `src/encoding.rs`, or
  `src/link_audio/{messages,payload,encoding,codec}.rs` as `risk = "wire-format"`.
  Those change bytes on the network and need a human before they ship.
- If the file has more than 25 `[[port]]` items with `status = "outstanding"`, the
  backlog is saturated and porting needs to catch up before more work is queued.
  **Still triage every commit into a bucket, and still open the pull request.**
  Coverage is not optional: a drift commit in no bucket fails the required check, and
  once the pin passes it, it is gone from every future drift report. What saturation
  changes is only the *message*, not the triage — say at the top of the pull request
  body that the backlog is saturated, give the outstanding count, and ask for porting
  to catch up before the next run. Do not withhold the triage, and in particular do
  not open a watermark-only pull request: advancing `[watermark].upstream` past
  commits you did not bucket is exactly the state the validator rejects, so that pull
  request could never merge.
  Count `status = "outstanding"` only — never raw item count. Five of the twenty items
  in the old backlog were already behind the watermark, so counting every line
  overstates the queue and can freeze triage over finished work.

## Watch the watermark itself

`[watermark].pinned` in the file records the pin the backlog was last written against.
Compare it to `pinned.txt` on this run.

If the pin moved but no `[upstream-sync]` pull request merged to explain it, say so
loudly in the pull request body and list the SHAs that were jumped over. A submodule
bump landing through an unrelated PR marks upstream commits as reconciled when nobody
ported them, and it is silent — the drift report will simply never mention them again.
Recovering that range depends on somebody noticing here.

Separately, if `commits.txt` contains a security fix or a crash fix (upstream subjects
like "Fix a rare crash during initialization"), it still goes in `[[port]]` like
everything else. Put it first in ancestry order among the outstanding items, mark it
`risk = "behavior"`, begin its `title` with `URGENT: `, and call it out at the top of
the pull request body so it is visible without opening the file. Do not file it as its
own issue — the reconcile workflow will open its tracking issue from the file, under
the `upstream-item` label, where it cannot be mistaken for a stranded port. This
paragraph used to say the opposite, and that contradiction is what produced #71
(`5bf14d9`) on run 31417683862 and stalled the port workflow on run 31667043733.
