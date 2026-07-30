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
    protected-files: fallback-to-issue
    # Exclusive allowlist. Anything outside it is refused, so a run cannot quietly
    # add a dependency, rewrite CI, or edit its own prompt to widen its reach.
    allowed-files:
      - "src/**"
      - "tests/**"
      - "examples/**"
      - "vendor/ableton-link"
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
   `gh pr list --state open --label upstream-sync`. If any port pull request from this
   workflow is already open, **stop** and do nothing else.

   This is not a politeness rule. The watermark advances one step at a time, and every
   port branches off `main`. A second PR opened against the same base would advance
   the submodule pin from the same starting point, conflict on the gitlink, and
   silently drop the first PR's Rust changes from its own assumptions. Wait for the
   open one to merge.

   **Then check for a stranded port.** Run
   `gh issue list --state open --label upstream-sync`. Ignore the backlog issue
   (`Porting backlog: upstream Ableton Link`). If any *other* open issue is there, it
   is a port whose push failed and got turned into an issue instead of a pull request,
   and it holds work that is not on `main` yet. **Stop** and comment on that issue
   saying the port is still stranded and needs a maintainer. Redoing the port would
   just produce a second copy of the same change.
3. Find the backlog issue:
   `gh issue list --state open --label upstream-sync --search "Porting backlog in:title"`
4. Take the **first `Port` item whose SHA still appears in
   `/tmp/gh-aw/agent/upstream/commits.txt`**. An item whose SHA is no longer in that
   file is already behind the watermark and is done, whether or not its checkbox got
   ticked — skip it and say so. The backlog is ordered oldest upstream commit first
   and the watermark only moves forward, so work it front to back.
5. Skip an item, and move to the next one, if either holds:
   - It is `risk: api-break`. Those need a maintainer to decide. Comment on the
     backlog issue rather than porting it.
   - It is `risk: wire-format` **and** upstream shipped no test for it that you can
     port as concrete byte-level expectations. See the wire-format rule below.
6. If the backlog issue does not exist yet, do not invent a backlog. Read
   `commits.txt` yourself, take the **oldest** commit that touches a mapped path,
   and port that.

## Wire-format changes need byte-level proof

A change to `src/discovery/messages.rs`, `src/link/payload.rs`, or `src/encoding.rs`
moves bytes on a live network shared with Ableton Live, Bitwig, and hardware. `cargo
test` passing proves nothing about interoperability — the Rust tests and the Rust
encoder can agree with each other and both be wrong.

So for those files: only open a pull request if you can port concrete expected-byte
assertions from upstream's own tests, or derive them by hand from the upstream encoder
and show the working in the PR body. If you cannot, open an issue describing the
upstream change and what evidence a human would need to confirm the encoding. Do not
open a wire-format PR whose only evidence is that your own new test agrees with your
own new code.

## Nothing portable is a valid outcome

If nothing is portable, say why in one line and stop. A run that ports nothing is a
fine outcome; a run that ports something nobody asked for is not.

## When the whole range is out of scope

If every commit between the pin and upstream is genuinely not applicable — ASIO,
CMake, C++ examples, Catch2 tests, LinkAudio-only files — the watermark would
otherwise stall forever and the drift report would repeat itself every week.

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
cargo test --all -- --nocapture --test-threads=1
cargo check --lib --no-default-features
```

`--test-threads=1` is required — the tests bind multicast port 20808 and collide in
parallel.

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

Only advance to a commit you actually ported, or one you can justify skipping in the
PR body because it is genuinely not applicable. Never fast-forward the pin to
`origin/master`. If your change spans several upstream SHAs, pin to the newest one in
that group.

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

Backlog item: #<issue>
```

Reference the backlog issue by number; do not write `Closes #<issue>`. The backlog is
one long-lived issue covering many items, and closing it because one item shipped
would throw away the rest. If you have an `add-comment` budget left over, tick the
item's checkbox by commenting which item this PR covers so the next run skips it.

Be honest in that body about anything you were unsure of. This is a protocol
implementation talking to hardware and DAWs on a live network; a reviewer needs to
know where to look hardest. If a behavior in the upstream diff is ambiguous, say so
explicitly instead of picking an interpretation silently.

## What you cannot change

A pull request from this workflow may only touch `src/**`, `tests/**`, `examples/**`,
and the `vendor/ableton-link` submodule pin. Anything else — `Cargo.toml`, CI
configuration, the README, these workflow files — is refused outright, and the whole
patch goes with it.

So if your port needs a new dependency, a CI change, or a documentation update, do not
try to sneak it in and do not work around the restriction. Open an issue instead:
describe the upstream change, say exactly what needs to change outside `src/`, and why
nothing already in the tree covers it. A maintainer will make that change by hand and
the port can land on the next run.
