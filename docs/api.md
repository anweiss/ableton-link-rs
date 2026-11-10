# API guide

[Documentation index](README.md) | [Project README](../README.md)

Repository paths and shell commands below are relative to the repository root.

## `no_std` Usage

For embedded or `no_std` environments, disable the default `std` feature to access only the core types:

<!-- x-release-please-start-version -->
```toml
[dependencies]
ableton-link-rs = { version = "0.4.0", default-features = false }
```
<!-- x-release-please-end -->

This gives you access to core types like `Beats`, `Tempo`, `Timeline`, `GhostXForm`, `StartStopState`, and `NodeId` without pulling in networking dependencies, along with the wire encoding layer in `src/encoding.rs` (`Encode`/`Decode`, `encode_to_vec`, `decode_from_slice`). Requires `alloc`.

The three private nanosecond clock adapters used only by the controller compile
with `std` or unit tests. Public `GhostXForm` methods and the core timeline
composition helpers remain available without `std`.

## LinkAudio Usage

Audio sharing is opt-in via the `audio` feature, which implies `std`:

<!-- x-release-please-start-version -->
```toml
[dependencies]
ableton-link-rs = { version = "0.4.0", features = ["audio"] }
```
<!-- x-release-please-end -->

## BasicLink

The main entry point for using Ableton Link:

```rust
let mut link = BasicLink::new(120.0).await?;

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

## SessionState

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
let state_b = state_a.clone();
assert_eq!(state_a, state_b);
```

## LinkAudio (`feature = "audio"`)

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

The example waits for 50 ms of received audio before starting or recovering from starvation.
Input received while the output device is opening is metered but discarded.
Prefill begins with fresh input after the source is first rendered, rather than
replaying a stale queue accumulated during device startup.
Queued audio expires after the larger of one second or twice the ring's frame
capacity expressed as a duration. Expired frames are discarded and counted, so a
partial refill cannot replay arbitrarily old audio after a long outage.
Expiry discards at most eight queued frames per rendered frame, keeping playback
in recovery until the stale prefix is gone and fresh prefill is available.
`LINK_AUDIO_LATENCY_MS` requests this prefill target, clamped to 1-1000 ms, not total
end-to-end latency. `LINK_AUDIO_OUTPUT_BURST_MS` reserves a burst budget for the
output device (default 50 ms, also clamped to 1-1000 ms). The effective prefill
is the larger of those two settings, rounded up to whole received frames, and
is printed at startup and in status. This avoids sizing the ring only for tiny
network chunks when the OS pulls larger output blocks. The budget is explicit,
not detected from the device: lower it only after verifying the output callback
duration, or raise it if callbacks consume more than 50 ms at once.

The preallocated SPSC ring holds
`effective target + max(effective target, fade-out frames) + maximum wire-chunk frames`
(all sizes rounded to whole frames).
This reserves room for a complete receive chunk even if a partial prefill is
already queued, consumption is paused for the recovery fade, or a full output
burst arrives at fade completion, including at the
1 ms setting. Maximum chunk size derives from
`AudioBuffer::MAX_AUDIO_BYTES` and the received channel count. Its audio-side
operations do not allocate or take a shared blocking mutex. The example uses the dev-only `rtrb` dependency for this
safe handoff; library consumers do not acquire that dependency.

On starvation it fades the last output frame to silence over 5 ms, waits until
both that fade and the prefill target are complete, then fades back in. When full, it drops incoming whole
frames and crossfades at the resulting gap when it reaches playback. This
concealment smooths discontinuities; it cannot reconstruct missing audio.
The example does not compensate sender/output clock drift or schedule speaker
playback against Link's beat grid. Persistent drift can still cause rebuffering
or overflow, and speakers on different devices are not sample-aligned.

Status distinguishes underrun **events** from missing output **frames**, counts
startup waiting frames separately, and reports dropped frames (including input
discarded while output is opening or expired after an outage), rejected buffers
and incoming peak level, including valid frames dropped during overflow.
Buffered milliseconds use the received sample rate. Missing
frames include the fade-out/rebuffer interval after starvation, not just
zero-valued samples. The receiver accepts 1-32 channels and sample rates through
192 kHz; later format changes or malformed sample counts are rejected and counted,
not played with the wrong format. Restart the example to accept a changed format.
Queue occupancy is accounted on admission and consumption, so buffered duration
also updates before the first render or while output is idle. Rejections before
the first supported format are reported while playback is inactive and retained
in the same counter after startup.
Accepted channel formats do not guarantee speaker routing: the default output
device and Rodio's channel conversion determine which channels are audible.
Surplus channels can be dropped rather than downmixed; use mono or stereo for a
two-speaker monitor. Rejection counts cover decoded buffers considered for this
example's playback, not malformed datagrams or codec data discarded before the
callback. Receive-only mode does not perform playback validation.

## Core Types (available in `no_std`)

| Type | Description |
|------|-------------|
| `Beats` | Beat position stored as integer microbeats |
| `Tempo` | Tempo in BPM with conversion utilities |
| `Timeline` | Maps between beat and time coordinates |
| `GhostXForm` | Linear transform between host and ghost time |
| `StartStopState` | Play/stop state with beat and timestamp |
| `NodeId` | 8-byte peer identifier |

## Wire Encoding (available in `no_std`)

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

> **Breaking change in 0.4.0:** `Encode::encode_to` previously returned `()`. It now returns
> `Result<(), EncodeError>`, and `EncodeError` — previously an uninhabited enum — has its first
> constructible variant, `StringTooLong`. Downstream `Encode` implementations must return
> `Ok(())` (and propagate nested encodes with `?`); downstream callers of `encode_to` must handle
> the `Result`. `EncodeError` is now `#[non_exhaustive]`, so future variants will not be breaking.

## Received data and panics

Everything a peer sends is untrusted, and a host may be built with
`panic = "abort"`, where any panic on a Link task ends the process. The following
parser, measurement, and shared-state paths handle failures without relying on
panic recovery; this is not a blanket guarantee for every API or invalid state:

- Malformed datagrams are errors, never panics. `discovery::messages::parse_message_header`
  and `link::pingresponder::parse_message_header` return
  `Err(Error::Protocol("invalid message size" | "invalid protocol header"))`, and
  `discovery::messages::parse_payload` returns `Error::Protocol("truncated payload entry")` for
  an entry that announces more bytes than follow. The receive loops drop what doesn't parse.
- Timeline and start/stop timestamp encoders use saturating microsecond conversions.
  The `HostTime`, `GhostTime`, and `PrevGhostTime` payload entry encoders instead return
  `Error::Protocol("time out of range")` for times outside the wire format's range.
  This can be a local encoding failure from caller-supplied state, not just an
  error in received network data. Saturation tests cover the helper, timeline,
  start/stop, and ghost-time conversion paths independently of those encoders.
- Synchronous hardened state access uses the poison-tolerant `sync::lock` helper.
  Async gateway and observer commits instead use `try_state`/`retry_state`: they
  yield on contention without partial mutations, retaining lease, deadline and
  epoch cancellation while waiting.
- A peer that can't be measured (no measurement endpoint, or no socket to measure from) is
  reported as a failed measurement, like one that never answers. `measure_peer`
  rejects a missing endpoint before opening its per-call socket or starting a
  dispatcher. The public
  `Measurement::new` and `Measurement::with_socket` constructors also send an empty
  sample vector before returning when the peer has no endpoint, so a retained
  result sender does not leave consumers waiting indefinitely. `Measurement::new`
  handles that case before binding, returning an inert measurement with no socket
  or workers even if a bind attempt would fail.

> **API changes:** `link::error::Error` has a new variant, `Protocol(&'static str)`; code that
> matches `Error` exhaustively needs an arm for it. `link::measurement::Measurement::new` returns
> `io::Result<Measurement>` instead of panicking when it can't bind its socket.

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
