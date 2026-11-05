# Ableton Link Rust Implementation

[![CI](https://github.com/anweiss/ableton-link-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/anweiss/ableton-link-rs/actions/workflows/ci.yml)

A native Rust implementation of [Ableton Link](https://ableton.github.io/link), a technology that synchronizes musical beat, tempo, and phase across multiple applications running on one or more devices. Applications on devices connected to a local network discover each other automatically and form a musical session in which each participant can perform independently: anyone can start or stop while still staying in time. Anyone can change the tempo, the others will follow. Anyone can join or leave without disrupting the session.

## Features

* **Full Link Protocol Support**: Implements the complete Ableton Link specification for tempo, timeline, and start/stop synchronization
* **`no_std` Support**: Core types (`Beats`, `Tempo`, `Timeline`, `GhostXForm`, `StartStopState`, `NodeId`, phase math) work in `no_std` environments with `alloc`
* **Async/Await**: Built on Tokio for efficient asynchronous network operations
* **Cross-Platform**: Works on macOS, Linux, and Windows with platform-specific optimizations
* **Platform-Specific Timing**: High-resolution clocks using `mach_absolute_time` (macOS), `clock_gettime` (Linux), and `QueryPerformanceCounter` (Windows)
* **Session Management**: Automatic peer discovery, session state synchronization, and callbacks for tempo and peer audio-endpoint changes
* **Start/Stop Sync**: Synchronization of play/stop states across devices
* **Memory Safe**: Uses Rust's ownership system for safe concurrent networking. The package sets `unsafe_code = "deny"` in `[lints.rust]`, covering examples and tests as well as the library. Unix indexed multicast egress uses safe `rustix` APIs; Windows example console mode uses the dev-only `crossterm` wrapper. Remaining narrowly scoped exceptions cover unicast egress options, Darwin indexed memberships, Unix topology descriptor registration and Windows topology notifications; this is not yet an entirely safe-Rust codebase
* **Ingress-Aware Discovery**: On Linux, macOS and Windows, managed discovery receives the OS interface index with each datagram and selects that interface's registered response socket, not a source-address prefix match. Raw topology notifications invalidate older leases even when the final interface snapshot is unchanged. Address-keyed groups retain duplicate-address adapters; memberships and egress select the interface by index. Optional `std` dependencies include `socket-pktinfo`, Unix `nix`, and Windows SDK bindings. See [discovery interface behavior and evidence](docs/discovery-ingress.md) for platform fixtures and remaining limits
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
    let mut link = BasicLink::new(120.0).await?;

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
    link.commit_app_session_state(session_state).await?;

    // Clean shutdown
    link.disable().await;

    Ok(())
}
```

Managed discovery advertises each interface's bound reply socket as its measurement
endpoint. PINGs received there produce PONGs from that same registered endpoint,
so overlapping subnet routes cannot substitute a different source address.
Managed outbound measurements use the registration where discovery learned that
peer endpoint, including its interface-bound socket and its own PONG dispatcher.
Missing, expired, or invalidated routing hints fail the measurement rather than
falling back to an ambiguous wildcard route. Standalone measurement APIs still
use caller-supplied sockets or OS routing; exact peer endpoint matching is unchanged.

Discovery forwarding uses a bounded queue and a lifecycle-owned delivery worker,
so a slow event consumer does not stop that socket from handling PINGs/PONGs.
Overflow discovery updates are dropped; periodic announcements can refresh them.
Each queued event retains its ingress lease: invalidation cancels a capacity wait,
and publication rechecks that exact registration. Route hints prune expired and
retired leases and are capped at 1,024 entries overall and 16 endpoints per peer;
new hints evict the earliest-expiring entry at a limit, so an evicted hint fails
closed until a later accepted announcement refreshes it.
Each measurement keeps competing terminal results on its own private channel,
then forwards the winning result with caller backpressure. Public measurements
sharing a full result channel therefore do not discard each other's completions.

Received announcements retain an absolute deadline through discovery and gateway
capacity waits. Expired work is discarded rather than renewing TTL. Fresh ingress
traffic can refresh an existing published route even when the handoff is full;
it cannot add a route for an unpublished identity. Cache sweeps drain each shared
topology monitor once. Same-adapter address selection prefers the peer's link-local
scope and verified subnet; source addresses never select a different adapter.
Periodic adapter and local-identity enumeration run in the blocking scan together; reconciliation
reuses that one snapshot, including retrying failed identities without replacing sockets.
Managed measurement cancellation follows the captured lease through queued PONGs
and final result publication. PONG samples are appended only after every applicable
time sum validates, and standalone early failure reporting observes cancellation.

Queued announcements own node-scoped supersession tokens independently of endpoint-keyed
route hints. Socket receipt, timestamp capture, decoding and revision admission share
one interface-map critical section, so an earlier reader cannot admit behind a newer
withdrawal. Positive announcements retain tokens, leases and their original deadlines
through the bounded handoff, gateway queue and observer wait, including state-lock contention. Each stage reserves downstream capacity before a synchronous checked
commit; the observer commits peer changes and its notification together. The weak
index owns no tombstones or leases: live keys are bounded by owned handoff/delivery
items plus the bounded gateway/observer queues and handlers (not arbitrary copies
retained by external consumers). Dead keys are reclaimed on the next observation.
TTL-zero and managed BYEBYE are explicit existing-peer withdrawals; expired positive
announcements are discarded. Wire TTL is preserved and expires_at carries exact life.
Once the gateway removes a timeout, it queues an irrevocable FIFO `PeerLeft` cleanup
obligation. Newer dropped traffic cannot cancel that removal and orphan observer state;
a later admitted positive announcement follows it and may recreate the peer.
Managed retry selection holds current-session, candidate and peer guards in that order.
When no peer remains for a candidate, it retires only that incarnation; an unchanged
same-ID sighting rearms measurement with a fresh incarnation rather than stranding it.

## Source migration and managed lifecycle

### Breaking managed-event and measurement identity changes

- `PeerStateMessageType` literals add `expires_at` and `observation`. Standalone
  callers use `None` for both. Managed forwarders must preserve the original deadline,
  opaque `Arc<PeerObservation>` and advertised `ttl`; never regenerate any of them.
  A true zero TTL withdraws existing state. Managed BYEBYE is normalized to this
  zero-TTL peer-state event; standalone `receive_bye_bye` still emits `OnEvent::Byebye`.
- `PeerEvent::Observed(PeerStateMessageType)` carries positive gateway announcements
  (including standalone calls) with the original deadline to `GatewayObserver`.
  Exhaustive matches need the new variant. Forward it intact to the stock observer;
  converting it to unguarded `SawPeer` discards ordering and lease protection.
- Construct sessions with `Session::new(id, timeline, measurement)` rather than struct
  literals. Cloning retains a local incarnation; `reset_session` replaces it even for
  the same wire ID. Capture `session.key()` **when requesting** a measurement.
- `MeasurePeerEvent::PeerState` and `XForm` now carry `SessionKey` instead of `SessionId`;
  obtain the wire ID through `key.session_id`. Forward the same key with the result.
  `Cancelled(key, attempted_peer)` is retryable lease retirement, not a successful transform or a
  lifecycle-wide shutdown. Managed candidates retry only while that incarnation lives.
- `measure_peer`, `measure_peer_with_socket`, `handle_successful_measurement` and
  `handle_failed_measurement` take `SessionKey` in the former session-ID position.
  Standalone users may create `SessionKey::new(id)` only for independently consumed
  results. It cannot match an existing `Session`. Both session result handlers require
  `session.key()` captured when requesting the measurement, including standalone setups.

These are source-breaking changes, not wire-format changes. Managed result commits
check the originating incarnation before success/failure mutation and after capacity
waits. Controller consumption validates queued joins under the same current-session
lock as resets; user callbacks run after that lock is released. Lease cancellation
still observes lifecycle shutdown while reporting its retryable terminal result.
Pruning and departure reserve notification capacity before changing timeout state,
then recheck/remove/publish under one poison-recovering guard.

### Session-aware observers and retry ownership

`PeerGateway::new` and `GatewayObserver::new` now take a final `Option<Sessions>`.
Construct `Sessions` first and pass `Some(sessions.clone())` when results feed its
session handlers; the observer and coordinator must share the same peer registry.
Use `None` only for an independent peer observer whose consumer owns session state.
The coordinated observer holds current-session and candidate guards across peer
mutation and retirement: removing the last candidate peer immediately invalidates
its incarnation, before a queued controller notification or late result can reuse it.
Current-session retry ownership survives temporary peer absence until reset/disable.

Observers copy the local identity and release its mutex before waiting for peer-list
or ingress guards. This avoids inversion with the receive path, which reads that
identity under ingress serialization; coordinated session guards still cover resets.
Discovery admission and receive timestamps remain serialized, including withdrawals:
state contention does not discard them or restart their TTL.
Observers acquire peer/session guards before checked ingress publication, so contention on
those guards does not retain the interface-map/revision locks needed by PING/PONG.
Measurement sample append likewise acquires its data guard before its ingress check.
All receive deadlines are still rechecked after blocking state acquisition. A rejected
final sample publication reports originating-key cancellation and releases workers.
Queued controller joins use the latest guarded snapshot of the matching incarnation,
not the older timeline/transform carried in the notification.

Public `schedule_remeasurement` is one-shot. The standalone success/failure handlers
may rearm it after each result; they no longer accumulate permanently recurring loops.
Managed retry workers remain lifecycle-owned and deduplicated per incarnation.
A peer without an advertised measurement endpoint reports a failed transform, not
retryable route retirement; public measurement constructors start no workers for it.

### Cooperative registry contention and eligible measurement ownership

Observer commits and managed session/count operations retry contended mutex acquisition
as one cancellable async operation, releasing every acquired guard before waiting.
They do not park the owned single-thread IO executor or create blocking-pool jobs.
Ingress/revision checks in observer publication are also nonblocking; original deadlines,
receive ordering, and irrevocable gateway departure cleanup remain in force. A controller
membership batch stays ordered while waiting and is cancelled with its lifecycle epoch.
The synchronous `unique_session_peer_count` waits for a correct snapshot; async callers
can use `unique_session_peer_count_async`. Candidate retirement builds one live-session
set rather than rescanning peers for each candidate.

Measurement selection prefers endpoint-advertising members, with leader preference
within that group; no-endpoint sessions still receive terminal failed measurements.
Failed-origin retirement removes every matching incarnation entry, without deleting a
distinct incarnation or its peers. Join consumption validates live peers under the same
session/registry guards, resetting a peerless remote promotion even if both counts were zero.

`schedule_remeasurement` returns `RemeasurementTask`, replacing `JoinHandle<()>`.
Public `handle_successful_measurement` and `handle_failed_measurement` return
`Option<RemeasurementTask>`. Retain each returned owner while a retry is wanted;
dropping it aborts pending work instead of detaching it. `abort`, `is_finished`, and
awaiting completion remain available. Each owner sends at most one request, cancels
on receiver closure or replacement of its originating current incarnation, and uses
an async channel-capacity wait plus bounded-rate peer-absence checks rather than
5ms polling for either condition. Already committed requests are not retracted.

Join/reset commits acquire identity, session/client timing, sync policy, peer count,
and responder publication guards in the same cooperative attempt as session and peer
ownership. A busy guard means retry, not completed/no-change. No state is mutated until
all required guards are held; callbacks run after release. This includes peerless resets
and equal-timing joins whose client session identity still needs to change.
Timeline announcements publish the accepted current identity, timeline and clock mapping
from one guarded snapshot, never a candidate's wire ID paired with the current timeline.
A pending promotion leaves local publication to its owned join, including equal-timing joins.


### Atomic admission and clock publication

Initial sightings reserve measurement capacity before installing a candidate. Managed
retries reserve capacity, then validate the originating key, select the latest peer,
and publish synchronously under current/candidate/peer guards. Cancellation before
admission leaves no candidate or queued stale request. Standalone launch also selects
a peer only after capacity is available. `Cancelled(key, attempted_peer)` identifies
the failed `NodeId`; managed retries rotate through endpoint-advertising peers instead
of repeatedly choosing a leader with an unavailable route. Candidate result commits
match the full key in every lookup and compare against the latest current session;
results originating from a replaced current session are rejected.

Construct `Sessions` with `Sessions::new`, not field literals: publication identity is
now private coordinator state. `Sessions::reset_timeline` takes
`(&SessionKey, GhostXForm, Timeline)` and returns `bool`; capture the key and mapping
with the source timeline, and handle `false` if that source changed. This low-level
synchronous helper is not a controller client-state publication API. Use async
`Controller::set_state` for client edits: calculation and installation share the full
publication commit, including incarnation and clock mapping. Pending promotions wait
for their owned join; even a same-wire-ID replacement cannot receive an old-clock edit.
`disable` cooperatively awaits peer reset and publishes a fresh local incarnation, so
cancelling a queued promotion does not strand later edits while disabled.

PING replies snapshot membership and clock together in writer lock order using
nonblocking acquisition. A contended PING is skipped, leaving retry ownership with the
remote measurement; no deferred reply tasks accumulate or park the sole IO executor.
Async responder publication also waits cooperatively for both guards.

Beat arithmetic saturates at the signed microbeat limits; remainder with a zero
quantum or MIN/-1 returns zero. Phase arithmetic uses wide intermediates. Received
extreme beat origins and start/stop values therefore do not overflow during joining
or client conversion. Inverse ghost-clock conversion retains the wide difference
through division and saturates only the final host-microsecond conversion.

Client edits use the accepted current timeline under the matching published identity
and mapping, even when a same-incarnation announcement is ahead of local publication.
Received start/stop updates use that same cooperative commit boundary: no timing or
client state changes before every publication guard is acquired, and the ordered
receive batch remains epoch-cancellable while waiting. An intervening join cannot
cause a responder to publish a new identity with an old session's clock snapshot.

Local commit ordering is checked before any authoritative state changes.
`timeline::update_session_timeline_from_client` returns `Result<Timeline, CommitError>`;
`Controller::{set_state, handle_client_state}` and
`BasicLink::commit_app_session_state` return `Result<(), CommitError>`.
`LinkAudio::commit_app_session_state` forwards that result. Handle
`TimelineOriginExhausted` and `StartStopTimestampExhausted`: the entire edit is
rejected, including a combined tempo/start-stop edit, and success callbacks do not
run. Exhaustion does not automatically create a new session or restore synchronization.
`StaleClientState` also rejects a proposal captured from a replaced incarnation or
overtaken client-edit revision, before either field of a combined edit is installed.
Capture-time provenance is retained across queueing/retries, not refreshed when
draining. Same-ID cloned resets, client-only alignment, accepted playback observations,
mapping changes and value A->B->A changes invalidate old captures. Provenance contains
only copyable values, not reference-counted allocations that could be freed on an
audio thread. Process-local edit identifiers never wrap; exhaustion prevents new
captures from authorizing commits rather than reusing an old identifier.
`SessionState` is now `Clone`, not `Copy`; use `.clone()` if retaining a submitted
snapshot. Commit states captured by the same `BasicLink`, rather than constructing
an untracked `SessionState::new` or default value for submission. A contended or
not-yet-published capture has no commit provenance and returns `StaleClientState`
on submission processing; recapture after publication settles. Read-only captures
still return accepted client values, not a fabricated zero-tempo/stopped state.
Custom deferred controller callers use `capture_client_state()` and
`set_state_checked(state, &origin)`. The untracked `set_state`/`handle_client_state`
entry points are immediate requests; callers must not use them to bypass deferred
provenance checks. Start/stop proposals older than or equal to the last local
capture/observation are also stale. Received start/stop changes use a local observation
timestamp; their remote logical timestamp cannot permanently outrun the local clock.
Accepted synchronized edits advance a checked signed-microsecond wire timestamp,
even when successive fractional mapped timestamps would encode identically.

Audio commits now explicitly mean **queued**, not accepted.
`BasicLink::commit_audio_session_state` returns
`Result<AudioCommitTicket, AudioQueueError>` without allocating or blocking.
It never changes captured state optimistically. There is one outstanding slot:
handle `Busy` (including lock contention) without assuming submission, and
`TicketExhausted` rather than expecting identifiers to wrap.
Call `process_audio_session_state_updates().await` periodically from a non-RT
application thread, including to refresh remote accepted state. It drains at most
one request through the checked controller path; `LinkAudio` forwards this method.
Both drains take `&self`, so a non-RT consumer can share `Arc<BasicLink>` with an
audio callback. A concurrent or reentrant drain returns `None` without consuming
the request. Audio capture/submission never acquires the non-RT consumer claim.
Read `take_audio_commit_result(ticket)` until it returns `Some(result)`, then handle
that commit result. A completed ticket occupies the slot until acknowledged.
Cancellation before acceptance leaves the original request queued, with its
changed-field mask intact; capture cannot consume it. After controller acceptance,
the ticket's terminal result is recorded before any cancellable refresh. A later
drain resumes accepted-state refresh and pending playback notification, not the
proposal. Playback dispatch has its own nonblocking claim and records delivered
state only after invoking the callback. A busy dispatcher retains the latest
pending playback value; intermediate undelivered values may coalesce. No
controller/cache guard is held while user callbacks run.
Tempo notification remains in the controller and is not dispatched again
by the drain. The lower-level `AtomicSessionState` and
`SafeRtSessionStateHandler` follow the same proposal/completion contract:
`process_pending_updates` peeks, `pending_update` also supplies the ticket,
`publish_client_state` installs only a controller-accepted snapshot, and
`complete_pending_update` records the validation result before acknowledgment.

Accepted RT snapshots use pinned, preallocated value slots. A preempted publisher
does not lock out readers or cause default values to replace an accepted snapshot.
Publication can remain pending while concurrent readers overlap continuously;
there is no bounded freshness guarantee under that scheduling pattern.
The obsolete local grace period is removed: `AtomicSessionState::new` and
`SafeRtSessionStateHandler::new` take only the initial state, capture/get methods
take no time argument, and low-level commit/update methods take only the incoming
proposal. Remove `grace_period`, `set_grace_period`, `LOCAL_MOD_GRACE_PERIOD`, and
the two obsolete `RtClientState` grace timestamps from downstream code.

Runtime tempo normalization uses 120 BPM for NaN and bounds other values to
20..999 BPM, including construction, local edits and accepted remote timelines.
Canonical storage and publication use the same normalized tempo; raw `Tempo` and
`Timeline` arithmetic remain separate from this runtime policy.

`controller::update_discovery` now accepts `&Controller`, replacing three independent
state arguments. It reads and publishes through the owner's full guarded boundary;
callers no longer provide separately captured session, peer and gateway state.

The minimum Chrono version is now **0.4.39**, including for `no_std` consumers.
Elapsed time, timeline beat differences and clock scaling retain wide intermediates
through conversion and origin/intercept addition. Public `Beats` operators still
saturate; timeline composition does not use their narrowed intermediate results.
Tempo periods stay wide in both beat conversion directions. Forced-beat requests
re-anchor before narrowing an unrepresentable origin when a bounded stored origin
and time can encode the request, including an effective quantum boundary just
outside the stored origin's range. Re-anchoring retains nanosecond precision when
whole-microsecond rounding misses the requested microbeat, including at Chrono's
duration limits. Ordinary timeline shifts retain their existing conversion rounding.
A clamped anchor is accepted only if it still encodes the requested microbeat.
Timeline beat-to-time and phase-inverse conversions retain nanosecond offsets.
Controller client/session and start/stop mappings use internal nanosecond clock
conversions so publication does not discard an accepted alignment. The public
`GhostXForm` methods now retain fractional-microsecond inputs through scaling or
division instead of truncating those inputs first. They round the scaled forward
offset or inverse host result to microseconds, with half-ties away from zero.
With an identity mapping and zero intercept, both directions now map +/-1,500 ns
to +/-2 microseconds rather than +/-1 microsecond; +/-500 ns maps to +/-1
microsecond rather than zero. This is a compatibility change for callers relying
on the former input truncation. Wire timestamps remain signed microseconds;
their format is unchanged.
The public rounding correction clamps at Chrono's duration limits.
Link Classic and LinkAudio tempo encoders saturate signed wire periods without
changing the sign of a negative out-of-range value.
PONG ghost timestamps saturate at the final signed-microsecond wire boundary.
Gateway node publication acquires peer and both responder guards cooperatively
before any mutation. Checked announcement commits release timeout state while
waiting on interface/revision guards and remain lease/deadline/epoch cancellable.

Raw clock payload fields and start/stop timestamps clamp when encoded as signed
wire microseconds; checked local ordering commits use canonical wire microseconds.
PONG sample sums and differences remain wide through averaging;
individual received fields must still be representable in the wire format.
`Tempo::micros_per_beat`, `Tempo::beats_to_micros`, timeline conversion and forward ghost scaling can return
Chrono durations beyond the signed-microsecond range. Callers must handle
`num_microseconds() == None` instead of unwrapping an arbitrary derived duration.
Measurement workers acquire public sample, success and retry-counter mutexes
cooperatively with poison recovery. Sample append returns its count under the same
guard; completion drains under one guard, and timer state waits remain cancellable
by completion without parking the current-thread executor.
Managed discovery forwarding and measurement publication use the same cooperative
interface-lease checks; timeout lookup, pruning and shutdown also release guards
before waiting. Ignoring an `Option<RemeasurementTask>` returned by a public result
handler drops the owner and aborts that pending retry immediately.

The receive path acquires the interface map, peer/enabled policy, observation
revision index and route cache before its nonblocking receive syscall. Receipt
sampling, observation and bounded admission remain one synchronous region; a
contended guard cannot consume and silently drop a withdrawal. Final forwarding
acquires its route-cache guard before checking the original receive deadline.
Registered sends likewise acquire their map cooperatively, then check the exact
registration and perform the syscall without an intervening await. Lease and
epoch cancellation remain active while these attempts wait.

Owned startup snapshots, scanning/reconciliation, route pruning/resolution and
broadcast snapshots also yield on contention. Reconciliation revalidates topology
after acquiring the map; membership changes, queue draining and registration
publication stay serialized with receive and send. Enable acquires every final
drain/policy guard before clearing hints and opening its gate; asynchronous
lifecycle checks never block on the enabled flag. Synchronous setup helpers remain
synchronous. The owned-runtime regression overlaps raw withdrawals, encoded PING
traffic, scanning/reconciliation and broadcasting under five contended guards,
and checks original-deadline expiry, registered-source delivery and cancellation.

An owned ALIVE broadcast holds its originating discovery-epoch permit through
snapshot acquisition and the registered send. Closing that epoch cancels a send
waiting on the interface map before disable completes, even though the interface
lease remains live. Re-enabling cannot revive the old announcement; fresh work
uses the new epoch. The shared notifier remains a wakeup, not an inferred stop.
Wire regressions pause after snapshot/encoding, complete actual controller disable
with an unchanged registration, and distinguish a fresh150BPM announcement from
the captured120BPM payload during rapid re-enable.

Phase-origin differences, modulo reduction, forced-beat composition and timeline
shifts now retain wide intermediates through the final result. Session comparison
also subtracts mapped clock times in wide nanoseconds, not bounded Chrono
durations. The public saturating `Beats` operators are unchanged.

An announcement commits its timeout and registers the replacement pruning worker
without an intervening suspension, so cancelling a standalone announcement future
cannot strand the committed peer. Ctrl-C cleanup retries a contended peer identity
cooperatively and releases its guard before sending BYEBYE.

### Interface scanning and controller lifecycle

Interface scanning retains IPv4 link-local addresses for networks without DHCP,
while excluding adapters the OS reports as operationally down. This applies to
managed discovery and both public scanners, including their `if-addrs` fallback;
an unavailable status lookup or an unknown adapter index does not exclude an address.

Discovery, measurement, and reply sockets bound to port `0` leave address/port
reuse disabled so ephemeral unicast sockets do not opt into port sharing.
Explicit nonzero ports retain sharing, including the discovery listener on UDP
20808. This is not a Windows `SO_EXCLUSIVEADDRUSE` guarantee against another
application forcibly binding with `SO_REUSEADDR`.

Each controller owns a dedicated `Link IO` thread and current-thread Tokio runtime.
External drop closes callback admission and joins that thread before returning.
`link.disable().await` remains the restartable way to quiesce dispatch.
Disable cancels active measurements
and parks request intake before stopping the measurement-result,
join-session, and peer-state-change consumers rather than destroying them, and
suppresses discovery broadcasts. `link.enable().await` resets session state and
waits for those consumers to discard disabled-lifecycle queues before admitting
fresh work. Measurement results are also checked against their originating
lifecycle so a late result cannot be forwarded after a restart.
The discovery event and observer queues participate in their own acknowledged
gate; prior-lifecycle packets, blocked sends, and pruning work cannot refill the
reset peer list. Session remeasurement has one owned, epoch-scoped retry worker
rather than accumulating detached schedulers. Transient UDP receive errors retry
with a short backoff instead of permanently removing an interface receiver.
Socket reads are cancelled and re-armed across the barrier so the first fresh
packet is accepted, and BYEBYE forwarding remains inside that cancellation scope.
Receivers continue discarding during preparation; the final admission transition
performs another queued-datagram drain before publishing enabled.
Interrupted enable/disable can be retried. Startup resets peer counts and
publishes enabled after preparation but before admitting packets; disable clears
both controller and real-time atomic enabled flags before awaiting shutdown.
The standalone `Messenger::listen` and `PeerGateway::listen` APIs retain their
notifier-based terminal cancellation; the controller uses separate owned paths.
Repeated disable/enable cycles resume peer measurement and session
joining; enabling an already-enabled instance is a no-op.

Dropping alone closes callback/dispatch admission and cancels owned
dispatch, discovery/broadcast (including socket receivers and interface scanning),
gateway-observer, and measurement tasks. The observer also exits when its input
channel closes and can be cancelled while its downstream queue is full.
Temporary-disable notifications cannot terminate the
owned broadcaster, even when handled after re-enable;
external drop waits for an executing core callback and for owned runtime cleanup.
LinkAudio constructs its audio engine and peer-sync task on that same executor.
It closes audio admission first, then the core join waits for both subsystems;
the audio engine's bounded best-effort BYEBYE cleanup is not a substitute for
that join. Channel notifications coalesce while a callback is running and drain
after it returns; no registration lock is held across user code. Managed source
callbacks retain the shutdown check even when replaced.
If a channel callback panics, it is removed, pending work is drained through any
registered replacement, and the original panic propagates. A failed callback
is not retried automatically.
If a core callback drops its own controller, that invocation may finish but no
later invocation is admitted. A process-wide, owned join service retains the IO
thread handle and joins it after the callback returns; it is not self-joined.
Do not block a callback on work that requires its own IO thread, or hold an
application lock across drop that the callback needs to finish.
See [execution and shutdown contracts](docs/execution-lifecycle.md) for scope,
reentrancy, and the distinction between core and standalone execution.

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
// Fires when a peer's advertised audio endpoint (LinkAudio) is first seen or
// later changes — edge-triggered, not once per sighting. Not called when a
// peer leaves.
link.set_audio_endpoint_callback(|peer_id, endpoint| { /* ... */ });

// Session state
let state = link.capture_app_session_state();
link.commit_app_session_state(state).await?;
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

Audio sharing is initially paused. Requesting it with `enable_link_audio(true)`
starts network activity only while Link is enabled; disabling either pauses
audio receive/send activity and disconnects sink receivers. Re-enabling Link
resumes a preserved audio-sharing request without recreating the engine.
Pause fences individual UDP sends by generation and discards queued/encoded
audio, including buffers retained by the application before pause but committed
after resume. Normal endpoint withdrawal waits for contention; final destruction
uses best-effort withdrawal before shutting down and joining discovery, so a
caller-held peer-state guard cannot deadlock destruction.

`LinkAudio` derefs to `BasicLink`, so the read-only Link API remains available directly. The
`&mut self` parts of `BasicLink` — `enable`, `disable`, `enable_start_stop_sync`,
`commit_app_session_state` and the callback setters — are forwarded as inherent methods on
`LinkAudio` instead of through `DerefMut`, which `LinkAudio` deliberately does not implement: audio
sharing only runs while Link itself is enabled, and handing out a `&mut BasicLink` would let
`disable` be called behind `LinkAudio`'s back, leaving the peer-sync task and this peer's announced
audio endpoint live against a disabled session. On top of that it publishes
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
returns `Result<(), EncodeError>`; the only failure `encode_to` itself raises is
`EncodeError::StringTooLong`, raised
when a string is longer than its `u32` length prefix can describe, so a truncated prefix can never
desynchronize the rest of the stream.

`EncodeError` also carries `MessageTooLarge { size, max }`, which the encoding layer itself never
raises; it is produced by the message encoders that sit above it. With the `std` feature,
`discovery::messages::encode_message` and `link::pingresponder::encode_message` return
`Err(Error::Encoding(EncodeError::MessageTooLarge { size, max }))` when a message's total encoded
size — protocol header plus message header plus payload — would exceed that module's
`MAX_MESSAGE_SIZE`; the discovery and ping/pong send paths log and drop such a message
rather than failing the caller. Note the wrapper: both yield `link::Result`, so
callers match on `Error::Encoding(..)` and destructure the `EncodeError` inside it. The ping and
pong payloads this crate builds are bounded by construction, so the ping/pong encoder only reports
this for payloads supplied by a caller. With the `audio` feature,
`link_audio::messages::encode_message` enforces its own limit and reports it as
`AudioError::MessageTooLarge`.

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
| ESP32 (ESP-IDF) | `esp_timer_get_time()` (via `esp-idf-svc`) |

## Thread Priority

`platform::ThreadPriority` raises (and can later restore) the scheduling priority
of the *calling* thread. It is intended to be called from a thread that drives
time-critical Link work, mirroring upstream's `platform::ThreadPriority`:

```rust
use ableton_link_rs::platform::ThreadPriority;

let mut priority = ThreadPriority::new();
priority.set_high(); // capture current params, then raise
// ... time-critical work on this thread ...
priority.reset();    // restore the captured params
```

`set_high` captures the thread's current scheduling parameters on the first call
and is a no-op until `reset` is called; `reset` restores them and is a no-op if
nothing was captured. The captured state is thread-affine: `set_high` and the
matching `reset` must run on the same thread.

This is implemented over the [`audio_thread_priority`][atp] crate rather than
hand-written FFI, so it contains no `unsafe`. The crate drives the same three OS
mechanisms upstream uses:

| Platform | Mechanism | Privileges |
|----------|-----------|------------|
| Linux | `pthread_setschedparam` with `SCHED_FIFO`, priority 10 | Requires `CAP_SYS_NICE` or a suitable `RLIMIT_RTPRIO`; has no effect otherwise |
| macOS, iOS | mach `thread_policy_set` with `THREAD_TIME_CONSTRAINT_POLICY` | None |
| Windows | MMCSS, `Audio` task class | None |
| Android | `setpriority` to nice -19 (urgent audio) | None |
| Everything else | No-op | — |

Three deviations from upstream are deliberate and documented on the type. In each
case the crate's behaviour is given first, then upstream's:

- **Linux priority** — the crate uses `SCHED_FIFO` priority 10; upstream uses 35.
  The crate's override for this writes a process-global that is never restored,
  and is absent when its `dbus` feature is enabled, so a library must not call it.
- **macOS time constraint** — the crate asks for `computation = period / 2`;
  upstream asks for `0.2 * period`.
- **Windows task class** — the crate registers the `Audio` MMCSS task and never
  calls `AvSetMmThreadPriority`; upstream registers `Distribution` and
  additionally boosts to `AVRT_PRIORITY_HIGH`.

None is network-visible — this is host-side scheduling only.

Upstream invokes `ThreadPriority` from its `linkaudiohut` example through
`link.callOnLinkThread`. This port offers
`link.set_io_thread_priority(true).await` on `BasicLink` (also through `LinkAudio`).
It sends the request to that controller's owned IO thread; `false` restores its
captured scheduling. Default construction does not request elevated priority.
The method returns OS errors and shutdown attempts to restore priority on the
same thread, logging restoration failures. Tempo callbacks remain nonblocking;
overtaken notifications are suppressed after acquiring the callback mutex so
an older tempo cannot be delivered after a newer callback. The managed wrapper
also rechecks client tempo after taking the actual user-callback mutex, which
serializes it with synchronous application callbacks.
It never changes a shared caller-runtime worker or an audio playback thread.
The `link_audio` example opts in with `LINK_IO_REALTIME=1`.

The lower-level `ThreadPriority::set_high` and `reset` remain best-effort APIs;
`try_set_high` and `try_reset` return errors. Promotion failures are logged at
debug level by the legacy wrapper; restoration failures are logged as errors.
Successful scheduling requests are not evidence of measured clock accuracy.

[atp]: https://crates.io/crates/audio_thread_priority

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

# Validate the upstream porting backlog (stdlib only; watermark checks need the submodule)
python3 .github/scripts/validate-upstream-backlog.py
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
| `link-upstream-watch.md` | Weekly, Monday | Triages upstream commits landed since the pin and proposes a pull request against [`.github/upstream-backlog.toml`](.github/upstream-backlog.toml) |
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
[`.github/workflows/shared/link-upstream-context.md`](.github/workflows/shared/link-upstream-context.md).
Edit that file to change how either workflow reasons about a change.

**The backlog file.** The backlog is
[`.github/upstream-backlog.toml`](.github/upstream-backlog.toml), versioned with the
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
[`.github/scripts/validate-upstream-backlog.py`](.github/scripts/validate-upstream-backlog.py)
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
[`copilot-review-loop.yml`](.github/workflows/copilot-review-loop.yml) clears that for
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
[`auto-merge-upstream-port.yml`](.github/workflows/auto-merge-upstream-port.yml)
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

**The `gh-aw lock drift` CI job** (in [`ci.yml`](.github/workflows/ci.yml)) downloads
the gh-aw v0.87.10 release binary, verifies its checksum, and runs
[`.github/scripts/check-aw-lock-drift.sh`](.github/scripts/check-aw-lock-drift.sh). The
script recompiles every workflow and fails if any `.lock.yml` or `actions-lock.json`
differs from the committed copy, or if the compiler produced a new file that was not
committed. It is not a required check, but a failure means the committed lock files do
not match their sources. To fix it, edit the `.md` source rather than the lock file,
run `gh aw compile --no-check-update` with v0.87.10, and commit the regenerated
`.lock.yml` files and `actions-lock.json` together with the source change. You can run
the same check locally with `bash .github/scripts/check-aw-lock-drift.sh`.

**Dependabot and lock files.** [`dependabot.yml`](.github/dependabot.yml) excludes
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

### Release PRs and the approval gate

release-please opens its release PR from a branch in this repository. If it opens that
PR with `GITHUB_TOKEN`, GitHub creates the PR's workflow runs in the `action_required`
state, so every release needs someone to click **Approve workflows to run** before CI
starts. This is intended GitHub behavior for automation-authored pull requests, not a
misconfiguration, and it cannot be switched off with a repository setting — the
fork-PR approval policy governs pull requests *from forks* and does not apply here.

To make release PRs run CI unattended, give release-please an identity other than
`GITHUB_TOKEN`. [`release-please.yml`](.github/workflows/release-please.yml) tries
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

## Documentation

* [Official Ableton Link Documentation](https://ableton.github.io/link)
* [Changelog](CHANGELOG.md)

## Compatibility

This implementation aims for full compatibility with the official Ableton Link specification and interoperates with applications using the official C++ Link library.

## Support

For questions about this Rust implementation, please [open an issue](https://github.com/anweiss/ableton-link-rs/issues). For general Ableton Link questions, contact <link-devs@ableton.com>.
