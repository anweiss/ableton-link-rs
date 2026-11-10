# Development

[Documentation index](README.md) | [Project README](../README.md)

Repository paths and shell commands below are relative to the repository root.

## Features

* **Full Link Protocol Support**: Implements the complete Ableton Link specification for tempo, timeline, and start/stop synchronization
* **`no_std` Support**: Core types (`Beats`, `Tempo`, `Timeline`, `GhostXForm`, `StartStopState`, `NodeId`, phase math) work in `no_std` environments with `alloc`
* **Async/Await**: Built on Tokio for efficient asynchronous network operations
* **Cross-Platform**: Works on macOS, Linux, and Windows with platform-specific optimizations
* **Platform-Specific Timing**: High-resolution clocks using `mach_absolute_time` (macOS), `clock_gettime` (Linux), and `QueryPerformanceCounter` (Windows)
* **Session Management**: Automatic peer discovery, session state synchronization, and callbacks for tempo and peer audio-endpoint changes
* **Start/Stop Sync**: Synchronization of play/stop states across devices
* **Memory Safe**: Uses Rust's ownership system for safe concurrent networking. The package sets `unsafe_code = "deny"` in `[lints.rust]`, covering examples and tests as well as the library. Unix indexed multicast egress uses safe `rustix` APIs; Windows example console mode uses the dev-only `crossterm` wrapper. Darwin memberships use libc 0.2.191's native RFC 3678 types/constants instead of a repository-maintained ABI layout. Remaining narrowly scoped exceptions cover unicast egress options, Darwin indexed memberships, Unix topology descriptor registration and Windows topology notifications; this is not yet an entirely safe-Rust codebase
* **Ingress-Aware Discovery**: On Linux, macOS and Windows, managed discovery receives the OS interface index with each datagram and selects that interface's registered response socket, not a source-address prefix match. Raw topology notifications invalidate older leases even when the final interface snapshot is unchanged. Address-keyed groups retain duplicate-address adapters; memberships and egress select the interface by index. Optional `std` dependencies include `socket-pktinfo`, Unix `nix`, and Windows SDK bindings. See [discovery interface behavior and evidence](discovery-ingress.md) for platform fixtures and remaining limits
* **LinkAudio (optional)**: Stream PCM audio between Link peers, aligned to the shared beat grid, behind the optional `audio` feature — a fully safe-Rust port of the upstream LinkAudio subsystem

## Architecture

Start with the [component and network diagrams](architecture.md) for how the
public API, managed discovery, clock measurement and optional audio transport
fit together. The tree below maps those components to source files.

```
ableton-link-rs
├── encoding.rs              # Wire encoding traits and primitives (no_std)
├── sync.rs                  # Poison-tolerant lock helpers (std)
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

Without `std`, core timing types, math and wire encoding are available (requires `alloc`).

## Build Requirements

| Requirement | Version |
|-------------|---------|
| Rust | 1.87+ to use the library (enforced by `rust-version`); 1.88+ to run the test suite, which pulls `console-subscriber` |
| Tokio | 1.53.3+ with `std`, for `AsyncFd::register` (the replacement for the deprecated `AsyncFd::new`) |
| macOS | 10.15+ |
| Linux | glibc 2.28+, `libasound2-dev` (for `rodio`) |
| Windows | Windows 10+ |
| ESP32 | ESP-IDF v5.3+, espup toolchain |

The `std` feature requires tokio 1.53.3 or newer. Discovery registers its
interface-change socket with `AsyncFd::register`, which older tokio releases do
not provide; it replaces the now-deprecated `AsyncFd::new`.

Linux and macOS networking enable both the `net` and `time` features of `rustix`,
including the time types used by its socket timeout helpers. This dependency
remains optional under `std`; `no_std` builds do not enable it.
Unused direct `async-trait`, `winapi`, and `mach2` dependencies have been removed.
The removed implicit `async-trait` and `triple_buffer` features require downstream
changes; see [source migration](migration.md).
`winapi` and `mach2` were non-optional
target dependencies and did not expose corresponding implicit crate features.
Platform implementations retain their used safe wrappers and Windows SDK
bindings; transitive dependencies of those wrappers are managed separately.

## Contributing

Contributions are welcome! This project uses [conventional commits](https://www.conventionalcommits.org/) for automated releases.

`Cargo.lock` is committed, and every dependency-resolving CI cargo invocation passes
`--locked`, so CI builds exactly the dependency versions in the lock file. A new upstream release
cannot change what CI compiles or turn an unrelated pull request red. A build that
would need to modify the lock file fails instead. If your change needs newer
dependencies, run `cargo update` (or `cargo update -p <crate>`) deliberately and
commit the resulting `Cargo.lock` in the same pull request so the bump is reviewed.

```bash
# Format
cargo fmt --all -- --check

# Lint
cargo clippy --all-targets --all-features --locked -- -D warnings

# Test (must be serial — tests share multicast port 20808)
# --all-features is required to exercise the optional `audio` (LinkAudio) module
cargo test --all --all-features --locked -- --nocapture --test-threads=1

# Verify no_std
cargo check --lib --no-default-features --locked

# Verify the default build (audio off) still compiles
cargo check --all-targets --locked

# Deterministic LinkAudio playback tests (no audio device or network needed)
cargo test --example link_audio --features audio --locked

# Validate the upstream porting backlog (stdlib only; watermark checks need the submodule)
python3 .github/scripts/validate-upstream-backlog.py
```

All CI checks must pass before merging to `main`.

The backlog validator requires Python 3.11+ for the standard-library `tomllib`
module. When changing Windows fixture scripts or automation, also run the
corresponding checks:

```bash
pwsh -NoLogo -NoProfile -File .github/scripts/test-discovery-ingress-windows-parser.ps1
node --test .github/scripts/test-upstream-automation.cjs
```

For agentic workflow source changes, use the pinned compiler and
[lock-file verification procedure](upstream-automation.md); never edit generated
locks by hand.
