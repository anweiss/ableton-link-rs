# Ableton Link Rust Implementation

[![CI](https://github.com/anweiss/ableton-link-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/anweiss/ableton-link-rs/actions/workflows/ci.yml)

A native Rust implementation of [Ableton Link](https://ableton.github.io/link), a technology that synchronizes musical beat, tempo, and phase across multiple applications running on one or more devices. Applications on devices connected to a local network discover each other automatically and form a musical session in which each participant can perform independently: anyone can start or stop while still staying in time. Anyone can change the tempo, the others will follow. Anyone can join or leave without disrupting the session.

## Features

* **Full Link Protocol Support**: Implements the complete Ableton Link specification for tempo, timeline, and start/stop synchronization
* **`no_std` Support**: Core types (`Beats`, `Tempo`, `Timeline`, `GhostXForm`, `StartStopState`, `NodeId`, phase math) work in `no_std` environments with `alloc`
* **Async/Await**: Built on Tokio for efficient asynchronous network operations
* **Cross-Platform**: Works on macOS, Linux, and Windows with platform-specific optimizations
* **Platform-Specific Timing**: High-resolution clocks using `mach_absolute_time` (macOS), `clock_gettime` (Linux), and `QueryPerformanceCounter` (Windows)
* **Session Management**: Automatic peer discovery, session state synchronization, and tempo change callbacks
* **Start/Stop Sync**: Synchronization of play/stop states across devices
* **Memory Safe**: Leverages Rust's ownership system for safe concurrent networking
* **LinkAudio (optional)**: Stream PCM audio between Link peers, aligned to the shared beat grid, behind the optional `audio` feature — a fully safe-Rust port of the upstream LinkAudio subsystem

## License

Licensed under the [GNU General Public License v3.0](LICENSE), consistent with the original Ableton Link project.

## Quick Start

Add this to your `Cargo.toml`:

```toml
[dependencies]
ableton-link-rs = "0.3.0"
```

### `no_std` Usage

For embedded or `no_std` environments, disable the default `std` feature to access only the core types:

```toml
[dependencies]
ableton-link-rs = { version = "0.3.0", default-features = false }
```

This gives you access to core types like `Beats`, `Tempo`, `Timeline`, `GhostXForm`, `StartStopState`, and `NodeId` without pulling in networking dependencies, along with the wire encoding layer in `src/encoding.rs` (`Encode`/`Decode`, `encode_to_vec`, `decode_from_slice`). Requires `alloc`.

### LinkAudio Usage

Audio sharing is opt-in via the `audio` feature, which implies `std`:

```toml
[dependencies]
ableton-link-rs = { version = "0.3.0", features = ["audio"] }
```

### Basic Usage

```rust
use ableton_link_rs::link::BasicLink;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a new Link instance with 120 BPM
    let mut link = BasicLink::new(120.0).await;

    // Set up callbacks
    link.set_tempo_callback(|bpm| println!("Tempo changed: {bpm} BPM"));
    link.set_num_peers_callback(|count| println!("Peers: {count}"));
    link.set_start_stop_callback(|playing| println!("Playing: {playing}"));

    // Enable Link (starts network discovery)
    link.enable().await;

    // Capture current session state
    let mut session_state = link.capture_app_session_state();

    // Get current tempo
    println!("Current tempo: {} BPM", session_state.tempo());

    // Change tempo
    let current_time = link.clock().micros();
    session_state.set_tempo(140.0, current_time);

    // Commit changes back to Link
    link.commit_app_session_state(session_state).await;

    // Clean shutdown
    link.disable().await;

    Ok(())
}
```

## Building and Running Examples

```bash
# Clone the repository
git clone https://github.com/anweiss/ableton-link-rs.git
cd ableton-link-rs

# Build the project
cargo build

# Run the RustHut example
cargo run --example rusthut

# Run the LinkAudio example (requires the optional audio feature).
# The second argument selects which remote channel to subscribe to by
# (case-insensitive) substring; it defaults to "main".
cargo run --features audio --example link_audio -- my-peer-name main

# Run the platform optimizations demo
cargo run --example platform_demo
```

### Running RustHut in Docker

```bash
docker build -t rusthut-app .
docker run -it --network host rusthut-app
```

Host networking is required for Ableton Link peer discovery across the network.

### ESP32

The `examples/esp32/` directory contains a standalone ESP32 project that mirrors the
[C++ Link ESP32 example](https://github.com/Ableton/link/tree/master/examples/esp32).
It connects to WiFi, joins a Link session, and blinks the on-board LED on every beat.

**Prerequisites:** [espup](https://github.com/esp-rs/espup), ldproxy, espflash

```bash
cd examples/esp32
# Edit src/main.rs to set WIFI_SSID and WIFI_PASS
cargo build
espflash flash target/xtensa-esp32-espidf/debug/link-esp32-example --monitor
```

See [`examples/esp32/README.md`](examples/esp32/README.md) for full setup instructions.

### RustHut Controls

| Key | Action |
|-----|--------|
| `a` | Enable/disable Link |
| `space` | Start/stop playback |
| `w` / `e` | Decrease / increase tempo |
| `r` / `t` | Decrease / increase quantum |
| `s` | Enable/disable start/stop sync |
| `q` | Quit |

## API Overview

### BasicLink

The main entry point for using Ableton Link:

```rust
let mut link = BasicLink::new(120.0).await;

// Enable/disable Link
link.enable().await;
link.disable().await;

// Status
let is_enabled = link.is_enabled();
let peer_count = link.num_peers();

// Callbacks
link.set_tempo_callback(|bpm| { /* ... */ });
link.set_num_peers_callback(|count| { /* ... */ });
link.set_start_stop_callback(|playing| { /* ... */ });

// Session state
let state = link.capture_app_session_state();
link.commit_app_session_state(state).await;
```

### SessionState

Represents the current state of the Link session:

```rust
let mut state = link.capture_app_session_state();

// Tempo
let tempo = state.tempo();
state.set_tempo(140.0, current_time);

// Beat/time/phase
let beat = state.beat_at_time(current_time, 4.0);
let time = state.time_at_beat(1.0, 4.0);
let phase = state.phase_at_time(current_time, 4.0);

// Beat alignment
state.request_beat_at_time(0.0, current_time, 4.0);
state.force_beat_at_time(0.0, current_time, 4.0);

// Start/stop
state.set_is_playing(true, current_time);
let is_playing = state.is_playing();
```

`SessionState` and `ApiState` implement `PartialEq`, so captured states can be compared directly:

```rust
let state_a = link.capture_app_session_state();
let state_b = state_a;
assert_eq!(state_a, state_b);
```

### LinkAudio (`feature = "audio"`)

`LinkAudio` derefs to `BasicLink`, so the entire Link API remains available. On top of that it publishes
audio channels (sinks) and subscribes to channels published by peers (sources). Audio is interleaved
16-bit signed PCM, and buffers carry the beat time and tempo needed to align them across peers.

```rust
use ableton_link_rs::link_audio::LinkAudio;

let mut link = LinkAudio::new(120.0, "my-app").await?;
link.enable().await;
link.enable_link_audio(true);

// Publish a channel. max_num_samples = frames per callback * channels.
let sink = link.add_sink("drums", 256 * 2);

link.set_channels_changed_callback(|| println!("channels changed"));

// Discover channels published by other peers.
for channel in link.channels() {
    println!("{} from {}", channel.name, channel.peer_name);
}

// Subscribe to one.
let source = link.add_source(channel_id, move |handle| {
    let samples: &[i16] = handle.samples;
    let begin = handle.begin_beats(&session_state, 4.0); // None if from another session
});

// Send audio. Returns None when no peer is listening or no buffer is free.
if let Some(mut buffer) = sink.buffer() {
    buffer.samples_mut()[..num_samples].copy_from_slice(&rendered);
    buffer.commit_with_session_state(
        &session_state,
        link.controller().session_id().0,
        beats_at_buffer_begin,
        4.0,   // quantum
        256,   // frames
        2,     // channels
        44100, // sample rate
    );
}
```

LinkAudio runs its own UDP protocol (`"chnnlsv" + 1`) on a dedicated unicast socket. Peers discover each
other's audio endpoints through the `aep4` entry of the standard Link `PeerState` payload, so LinkAudio
peers are found via ordinary Link discovery. The subsystem contains no `unsafe` code.

Audio buffer messages are the one exception to the usual payload framing: their body is written directly
into the message rather than being wrapped in the `key`/`size` entry header, saving eight bytes per packet
on the hot path. This matches upstream and is required for interoperability.

Receiving from Ableton Live has been verified against a real Live instance: Live publishes one channel per
track (plus `Main`), so pick the channel you actually want — most tracks stream silence unless they are
audible in the mix. The `link_audio` example plays the subscribed channel through the default output device
and prints a peak level meter; set `LINK_AUDIO_PLAYBACK=0` to receive without opening an output device.

The example buffers 50 ms of received audio by default (`LINK_AUDIO_LATENCY_MS`). It performs no
clock-drift compensation between the sending peer and the local output device, so that buffer has to
absorb raw network jitter, which was measured at roughly 30 ms peak to peak on an ordinary LAN. Lower
values underrun audibly.

### Core Types (available in `no_std`)

| Type | Description |
|------|-------------|
| `Beats` | Beat position with microsecond precision |
| `Tempo` | Tempo in BPM with conversion utilities |
| `Timeline` | Maps between beat and time coordinates |
| `GhostXForm` | Linear transform between host and ghost time |
| `StartStopState` | Play/stop state with beat and timestamp |
| `NodeId` | 8-byte peer identifier |

### Wire Encoding (available in `no_std`)

`src/encoding.rs` is a hand-rolled, big-endian fixed-int layer exposing the `Encode`/`Decode`
traits plus `encode_to_vec` / `decode_from_slice`. Implementations are provided for `bool`,
`u8`, `u16`, `i16`, `u32`, `u64`, `i64`, `f64`, `[u8; N]`, 2- and 3-tuples, and `String`
(plus `Ipv4Addr` with `std`). Strings are encoded as a `u32` big-endian byte length followed
by the raw UTF-8 bytes; on decode, invalid UTF-8 is replaced rather than rejected, so a peer
with a non-UTF-8 name does not invalidate an otherwise well-formed message. `Encode::encode_to`
returns `Result<(), EncodeError>`; the only failure today is `EncodeError::StringTooLong`, raised
when a string is longer than its `u32` length prefix can describe, so a truncated prefix can never
desynchronize the rest of the stream.

> **Breaking change in 0.3.0:** `Encode::encode_to` previously returned `()`. It now returns
> `Result<(), EncodeError>`, and `EncodeError` — previously an uninhabited enum — has its first
> constructible variant, `StringTooLong`. Downstream `Encode` implementations must return
> `Ok(())` (and propagate nested encodes with `?`); downstream callers of `encode_to` must handle
> the `Result`. `EncodeError` is now `#[non_exhaustive]`, so future variants will not be breaking.

## Time and Clocks

The Link implementation uses platform-specific high-resolution clocks:

```rust
let clock = link.clock();
let current_time = clock.micros(); // chrono::Duration
```

| Platform | Implementation |
|----------|----------------|
| macOS | `mach_absolute_time()` |
| Linux | `clock_gettime(CLOCK_MONOTONIC_RAW)` |
| Windows | `QueryPerformanceCounter()` |
| Other | `std::time::Instant` fallback |
| ESP32 (ESP-IDF) | `esp_timer_get_time()` |

## Architecture

```
ableton-link-rs
├── encoding.rs              # Wire encoding traits and primitives (no_std)
├── link/                    # Core Link types and API
│   ├── mod.rs               # BasicLink, SessionState, ApiState
│   ├── beats.rs             # Beat position type (no_std)
│   ├── tempo.rs             # Tempo type (no_std)
│   ├── timeline.rs          # Timeline mapping (no_std)
│   ├── ghostxform.rs        # Ghost time transform (no_std)
│   ├── state.rs             # StartStopState, ClientState (no_std)
│   ├── node.rs              # NodeId (no_std), NodeState (std)
│   ├── encoding.rs          # Shared PayloadEntryHeader (no_std)
│   ├── phase.rs             # Phase calculations (no_std)
│   ├── median.rs            # Median filter (no_std)
│   ├── linear_regression.rs # Clock sync regression (no_std)
│   ├── error.rs             # Error types (no_std)
│   ├── controller.rs        # Session controller (std)
│   ├── sessions.rs          # Session management (std)
│   ├── measurement.rs       # Clock measurement (std)
│   ├── payload.rs           # Protocol encoding (std)
│   ├── pingresponder.rs     # Ping/pong responder (std)
│   ├── clock.rs             # Platform clocks (std); includes EspClock on target_os = "espidf"
│   └── ...
├── discovery/               # Peer discovery (std)
│   ├── gateway.rs           # Peer gateway
│   ├── messages.rs          # Protocol messages
│   ├── messenger.rs         # UDP messaging
│   └── peers.rs             # Peer tracking
├── link_audio/              # LinkAudio subsystem (feature = "audio", no unsafe code)
│   ├── mod.rs               # Module docs and re-exports
│   ├── api.rs               # LinkAudio, LinkAudioSink, LinkAudioSource
│   ├── engine.rs            # UDP messenger, sink/source/main processors
│   ├── messages.rs          # v1 message framing ("chnnlsv" + 1)
│   ├── payload.rs           # Payload entries (__pi, chid, auca, aucb, _abu, sess, __ht)
│   ├── encoding.rs          # LinkAudio byte stream primitives
│   ├── channels.rs          # Discovered channel registry
│   ├── receivers.rs         # Peers requesting a sink's channel
│   ├── sink.rs              # Published channel + buffer handles
│   ├── source.rs            # Channel subscription
│   ├── codec.rs             # PCM encode/decode
│   ├── resizer.rs           # Chunking into message-sized buffers
│   ├── queue.rs             # Safe SPSC buffer pool
│   ├── buffer.rs            # Audio buffers and metadata
│   ├── beat_time_mapping.rs # Local ↔ session-global beat mapping
│   └── network_metrics.rs   # Ping/pong link quality filter
└── platform/                # Platform abstractions (std)
```

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std` | ✅ | Full functionality including networking, async, and peer discovery |
| `audio` | ❌ | LinkAudio: publish and receive PCM audio channels over the Link session. Implies `std`. |

Without `std`, only core types and math are available (requires `alloc`).

## Build Requirements

| Requirement | Version |
|-------------|---------|
| Rust | 1.70+ |
| macOS | 10.15+ |
| Linux | glibc 2.28+, `libasound2-dev` (for `rodio`) |
| Windows | Windows 10+ |
| ESP32 | ESP-IDF v5.3+, espup toolchain |

## Contributing

Contributions are welcome! This project uses [conventional commits](https://www.conventionalcommits.org/) for automated releases.

```bash
# Format
cargo fmt --all -- --check

# Lint
cargo clippy --all-targets --all-features -- -D warnings

# Test (must be serial — tests share multicast port 20808)
# --all-features is required to exercise the optional `audio` (LinkAudio) module
cargo test --all --all-features -- --nocapture --test-threads=1

# Verify no_std
cargo check --lib --no-default-features

# Verify the default build (audio off) still compiles
cargo check --all-targets
```

All CI checks must pass before merging to `main`.

### Staying in sync with upstream

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
| `link-upstream-watch.md` | Weekly, Monday | Triages upstream commits landed since the pin and maintains the `Porting backlog: upstream Ableton Link` issue |
| `link-upstream-port.md` | Weekly, Thursday | Takes the next backlog item, ports it, runs the full CI suite, and opens a draft PR |
| `auto-merge-upstream-port.yml` | On every port PR event | Marks a qualifying port PR ready for review and enables auto-merge, so it lands once branch protection is satisfied |
| `copilot-review-loop.yml` | On port PR events, plus a periodic sweep | Approves held CI runs, requests Copilot code review, batches its comments to `Copilot Review Fix`, and marks the PR `copilot-reviewed` when the loop finishes |
| `copilot-review-fix.md` | Dispatched by the review loop only | Fixes a batch of Copilot review comments on Opus 5 and pushes to the port PR's branch. Never picks its own PR; the loop owns that decision |

Both compute their input with `.github/scripts/link-upstream-drift.sh`, which is
plain shell and runnable locally:

```bash
git submodule update --init vendor/ableton-link
OUT_DIR=/tmp/drift ./.github/scripts/link-upstream-drift.sh
cat /tmp/drift/summary.md
```

The porting rules and the C++ header to Rust module map both live in
[`.github/workflows/shared/link-upstream-context.md`](.github/workflows/shared/link-upstream-context.md).
Edit that file to change how either workflow reasons about a change.

**The backlog issue.** `link-upstream-watch` is the only writer. Each run rewrites the
issue body to the current state — ticking off items whose SHAs have dropped out of the
drift report, and refreshing the pinned/upstream SHA header — and adds one comment
describing what changed. It can edit only that issue: the safe output is gated on the
title prefix `[upstream-sync] Porting backlog` **and** the `upstream-sync` and
`automation` labels, and it is permitted to change the body only, so it cannot rename
or close the issue. The workflow itself still runs with `issues: read`; the write
happens in a separate job.

The checkboxes are bookkeeping for humans. `link-upstream-port` picks its next item by
whether a SHA is still present in `commits.txt`, never by checkbox state, so a missed
tick cannot cause the same commit to be ported twice or skipped.

**Reviewing a port PR.** These are opened by `github-actions[bot]`, so their CI runs
land in the `action_required` state waiting on a maintainer to approve them.
[`copilot-review-loop.yml`](.github/workflows/copilot-review-loop.yml) clears that for
you when the `PIPELINE_PAT` secret is set (see below); without it, approve the runs
from the PR's Checks tab or with
`gh api -X POST repos/anweiss/ableton-link-rs/actions/runs/<id>/approve`. Nothing is
wrong with the PR when this happens.

**The Copilot review loop.** Before a port PR is allowed to merge it goes through
`copilot-review-loop.yml`, which on each qualifying PR:

1. approves any workflow runs held in `action_required`, so the required checks
   actually report;
2. requests a review from Copilot code review of the **current head commit**;
3. when Copilot has reviewed that exact commit, hands every comment from that review
   to `Copilot Review Fix` as a single dispatch, so it fixes them in one pass. That
   workflow is a gh-aw agent pinned to `claude-opus-5` - the fixing model is chosen
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

**Why the `copilot-reviewed` label exists.** Nothing in branch protection makes a port
PR wait for review. Copilot code review always leaves a *comment* review, never an
approval or a change request, and comment reviews do not block merging; `main` also
requires zero approving reviews. So auto-merge would otherwise fire the moment the
required checks went green — typically before Copilot had read the diff, leaving the
review to land on an already-merged PR. The label is an additional gate layered on top
of the required checks, not a replacement for any of them.

It is also withheld on every failure. The workflow keeps one status comment per PR,
rewritten in place, naming the phase it is in. If the `PIPELINE_PAT` secret is
missing, if Copilot never reviews, if the agent never pushes, or if Copilot still has
comments after two agent passes, that comment says so and the label stays off, so the
PR parks visibly instead of merging unreviewed. Stalls are called out after six hours.

**Auto-merge.** Port PRs are merged for you.
[`auto-merge-upstream-port.yml`](.github/workflows/auto-merge-upstream-port.yml)
watches for a pull request that targets `main`, comes from a branch in this
repository, was opened by `github-actions[bot]`, and carries the `upstream-sync`,
`automation` and `copilot-reviewed` labels — which is exactly the shape
`link-upstream-port.md` produces once the review loop has signed off, and nothing
else. release-please PRs (`autorelease: pending`) and Dependabot PRs do not match. For
a PR that matches, it marks the draft ready for review and turns on squash auto-merge.

It also cross-checks the SHA. The label says a sign-off happened, not *which* commit
was signed off, so before queuing anything auto-merge reads the review loop's status
comment and requires the recorded `reviewedSha` to equal the current head. Both
workflows fire on the same `synchronize`, and without that check auto-merge could
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
apply here. Recompile after any frontmatter change:

```bash
gh extension install github/gh-aw
gh aw compile
```

Both the `.md` source and the generated `.lock.yml` are committed.

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

### Release PRs and the approval gate

release-please opens its release PR from a branch in this repository. If it opens that
PR with `GITHUB_TOKEN`, GitHub creates the PR's workflow runs in the `action_required`
state, so every release needs someone to click **Approve workflows to run** before CI
starts. This is intended GitHub behavior for automation-authored pull requests, not a
misconfiguration, and it cannot be switched off with a repository setting — the
fork-PR approval policy governs pull requests *from forks* and does not apply here.

To make release PRs run CI unattended, give release-please an identity other than
`GITHUB_TOKEN`. Either works, and
[`release-please.yml`](.github/workflows/release-please.yml) picks the first one that
is configured:

```bash
# Preferred: a GitHub App — no expiry, not tied to a person.
# Grant it Contents: write and Pull requests: write, install it on this repo.
gh variable set RELEASE_PLEASE_APP_ID --body "<app id>"
gh secret   set RELEASE_PLEASE_APP_PRIVATE_KEY < private-key.pem

# Or a fine-grained PAT with the same two permissions. Simpler, but expires.
gh secret set RELEASE_PLEASE_PAT --body "<token>"
```

With neither configured the release is still proposed correctly; it just needs the
approval click, and the workflow logs a warning saying so rather than leaving you to
work out why the PR has no checks.

## Documentation

* [Official Ableton Link Documentation](https://ableton.github.io/link)
* [Changelog](CHANGELOG.md)

## Compatibility

This implementation aims for full compatibility with the official Ableton Link specification and interoperates with applications using the official C++ Link library.

## Support

For questions about this Rust implementation, please [open an issue](https://github.com/anweiss/ableton-link-rs/issues). For general Ableton Link questions, contact <link-devs@ableton.com>.
