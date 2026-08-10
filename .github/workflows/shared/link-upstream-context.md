---
description: Shared context for the upstream Ableton Link sync workflows — module map, verification commands, and porting rules.
---

# Upstream Ableton Link sync — shared context

This repository is a native Rust port of [Ableton Link](https://github.com/ableton/link)
(C++). The upstream C++ source is vendored as a git submodule at
`vendor/ableton-link`, tracking `Ableton/link@master`.

## The submodule pin is the port watermark

`vendor/ableton-link`'s pinned commit records **how far upstream this port has been
reconciled**, not merely which checkout happens to be handy. Treat it that way:

- Everything at or before the pinned commit is considered already reviewed.
- Everything after the pinned commit is the backlog.
- Only advance the pin to a commit whose changes have actually been ported,
  deliberately declined, or confirmed not applicable. Never advance it to
  `origin/master` just to make the diff go away.

## Drift report

Before you run, a deterministic setup step writes an upstream drift report to
`/tmp/gh-aw/agent/upstream/`:

| File | Contents |
| --- | --- |
| `pinned.txt` | The submodule's currently pinned upstream SHA |
| `upstream.txt` | `Ableton/link@master` HEAD SHA |
| `commits.txt` | `<sha>\t<iso-date>\t<subject>` for every commit after the pin, **oldest first** |
| `files.txt` | `<added>\t<deleted>\t<path>` numstat across the whole range |
| `summary.md` | Human-readable rollup of the above |

Read these files first. They are authoritative — do not re-derive the drift by
guessing. `git -C vendor/ableton-link log/show/diff` is available for detail on any
individual commit, and the submodule has full history (`fetch-depth: 0`).

## C++ to Rust module map

Verified against the tree. Use it to decide which Rust files an upstream commit
touches; a commit that only touches paths in the "not ported" list is almost always
out of scope.

| Upstream C++ | Rust |
| --- | --- |
| `ableton/link/Beats.hpp` | `src/link/beats.rs` |
| `ableton/link/Tempo.hpp` | `src/link/tempo.rs` |
| `ableton/link/Timeline.hpp` | `src/link/timeline.rs` |
| `ableton/link/Phase.hpp` | `src/link/phase.rs` |
| `ableton/link/GhostXForm.hpp` | `src/link/ghostxform.rs` |
| `ableton/link/Controller.hpp` | `src/link/controller.rs` |
| `ableton/link/Sessions.hpp`, `SessionId.hpp` | `src/link/sessions.rs` |
| `ableton/link/SessionState.hpp`, `StartStopState.hpp`, `ClientSessionTimelines.hpp` | `src/link/state.rs` |
| `ableton/link/NodeId.hpp`, `NodeState.hpp` | `src/link/node.rs` |
| `ableton/link/PeerState.hpp`, `Peers.hpp` | `src/discovery/peers.rs` |
| `ableton/link/Measurement.hpp`, `MeasurementService.hpp`, `MeasurementEndpointV4.hpp`, `MeasurementEndpointV6.hpp` | `src/link/measurement.rs` |
| `ableton/link/PingResponder.hpp` | `src/link/pingresponder.rs` |
| `ableton/link/HostTimeFilter.hpp` | `src/link/host_time_filter.rs` |
| `ableton/link/LinearRegression.hpp` | `src/link/linear_regression.rs` |
| `ableton/link/Median.hpp` | `src/link/median.rs` |
| `ableton/link/PayloadEntries.hpp` | `src/link/payload.rs`, `src/link/encoding.rs` |
| `ableton/link/TripleBuffer.hpp` | `src/link/atomic_session_state.rs` (via the `triple_buffer` crate) |
| `ableton/link/Gateway.hpp`, `ableton/discovery/PeerGateway.hpp`, `PeerGateways.hpp` | `src/discovery/gateway.rs` |
| `ableton/discovery/UdpMessenger.hpp` | `src/discovery/messenger.rs`, `src/discovery/multi_interface_messenger.rs` |
| `ableton/discovery/v1/Messages.hpp`, `ableton/link/v1/Messages.hpp`, `MessageTypes.hpp` | `src/discovery/messages.rs` |
| `ableton/discovery/Payload.hpp`, `NetworkByteStreamSerializable.hpp` | `src/link/payload.rs`, `src/encoding.rs` |
| `ableton/discovery/InterfaceScanner.hpp` | `src/discovery/interface_scanner.rs` |
| `ableton/discovery/IpInterface.hpp` | `src/discovery/ip_interface.rs` |
| `ableton/platforms/*/Clock.hpp` | `src/link/clock.rs`, `src/platform/clock.rs` |
| `ableton/platforms/*/ScanIpIfAddrs.hpp` | `src/platform/network.rs` |
| `ableton/platforms/*/ThreadFactory.hpp` | `src/platform/thread.rs` |
| `ableton/Link.hpp` | `src/lib.rs`, `src/link/mod.rs` (`BasicLink` public API) |

**LinkAudio** — ported behind the optional, off-by-default `audio` cargo feature. The
Rust module is `src/link_audio/`:

| Upstream C++ | Rust |
| --- | --- |
| `ableton/LinkAudio.hpp`, `LinkAudio.ipp` | `src/link_audio/api.rs` (`LinkAudio`, `LinkAudioSink`, `LinkAudioSource`) |
| `ableton/link_audio/Messages.hpp` | `src/link_audio/messages.rs` |
| `ableton/link_audio/PeerInfo.hpp`, `ChannelId.hpp`, `PayloadEntries.hpp` | `src/link_audio/payload.rs` |
| `ableton/link_audio/Buffer.hpp` | `src/link_audio/buffer.rs` |
| `ableton/link_audio/Queue.hpp` | `src/link_audio/queue.rs` |
| `ableton/link_audio/Codec.hpp`, `PcmEncoder.hpp`, `PcmDecoder.hpp` | `src/link_audio/codec.rs` |
| `ableton/link_audio/Resizer.hpp` | `src/link_audio/resizer.rs` |
| `ableton/link_audio/BeatTimeMapping.hpp` | `src/link_audio/beat_time_mapping.rs` |
| `ableton/link_audio/NetworkMetrics.hpp` | `src/link_audio/network_metrics.rs` |
| `ableton/link_audio/Sink.hpp`, `SinkProcessor.hpp` | `src/link_audio/sink.rs` |
| `ableton/link_audio/Source.hpp`, `SourceProcessor.hpp` | `src/link_audio/source.rs` |
| `ableton/link_audio/Receivers.hpp` | `src/link_audio/receivers.rs` |
| `ableton/link_audio/Channels.hpp` | `src/link_audio/channels.rs` |
| `ableton/link_audio/Controller.hpp`, `UdpMessenger.hpp`, `MainProcessor.hpp` | `src/link_audio/engine.rs` |
| `ableton/link/AudioEndpointV4.hpp`, `AudioEndpointV6.hpp` (`aep4`/`aep6`) | `src/link/audio_endpoint.rs` |
| `examples/linkaudio*` | `examples/link_audio.rs` |

**Deliberately not ported** — changes confined to these are out of scope, and the
correct action is to note them and move the watermark past them:

- `ableton/platforms/asio/**` and the vendored `modules/asio-standalone` — the Rust
  port uses Tokio, not ASIO.
- `ableton/util/Injected.hpp`, `SafeAsyncHandler.hpp` — C++ dependency-injection
  plumbing with no Rust analogue.
- `ableton/test/**`, `**/test/**`, `ableton/discovery/test/**` — Catch2 harness.
- `examples/**`, `cmake/**`, `CMakeLists.txt`, `.clang-format`, `ci/**` — C++ build
  and example scaffolding.
- `ableton/platforms/esp32/**` beyond what `examples/esp32` already needs.

### `link_audio/**` is ported, behind the `audio` feature

Upstream's **LinkAudio** subsystem — an audio-streaming protocol layered beside the
Link protocol (`include/ableton/link_audio/**`, `ableton/LinkAudio.hpp`,
`examples/linkaudio*`) — **has been ported** to `src/link_audio/` and is gated behind
the optional, off-by-default `audio` cargo feature. It is no longer a standing open
question and it is no longer out of scope.

Triage `link_audio/**` commits **normally**, against the LinkAudio module map above.
The old guidance to treat the whole subsystem as one design decision for the
maintainer is obsolete — do not re-raise it, and do not classify a LinkAudio commit as
"not applicable" merely because it is a LinkAudio commit.

Two things about this subsystem specifically:

- **`src/link_audio/` must contain no `unsafe`.** The module carries
  `#![forbid(unsafe_code)]`, which the compiler enforces. Upstream's design leans on
  raw pointers and a lock-free ring buffer; the Rust port deliberately replaces those
  with a channel-based buffer pool (`queue.rs`) and RAII buffer handles that lend
  `&mut [i16]` (`sink.rs`). A port that reintroduces `unsafe` here will not compile —
  find the safe equivalent rather than removing the attribute.
- **The `audio` feature already exists in `Cargo.toml`.** New files under
  `src/link_audio/` need no manifest change, so the "no new dependencies" rule below
  is not an obstacle to porting LinkAudio work.

**The core still needs narrow scoping.** A lot of the LinkAudio work reaches back into
the core: `PeerState` gained an audio endpoint, `Peers` notifies on audio-endpoint
changes, `PeerAnnouncement` gained channel announcements, `Optional` was replaced with
`std::optional`, `Endpoint` and `UnicastSocket` were made reusable.

Anything touching `ableton/link/**`, `ableton/discovery/**`, payload keys, peer state,
or message encoding is **in scope and must be triaged normally**, however the upstream
commit message frames it. Those are exactly the changes that decide whether this crate
still interoperates with a current Ableton Live or Bitwig, and they are the easiest
ones to wave through by mistake.

The `aep4` peer-state payload entry (`src/link/audio_endpoint.rs`) is how LinkAudio
peers find each other through ordinary Link discovery, so it is core wire format, not
audio-only. Note one deliberate divergence to preserve if you touch it: this crate
emits `aep4` **only when an audio endpoint is set**, whereas upstream always emits it
with an unspecified-address fallback.

## Porting rules

1. **Behavior over transliteration.** Match upstream's observable protocol and
   timing behavior. Do not import C++ structure (templates, CRTP, `Injected<>`,
   header-only inheritance) where idiomatic Rust is clearer.
2. **Wire compatibility is non-negotiable.** Anything touching
   `src/discovery/messages.rs`, `src/link/payload.rs`, `src/link/audio_endpoint.rs`,
   or `src/encoding.rs` changes bytes on the Link Classic network. The LinkAudio
   protocol has its own wire files — `src/link_audio/{messages,payload,encoding,codec}.rs` —
   and the same rule applies to them. Preserve field order, endianness, and sizes
   exactly as upstream encodes them, and say in the PR body which upstream encoder you
   matched. Note that LinkAudio length-prefixes strings and vectors with a **u32**,
   unlike Link Classic. Note also that a LinkAudio **audio buffer message body is written
   raw**, without the `key`/`size` payload entry header every other entry uses
   (`AudioBuffer::{encode_raw,decode_raw}`); wrapping it breaks interoperability with
   Ableton Live, which is verified working today.
3. **Respect `no_std`.** `src/link/{beats,tempo,timeline,ghostxform,state,node,phase}.rs`
   and `src/encoding.rs` must keep compiling without the `std` feature. Use `alloc`,
   not `std`, in those modules. `src/link_audio/**` is exempt — the `audio` feature
   implies `std` — but it must not break the `no_std` build of anything else.
4. **Keep the public API stable** unless the upstream change genuinely requires
   breaking it. If it does, call that out prominently — this crate is published.
5. **No new dependencies.** The port workflow cannot modify `Cargo.toml` at all. If an
   upstream change genuinely needs one, raise it as an issue rather than working
   around it. The `audio` feature is already declared, so LinkAudio ports do not need
   a manifest change.
6. **No `unsafe` in `src/link_audio/`.** The module is `#![forbid(unsafe_code)]`.
   Where upstream uses raw pointers or a lock-free ring, use the safe equivalents
   already established in `queue.rs` and `sink.rs`. Never remove the attribute to make
   a port fit.
7. **One upstream concern per PR.** A reviewer should be able to hold the whole
   change in their head.

## Verification

These mirror `.github/workflows/ci.yml` and must all pass before you propose a change:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all --all-features -- --nocapture --test-threads=1
cargo check --lib --no-default-features
cargo check --all-targets
```

Some things to know:

- Tests **must** run with `--test-threads=1`. Many bind the Link multicast port
  20808 and collide when run in parallel.
- `--all-features` on the test run is required to exercise LinkAudio. `audio` is
  off by default, so a bare `cargo test --all` silently skips every
  `src/link_audio/` test, including the end-to-end sink-to-source test in
  `engine.rs`.
- `cargo check --all-targets` (no `--all-features`) confirms the default,
  audio-less build still compiles — that is the configuration most users get.
- `libasound2-dev` is installed by a setup step for `rodio`; you do not need to
  install it yourself.
