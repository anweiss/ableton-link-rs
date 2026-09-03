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
    # One comment per item skipped this run, plus one for the terminal stop.
    # At `max: 1` a single skip comment consumed the whole allowance and the
    # terminal `noop` could then not announce itself - reintroducing exactly
    # the unannounced stop that "Every early stop must be announced" exists to
    # prevent. Ten is well clear of the five items currently outstanding.
    #
    # The quota is a hard ceiling, so the prompt reserves the tenth slot: at
    # most nine per-item skip comments per run, with anything beyond that
    # rolled into the terminal announcement. Without that reservation a run
    # that skipped ten items would spend the whole allowance on skips and lose
    # the terminal report - the same unannounced stop, one level up.
    max: 10
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

   ```bash
   gh api --paginate "repos/$GITHUB_REPOSITORY/pulls?state=open&per_page=100" \
     --jq '.[] | select([.labels[].name] | index("upstream-sync")) | .number'
   ```

   If any pull request comes back, **stop**: do not port anything, do not touch the
   backlog, and do not open a second pull request.

   **Use `gh api --paginate`, not `gh pr list`.** `gh pr list` fails in this sandbox
   with `malformed version`; run 33717533039 spent four tool calls discovering that
   before falling back to `gh api`. The same bug hits `gh issue list`, so the
   stranded-port check below uses `gh api` too. `--paginate` is not optional: a single
   page caps at 100, so an older `upstream-sync` pull request beyond it would exit
   successfully with no match and read as clear — a silent false negative on the one
   check that must not have one.

   **This check fails closed.** If the command errors, returns something you cannot
   parse, or you are otherwise unsure whether a port PR is open, treat that as
   **blocked** and stop — never as "nothing is open". A false "clear" is the one
   outcome that actually loses work: two ports branch off the same watermark, both
   advance the submodule pin from the same base, and the second silently drops the
   first's changes from its own assumptions. Stopping a week early costs one cycle;
   guessing wrong costs a port.

   "Stop" here means stop *porting*, not stop *reporting*. This is an early stop like
   any other, so it is still governed by *Every early stop must be announced* below —
   `noop` with the blocking PR number, and a comment on that PR unless it already
   carries this run's blocker marker, exactly as defined there.

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

   ```bash
   gh api --paginate "repos/$GITHUB_REPOSITORY/issues?state=open&labels=upstream-sync&per_page=100" \
     --jq '.[] | select(.pull_request == null) | .number'
   ```

   The `select(.pull_request == null)` is required: the issues endpoint returns pull
   requests too, and without it every open port PR would also read as a stranded port.
   A stranded port is a port whose
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
   upstream SHAs it covers), `rust`, `risk`, `status`, `retired_at_pin`, `impact` and
   `why`.

   **`title` and `impact` are the plain-language pair; `why` and `note` are yours.**
   `title` and `impact` become the tracking issue a person reads, and the validator
   rejects backticks, `::`, `()`, `->`, source paths, and identifiers in any of
   camelCase, PascalCase, acronym-prefixed PascalCase, snake_case or
   SCREAMING_SNAKE_CASE. `why` and `note` are exempt and are where the mechanics
   belong — that is the pair you read from, and the pair you write to.
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
5. Skip an item, and move to the next one, if any of these hold:
   - It is `risk: api-break`. Those need a maintainer to decide. Comment on that
     item's tracking issue (see step 7) rather than porting it.
   - It is `risk: wire-format` **and** upstream shipped no test for it that you can
     port as concrete byte-level expectations. See the wire-format rule below.
   - It has a non-empty **`blocked_on`** field. That field means the item needs a
     design decision before any of it can be written, and it names the decision.
     Comment on its tracking issue as above — subject to the do-not-repeat rule in
     "Every early stop must be announced", which applies to these skip comments even
     when the run goes on to open a pull request — then **carry on to the next item**;
     do not stop the run. One undecidable item must never head-of-line block every
     item behind it; that is exactly how a crash fix ends up parked behind an
     architecture question for weeks.

     Do not add `blocked_on` to an item yourself. It is a maintainer's judgement,
     it is reviewed in a pull request like any other change to this file, and an
     agent that can mark its own work blocked can mark anything blocked.

   Skipping an item does **not** let you advance the watermark past it. See
   "Advance the watermark" — a skipped item is an unported commit, so the pin stops
   before it. Porting a later item while an earlier one is skipped is fine and
   expected: flip that later item to `retired` and leave the pin where it was.
6. If `.github/upstream-backlog.toml` does not exist, or every item in it is
   retired, do not invent a backlog. Read `commits.txt` yourself, take the
   **oldest** commit that touches a mapped path, and port that.
7. Find the item's tracking issue so the pull request can close it:

   ```bash
   gh api --paginate "repos/$GITHUB_REPOSITORY/issues?state=open&labels=upstream-item&per_page=100" \
     --jq '.[] | select(.pull_request == null)
              | select((.body // "") | contains("<!-- upstream-backlog-id: THE_ID -->")) | .number'
   ```

   `gh api` again rather than `gh issue list`, for the same sandbox reason as step 2.
   Match on that marker, never on the title — titles get edited, and since these
   titles are now plain-language prose they get edited more, not less. If exactly one
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

**`noop` alone is not an announcement.** It writes to a run artifact that nobody opens.
A `noop`-only run is a green check with no notification, which is precisely the
"unannounced stop" this section exists to prevent — run 33665748688 stopped on a
design-blocked item, said so at length, and reached nobody.

So whenever you `noop` **because something is blocking progress**, also do exactly one
of these, in this order of preference:

1. If the blocker belongs to a backlog item, `add-comment` on that item's tracking
   issue (step 7 tells you how to find it), naming the run URL and what has to happen
   before the next run can get further.
2. If it belongs to an existing PR or issue, comment there instead.
3. Only if there is no such issue or PR, `create-issue` — it is configured with the
   `needs-decision` label for this.

**A run with nothing to do is not blocked.** If `summary.md` says the port is level
with upstream, or the backlog is entirely retired, `noop` alone is the whole correct
answer: there is no decision owed and no failure to report, and opening a
`needs-decision` issue for it would be noise. The rule above is for a run that wanted
to make progress and could not.

**Say nothing twice — and decide that mechanically.** Before commenting anywhere,
whether for a skipped item in step 5 or for a terminal stop here, stamp every blocker
comment you write with a marker on its own line, built from the blocker and the
watermark you are stopping at:

```
<!-- link-upstream-port:blocked id=<canonical blocker id> pin=<full 40-char pin> -->
```

**`id` has exactly one canonical spelling per blocker.** If the blocker is a backlog
item — the step 5 skip case — `id` is its **backlog `id`**, never the tracking issue
number, even though you are commenting on that issue. A backlog blocker has both, and
if one run stamps the backlog id while the next stamps the issue number, the two
markers miss each other and the comment reposts. Use the issue or pull request number
only for blockers that have no backlog item behind them, such as the open port PR in
step 2 or a stranded port.

Then the rule is a search, not a judgement call: list that issue or pull request's
comments and look for **that exact marker**.

```bash
gh api --paginate "repos/$GITHUB_REPOSITORY/issues/THE_NUMBER/comments?per_page=100" \
  --jq '.[] | (.body // "")' \
  | grep -F '<!-- link-upstream-port:blocked id=THE_ID pin=THE_FULL_PIN -->'
```

That endpoint serves pull request conversation comments as well as issue comments, so
it is the same command either way. `--paginate` is required here for the same reason it
is on the blocker queries: a long-lived thread past 100 comments would drop an older
marker off the first page and repost the blocker every week thereafter.

If the marker is there, say nothing and let `noop` (or the pull request you are opening)
stand. If it is not, comment — no matter what else has been said there.

**`pin` is the full 40-character watermark, never an abbreviation.** Exact matching
needs one canonical spelling, and "short pin" does not fix a length: one run emitting
seven characters and the next emitting twelve for the same watermark would miss each
other's marker and repost the comment this is meant to suppress. `summary.md` already
carries the full SHA — use it verbatim.

**Do not substitute your own reading of the latest comment for that search.** Run
33717533039 halted on open PR #147, read its most recent comment — a maintainer's
note about review scope, which said nothing about porting being blocked — and reasoned
"this last comment already reports the current status at this watermark", so it stayed
silent. The weekly cycle was lost with a green check and no notification: exactly the
unannounced stop this section exists to prevent. A human status update is not this
workflow's blocker announcement, and only the marker can tell the two apart.

Search **all** comments, not only the last one. A later unrelated comment must not
un-suppress an announcement you have already made, or a long-lived blocker reposts
itself every week forever. This applies to **every** run, not only ones that end
in `noop`: a weekly run that skips a blocked item and then successfully opens a port PR
would otherwise repost the identical blocker comment every week.

**Reserve the last comment for the terminal report.** `add-comment` is capped at ten
per run and the cap is enforced by the tooling, not by you — the eleventh call is
dropped, not queued. The do-not-repeat rule bounds comments *across* runs but not
*within* one, so a run that newly skips ten items would spend the entire allowance on
skip notices and have nothing left to announce how it ended. So: **post at most nine
per-item skip comments in a single run.** If a tenth item needs one, stop posting them
individually and name the remaining skipped items in the terminal announcement instead
— one comment listing them all, on the last of them if the run ends in `noop`, or in
the pull request body if the run went on to port something.

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

**Do not write `unsafe`.** `Cargo.toml` sets `unsafe_code = "deny"` under
`[lints.rust]`, covering examples and tests as well as the library, so
hand-rolled FFI fails the build on every target rather than merely reading badly.
Upstream is C++ and many of its platform items look like they need an FFI shim;
they usually do not. Search crates.io for a crate that already wraps the OS
mechanism and use it, even when its parameters are not a byte-for-byte match with
upstream's — document the deviations on the Rust type instead. A vetted crate is
tested on Linux, macOS and Windows; a shim you wrote against documentation and
compiled on one of them is not.

If you genuinely cannot find one, add `#[allow(unsafe_code)]` at the narrowest
scope that works, comment it with the crates you evaluated and why each was
rejected, and repeat that in the pull request body. `src/link_audio/` is stricter
still — `#![forbid(unsafe_code)]` there cannot be opted out of at all.

If you cannot get them green, **do not open a pull request**. Open an issue instead
describing the upstream change, what you tried, and the exact failure. A red PR costs
the maintainer more than no PR.

## Advance the watermark

**Unless an earlier item is skipped.** If you ported a later item because something
before it is `blocked_on` a decision, or is a skipped `api-break` or `wire-format`
item, then **do not touch the pin at all** — leave `vendor/ableton-link` exactly where
it was and skip the rest of this section. The pin asserts that everything behind it is
dealt with, and the skipped item is by definition not dealt with, so advancing it even
to the SHA you just ported would claim work nobody has done and fail the validator.

In that case set the retired item's `retired_at_pin` to the **current, unchanged** pin
— the same 40-char SHA already in `[watermark].pinned` — not to the upstream commit you
ported. The validator will emit `retired but <sha> is still ahead of the pin`; that
warning is expected here and is exactly what a non-contiguous retirement looks like.
Say in the PR body which item blocked the advance, and that the pin was deliberately
left alone.

Otherwise: the submodule pin at `vendor/ableton-link` records how far upstream this
port has been reconciled. In the same commit as your change, advance it to the upstream
commit you just ported:

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
upstream head, `commits_behind` to what is left.

If what you actually did differs from what the item said you would do, correct `why`
and `note` too — they are the record, and a stale one sends the next run at work that
is already done. Leave `title` and `impact` alone unless the *effect* changed; if it
did, rewrite them in plain language, because the validator will reject any identifier
you put there. Then run:

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
<Two to four sentences of plain language: what was wrong or missing before this pull
request, what is true after it, and who notices — an application using this crate, a
listener hearing drift, a peer running another implementation. No backticks, no
function names, no file paths; those go in the detail section below. If the change is
genuinely invisible from outside, say that outright and say why it was still worth
doing.>

Closes #<issue>

<details>
<summary><b>Porting detail</b></summary>

Ports [`<short-sha>`](https://github.com/Ableton/link/commit/<sha>) — <upstream subject>.

**Upstream change.** <what upstream did and why, from reading the diff>

**Rust change.** <what you did here, and anywhere you deliberately diverged from the
C++ structure and why>

**Wire format.** <"unchanged", or exactly which bytes moved and which upstream encoder
you matched>

</details>

**Watermark.** `vendor/ableton-link` advanced <old-sha> -> <new-sha>.
<if you skipped commits in that range, list them and why>

**Verification.** fmt, clippy, build, test, no_std — all passing locally.

<if the port touches `src/**` or `examples/**` but genuinely needs no README
change — an internal refactor, or a bug fix with no API surface — say so in one
sentence and then include this line verbatim, on its own line:>
<!-- docs-not-needed -->
```

**Lead with the effect, not the mechanics.** The opening paragraph is what a
maintainer scanning the pull-request list reads, what lands in the merge notification,
and what someone hitting this bug in six months finds. A body that opens with
`shutdown()` renamed from `stopIoService()` moved into `~SessionController()` tells
that reader nothing about whether it affects them. Everything technical is still
required — it just lives under `Porting detail`, which is markup a reviewer clicks and
a later agent reads straight through. Do not thin the detail out to compensate: the
split is by audience, not a budget.

**The upstream subject goes inside the disclosure, not above it.** Upstream subjects
are written for a C++ codebase and are frequently a bare function name, so hoisting
one to the top of the body reintroduces exactly the problem the opening paragraph
exists to solve. The `Ports <sha>` line is provenance for a reviewer who has already
decided to look, and grep finds a SHA inside a `<details>` block perfectly well.

**The watermark, verification and marker stay outside the disclosure triangle.** They
are review gates rather than background, and a reviewer must not have to expand
anything to check that the pin moved where the body claims it did. `Closes` stays
outside too, so the linkage to the tracking issue is visible at a glance.

**That marker is part of the template, not an afterthought.** The `README maintenance`
check fails any pull request touching `src/**` or `examples/**` that does not also
touch `README.md`, and the prose sentence alone does not satisfy it — the check greps
for the literal marker. #147 wrote the justification ("no public API, feature flag,
dependency, or build-requirement change; this is an internal shutdown-ordering
reliability fix") and omitted the marker, and the check failed on an otherwise
complete port that needed a maintainer to unblock by hand. If you write the sentence,
write the marker.

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
check fails a pull request that touches `src/**`, `examples/**` or `Cargo.toml`
without touching `README.md` — though `Cargo.toml` is outside your writable set
anyway. If the
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
