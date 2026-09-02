---
name: Link Upstream Port
description: Ports the next item from the upstream Ableton Link backlog into the Rust implementation and opens a pull request.
emoji: "🦀"
labels: [upstream-sync, automation]
tracker-id: link-upstream-port

on:
  schedule:
    - cron: "weekly on thursday"
  workflow_dispatch:
    inputs:
      upstream_sha:
        description: "Upstream Ableton/link commit to port. Leave empty to take the next item off the backlog issue."
        required: false
        type: string

permissions:
  contents: read
  issues: read
  pull-requests: read
  actions: read

engine: copilot

concurrency:
  group: link-upstream-port
  cancel-in-progress: false

network:
  allowed:
    - defaults
    - rust

timeout-minutes: 45
max-turns: 300

checkout:
  fetch-depth: 0
  submodules: recursive

tools:
  github:
    mode: gh-proxy
    toolsets: [default]
  edit:
  bash:
    - "cargo:*"
    - "git:*"
    - "rustc:*"
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
    - "mkdir:*"
    - "test:*"

safe-outputs:
  create-pull-request:
    # Deliberately no title-prefix. `Validate PR title` is a required check on main
    # and enforces conventional commits, so a "[upstream-sync] " prefix makes every
    # port PR unmergeable. The labels below identify these PRs instead.
    labels: [upstream-sync, automation]
    reviewers: [anweiss]
    draft: true
    max: 1
    if-no-changes: ignore
    expires: 30
    # This workflow's whole purpose is to advance the submodule pin, which is a
    # gitlink (mode 160000). gh-aw's default signed-commit push goes through the
    # createCommitOnBranch GraphQL mutation, and that mutation cannot represent
    # gitlinks - so it refuses the push and silently falls back to opening an
    # issue, which is what happened on run 30514772205. Push over plain git
    # instead. Safe here because main has required_signatures disabled.
    signed-commits: false
    # `.github/` has to be excluded from protected-file protection, not merely
    # listed in `allowed-files` below. gh-aw protects every top-level dot
    # directory by default (ADR 28486), and that check runs independently of the
    # allowlist — so a correct port that touches the backlog file is refused at
    # the safe-output step and turned into a review issue instead of a pull
    # request. That is exactly the stranded-port shape this prompt tells the
    # agent to stop on, so the first real run under the new backlog
    # (run 33420103405) blocked the pipeline on its own output.
    #
    # Safe because `allowed-files` is an *exclusive* allowlist:
    # `.github/upstream-backlog.toml` is the only path under `.github/` it
    # permits, so nothing else in the directory becomes writable. The exclusion
    # must name the path segment, which is why it is `.github/` and not the file.
    protected-files:
      policy: fallback-to-issue
      exclude:
        - ".github/"
        # `README.md` is in gh-aw's *default* protected set, and that check runs
        # independently of `allowed-files` - the allowlist only ever denies, it
        # never grants. Listing README.md below without excluding it here would
        # silently turn a README-touching port into a fallback issue.
        - "README.md"
    # Exclusive allowlist. Anything outside it is refused, so a run cannot quietly
    # add a dependency, rewrite CI, or edit its own prompt to widen its reach.
    allowed-files:
      - "src/**"
      - "tests/**"
      - "examples/**"
      - "vendor/ableton-link"
      # The pin and the backlog file have to move in the same commit, so the file
      # has to be writable here. Without it a correct port is refused at the push
      # step and falls back to an issue, which reads as a stranded port.
      - ".github/upstream-backlog.toml"
      # A port that changes the public API has to update the README in the same
      # pull request - the repository's README-maintenance policy, which Copilot
      # enforces in review (PR #124). Without this the port opens knowing it will
      # take a review finding it was never able to pre-empt.
      - "README.md"
  create-issue:
    title-prefix: "[upstream-sync] "
    labels: [upstream-sync, automation, needs-decision]
    max: 1
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
  - name: Install Rust toolchain
    uses: dtolnay/rust-toolchain@2c7215f132e9ebf062739d9130488b56d53c060c # stable
    with:
      toolchain: stable
      components: rustfmt, clippy
  - name: Install ALSA development headers
    run: sudo apt-get update && sudo apt-get install -y libasound2-dev
---

# Upstream Link port

Port one upstream change into the Rust implementation and open a pull request for it.

**One change per run.** Do not batch. A reviewer has to be able to read the diff
against the upstream commit and agree it is faithful; that stops being possible the
moment two unrelated changes share a PR.

## Pick the change

{{#if github.event.inputs.upstream_sha}}
Port upstream commit `${{ github.event.inputs.upstream_sha }}`, which a maintainer
selected explicitly. Read it with `git -C vendor/ableton-link show <sha>`. If it turns
out to be out of scope under the module map, say so and stop rather than inventing
work.
{{/if}}

Otherwise, take the next item off the backlog:

1. Read `/tmp/gh-aw/agent/upstream/summary.md`. If the port is level with upstream,
   stop.
2. **Check for an open port PR first.** Run
   `gh pr list --state open --label upstream-sync --json number,labels`. If any pull
   request comes back, **stop** and do nothing else.

   Match on `upstream-sync` and nothing else. The watch workflow's backlog pull
   requests carry `upstream-triage` instead, precisely so that a triage PR sitting
   open for review — it may sit for up to 30 days — does not halt porting. Do not
   "helpfully" widen this check to cover them; that reintroduces the #71 deadlock on
   the pull request side, where the workflow stops forever on something that holds no
   port work while still reporting success.

   This is not a politeness rule. The watermark advances one step at a time, and every
   port branches off `main`. A second PR opened against the same base would advance
   the submodule pin from the same starting point, conflict on the gitlink, and
   silently drop the first PR's Rust changes from its own assumptions. Wait for the
   open one to merge.

   **Then check for a stranded port.** Run
   `gh issue list --state open --label upstream-sync`. A stranded port is a port whose
   push failed and got turned into an issue instead of a pull request, so it holds work
   that is not on `main` yet. Redoing it would just produce a second copy of the same
   change, so if one exists, **stop** and comment on it saying the port is still
   stranded and needs a maintainer.

   **The label alone does not identify one.** `upstream-sync` is shared by both
   workflows, so read each candidate issue's body with `gh issue view <n> --json body`
   and treat it as a stranded port **only if both** of these hold:

   - it contains `<!-- gh-aw-tracker-id: link-upstream-port -->`, meaning this workflow
     produced it rather than the watch workflow, **and**
   - it contains the push-failure marker
     `This was originally intended as a pull request, but the git push operation failed`.

   Everything else is **not** a blocker and you must carry on past it:

   - the backlog issue (`Porting backlog: upstream Ableton Link`);
   - anything carrying `<!-- gh-aw-tracker-id: link-upstream-watch -->` — that is
     triage output from the watch workflow describing work still to be done, not work
     already done and stranded. Ignore it entirely and port from the backlog as normal.

   This precision is the whole point. Treating any open `upstream-sync` issue as a
   stranded port deadlocked this workflow: on run 31667043733 it halted on #71 — a
   watch-authored triage issue for `5bf14d9` that had never been pushed — and posted
   "this port is still stranded" on an issue that held no work at all. Nothing could
   ever close that issue, so every subsequent run would have stopped in the same place
   while still reporting success.

   If you find no stranded port under that definition, **continue to step 3**. Do not
   stop, and do not comment.
3. Read the backlog from **`.github/upstream-backlog.toml`**, which is checked out in
   the repository you are working in. It is the backlog. Do not look for a backlog
   issue and do not reconstruct the backlog from issue comments; a backlog issue no
   longer holds one.

   Each item is a `[[port]]` table with `id`, `title`, `upstream` (the list of
   upstream SHAs it covers), `rust`, `risk`, `status`, `retired_at_pin` and `why`.
4. Consider only items with `status = "outstanding"`. Of those, take the **first item
   any of whose `upstream` SHAs still appears in
   `/tmp/gh-aw/agent/upstream/commits.txt`**. An item whose SHAs are all gone from
   that file is already behind the watermark and is done, whether or not anyone got
   round to marking it retired — skip it and say so.

   **`commits.txt` is still the ordering authority.** It comes from
   `git log --reverse`, so it is in true ancestry order, oldest first. The backlog
   file is now kept in ancestry order and CI enforces that, so front-to-back is
   normally right — but if the two ever disagree, `commits.txt` wins, because it is
   derived from the actual commit graph and the file is not. Concretely: for every
   outstanding item, find the lowest line number any of its SHAs has in
   `commits.txt`, and take the item with the lowest such number.
5. Skip an item, and move to the next one, if either holds:
   - It is `risk: api-break`. Those need a maintainer to decide. Comment on that
     item's tracking issue (see step 7) rather than porting it.
   - It is `risk: wire-format` **and** upstream shipped no test for it that you can
     port as concrete byte-level expectations. See the wire-format rule below.

   Skipping an item does **not** let you advance the watermark past it. See
   "Advance the watermark" — a skipped item is an unported commit, so the pin stops
   before it.
6. If `.github/upstream-backlog.toml` does not exist, or every item in it is
   retired, do not invent a backlog. Read `commits.txt` yourself, take the
   **oldest** commit that touches a mapped path, and port that.
7. Find the item's tracking issue so the pull request can close it:

   ```bash
   gh issue list --state open --label upstream-item --limit 100 \
     --json number,body \
     --jq '.[] | select(.body | contains("<!-- upstream-backlog-id: THE_ID -->")) | .number'
   ```

   Match on that marker, never on the title — titles get edited. If exactly one
   number comes back, remember it as the item's issue number. If none comes back,
   carry on with the port and leave `Closes` out of the pull request body; the issue
   is opened by a separate reconcile workflow and may simply not exist yet. That is
   not a reason to stop.

## Wire-format changes need byte-level proof

A change to `src/discovery/messages.rs`, `src/link/payload.rs`,
`src/link/audio_endpoint.rs`, `src/encoding.rs`, or
`src/link_audio/{messages,payload,encoding,codec}.rs` moves bytes on a live network
shared with Ableton Live, Bitwig, and hardware. `cargo test` passing proves nothing
about interoperability — the Rust tests and the Rust encoder can agree with each other
and both be wrong.

So for those files: only open a pull request if you can port concrete expected-byte
assertions from upstream's own tests, or derive them by hand from the upstream encoder
and show the working in the PR body. If you cannot, open an issue describing the
upstream change and what evidence a human would need to confirm the encoding. Do not
open a wire-format PR whose only evidence is that your own new test agrees with your
own new code.

## Nothing portable is a valid outcome

If nothing is portable, say why in one line and stop. A run that ports nothing is a
fine outcome; a run that ports something nobody asked for is not.

## Every early stop must be announced

Whenever you stop without opening a pull request — an open port PR, a genuine stranded
port, nothing portable, a blocked wire-format item, a failing build — record the reason
with the `noop` tool, naming the specific blocker and the issue or PR number that
caused it.

The run exits `success` either way, so an unannounced stop is indistinguishable from a
healthy week. Five consecutive green runs hid the #71 deadlock precisely because a
stopped run looked exactly like a completed one.

## When the whole range is out of scope

If every commit between the pin and upstream is genuinely not applicable — ASIO,
CMake, C++ examples, Catch2 tests — the watermark would otherwise stall forever and
the drift report would repeat itself every week.

**LinkAudio-only files no longer qualify.** `link_audio/**`, `LinkAudio.hpp`/`.ipp`,
and `examples/linkaudio*` are mapped to `src/link_audio/` now that the subsystem is
ported, so a commit confined to them is portable work, not an out-of-scope commit to
wave the watermark past.

In that case, and only that case, open a **watermark-only pull request**: no Rust
changes, just the submodule pin advanced to the newest commit in the not-applicable
run, with a body listing every skipped SHA and the one-line reason each is out of
scope. Stop at the first commit that *is* applicable — never skip past something
portable to make the number go down.

## Port it

1. Read the upstream diff in full: `git -C vendor/ableton-link show <sha>`.
2. Read the corresponding Rust module and the code around it. Match the *existing*
   style of that file — this codebase is already idiomatic Rust, not transliterated
   C++, and your change should not stand out as machine-written.
3. Make the change. Follow the porting rules in the shared context: behavior over
   transliteration, wire compatibility exact, `no_std` modules stay `no_std`, public
   API stable unless upstream forces otherwise.
4. Add or update a test whenever the change is observable. If upstream added a test
   for it (`src/ableton/**/tst_*.cpp`), port the *cases* — the inputs and expected
   outputs — not the Catch2 scaffolding.

## Verify before you propose anything

Run all of these and get them passing:

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all --all-features -- --nocapture --test-threads=1
cargo check --lib --no-default-features
cargo check --all-targets
```

`--test-threads=1` is required — the tests bind multicast port 20808 and collide in
parallel.

`--all-features` on the test run is required to exercise LinkAudio. `audio` is off by
default, so a bare `cargo test --all` compiles none of `src/link_audio/` and reports
success without having run a single audio test. The final `cargo check --all-targets`
covers the opposite risk: that a change compiles only *with* `audio` enabled and
breaks the default build everyone else gets.

If your change touches `src/link_audio/`, also confirm the module still has no
`unsafe` — it is `#![forbid(unsafe_code)]`, so a violation shows up as a compile
error under `--all-features` rather than as a lint.

If you cannot get them green, **do not open a pull request**. Open an issue instead
describing the upstream change, what you tried, and the exact failure. A red PR costs
the maintainer more than no PR.

## Advance the watermark

The submodule pin at `vendor/ableton-link` records how far upstream this port has been
reconciled. In the same commit as your change, advance it to the upstream commit you
just ported:

```bash
git -C vendor/ableton-link checkout <sha>
git add vendor/ableton-link
```

**The pin is a claim about the whole range behind it, not a bookmark on one commit.**
Moving it from `OLD` to `NEW` asserts that every commit in `(OLD, NEW]` has been dealt
with, because the next run computes drift as `NEW..master` and everything before `NEW`
disappears from `commits.txt` forever. This rule is the only thing standing between an
unported upstream bugfix and silent deletion, so treat it as hard:

Before you stage the pin, list every commit you are about to move past:

```bash
git -C vendor/ableton-link log --reverse --no-merges --oneline <OLD>..<NEW>
```

Every single line of that output must be either (a) ported by this pull request, or
(b) genuinely not applicable under the module map. If even one line is a commit you
skipped, deferred, or never triaged, **the pin is too far forward** — move it back to
the commit immediately before the first unhandled one, and say so in the PR body. It
is always correct to advance the pin less than you could. It is never correct to
advance it past work that has not been done.

This is not hypothetical. PR #59 advanced the pin to `4d10802c` (upstream position 8)
while positions 1–7 were untouched, which would have silently retired `5bf14d9`
("Fix a rare crash during initialization"), `588fd85` ("Output random bytes to logs in
hexadecimal form"), and `0fc58dc` ("Use int64_t consistently for time").

In the PR body, reproduce that range as the **Watermark** section: one line per commit
moved past, each marked `ported` or `not applicable: <reason>`. If the range is just
the single commit you ported, say that. Never fast-forward the pin to `origin/master`.

### Retire the item in the same commit

The pin and `.github/upstream-backlog.toml` have to move together. In the same commit
that advances the gitlink, edit the item you just ported:

```toml
status = "retired"
retired_at_pin = "<the new 40-character pin>"
```

and update `[watermark]` — `pinned` to the new pin, `upstream` to the current
upstream head, `commits_behind` to what is left. Then run:

```bash
python3 .github/scripts/validate-upstream-backlog.py
```

`status = "retired"` means **you ported the work**, not that the pin is past every
SHA in the item. Upstream routinely splits one idea across commits that sit far apart
— one item here spans upstream positions 0-2 and 75-79 — and you can only advance the
pin as far as the last fully handled commit. Retire the item anyway. If you left it
outstanding, the next run would pick it again as the earliest outstanding item and
re-port work that is already on `main`.

It must exit 0 before you open the pull request. That validator is a required check,
and these are the two failure modes it actually protects against:

- An item left `outstanding` after the watermark has already passed **all** of its
  commits — a port that would be attempted forever and never found. This is an error.
- **Advancing the pin across a commit that is unclassified, still `[[undecided]]`, or
  owned by an `outstanding` `[[port]]` item.** That is the one that loses work, and it
  is the failure PR #59 produced by hand. This is an error, and it is the check that
  makes `retired` mean something.

Note what is *not* an error: a `retired` item whose commits are still ahead of the pin
only warns, because non-contiguous items make that the normal case. Do not rely on
that warning to catch a dropped port — the pin-advance check above is what catches it.
Do not edit the file by hand and skip the validator — hand edits are precisely what it
exists to catch.

## Open the pull request

Title: a **conventional commit** subject — `<type>: <description>`.

`.github/workflows/conventional-commits.yml` runs `Validate PR title` as a required
check on `main`, and it rejects anything without one of these types: `feat`, `fix`,
`docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`. This
repo squash-merges, so the PR title becomes the commit message on `main` and is what
release-please reads to decide the next version and write the changelog. Get the type
wrong and you either miss a release or ship a bogus one.

Pick the type from what the change does **in this crate**, not from how upstream
worded it:

- `fix:` — corrects wrong behavior (the usual case for a ported upstream bugfix)
- `feat:` — adds capability users can reach
- `refactor:` — restructures with no behavior change
- `perf:`, `docs:`, `test:`, `chore:` — as they normally read

Don't prefix the title with `port` or `[upstream-sync]`. The `upstream-sync` label is
what marks these PRs, and the body carries the upstream SHA for grepping against
upstream history. Write the description in your own words for *this* crate rather than
pasting the upstream subject, since upstream subjects are often C++-specific (e.g.
`Link Classic Fix: Catch Exception` should become
`fix: handle UDP send failures when announcing peer state`).

Body:

```markdown
Ports [`<short-sha>`](https://github.com/Ableton/link/commit/<sha>) — <upstream subject>.

**Upstream change.** <what upstream did and why, from reading the diff>

**Rust change.** <what you did here, and anywhere you deliberately diverged from the
C++ structure and why>

**Wire format.** <"unchanged", or exactly which bytes moved and which upstream encoder
you matched>

**Watermark.** `vendor/ableton-link` advanced <old-sha> -> <new-sha>.
<if you skipped commits in that range, list them and why>

**Verification.** fmt, clippy, build, test, no_std — all passing locally.

Closes #<issue>
```

`Closes #<issue>` is the tracking issue you found in step 7 — the one carrying
`<!-- upstream-backlog-id: <id> -->`, not a shared backlog issue. Each backlog item
now has its own issue, so closing it on merge is exactly right and throws nothing
away. If step 7 found no issue, leave the `Closes` line out entirely rather than
guessing a number; closing an unrelated issue is worse than closing nothing.

**Carry `Closes` on a bookkeeping-only pull request too.** If you found the work was
already done and you are only flipping `status` to `retired`, the issue tracks the
*item*, not the diff — retiring the item is precisely the moment it should close, and
a pull request that touches no `src/**` still needs the line. This has been got wrong
once already: #127 retired an item with an otherwise complete body, omitted `Closes`
because it read as "not a real port", and left #104 open to be closed by hand.

Never write `Closes` against an issue labelled `upstream-sync`.

Be honest in that body about anything you were unsure of. This is a protocol
implementation talking to hardware and DAWs on a live network; a reviewer needs to
know where to look hardest. If a behavior in the upstream diff is ambiguous, say so
explicitly instead of picking an interpretation silently.

## What you cannot change

A pull request from this workflow may only touch `src/**`, `tests/**`, `examples/**`,
the `vendor/ableton-link` submodule pin, `.github/upstream-backlog.toml`, and
`README.md`. Anything else — `Cargo.toml`, CI configuration, these workflow files —
is refused outright, and the whole patch goes with it.

**`README.md` is inside that set, and updating it is not optional.** The repository's
README-maintenance policy in `.github/copilot-instructions.md` requires any change
that adds a feature, alters the public API, or changes build requirements to update
the README **in the same pull request**, and CI enforces it: the `README maintenance`
check fails a pull request that touches `src/**` without touching `README.md`. If the
port genuinely needs no documentation change — an internal refactor, a bug fix with no
API surface — say so in the pull request body and add the line `<!-- docs-not-needed -->`
to it, which is the supported way past that check. You cannot set the `docs-not-needed`
label yourself, because `safe-outputs` pins your label list to `upstream-sync` and
`automation`; the body marker is the escape hatch you can actually reach, and it works
precisely *because* your pull requests carry `automation` — the check ignores that
marker on any pull request that does not, since a body marker can otherwise be
self-applied by anyone. Do not leave it to review to notice.

So if your port needs a new dependency or a CI change, do not try to sneak it in and do
not work around the restriction. Open an issue instead: describe the upstream change,
say exactly what needs to change outside the writable set, and why nothing already in
the tree covers it. A maintainer will make that change by hand and the port can land on
the next run.
