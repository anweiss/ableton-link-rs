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

### `link_audio/**` is a separate question

Upstream has added **LinkAudio** — a whole audio-streaming subsystem
(`include/ableton/link_audio/**`, `ableton/LinkAudio.hpp`, `examples/linkaudio*`).
It dominates the raw drift numbers and it is not a change to the Link protocol this
crate implements; it is a new protocol layered beside it.

Do not attempt to port it commit-by-commit. Treat the whole subsystem as one
design-level decision for the maintainer, raise it once, and do not let it block the
watermark.

**But scope that narrowly.** "LinkAudio is out of scope" applies to files under
`link_audio/`, `LinkAudio.hpp`/`.ipp`, and `examples/linkaudio*` — not to every commit
with LinkAudio in its subject. A lot of the LinkAudio work reaches back into the core
to make room for it: `PeerState` gained an audio endpoint, `Peers` notifies on
audio-endpoint changes, `PeerAnnouncement` gained channel announcements, `Optional`
was replaced with `std::optional`, `Endpoint` and `UnicastSocket` were made reusable.

Anything touching `ableton/link/**`, `ableton/discovery/**`, payload keys, peer state,
or message encoding is **in scope and must be triaged normally**, however the upstream
commit message frames it. Those are exactly the changes that decide whether this crate
still interoperates with a current Ableton Live or Bitwig, and they are the easiest
ones to wave through by mistake.

## Porting rules

1. **Behavior over transliteration.** Match upstream's observable protocol and
   timing behavior. Do not import C++ structure (templates, CRTP, `Injected<>`,
   header-only inheritance) where idiomatic Rust is clearer.
2. **Wire compatibility is non-negotiable.** Anything touching
   `src/discovery/messages.rs`, `src/link/payload.rs`, or `src/encoding.rs` changes
   bytes on the network. Preserve field order, endianness, and sizes exactly as
   upstream encodes them, and say in the PR body which upstream encoder you matched.
3. **Respect `no_std`.** `src/link/{beats,tempo,timeline,ghostxform,state,node,phase}.rs`
   and `src/encoding.rs` must keep compiling without the `std` feature. Use `alloc`,
   not `std`, in those modules.
4. **Keep the public API stable** unless the upstream change genuinely requires
   breaking it. If it does, call that out prominently — this crate is published.
5. **No new dependencies.** The port workflow cannot modify `Cargo.toml` at all. If an
   upstream change genuinely needs one, raise it as an issue rather than working
   around it.
6. **One upstream concern per PR.** A reviewer should be able to hold the whole
   change in their head.

## Verification

These mirror `.github/workflows/ci.yml` and must all pass before you propose a change:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all -- --nocapture --test-threads=1
cargo check --lib --no-default-features
```

Two things to know:

- Tests **must** run with `--test-threads=1`. Many bind the Link multicast port
  20808 and collide when run in parallel.
- `libasound2-dev` is installed by a setup step for `rodio`; you do not need to
  install it yourself.
