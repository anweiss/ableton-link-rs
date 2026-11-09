# Source migration and managed lifecycle

[Documentation index](README.md) | [Project README](../README.md)

Repository paths and shell commands below are relative to the repository root.

## Managed discovery overview

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
Missing-endpoint constructors also honor cancellation while the caller's result
channel is full. Session transitions reserve join-notification capacity before
committing, then recheck current state; cancellation while waiting does not leave
a half-published switch. Library-managed session operations use a consistent
current/other-session lock order, and PONGs snapshot session identity and clock
transform together. Callers directly accessing public mutex fields must preserve
that lock order; these guarantees do not make arbitrary external mutations atomic.

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

## Protocol-error classification

`link::error::Error` adds `Protocol(&'static str)`; downstream exhaustive matches
must handle it. Discovery and PING/PONG header parsers distinguish an invalid
message size from an invalid protocol header. Payload decoding distinguishes a
declared body extending past the available bytes (`truncated payload entry`) from
a fully present known entry with the wrong declared size (`payload entry size
mismatch`). These failures no longer masquerade as
`Decoding(DecodeError::UnexpectedEnd)`. Primitive decoders still use
`DecodeError::UnexpectedEnd` for missing bytes. Packet acceptance, unknown-entry
skipping and explicit unsupported-IPv6-entry handling are unchanged.

## Stricter packet validation

Compared with 0.3.0, supported known payload entries must declare their fixed
encoded size, and every declared entry body must fit in the remaining datagram.
The earlier decoder ignored the declared size for supported entries, so some
noncanonical packets were accepted or parsed differently; short bodies could
also panic. A truncated unknown or unsupported `aep6` entry now rejects the
payload instead of silently stopping. Custom packet producers must use the
fixed-size encoders and put extensions under separate unknown keys, not enlarge
or mislabel an existing supported entry. There is no wire-version change.

Fully present unknown entries still skip by their declared size, including
zero-length entries. Bounded unsupported `aep6` entries still skip even with an
unexpected size (with a warning); they are not decoded as IPv4 endpoints.
Trailing fragments shorter than the eight-byte entry header are still ignored.
Regression coverage checks all eight supported entry types, bounded extensions
around a canonical payload, and every trailing-fragment length from zero to
seven bytes.

Stock discovery, measurement and PING receive paths discard a payload that
fails parsing. Direct callers of `payload::decode(&mut payload, bytes)` must
discard partial entries after an error: entries decoded before the failure
can already have been appended. `parse_payload` returns no partial payload.

## Breaking managed-event and measurement identity changes

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

## Session-aware observers and retry ownership

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

## Cooperative registry contention and eligible measurement ownership

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


## Atomic admission and clock publication

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
snapshot. Value equality excludes private commit provenance, so equal captured
values do not compare differently just because one capture lacked provenance.
Commit states captured by the same `BasicLink`, rather than constructing
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
`Busy` does not prove that there is an outstanding request: admission may only
have encountered a contended lock and returns no ticket. During result
acknowledgment, `Busy` consumes nothing and does not validate the supplied
ticket or prove that a result is ready. Retain the ticket and retry later,
without spinning in the audio callback; drain or acknowledge only actual tickets.
Retain a ticket until acknowledgment. If every caller-held copy is discarded,
recover the current ticket with `outstanding_audio_commit_ticket()` on
`BasicLink` or `LinkAudio` (through its shared `BasicLink` dereference).
Low-level handlers expose `outstanding_commit_ticket()`. Lookup uses one
try-only lock without allocation; it neither drains a proposal nor consumes a
completed result. `None` means the slot was empty at lookup, while `Busy` means
the lock was unavailable. Drain pending work normally and explicitly read its
result with the recovered ticket before reusing the slot. A concurrent consumer
can acknowledge first, making that copy stale. Lookup does not reset exhausted
identifiers, and discarding a ticket is not cancellation or acknowledgment.
Tickets also carry a non-reused queue identity. A ticket from another instance
returns `UnknownTicket` without acknowledging the local result; low-level
completion rejects it too.
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
by the drain. Registration and latest-value delivery survive callback/registry
contention and are retried on the owned IO runtime. Intermediate undelivered
tempo changes may coalesce. The current accepted tempo is selected after
callback serialization, and delivery is recorded after invocation.
The lower-level `AtomicSessionState` and
`SafeRtSessionStateHandler` follow the same proposal/completion contract:
the legacy `process_pending_updates` helpers are removed rather than silently
changing consuming drains into repeated peeks. Migrate to `pending_update`,
which returns `(ticket, proposal)` without consuming it. Validate the proposal,
use `publish_client_state` only for an accepted snapshot, and record the result
with `complete_pending_update` before advancing a drain loop. Read the result
with `take_commit_result` before reusing the slot. Neither peeking nor successful
queue admission is acceptance.
The non-RT peek, publication and completion helpers acquire synchronous mutexes
and may block. App commits and audio-drain futures call these and other cache
or notification helpers, so their `async` signatures do not make every lock wait
cooperative or offload it to another thread. Run them in a non-RT application
context where synchronous waits are acceptable. The one-request drain limit is
not a wall-clock bound.

Accepted RT snapshots use pinned, preallocated value slots. A preempted publisher
does not force readers to block or substitute default values for accepted state.
`BasicLink::capture_audio_session_state` now returns
`Result<SessionState, AudioCaptureError>`; the low-level
`AtomicSessionState::capture_audio_session_state` and
`SafeRtSessionStateHandler::get_rt_client_state` return
`Result<ClientState, AudioCaptureError>`.
Each audio capture makes one checked reader-pin attempt. A competing reader-count
change returns `Busy`, and counter exhaustion returns `ReaderExhausted` before
incrementing. Handle either by skipping the capture or reusing a caller-owned
accepted value; never retry inside the audio callback. A failed capture may
already have flushed a pending accepted publication, so it does not mean that
accepted state stayed unchanged.
Capture uses try-only locks and performs no allocation; it is not a lock-free or
wait-free API and does not promise a wall-clock deadline. App capture retains its
infallible result through a separate non-RT cache read that serializes with
publication and may block. Do not use app capture as an audio-callback fallback.
Publication can remain pending while concurrent readers overlap continuously;
there is no bounded freshness guarantee under that scheduling pattern.
The controller can also advance after a refresher samples state but before its
RT-cache publication. App and audio captures need not be equally fresh. This
cache lag does not roll back authoritative controller state: managed commits
check capture provenance under their commit guards, and an overtaken or
unprovenanced edit returns `StaleClientState` rather than overwriting newer state.

The public low-level `publish_client_state` methods do not compare revisions.
Custom callers own snapshot order: an old accepted snapshot published after a
newer publication can replace newer cache contents. Serialize sampling and
publication across publishers, or reject superseded snapshots before calling.
The managed BasicLink refreshers serialize their sampling/publication sequence;
that does not give arbitrary low-level callers a monotonic-publication guarantee.

The obsolete local grace period is removed: `AtomicSessionState::new` and
`SafeRtSessionStateHandler::new` take only the initial state, capture/get methods
take no time argument, and low-level commit/update methods take only the incoming
proposal. Remove `grace_period`, `set_grace_period`, `LOCAL_MOD_GRACE_PERIOD`, and
the two obsolete `RtClientState` grace timestamps from downstream code.

Runtime tempo normalization uses 120 BPM for NaN and bounds other values to
20..999 BPM, including construction, local edits and accepted remote timelines.
Canonical storage and publication use the same normalized tempo; raw `Tempo` and
`Timeline` arithmetic remain separate from this runtime policy.
`SessionState::set_tempo` retains the wide reference beat through origin
cancellation rather than narrowing it through public `Timeline::to_beats`.
A sampled beat outside signed microbeat storage no longer shifts an otherwise
representable replacement origin. An unrepresentable final time still clamps
at Chrono's duration limits.
If the normalized tempo is unchanged, the setter leaves the complete timeline
unchanged instead of re-anchoring it at a rounded reference beat. Repeated
same-tempo commits therefore do not move the grid or invalidate held captures.

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
For raw tempos with a rounded beat-period magnitude below 1,000 microseconds,
nanosecond samples can skip microbeats. If the nearest anchors miss, forcing
solves the quantum congruence over the nanosecond residues instead of limiting
the search to two neighboring boundaries. This fallback checks at most 1,998
residue candidates, respecting both duration limits and storable origins.
Timeline beat-to-time and phase-inverse conversions retain nanosecond offsets.
`Tempo::from(Duration)` now includes nanoseconds in its input period instead of
truncating to microseconds: a 500 ns period produces 120,000,000 BPM rather than
infinity. Its `micros_per_beat` result still rounds the period to whole
microseconds, so sub-microsecond periods are not promised an inverse round trip.
`Tempo::micros_to_beats` and `Timeline::to_beats` now include sub-microsecond
input time before rounding to microbeats, rather than truncating it first.
At 120 BPM, a 500 ns offset now yields one microbeat instead of zero.
`Timeline::from_beats` now rounds its offset to nanoseconds, not microseconds:
one microbeat at 120 BPM now yields 500 ns instead of 1 microsecond.
`Tempo::beats_to_micros` still rounds to whole microseconds; its name does not
describe the separate nanosecond timeline inverse. Callers must not use exact
whole-microsecond comparisons for timeline inverse results. These precision
changes apply to the raw public types in both default and `no_std` builds.
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
The public inverse also retains results beyond signed wire-microsecond limits;
an identity transform preserves Chrono's endpoints. Client/session composition
keeps intermediate beats and mapped times wide until offset cancellation is
complete, while preserving its existing separate rounding steps. Wider tempo
scaling divides without overflowing its intermediate numerator. These paths use
signed 128-bit intermediates; affine results outside that range still saturate,
so arbitrary out-of-range cancellation is not an exactness guarantee.
`update_client_timeline_from_session` now returns `Result<Timeline, CommitError>`.
It rejects a mapped session-zero boundary outside Chrono, an unstorable canonical
beat origin, a zero/nonfinite inverse slope, or a composition that changes the
beat at the observation time.
Moving the anchor merely to preserve that raw beat is not sufficient: the origin
also defines public quantum phase. Handle `TimelineOriginExhausted` rather than
publishing a clamped or phase-changing replacement.
`update_session_timeline_from_client` also rejects an out-of-Chrono final origin.
It preserves a fractional mapped zero anchor even when the previous timeline
rounds to beat zero there; only an identical canonical zero time and tempo
leave the session timeline unchanged.
Managed publication additionally rejects session time origins outside the signed
wire-microsecond range with `TimelineOriginExhausted`, before either timeline or
playback state changes. Raw composition helpers retain their wider Chrono range,
and standalone encoders still saturate. Accepted fractional-microsecond origins
retain their nanoseconds locally and truncate to microseconds on the wire.
Managed receipt and join rejection retain the last accepted session, mapping,
client state and capture revision; a rejected promotion is retired so a later
announcement can create a fresh candidate.
`PeerStateChange::SessionStartStopState` now carries
`(SessionId, Timeline, StartStopState)`; preserve the advertised timeline even
when its separate notification was deduplicated. Received playback is applied
only when that timeline matches accepted timing at wire precision: clamped
tempo's rounded beat period, beat origin, and signed-microsecond time origin.
An ignored or rejected timeline cannot supply playback under another peer's
accepted timeline. A newly announced timeline reoffers unchanged playback so a
valid recovery is not lost to deduplication. Unrelated endpoint notifications
remain independent. Combined local edits preflight both fields before changing
revisions, peer state or publication. The lower-level
`controller::update_session_timing` now returns `Result<(), CommitError>` too;
handle rejection. Its existing best-effort lock-contention behavior is unchanged.
Rusthut retries rejected playback alignment from a fresh capture while playback
remains active, with a 100 ms backoff for every commit error. Stopping clears
that backoff; successful alignment is not repeated on each display refresh.
`triple_buffer` is now a dev-dependency for the retained comparison tests, not
part of the normal `std` dependency graph. Its obsolete implicit Cargo feature
is removed. Removing the unused optional `async-trait` dependency also removes
its implicit Cargo feature. Both are source-breaking feature-resolution changes:
remove `async-trait` and `triple_buffer` from downstream dependency
`features = [...]` lists and forwarded features such as
`ableton-link-rs/async-trait` or `ableton-link-rs/triple_buffer`. There is no
replacement crate feature to enable.
Link Classic and LinkAudio tempo encoders saturate signed wire periods without
changing the sign of a negative out-of-range value.
PONG ghost timestamps saturate at the final signed-microsecond wire boundary.
Gateway node publication acquires peer and both responder guards cooperatively
before any mutation. Checked announcement commits release timeout state while
waiting on interface/revision guards and remain lease/deadline/epoch cancellable.

Derived PONG and state timestamps clamp at the final signed wire-microsecond
boundary; checked local ordering commits use canonical wire microseconds.
Direct clock payload encoders retain their fallible range validation: callers
supplying an unrepresentable raw field still receive an error. PONG sample sums
and differences remain wide through averaging;
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

## Interface scanning and controller lifecycle

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
See [execution and shutdown contracts](execution-lifecycle.md) for scope,
reentrancy, and the distinction between core and standalone execution.
