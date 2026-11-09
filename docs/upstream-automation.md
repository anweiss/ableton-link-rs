# Upstream automation

[Documentation index](README.md) | [Project README](../README.md)

Repository paths and shell commands below are relative to the repository root.


Two [agentic workflows](https://github.github.com/gh-aw/) track the upstream C++
project at [Ableton/link](https://github.com/ableton/link), which is vendored as a
submodule at `vendor/ableton-link`.

The submodule pin is the **port watermark**: everything at or before it has been
reconciled with this port, everything after it is the backlog. Neither workflow moves
the pin without a corresponding code change.

Moving the pin from `OLD` to `NEW` asserts that every commit in `(OLD, NEW]` has been
handled, because the next run computes drift as `NEW..master` and anything earlier
disappears from the report permanently. So a port PR must account for its whole range
in the **Watermark** section of its body — each commit either ported or explained as
not applicable — and the pin stops before the first commit that is neither. Reviewing
that list is the highest-value part of reviewing one of these PRs.

| Workflow | Cadence | What it does |
| --- | --- | --- |
| `link-upstream-watch.md` | Weekly, Monday | Triages upstream commits landed since the pin and proposes a pull request against [`.github/upstream-backlog.toml`](../.github/upstream-backlog.toml) |
| `link-upstream-port.md` | Weekly, Thursday | Takes the earliest outstanding backlog item it can actually port — skipping ones that are `blocked_on` a design decision, `api-break`, or wire-format without a portable byte-level test — ports it, runs the full CI suite, and opens a draft PR |
| `upstream-backlog-issues.yml` | On pushes touching the backlog file, plus daily | Reconciles one tracking issue per outstanding backlog item from the file. Deterministic `github-script`, not an agent |
| `auto-merge-upstream-port.yml` | On upstream PR events and pushes to main | Marks a qualifying port or backlog-only triage PR ready for review and enables auto-merge once the review loop signs off |
| `copilot-review-loop.yml` | On watch/port/fix workflow completion, plus a five-minute scheduled sweep | Requests Copilot review, routes findings to the appropriate fix agent, and signs off the reviewed head; also approves held CI runs on same-repository bot branches |
| `copilot-review-fix.md` | Dispatched by the review loop only | Fixes a batch of Copilot review comments on Opus 5 and pushes to the port PR's branch. Never picks its own PR; the loop owns that decision |
| `copilot-triage-review-fix.md` | Dispatched by the review loop only | Fixes triage review findings on Opus 5 with an exclusive backlog-file allowlist; cannot change source, README, workflows, or the upstream pin |

Both compute their input with `.github/scripts/link-upstream-drift.sh`, which is
plain shell and runnable locally:

```bash
git submodule update --init vendor/ableton-link
OUT_DIR=/tmp/drift ./.github/scripts/link-upstream-drift.sh
cat /tmp/drift/summary.md
```

The porting rules and the C++ header to Rust module map both live in
[`.github/workflows/shared/link-upstream-context.md`](../.github/workflows/shared/link-upstream-context.md).
Edit that file to change how either workflow reasons about a change.

Port and port-review-fix agents can update `docs/**` as well as `README.md`.
Detailed API and implementation guidance belongs in the relevant topic document;
the root README stays a concise entry point. Backlog-only triage agents retain
their single-file allowlist and cannot change documentation.

**The backlog file.** The backlog is
[`.github/upstream-backlog.toml`](../.github/upstream-backlog.toml), versioned with the
repository. It replaced an issue whose body was a checkbox list: an issue body can be
edited by anyone, in any direction, with no review and no history that anybody reads,
and its checkboxes drifted out of agreement with reality within one run.

Every upstream commit that has been triaged gets a bucket:

| Bucket | Meaning |
| --- | --- |
| `[[port]]` | Real work, one *idea* per item with every SHA that makes it up. Carries a stable `id`, a `risk`, and `status = "outstanding"` or `"retired"` |
| `[[undecided]]` | The right Rust answer is not obvious yet. The pin may **not** advance past these |
| `[[not_applicable]]` | Confined to deliberately unported paths (ASIO, Catch2, CMake, C++ examples), with a required `reason` |

A `[[port]]` item describes itself twice, for two different readers. `title` and
`impact` are plain language for a person — what goes wrong today, what gets better,
and who notices — and the validator rejects backticks, `::`, `()`, `->`, source paths
and
identifiers in any of camelCase, PascalCase, acronym-prefixed PascalCase, snake_case
or SCREAMING_SNAKE_CASE in them, because those two fields become the tracking issue's
title and opening paragraph. `why` and `note` are the opposite: they
are what a port agent works from, are exempt from that rule, and should be as dense
and symbol-heavy as the work requires. The generated issue leads with `impact` and
puts `why` and `note` under a `Porting detail` disclosure triangle; the split is by
audience, not a limit on detail.

Two caveats on "gets a bucket". Commits that landed upstream *after*
`[watermark].upstream` — the point triage last reached — are expected to be
unclassified until the next watch run, and only warn; erroring on them would turn every
unrelated pull request red the moment Ableton pushes. And a commit may legitimately
appear in more than one bucket when it touches both a mapped and an unmapped path
(`7f222b559678` currently does), which also only warns: `[[port]]` outranks
`[[undecided]]` outranks `[[not_applicable]]`, so a commit cannot be hidden by adding a
weaker second classification. What is strictly enforced is that a commit in the triaged
range has *at least* one bucket, and that the pin never crosses one that is
unclassified, undecided, or outstanding.

`link-upstream-watch` proposes edits as a pull request (label `upstream-triage`); it
can touch no other file. `link-upstream-port` takes the earliest outstanding
`[[port]]` item, ports it, advances the pin, and flips the item to `retired` in the
same commit.

An item may also carry `blocked_on`: free text naming a design decision that has to be
made before any of it can be written. The port workflow skips such an item and works
the next one instead, so a single undecidable item cannot head-of-line block every
item behind it. The pin is unaffected — a blocked item is still `outstanding`, so the
watermark still stops before it, and the run retires the later item while leaving the
pin where it was. Only a maintainer sets `blocked_on`, in a reviewed pull request.

Note that `retired` means *the Rust work was done*, not *the pin is past it*. Upstream
routinely splits one idea across commits that sit far apart — one item here spans
ancestry positions 0–2 and 75–79 — and the pin only advances as far as the last fully
handled commit, so a retired item often keeps SHAs ahead of the pin.

**`Upstream backlog check` is a required status.**
[`.github/scripts/validate-upstream-backlog.py`](../.github/scripts/validate-upstream-backlog.py)
is stdlib-only Python and runs on every PR. It enforces what the checkboxes never
could:

- every commit in the triaged range is in some bucket, derived only from explicit
  `upstream` arrays and never from prose;
- outstanding items are in true upstream ancestry order, because the port workflow
  moves a monotonic watermark;
- an `id` is never renamed or deleted, because it is the join key to the tracking
  issue — checked against the merge base;
- and the one that actually protects work: **the pin may not advance across a commit
  that is unclassified, still `[[undecided]]`, or owned by an outstanding item.** That
  reproduces a real failure, where a pin jumped to upstream position 8 with 1–7
  unported, silently dropping a crash fix.

**Per-item tracking issues.** `upstream-backlog-issues.yml` opens one issue per
outstanding item, labelled `upstream-item`, so a port PR can say `Closes #N` and the
work is tracked per item. Issues are matched to items by a hidden
`<!-- upstream-backlog-id: ... -->` marker, never by title, and the item's issue number
is deliberately **not** stored in the TOML — a stored number that nothing syncs is the
same defect as the checkboxes. The workflow reopens an issue that was closed while its
item is still outstanding, closes it when the item retires, and fails if it cannot
converge.

These issues intentionally do **not** carry `upstream-sync`. That label is a blocker
signal: an open *pull request* carrying it always halts the port workflow, since a
second port would branch from the same base and conflict on the gitlink. An open
*issue* carrying it halts porting only when it is a **stranded port** — one whose push
failed and became an issue, identified by both the `link-upstream-port` tracker marker
and the push-failure marker in its body, not by the label alone. That narrowing exists
because a per-item issue carrying `upstream-sync` once deadlocked the pipeline on the
label match alone.

**Reviewing a port PR.** These are opened by `github-actions[bot]`, so their CI runs
land in the `action_required` state waiting on a maintainer to approve them.
[`copilot-review-loop.yml`](../.github/workflows/copilot-review-loop.yml) clears that for
you when the `PIPELINE_PAT` secret is set (see below); without it, approve the runs
from the PR's Checks tab or with
`gh api -X POST repos/anweiss/ableton-link-rs/actions/runs/<id>/approve`. Nothing is
wrong with the PR when this happens.

**The Copilot review loop.** Before a port or backlog-only triage PR is allowed to
auto-merge it goes through
`copilot-review-loop.yml`, which on each qualifying PR:

1. approves any workflow runs held in `action_required`, so the required checks
   actually report;
2. requests a review from Copilot code review of the **current head commit**;
3. when Copilot has reviewed that exact commit, hands every comment from that review
   to `Copilot Review Fix` (ports) or `Copilot Triage Review Fix` (triage) as a
   single dispatch, so it fixes them in one pass. Both
   workflows are gh-aw agents pinned to `claude-opus-5` - the fixing model is chosen
   deliberately rather than left to the cloud agent's default, because most comments
   here are about whether a decode path is faithful to the upstream C++;
4. when the agent pushes, the head moves, so step 2 runs again against the new commit
   — the agent's own work gets reviewed rather than assumed correct;
5. applies the `copilot-reviewed` label once Copilot has reviewed the current head and
   left nothing outstanding, after at most two agent passes.

**Everything is bound to a commit SHA, not to a timestamp.** A review counts only if
its `commit_id` is the current head, and comments count only if they belong to such a
review. That matters in both directions. A push does not imply the comments were
addressed, so the loop cannot sign off on work nobody reviewed. And a *maintainer's*
push is handled identically to the agent's — fixing the comments by hand is a
first-class path, not a stall — which is what makes the loop usable before the
`PIPELINE_PAT` secret below exists.

Sign-off is likewise granted to a SHA. If new commits land after the label was
applied, the workflow removes it **and cancels the queued auto-merge**, because GitHub
keeps auto-merge armed regardless of what happens to labels afterwards. Without that,
anything pushed after sign-off could merge unreviewed.

**Why the `copilot-reviewed` label exists.** Nothing in branch protection makes an
upstream PR wait for review: `main` requires zero approving reviews. Copilot can
approve, request changes, or leave a neutral comment review, but an approval alone
does not establish that every finding was addressed. The loop also checks unresolved
inline findings and the overview verdict; a non-green overview with only body
findings requires a human. An approving review with suppressed comments produces a
warning rather than blocking sign-off. Without its
explicit sign-off, auto-merge could fire the moment the
required checks went green — typically before Copilot had read the diff, leaving the
review to land on an already-merged PR. The label is an additional gate layered on top
of the required checks, not a replacement for any of them.

It is also withheld on every failure. The workflow keeps one status comment per PR,
rewritten in place, naming the phase it is in. If the `PIPELINE_PAT` secret is
missing, if Copilot never reviews, if the agent never pushes, or if Copilot still has
comments after two agent passes, that comment says so and the label stays off, so the
PR parks visibly instead of merging unreviewed. Stalls are called out after six hours.

**Auto-merge.** Port and backlog-only triage PRs are merged for you.
[`auto-merge-upstream-port.yml`](../.github/workflows/auto-merge-upstream-port.yml)
watches for a pull request that targets `main`, comes from a branch in this
repository, was opened by `github-actions[bot]`, and carries `automation`,
`copilot-reviewed`, and **exactly one** of `upstream-sync` (ports) or
`upstream-triage` (triage). release-please PRs (`autorelease: pending`) and Dependabot
PRs do not match. For
a PR that matches, it marks the draft ready for review and turns on squash auto-merge.

Both gates require a triage diff to modify only `.github/upstream-backlog.toml`,
with no added, deleted, or renamed files. Out-of-scope changes, unreadable triage
diffs, and loss of eligibility revoke sign-off and queued auto-merge. Triage review
fixes use a separate workflow with `upstream-triage` + `automation` authorization
and the same single-file allowlist. Never add `upstream-sync` to make a triage PR
qualify: that label blocks further port runs, and carrying both labels is rejected.
Threat flags and wire-format holds still prevent automated fixes and merging.

It also cross-checks the SHA. The label says a sign-off happened, not *which* commit
was signed off, so before queuing anything auto-merge reads the review loop's status
comment and requires the recorded `reviewedSha` to equal the current head. The
merge workflow can see `synchronize` before the next review sweep, and without that check it could
qualify a new head against a label the review loop was in the middle of revoking. It
fails closed: unreadable state means no auto-merge.

**A port tagged `risk: wire-format` is never auto-merged.** Those change bytes on
the network, and `main` requires zero approving reviews, so auto-merging one would
put a protocol change on `main` with nobody having read it. The workflow refuses,
comments on the PR saying so, and leaves it for you. The review loop still asks
Copilot to review it — a second opinion on a protocol change is worth having — but
never hands it to the coding agent and never signs it off.

Detection does not trust the body tag alone. That tag is prose written by an agent, so
it is treated as a fail-closed hint; the authoritative signal is the PR's file list
checked against the wire-format paths named in `link-upstream-port.md`
(`src/discovery/messages.rs`, `src/link/payload.rs`, `src/link/audio_endpoint.rs`,
`src/encoding.rs`, `src/link_audio/{messages,payload,encoding,codec}.rs`). A PR that
started as an ordinary port and *acquired* one of those files in a later commit is
caught, and both workflows revoke an existing sign-off rather than merely declining to
grant one.

This grants no exemption from review otherwise. Auto-merge is GitHub's own queue: the
PR merges only after every required status check on `main` passes, and not before. If
CI fails, or the runs are still parked in `action_required`, the PR simply stays open
the way it does today. It exists because the port workflow refuses to open a second PR
while one is already open, so an unattended port PR stalls the entire porting pipeline
rather than just itself.

Two known limitations, both reported in the run log rather than papered over. If
`main` moves ahead, the port branch must be updated by hand before a queued PR can
merge — `main` requires branches to be up to date, and updating the branch from the
workflow would push with `GITHUB_TOKEN`, whose events do not start the CI the PR then
needs. And labels arrive a second or two after the PR is created, so the workflow
polls for them rather than trusting the `opened` payload.

**Setup.** These need a `COPILOT_GITHUB_TOKEN` repository secret — a fine-grained PAT
with the *Copilot Requests* permission. Organization-owned repositories can drop the
secret and use `permissions: copilot-requests: write` instead; that path does not
apply here.

**Lock files are compiler output.** Each gh-aw workflow is a `.md` source plus a
generated `.lock.yml`, and both are committed. The lock files in this repository are
compiled with gh-aw **v0.87.10**, and that exact version must be used to regenerate
them. Never hand-edit a `.lock.yml`: change the `.md` source and recompile.

```bash
gh extension install github/gh-aw --pin v0.87.10   # once; check with `gh aw version`
gh aw compile --no-check-update
git diff --stat
```

`--no-check-update` stops the compiler from checking for a newer gh-aw release. Do not
upgrade the extension as part of a workflow change. A different compiler version
rewrites every lock file, and an upgrade needs a token that can resolve
`github/gh-aw-actions` commit SHAs. Without that access, the compiler writes tag refs
in place of SHAs and deletes the pin cache.

**Never delete `.github/aw/actions-lock.json`.** It is the compiler's cache of
action-to-SHA resolutions, and it is what lets a recompile keep every
`github/gh-aw-actions/...` reference pinned to a full commit SHA without resolving
it again. After compiling, check that the file is unchanged and that
`rg 'gh-aw-actions/[^@]+@v' .github/workflows/*.lock.yml` finds nothing.

**The `gh-aw lock drift` CI job** (in [`ci.yml`](../.github/workflows/ci.yml)) downloads
the gh-aw v0.87.10 release binary, verifies its checksum, and runs
[`.github/scripts/check-aw-lock-drift.sh`](../.github/scripts/check-aw-lock-drift.sh). The
script recompiles every workflow and fails if any `.lock.yml` or `actions-lock.json`
differs from the committed copy, or if the compiler produced a new file that was not
committed. It is not a required check, but a failure means the committed lock files do
not match their sources. To fix it, edit the `.md` source rather than the lock file,
run `gh aw compile --no-check-update` with v0.87.10, and commit the regenerated
`.lock.yml` files and `actions-lock.json` together with the source change. You can run
the same check locally with `bash .github/scripts/check-aw-lock-drift.sh`.

**Dependabot and lock files.** [`dependabot.yml`](../.github/dependabot.yml) excludes
`.github/workflows/*.lock.yml` from `github-actions` updates (`exclude-paths`), because
an edit to a `uses:` pin inside a lock file is reverted by the next compile and leaves
the `gh-aw-manifest` header stale. Dependabot still updates the hand-written workflows.
To bump an action that a gh-aw workflow uses, change the pin in the `.md` source and
recompile. Dependabot also ignores `github/gh-aw-actions/*` everywhere: the compiler
owns those pins and writes them together with the step bodies that call them, so they
only move when gh-aw itself is upgraded and the workflows are recompiled. If a
Dependabot PR ever does touch a lock file, the drift job fails it. Apply the same
change to the `.md` source and recompile instead of merging the lock-only edit.
`copilot-review-loop.yml` additionally wants a `PIPELINE_PAT` repository secret, a
classic or fine-grained PAT owned by a maintainer with `repo` / *Actions: write*,
*Pull requests: write* and *Contents: write* on this repository. Four steps across the
loop and the fixer cannot use `GITHUB_TOKEN` at all:

- **Approving held runs.** `GITHUB_TOKEN` cannot clear the `action_required` state on
  runs belonging to a PR it created.
- **Dispatching `Copilot Review Fix`.** A workflow run started with `GITHUB_TOKEN`
  cannot itself trigger further workflow runs, so the dispatch would silently create
  no run at all.
- **Pushing the fixes.** `Copilot Review Fix` pushes with `PIPELINE_PAT` for the same
  reason (`safe-outputs.push-to-pull-request-branch.github-token` in
  `copilot-review-fix.md`): a push authenticated as `GITHUB_TOKEN` raises no checks,
  leaving the PR unmergeable for want of the very checks the loop is waiting on.
- **Requesting the Copilot reviewer.** Under `GITHUB_TOKEN` the GraphQL
  `requestReviews` mutation reports success and attaches nobody.

**`PIPELINE_PAT` must be a personal access token**: fine-grained (`github_pat_`) or
classic (`ghp_`). Do not use an OAuth token (`gho_`), such as the output of
`gh auth token`, or a user-to-server token (`ghu_`). Those tokens work for reads, so
the pipeline looks healthy until gh-aw's push handler refuses the token inside
`Copilot Review Fix` and the whole fix round is wasted. Every workflow that consumes
the secret therefore checks the token prefix before first use and fails with a
`PIPELINE_PAT has the wrong token type` error when it sees `gho_` or `ghu_`. The
check never prints the token. It runs in `copilot-review-loop.yml`, in
`release-please.yml` (only when `PIPELINE_PAT` is the selected token), and in a
pre-agent job of both gh-aw review-fix workflows, so the agent job never receives the
secret. An empty secret passes the check, and each workflow keeps its existing
handling for a missing token. To fix a failure, create a fine-grained or classic PAT
with the permissions above and replace the secret with `gh secret set PIPELINE_PAT`.

Port PRs are still created by gh-aw with `GITHUB_TOKEN`, so they stay authored by
`github-actions[bot]` and keep matching the auto-merge author gate. Without the secret
the loop cannot do any part of its job, and says so rather than degrading silently —
the review request raises instead of taking the `GITHUB_TOKEN` path that quietly
attaches nobody, and every other stall writes its reason into the PR's status comment.
A maintainer then requests the review and fixes the comments by hand. Because progress
is tracked by head SHA, that manual push is picked up and re-reviewed exactly as an
agent push would be.

Copilot code review requires a paid Copilot plan. Separately, agent pushes to a PR are
themselves gated by default: turn that off under **Settings → Copilot → Coding agent →
Actions workflow approval**. That setting covers the agent's own pushes only — it does
not affect the runs on gh-aw's `GITHUB_TOKEN`-authored PRs, which is why the approval
step above still exists.

## Release PRs and the approval gate

release-please opens its release PR from a branch in this repository. If it opens that
PR with `GITHUB_TOKEN`, GitHub creates the PR's workflow runs in the `action_required`
state, so every release needs someone to click **Approve workflows to run** before CI
starts. This is intended GitHub behavior for automation-authored pull requests, not a
misconfiguration, and it cannot be switched off with a repository setting — the
fork-PR approval policy governs pull requests *from forks* and does not apply here.

To make release PRs run CI unattended, give release-please an identity other than
`GITHUB_TOKEN`. [`release-please.yml`](../.github/workflows/release-please.yml) tries
three credentials in order and uses the first one that is configured:

1. **`RELEASE_PLEASE_APP_ID` + `RELEASE_PLEASE_APP_PRIVATE_KEY`** — a GitHub App.
   Preferred: no expiry, not tied to a person.
2. **`RELEASE_PLEASE_PAT`** — a fine-grained PAT dedicated to releases. Simpler, but
   expires and must be rotated.
3. **`PIPELINE_PAT`** — the token the upstream porting workflows already use. It
   carries the same two permissions release-please needs, so if you have set up the
   porting pipeline you get unheld release PRs for free. It is tried last on purpose:
   a dedicated credential should win over a shared one where both exist.

All three need exactly `Contents: write` and `Pull requests: write` on this repo.

```bash
# Option 1 — GitHub App. Install it on this repo first.
gh variable set RELEASE_PLEASE_APP_ID --body "<app id>"
gh secret   set RELEASE_PLEASE_APP_PRIVATE_KEY < private-key.pem

# Option 2 — a PAT dedicated to releases.
gh secret set RELEASE_PLEASE_PAT --body "<token>"

# Option 3 — nothing to do if PIPELINE_PAT is already configured for the
# upstream porting workflows.
```

With none of the three configured the release is still proposed correctly; it just
needs the approval click, and the workflow logs a warning saying so rather than
leaving you to work out why the PR has no checks.

Changing this does not unhold a release PR that already exists. Whether runs are held
follows the identity that opened the pull request, so an existing release PR stays
held until release-please recreates it on the next push to `main`.
