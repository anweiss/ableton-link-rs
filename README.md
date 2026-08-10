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
ableton-link-rs = "0.2.0"
```

### `no_std` Usage

For embedded or `no_std` environments, disable the default `std` feature to access only the core types:

```toml
[dependencies]
ableton-link-rs = { version = "0.2.0", default-features = false }
```

This gives you access to core types like `Beats`, `Tempo`, `Timeline`, `GhostXForm`, `StartStopState`, and `NodeId` without pulling in networking dependencies. Requires `alloc`.

### LinkAudio Usage

Audio sharing is opt-in via the `audio` feature, which implies `std`:

```toml
[dependencies]
ableton-link-rs = { version = "0.2.0", features = ["audio"] }
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

**Reviewing a port PR.** These are opened by `github-actions[bot]`, so their CI runs
can land in the `action_required` state waiting on a maintainer to approve them. If a
port PR shows only the CodeQL checks, approve the runs from the PR's Checks tab (or
`gh api -X POST repos/anweiss/ableton-link-rs/actions/runs/<id>/approve`) to get the
full suite. Nothing is wrong with the PR when this happens.

**Setup.** These need a `COPILOT_GITHUB_TOKEN` repository secret — a fine-grained PAT
with the *Copilot Requests* permission. Organization-owned repositories can drop the
secret and use `permissions: copilot-requests: write` instead; that path does not
apply here. Recompile after any frontmatter change:

```bash
gh extension install github/gh-aw
gh aw compile
```

Both the `.md` source and the generated `.lock.yml` are committed.

## Documentation

* [Official Ableton Link Documentation](https://ableton.github.io/link)
* [Changelog](CHANGELOG.md)

## Compatibility

This implementation aims for full compatibility with the official Ableton Link specification and interoperates with applications using the official C++ Link library.

## Support

For questions about this Rust implementation, please [open an issue](https://github.com/anweiss/ableton-link-rs/issues). For general Ableton Link questions, contact <link-devs@ableton.com>.
