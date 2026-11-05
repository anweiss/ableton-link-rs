#![allow(clippy::too_many_arguments)]

#[cfg(feature = "std")]
pub mod atomic_session_state;
#[cfg(feature = "std")]
pub mod audio_endpoint;
pub mod beats;
#[cfg(feature = "std")]
pub mod clock;
#[cfg(feature = "std")]
pub mod controller;
pub mod encoding;
pub mod error;
pub mod ghostxform;
#[cfg(feature = "std")]
pub mod host_time_filter;
pub mod linear_regression;
#[cfg(feature = "std")]
pub mod measurement;
pub mod median;
pub mod node;
#[cfg(feature = "std")]
pub mod payload;
pub mod phase;
#[cfg(feature = "std")]
pub mod pingresponder;
#[cfg(feature = "std")]
pub mod safe_rt_session_state;
#[cfg(feature = "std")]
pub mod sessions;
pub mod state;
pub mod tempo;
pub mod timeline;

use core::result;

#[cfg(feature = "std")]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

#[cfg(feature = "std")]
use chrono::Duration;

#[cfg(feature = "std")]
use self::{
    atomic_session_state::AtomicSessionState,
    beats::Beats,
    clock::Clock,
    controller::Controller,
    phase::{force_beat_at_time_impl, from_phase_encoded_beats, phase, to_phase_encoded_beats},
    sessions::SessionId,
    state::{ClientStartStopState, ClientState},
    tempo::Tempo,
    timeline::{clamp_tempo, Timeline},
};

pub type Result<T> = result::Result<T, error::Error>;
pub use error::CommitError;
#[cfg(feature = "std")]
pub use safe_rt_session_state::{AudioCommitTicket, AudioQueueError};

/// Microseconds in `d`, saturating at the `i64` limits instead of failing.
/// Durations derived from a peer's times (ghost times, timeline origins,
/// start/stop timestamps) can fall outside what `num_microseconds` can
/// return, and unwrapping that would let a peer panic this host.
pub(crate) fn micros(d: chrono::Duration) -> i64 {
    d.num_microseconds()
        .unwrap_or(if d < chrono::Duration::zero() {
            i64::MIN
        } else {
            i64::MAX
        })
}

// Keep arithmetic wide until the enclosing semantic value is complete.
pub(crate) fn duration_nanos(d: chrono::Duration) -> i128 {
    i128::from(d.num_seconds()) * 1_000_000_000 + i128::from(d.subsec_nanos())
}

pub(crate) fn duration_from_nanos(nanos: i128) -> chrono::Duration {
    let nanos = nanos.clamp(
        duration_nanos(chrono::Duration::MIN),
        duration_nanos(chrono::Duration::MAX),
    );
    chrono::Duration::seconds((nanos / 1_000_000_000) as i64)
        + chrono::Duration::nanoseconds((nanos % 1_000_000_000) as i64)
}

pub(crate) fn round_ratio(numerator: i128, denominator: i128) -> i128 {
    if denominator == 0 {
        return match numerator.cmp(&0) {
            core::cmp::Ordering::Less => i128::MIN,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => i128::MAX,
        };
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder.unsigned_abs() >= denominator.unsigned_abs().div_ceil(2) {
        quotient
            + if (numerator < 0) == (denominator < 0) {
                1
            } else {
                -1
            }
    } else {
        quotient
    }
}

#[cfg(feature = "std")]
pub type PeerCountCallback = Arc<Mutex<Box<dyn Fn(usize) + Send>>>;
#[cfg(feature = "std")]
pub type TempoCallback = Arc<Mutex<Box<dyn Fn(f64) + Send>>>;
#[cfg(feature = "std")]
pub type StartStopCallback = Arc<Mutex<Box<dyn Fn(bool) + Send>>>;
/// Invoked when a peer's advertised audio endpoint (LinkAudio) is first seen
/// or subsequently changes.
///
/// The contract is deliberately narrower than upstream's. Upstream invokes
/// its `AudioEndpointCallback` on *every* `Peers::sawPeerOnGateway`, including
/// sightings that carry the same endpoint as before. This port fires only on a
/// transition:
///
/// * the first time a peer is seen, always - including with `None` when that
///   peer advertises no endpoint at all, so a consumer learns the peer's
///   endpoint state exactly once per peer rather than never;
/// * on a later sighting, only when the endpoint differs from the last one
///   recorded for that peer, `Some(_)` to `None` (withdrawn) included.
///
/// It is therefore an edge-triggered notification, not a per-message one, and
/// a consumer must not use its arrival rate to infer peer liveness.
///
/// Peer departure does not invoke it, mirroring upstream, which does not call
/// it from `peerLeftGateway`. A sighting whose peer-list update could not
/// acquire the lock is dropped rather than queued, so a missed edge is
/// possible; the next differing sighting re-reports it.
///
/// Unlike upstream, this callback is not passed a gateway address: this port
/// has no multi-gateway concept exposed to `Peers`/`Controller` (there is only
/// ever a single implicit gateway), so that parameter has no analogue here.
#[cfg(feature = "std")]
pub type AudioEndpointCallback =
    Arc<Mutex<Box<dyn Fn(node::NodeId, Option<std::net::SocketAddrV4>) + Send>>>;

#[cfg(feature = "std")]
pub struct BasicLink {
    peer_count_callback: Option<PeerCountCallback>,
    tempo_callback: Option<TempoCallback>,
    start_stop_callback: Option<StartStopCallback>,
    controller: Controller,
    clock: Clock,
    atomic_session_state: AtomicSessionState,
    last_is_playing_for_callback: AtomicBool,
    non_rt_commit: tokio::sync::Mutex<()>,
    pending_playback_notification: Mutex<Option<bool>>,
    notification_dispatch: tokio::sync::Mutex<()>,
}

#[cfg(feature = "std")]
impl BasicLink {
    pub async fn new(bpm: f64) -> std::result::Result<Self, std::io::Error> {
        // Only set up tracing if not already set up
        let _ = tracing_subscriber::fmt::try_init();

        let clock = Clock::default();
        let controller = Controller::new(tempo::Tempo::new(bpm), clock).await?;

        use crate::platform::io_context::{retry_state, try_state};
        let initial_client_state =
            retry_state(|| Some(*try_state(&controller.client_state)?)).await;

        let atomic_session_state = AtomicSessionState::new(initial_client_state);

        Ok(Self {
            peer_count_callback: None,
            tempo_callback: None,
            start_stop_callback: None,
            controller,
            clock,
            atomic_session_state,
            last_is_playing_for_callback: AtomicBool::new(false),
            non_rt_commit: tokio::sync::Mutex::new(()),
            pending_playback_notification: Mutex::new(None),
            notification_dispatch: tokio::sync::Mutex::new(()),
        })
    }
}

#[cfg(feature = "std")]
impl BasicLink {
    /// Requests or restores real-time scheduling on this instance's owned Link
    /// IO thread. This is opt-in and never changes a caller-runtime worker.
    /// Permission/platform errors are returned; shutdown restores priority.
    pub async fn set_io_thread_priority(&self, high: bool) -> std::io::Result<()> {
        self.controller.set_io_thread_priority(high).await
    }

    /// Enables discovery and dispatch, including after a previous [`Self::disable`].
    /// Waits for consumers to prepare the new lifecycle before returning.
    /// Calling this while already enabled leaves the current session unchanged.
    pub async fn enable(&mut self) {
        self.controller.enable().await;

        // Update the atomic session state to reflect the new enable state
        self.atomic_session_state.set_enabled(true);
    }

    /// Stops dispatch and suppresses discovery without destroying the consumers
    /// needed by a later [`Self::enable`].
    pub async fn disable(&mut self) {
        // Match Controller's transition before either future can be cancelled.
        self.atomic_session_state.set_enabled(false);
        self.controller.disable().await;
    }

    pub fn is_enabled(&self) -> bool {
        self.controller.is_enabled()
    }

    pub fn is_start_stop_sync_enabled(&self) -> bool {
        self.controller.is_start_stop_sync_enabled()
    }

    pub fn enable_start_stop_sync(&mut self, enable: bool) {
        self.controller.enable_start_stop_sync(enable);
    }

    pub fn num_peers(&self) -> usize {
        self.controller.num_peers()
    }

    pub fn set_num_peers_callback<F>(&mut self, callback: F)
    where
        F: Fn(usize) + Send + 'static,
    {
        self.peer_count_callback = Some(Arc::new(Mutex::new(Box::new(callback))));

        // Trigger initial callback with current peer count
        let current_count = self.num_peers();
        if let Some(ref callback) = self.peer_count_callback {
            if let Ok(callback) = callback.try_lock() {
                callback(current_count);
            }
        }
    }

    pub fn set_tempo_callback<F>(&mut self, callback: F)
    where
        F: Fn(f64) + Send + 'static,
    {
        let cb: TempoCallback = Arc::new(Mutex::new(Box::new(callback)));
        self.tempo_callback = Some(cb.clone());

        // Also wire the callback into the controller for peer-initiated tempo changes
        if let Ok(mut guard) = self.controller.tempo_callback.try_lock() {
            *guard = Some(cb);
        }

        let bpm = self
            .controller
            .client_state
            .try_lock()
            .ok()
            .map(|state| state.timeline.tempo.bpm());
        if let Some(bpm) = bpm {
            if let Some(ref callback) = self.tempo_callback {
                if let Ok(callback) = callback.try_lock() {
                    callback(bpm);
                }
            }
        }
    }

    pub fn set_start_stop_callback<F>(&mut self, callback: F)
    where
        F: Fn(bool) + Send + 'static,
    {
        self.start_stop_callback = Some(Arc::new(Mutex::new(Box::new(callback))));

        let playing = self
            .controller
            .client_state
            .try_lock()
            .ok()
            .map(|state| state.start_stop_state.is_playing);
        if let Some(playing) = playing {
            if let Some(ref callback) = self.start_stop_callback {
                if let Ok(callback) = callback.try_lock() {
                    callback(playing);
                    self.last_is_playing_for_callback
                        .store(playing, Ordering::Release);
                }
            }
        }
    }

    /// Registers a callback invoked when a peer's discovered audio endpoint is
    /// first seen or subsequently changes. Rust analogue of upstream's
    /// `SessionController::sawAudioEndpointCallback`, which upstream currently
    /// leaves as a no-op handler; `LinkAudio` still discovers peers by polling.
    ///
    /// This is edge-triggered rather than per-sighting, and is not passed a
    /// gateway address. See [`AudioEndpointCallback`] for the exact contract
    /// and for how it diverges from upstream.
    pub fn set_audio_endpoint_callback<F>(&mut self, callback: F)
    where
        F: Fn(node::NodeId, Option<std::net::SocketAddrV4>) + Send + 'static,
    {
        let cb: AudioEndpointCallback = Arc::new(Mutex::new(Box::new(callback)));
        match self.controller.audio_endpoint_callback.lock() {
            Ok(mut guard) => *guard = Some(cb),
            Err(mut poisoned) => **poisoned.get_mut() = Some(cb),
        }
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// The underlying controller. Exposed so the LinkAudio subsystem can read
    /// peer state and announce its audio endpoint.
    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    pub fn capture_audio_session_state(&self) -> SessionState {
        // Real-time safe capture using atomic session state
        let client_state = self.atomic_session_state.capture_audio_session_state();
        let mut captured = to_session_state(&client_state, self.num_peers() > 0);
        if let Some((accepted, origin)) = self.controller.capture_client_state() {
            if accepted == client_state {
                captured.origin = Some(origin);
            }
        }
        captured
    }

    /// Queue one request without changing captured state. Drain it on a non-RT
    /// thread with [`Self::process_audio_session_state_updates`], then acknowledge
    /// the returned ticket with [`Self::take_audio_commit_result`].
    pub fn commit_audio_session_state(
        &self,
        state: SessionState,
    ) -> result::Result<AudioCommitTicket, AudioQueueError> {
        // Real-time safe commit using atomic session state
        let current_time = self.clock.micros();
        let incoming_state =
            to_incoming_client_state(&state.state, &state.original_state, current_time);
        self.atomic_session_state
            .commit_audio_session_state_from_origin(incoming_state, state.origin)
    }

    /// Process at most one queued audio request and refresh the accepted RT view.
    /// Cancellation leaves an unprocessed request queued. Call periodically from
    /// the application thread, including when there are no audio requests.
    /// Concurrent or reentrant consumers return None without consuming a request.
    pub async fn process_audio_session_state_updates(
        &self,
    ) -> Option<(AudioCommitTicket, result::Result<(), CommitError>)> {
        let Ok(consumer) = self.non_rt_commit.try_lock() else {
            return None;
        };
        let pending = self.atomic_session_state.pending_update_from_origin();
        let completed = if let Some((ticket, incoming, origin)) = pending {
            let result = self
                .commit_incoming_client_state(incoming, origin.as_ref())
                .await;
            self.atomic_session_state
                .complete_pending_update(ticket, result);
            Some((ticket, result))
        } else {
            None
        };
        let accepted = self.refresh_accepted_client_state().await;
        self.queue_playback_notification(accepted);
        drop(consumer);
        self.dispatch_playback_notification();
        completed
    }

    pub fn take_audio_commit_result(
        &self,
        ticket: AudioCommitTicket,
    ) -> result::Result<Option<result::Result<(), CommitError>>, AudioQueueError> {
        self.atomic_session_state.take_commit_result(ticket)
    }

    pub fn capture_app_session_state(&self) -> SessionState {
        if let Some((accepted, origin)) = self.controller.capture_client_state() {
            let mut captured = to_session_state(&accepted, self.num_peers() > 0);
            captured.origin = Some(origin);
            captured
        } else {
            use crate::platform::io_context::try_state;
            let accepted = try_state(&self.controller.client_state)
                .map(|state| *state)
                .unwrap_or_else(|| self.atomic_session_state.capture_app_session_state());
            to_session_state(&accepted, self.num_peers() > 0)
        }
    }

    pub async fn commit_app_session_state(
        &mut self,
        state: SessionState,
    ) -> result::Result<(), CommitError> {
        let incoming_state =
            to_incoming_client_state(&state.state, &state.original_state, self.clock.micros());
        let consumer = self.non_rt_commit.lock().await;
        self.commit_incoming_client_state(incoming_state, state.origin.as_ref())
            .await?;
        let accepted = self.refresh_accepted_client_state().await;
        self.queue_playback_notification(accepted);
        drop(consumer);
        self.dispatch_playback_notification();
        Ok(())
    }

    async fn commit_incoming_client_state(
        &self,
        incoming_state: IncomingClientState,
        origin: Option<&sessions::SessionEditOrigin>,
    ) -> result::Result<(), CommitError> {
        if let Some(origin) = origin {
            self.controller
                .set_state_checked(incoming_state, origin)
                .await?;
        } else {
            return Err(CommitError::StaleClientState);
        }
        Ok(())
    }

    async fn refresh_accepted_client_state(&self) -> ClientState {
        use crate::platform::io_context::{retry_state, try_state};
        let accepted = retry_state(|| Some(*try_state(&self.controller.client_state)?)).await;
        self.atomic_session_state.publish_client_state(accepted);
        accepted
    }

    fn queue_playback_notification(&self, accepted: ClientState) {
        *self
            .pending_playback_notification
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(accepted.start_stop_state.is_playing);
    }

    fn dispatch_playback_notification(&self) {
        use crate::platform::io_context::try_state;
        let Ok(_dispatch) = self.notification_dispatch.try_lock() else {
            return;
        };
        let playing = self
            .pending_playback_notification
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(playing) = playing else { return };
        if self.last_is_playing_for_callback.load(Ordering::Acquire) == playing {
            return;
        }
        if let Some(callback) = &self.start_stop_callback {
            let Some(callback) = try_state(callback) else {
                self.pending_playback_notification
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(playing);
                return;
            };
            callback(playing);
        }
        self.last_is_playing_for_callback
            .store(playing, Ordering::Release);
    }
}

#[cfg(feature = "std")]
pub fn to_session_state(state: &ClientState, _is_connected: bool) -> SessionState {
    SessionState::new(
        ApiState {
            timeline: state.timeline,
            timeline_session_id: state.timeline_session_id,
            start_stop_state: ApiStartStopState {
                is_playing: state.start_stop_state.is_playing,
                time: state.start_stop_state.time,
            },
        },
        true, // Always respect quantum like C++ implementation
    )
}

#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IncomingClientState {
    pub timeline: Option<Timeline>,
    pub start_stop_state: Option<ClientStartStopState>,
    pub timeline_timestamp: Duration,
}

#[cfg(feature = "std")]
pub fn to_incoming_client_state(
    state: &ApiState,
    original_state: &ApiState,
    timestamp: Duration,
) -> IncomingClientState {
    let timeline = if original_state.timeline != state.timeline {
        Some(state.timeline)
    } else {
        None
    };

    let start_stop_state = if original_state.start_stop_state != state.start_stop_state {
        Some(ClientStartStopState {
            is_playing: state.start_stop_state.is_playing,
            time: state.start_stop_state.time,
            timestamp,
        })
    } else {
        None
    };

    IncomingClientState {
        timeline,
        start_stop_state,
        timeline_timestamp: timestamp,
    }
}

#[cfg(feature = "std")]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ApiState {
    timeline: Timeline,
    // Mirrors upstream's `ApiState::timelineSessionId` (SessionState.hpp). Not yet
    // consumed by a public getter in this port; kept for parity with upstream's
    // struct layout and to preserve provenance of the pinned session/timeline.
    #[allow(dead_code)]
    timeline_session_id: SessionId,
    start_stop_state: ApiStartStopState,
}

#[cfg(feature = "std")]
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
struct ApiStartStopState {
    is_playing: bool,
    time: Duration,
}

#[cfg(feature = "std")]
impl Default for ApiStartStopState {
    fn default() -> Self {
        Self {
            is_playing: false,
            time: Duration::zero(),
        }
    }
}

#[cfg(feature = "std")]
impl ApiStartStopState {
    fn new(is_playing: bool, time: Duration) -> Self {
        Self { is_playing, time }
    }
}

#[cfg(feature = "std")]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionState {
    original_state: ApiState,
    state: ApiState,
    respect_quantum: bool,
    origin: Option<sessions::SessionEditOrigin>,
}

#[cfg(feature = "std")]
impl SessionState {
    pub fn new(state: ApiState, respect_quantum: bool) -> Self {
        Self {
            original_state: state,
            state,
            respect_quantum,
            origin: None,
        }
    }

    pub fn tempo(&self) -> f64 {
        self.state.timeline.tempo.bpm()
    }

    /// The timeline this session state was captured with. Used by the
    /// LinkAudio subsystem to map buffers onto the session beat grid.
    pub fn timeline(&self) -> Timeline {
        self.state.timeline
    }

    pub fn set_tempo(&mut self, bpm: f64, at_time: Duration) {
        let desired_tl = clamp_tempo(Timeline {
            tempo: Tempo::new(bpm),
            beat_origin: self.state.timeline.to_beats(at_time),
            time_origin: at_time,
        });
        self.state.timeline.tempo = desired_tl.tempo;
        self.state.timeline.time_origin = desired_tl.from_beats(self.state.timeline.beat_origin);
    }

    pub fn beat_at_time(&self, time: Duration, quantum: f64) -> f64 {
        to_phase_encoded_beats(&self.state.timeline, time, Beats::new(quantum)).floating()
    }

    pub fn phase_at_time(&self, time: Duration, quantum: f64) -> f64 {
        phase(
            Beats::new(self.beat_at_time(time, quantum)),
            Beats::new(quantum),
        )
        .floating()
    }

    pub fn time_at_beat(&self, beat: f64, quantum: f64) -> Duration {
        from_phase_encoded_beats(&self.state.timeline, Beats::new(beat), Beats::new(quantum))
    }

    pub fn request_beat_at_time(&mut self, beat: f64, time: Duration, quantum: f64) {
        let time = if self.respect_quantum {
            self.time_at_beat(
                phase::next_phase_match(
                    Beats::new(self.beat_at_time(time, quantum)),
                    Beats::new(beat),
                    Beats::new(quantum),
                )
                .floating(),
                quantum,
            )
        } else {
            time
        };
        self.force_beat_at_time(beat, time, quantum);
    }

    pub fn force_beat_at_time(&mut self, beat: f64, time: Duration, quantum: f64) {
        force_beat_at_time_impl(
            &mut self.state.timeline,
            Beats::new(beat),
            time,
            Beats::new(quantum),
        );

        // Due to quantization errors the resulting BeatTime at 'time' after forcing can be
        // bigger than 'beat' which then violates intended behavior of the API and can lead
        // i.e. to missing a downbeat. Thus we have to shift the timeline forwards.
        if self.beat_at_time(time, quantum) > beat {
            force_beat_at_time_impl(
                &mut self.state.timeline,
                Beats::new(beat),
                duration_from_nanos(duration_nanos(time) + 1_000),
                Beats::new(quantum),
            );
        }
    }

    pub fn set_is_playing(&mut self, is_playing: bool, time: Duration) {
        self.state.start_stop_state = ApiStartStopState::new(is_playing, time);
    }

    pub fn is_playing(&self) -> bool {
        self.state.start_stop_state.is_playing
    }

    pub fn time_for_is_playing(&self) -> Duration {
        self.state.start_stop_state.time
    }

    pub fn request_beat_at_start_playing_time(&mut self, beat: f64, quantum: f64) {
        if self.is_playing() {
            self.request_beat_at_time(beat, self.state.start_stop_state.time, quantum);
        }
    }

    pub fn set_is_playing_and_request_beat_at_time(
        &mut self,
        is_playing: bool,
        time: Duration,
        beat: f64,
        quantum: f64,
    ) {
        self.state.start_stop_state = ApiStartStopState::new(is_playing, time);
        self.request_beat_at_start_playing_time(beat, quantum);
    }
}

#[cfg(test)]
mod tests {
    use tracing::info;

    use super::*;

    #[tokio::test]
    async fn ordering_callback_registration_releases_client_guard() {
        use std::sync::atomic::AtomicUsize;
        let mut link = BasicLink::new(120.0).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let client = link.controller.client_state.clone();
        let count = calls.clone();
        link.set_tempo_callback(move |_| {
            assert!(client.try_lock().is_ok());
            count.fetch_add(1, Ordering::SeqCst);
        });
        let client = link.controller.client_state.clone();
        let count = calls.clone();
        link.set_start_stop_callback(move |_| {
            assert!(client.try_lock().is_ok());
            count.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ordering_stale_playback_capture_rejects_app_and_audio_submission() {
        for audio in [false, true] {
            let mut link = BasicLink::new(120.0).await.unwrap();
            let mut old = link.capture_app_session_state();
            for playing in [true, false] {
                let mut newer = link.capture_app_session_state();
                newer.set_is_playing(playing, link.clock.micros());
                link.commit_app_session_state(newer).await.unwrap();
            }
            old.set_is_playing(true, link.clock.micros());
            old.set_tempo(150.0, link.clock.micros());
            let before = *link.controller.client_state.lock().unwrap();
            if audio {
                let ticket = link.commit_audio_session_state(old).unwrap();
                assert_eq!(
                    link.process_audio_session_state_updates().await,
                    Some((ticket, Err(CommitError::StaleClientState)))
                );
                assert_eq!(
                    link.take_audio_commit_result(ticket),
                    Ok(Some(Err(CommitError::StaleClientState)))
                );
            } else {
                assert_eq!(
                    link.commit_app_session_state(old).await,
                    Err(CommitError::StaleClientState)
                );
            }
            assert_eq!(*link.controller.client_state.lock().unwrap(), before);
        }
    }

    #[tokio::test]
    async fn ordering_idle_drain_notifies_accepted_controller_playback() {
        let mut link = BasicLink::new(120.0).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        link.set_start_stop_callback(move |playing| observed.lock().unwrap().push(playing));
        let mut state = link.capture_app_session_state();
        state.set_is_playing(true, link.clock.micros());
        let incoming =
            to_incoming_client_state(&state.state, &state.original_state, link.clock.micros());
        link.controller.set_state(incoming).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![false]);
        assert_eq!(link.process_audio_session_state_updates().await, None);
        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
    }

    #[tokio::test]
    async fn ordering_callback_dispatch_selects_latest_state_after_claim() {
        let mut link = BasicLink::new(120.0).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        link.set_start_stop_callback(move |playing| observed.lock().unwrap().push(playing));
        let link = Arc::new(link);
        let dispatch = link.notification_dispatch.try_lock().unwrap();
        for playing in [true, false] {
            let mut state = link.capture_audio_session_state();
            state.set_is_playing(playing, link.clock.micros());
            let ticket = link.commit_audio_session_state(state).unwrap();
            assert_eq!(
                link.process_audio_session_state_updates().await,
                Some((ticket, Ok(())))
            );
            assert_eq!(link.take_audio_commit_result(ticket), Ok(Some(Ok(()))));
        }
        assert_eq!(*calls.lock().unwrap(), vec![false]);
        assert_eq!(
            *link.pending_playback_notification.lock().unwrap(),
            Some(false)
        );
        drop(dispatch);
        assert_eq!(link.process_audio_session_state_updates().await, None);
        assert_eq!(*calls.lock().unwrap(), vec![false]);
        assert!(link.pending_playback_notification.lock().unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordering_callback_dispatch_retains_stop_while_start_callback_is_busy() {
        let mut link = BasicLink::new(120.0).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        link.set_start_stop_callback(move |playing| {
            observed.lock().unwrap().push(playing);
            if playing {
                entered.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
            }
        });
        let link = Arc::new(link);
        let mut start = link.capture_audio_session_state();
        start.set_is_playing(true, link.clock.micros());
        let first = link.commit_audio_session_state(start).unwrap();
        let worker = link.clone();
        let running =
            tokio::spawn(async move { worker.process_audio_session_state_updates().await });
        tokio::task::yield_now().await;
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(link.take_audio_commit_result(first), Ok(Some(Ok(()))));
        let mut stop = link.capture_audio_session_state();
        stop.set_is_playing(false, link.clock.micros());
        let second = link.commit_audio_session_state(stop).unwrap();
        assert_eq!(
            link.process_audio_session_state_updates().await,
            Some((second, Ok(())))
        );
        assert_eq!(link.take_audio_commit_result(second), Ok(Some(Ok(()))));
        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
        assert_eq!(
            *link.pending_playback_notification.lock().unwrap(),
            Some(false)
        );
        release.send(()).unwrap();
        assert_eq!(running.await.unwrap(), Some((first, Ok(()))));
        assert_eq!(link.process_audio_session_state_updates().await, None);
        assert_eq!(*calls.lock().unwrap(), vec![false, true, false]);
        assert!(!link.last_is_playing_for_callback.load(Ordering::Acquire));
        assert!(!link.capture_app_session_state().is_playing());
    }

    #[tokio::test]
    async fn ordering_shared_drain_serializes_consumers_and_notifies_tempo_once() {
        use std::sync::atomic::AtomicUsize;
        let mut link = BasicLink::new(120.0).await.unwrap();
        let callbacks = Arc::new(AtomicUsize::new(0));
        let count = callbacks.clone();
        link.set_tempo_callback(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        let link = Arc::new(link);
        let mut proposed = link.capture_audio_session_state();
        proposed.set_tempo(150.0, link.clock.micros());
        let ticket = link.commit_audio_session_state(proposed).unwrap();
        let consumer = link.non_rt_commit.lock().await;
        let audio = link.clone();
        std::thread::spawn(move || {
            let capture = audio.capture_audio_session_state();
            assert_eq!(capture.tempo(), 120.0);
            assert_eq!(
                audio.commit_audio_session_state(capture),
                Err(AudioQueueError::Busy)
            );
            assert_eq!(audio.take_audio_commit_result(ticket), Ok(None));
        })
        .join()
        .unwrap();
        assert_eq!(link.process_audio_session_state_updates().await, None);
        drop(consumer);
        let worker = link.clone();
        assert_eq!(
            tokio::spawn(async move { worker.process_audio_session_state_updates().await })
                .await
                .unwrap(),
            Some((ticket, Ok(())))
        );
        assert_eq!(link.process_audio_session_state_updates().await, None);
        assert_eq!(link.take_audio_commit_result(ticket), Ok(Some(Ok(()))));
        assert_eq!(link.capture_audio_session_state().tempo(), 150.0);
        assert_eq!(callbacks.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordering_idle_refresh_yields_to_the_holder_release_timer() {
        let link = BasicLink::new(120.0).await.unwrap();
        let client = link.controller.client_state.clone();
        let timed_out = Arc::new(AtomicBool::new(false));
        let expired = timed_out.clone();
        let (ready, ready_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = client.lock().unwrap();
            ready.send(()).unwrap();
            expired.store(
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .is_err(),
                Ordering::SeqCst,
            );
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let _ = release.send(());
        });
        assert_eq!(link.process_audio_session_state_updates().await, None);
        holder.join().unwrap();
        timer.await.unwrap();
        assert!(!timed_out.load(Ordering::SeqCst));
        assert_eq!(link.capture_audio_session_state().tempo(), 120.0);
    }

    #[tokio::test]
    async fn ordering_cancellation_after_acceptance_keeps_success_and_refresh_work() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let mut link = BasicLink::new(120.0).await.unwrap();
        let client = link.controller.client_state.clone();
        let (trigger, trigger_rx) = std::sync::mpsc::channel();
        let (ready, ready_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            trigger_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            let _guard = client.lock().unwrap();
            ready.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
        });
        link.set_tempo_callback(move |bpm| {
            if bpm == 150.0 {
                trigger.send(()).unwrap();
                ready_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            }
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        link.set_start_stop_callback(move |playing| observed.lock().unwrap().push(playing));
        let mut proposed = link.capture_audio_session_state();
        proposed.set_tempo(150.0, link.clock.micros());
        proposed.set_is_playing(true, link.clock.micros());
        let ticket = link.commit_audio_session_state(proposed).unwrap();
        let mut drain = Box::pin(link.process_audio_session_state_updates());
        assert!(drain
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert_eq!(link.take_audio_commit_result(ticket), Ok(Some(Ok(()))));
        assert_eq!(link.process_audio_session_state_updates().await, None);
        assert_eq!(link.capture_audio_session_state().tempo(), 120.0);
        drop(drain);
        assert_eq!(*calls.lock().unwrap(), vec![false]);
        release.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(link.process_audio_session_state_updates().await, None);
        assert_eq!(link.capture_audio_session_state().tempo(), 150.0);
        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
        assert_eq!(
            link.take_audio_commit_result(ticket),
            Err(AudioQueueError::UnknownTicket)
        );
    }

    #[tokio::test]
    async fn ordering_overtaken_audio_timeline_cannot_overwrite_app_commit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for combined in [false, true] {
            let mut link = BasicLink::new(120.0).await.unwrap();
            link.enable_start_stop_sync(true);
            let calls = Arc::new(AtomicUsize::new(0));
            let tempo_calls = calls.clone();
            link.set_tempo_callback(move |_| {
                tempo_calls.fetch_add(1, Ordering::SeqCst);
            });
            let playing_calls = calls.clone();
            link.set_start_stop_callback(move |_| {
                playing_calls.fetch_add(1, Ordering::SeqCst);
            });
            let mut old = link.capture_audio_session_state();
            old.set_tempo(150.0, link.clock.micros());
            if combined {
                old.set_is_playing(true, link.clock.micros());
            }
            let ticket = link.commit_audio_session_state(old).unwrap();
            let mut newer = link.capture_app_session_state();
            newer.set_tempo(180.0, link.clock.micros());
            link.commit_app_session_state(newer).await.unwrap();
            let before = *link.controller.client_state.lock().unwrap();
            let before_session = *link.controller.session_state.lock().unwrap();
            let before_calls = calls.load(Ordering::SeqCst);
            assert_eq!(
                link.process_audio_session_state_updates().await,
                Some((ticket, Err(CommitError::StaleClientState)))
            );
            assert_eq!(
                link.take_audio_commit_result(ticket),
                Ok(Some(Err(CommitError::StaleClientState)))
            );
            assert_eq!(link.capture_app_session_state().tempo(), 180.0);
            assert_eq!(*link.controller.client_state.lock().unwrap(), before);
            assert_eq!(
                *link.controller.session_state.lock().unwrap(),
                before_session
            );
            assert_eq!(calls.load(Ordering::SeqCst), before_calls);
        }
    }

    #[tokio::test]
    async fn ordering_cancelled_audio_drain_retains_request_until_acceptance() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let link = BasicLink::new(120.0).await.unwrap();
        let mut proposed = link.capture_audio_session_state();
        proposed.set_tempo(150.0, link.clock.micros());
        let ticket = link.commit_audio_session_state(proposed).unwrap();
        let session = link.controller.session_state.clone();
        let (ready, ready_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = session.lock().unwrap();
            ready.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let mut drain = Box::pin(link.process_audio_session_state_updates());
        assert!(drain
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        drop(drain);
        assert_eq!(link.take_audio_commit_result(ticket), Ok(None));
        assert_eq!(link.capture_audio_session_state().tempo(), 120.0);
        release.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(
            link.process_audio_session_state_updates().await,
            Some((ticket, Ok(())))
        );
        assert_eq!(link.take_audio_commit_result(ticket), Ok(Some(Ok(()))));
        assert_eq!(link.capture_audio_session_state().tempo(), 150.0);
    }

    #[tokio::test]
    async fn ordering_app_and_audio_rejection_preserve_capture_and_callbacks() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut link = BasicLink::new(120.0).await.unwrap();
        link.enable_start_stop_sync(true);
        let calls = Arc::new(AtomicUsize::new(0));
        let tempo_calls = calls.clone();
        link.set_tempo_callback(move |_| {
            tempo_calls.fetch_add(1, Ordering::SeqCst);
        });
        let playing_calls = calls.clone();
        link.set_start_stop_callback(move |_| {
            playing_calls.fetch_add(1, Ordering::SeqCst);
        });
        link.controller
            .session_state
            .lock()
            .unwrap()
            .start_stop_state
            .timestamp = Duration::microseconds(i64::MAX);
        let before = *link.controller.client_state.lock().unwrap();
        let before_session = *link.controller.session_state.lock().unwrap();
        let before_calls = calls.load(Ordering::SeqCst);
        let mut proposed = link.capture_app_session_state();
        let now = link.clock.micros();
        proposed.set_tempo(150.0, now);
        proposed.set_is_playing(true, now);
        assert_eq!(
            link.commit_app_session_state(proposed.clone()).await,
            Err(CommitError::StartStopTimestampExhausted)
        );
        assert!(!link.last_is_playing_for_callback.load(Ordering::Acquire));
        let ticket = link.commit_audio_session_state(proposed).unwrap();
        for _ in 0..3 {
            assert_eq!(link.capture_audio_session_state().tempo(), 120.0);
            assert!(!link.capture_audio_session_state().is_playing());
            assert_eq!(link.take_audio_commit_result(ticket), Ok(None));
        }
        assert_eq!(
            link.process_audio_session_state_updates().await,
            Some((ticket, Err(CommitError::StartStopTimestampExhausted)))
        );
        assert_eq!(
            link.take_audio_commit_result(ticket),
            Ok(Some(Err(CommitError::StartStopTimestampExhausted)))
        );
        assert_eq!(*link.controller.client_state.lock().unwrap(), before);
        assert_eq!(
            *link.controller.session_state.lock().unwrap(),
            before_session
        );
        assert_eq!(calls.load(Ordering::SeqCst), before_calls);
        assert!(!link.last_is_playing_for_callback.load(Ordering::Acquire));
    }

    #[test]
    fn micros_saturates_both_boundaries_and_preserves_in_range_values() {
        for (value, delta, expected) in [
            (i64::MIN, -1, i64::MIN),
            (i64::MIN, 0, i64::MIN),
            (i64::MIN, 1, i64::MIN + 1),
            (i64::MAX, -1, i64::MAX - 1),
            (i64::MAX, 0, i64::MAX),
            (i64::MAX, 1, i64::MAX),
        ] {
            let duration =
                chrono::Duration::microseconds(value) + chrono::Duration::microseconds(delta);
            assert_eq!(micros(duration), expected);
        }
        assert_eq!(micros(chrono::Duration::MIN), i64::MIN);
        assert_eq!(micros(chrono::Duration::MAX), i64::MAX);
    }

    #[test]
    fn nanosecond_alignment_round_trips_public_inverse() {
        for (bpm, microbeats) in [(120.0, 1), (150.0, 2)] {
            for time in [Duration::zero(), Duration::MAX, Duration::MIN] {
                let mut state = SessionState::default();
                state.set_tempo(bpm, Duration::zero());
                let beat = Beats::from_microbeats(microbeats).floating();
                state.force_beat_at_time(beat, time, 4.0);
                let recovered = state.time_at_beat(beat, 4.0);
                assert_eq!(recovered, time);
                assert_eq!(state.beat_at_time(recovered, 4.0), beat);
            }
        }
    }

    #[test]
    fn public_force_microbeats_preserves_nanosecond_anchors_at_max() {
        for (bpm, microbeats, offset) in [(150.0, 2, 800), (120.0, 1, 500)] {
            for origin in [Duration::zero(), Duration::MAX] {
                let mut state = SessionState::new(
                    ApiState {
                        timeline: Timeline {
                            tempo: Tempo::new(bpm),
                            time_origin: origin,
                            ..Timeline::default()
                        },
                        ..ApiState::default()
                    },
                    false,
                );
                let beat = Beats::from_microbeats(microbeats).floating();
                state.force_beat_at_time(beat, Duration::MAX, 4.0);
                assert_eq!(state.beat_at_time(Duration::MAX, 4.0), beat);
                assert_eq!(
                    state.timeline().time_origin,
                    Duration::MAX - Duration::nanoseconds(offset)
                );
            }
        }
    }

    #[test]
    fn review_round_public_force_beat_handles_duration_limits() {
        for time in [Duration::MAX, Duration::MIN] {
            let mut state = SessionState::new(
                ApiState {
                    timeline: Timeline {
                        tempo: Tempo::new(120.0),
                        ..Timeline::default()
                    },
                    ..ApiState::default()
                },
                false,
            );
            state.force_beat_at_time(0.0, time, 0.0);
            assert_eq!(state.beat_at_time(time, 0.0), 0.0);
        }
    }

    #[tokio::test]
    async fn cancelled_disable_keeps_atomic_and_controller_enabled_flags_consistent() {
        use std::future::Future;
        let mut link = BasicLink::new(120.0).await.unwrap();
        link.enable().await;
        let mut disable = Box::pin(link.disable());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(disable.as_mut().poll(&mut context).is_pending());
        drop(disable);
        assert!(!link.is_enabled());
        assert!(!link.atomic_session_state.is_enabled());
        link.enable().await;
        assert!(link.is_enabled());
        assert!(link.atomic_session_state.is_enabled());
        link.disable().await;
    }

    #[tokio::test]
    async fn test_basic_link() {
        let _ = tracing_subscriber::fmt::try_init();

        let bpm = 140.0;
        let mut link = BasicLink::new(bpm).await.unwrap();

        info!("initializing basic link at {} bpm", bpm);

        // Test basic initialization (Link starts disabled)
        assert!(!link.is_enabled());
        assert_eq!(link.num_peers(), 0);
        assert!(!link.is_start_stop_sync_enabled());

        // Test start/stop sync without enabling Link (to avoid blocking network operations)
        link.enable_start_stop_sync(true);
        assert!(link.is_start_stop_sync_enabled());

        link.enable_start_stop_sync(false);
        assert!(!link.is_start_stop_sync_enabled());

        // Test clock access
        let _clock = link.clock();

        // Test callback setters (they should not panic)
        link.set_num_peers_callback(|count| {
            println!("Peer count: {}", count);
        });

        link.set_tempo_callback(|bpm| {
            println!("Tempo changed: {}", bpm);
        });

        link.set_start_stop_callback(|is_playing| {
            println!("Playing state: {}", is_playing);
        });

        info!("test_basic_link completed successfully - Link functionality validated without network operations");
    }

    #[tokio::test]
    #[ignore] // Requires real UDP multicast — run locally with --include-ignored
    async fn test_peer_count_reset_on_disable() {
        let _ = tracing_subscriber::fmt::try_init();

        let bpm = 120.0;
        let mut link = BasicLink::new(bpm).await.unwrap();

        info!("testing peer count reset when Link is disabled");

        // Initially disabled, should have 0 peers
        assert!(!link.is_enabled());
        assert_eq!(link.num_peers(), 0);

        // Enable Link briefly
        let enable_result =
            tokio::time::timeout(std::time::Duration::from_millis(100), link.enable()).await;
        let _ = enable_result; // May timeout, that's ok

        // Now disable Link
        link.disable().await;

        // After disabling, peer count should be reset to 0
        assert!(!link.is_enabled());
        assert_eq!(
            link.num_peers(),
            0,
            "Peer count should be reset to 0 when Link is disabled"
        );

        info!("test_peer_count_reset_on_disable completed - peer count correctly resets to 0 when disabled");
    }

    #[tokio::test]
    async fn test_session_state_tempo_getset() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let mut state = link.capture_app_session_state();
        assert_eq!(state.tempo(), 120.0);

        let time = link.clock().micros();
        state.set_tempo(140.0, time);
        assert_eq!(state.tempo(), 140.0);
    }

    #[tokio::test]
    async fn test_session_state_clamped_tempo() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();

        // Set below minimum (20 BPM)
        state.set_tempo(5.0, time);
        assert_eq!(state.tempo(), 20.0);

        // Set above maximum (999 BPM)
        state.set_tempo(2000.0, time);
        assert_eq!(state.tempo(), 999.0);
    }

    #[tokio::test]
    async fn test_session_state_is_playing() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let mut state = link.capture_app_session_state();
        assert!(!state.is_playing());

        let time = link.clock().micros();
        state.set_is_playing(true, time);
        assert!(state.is_playing());
        assert_eq!(state.time_for_is_playing(), time);

        state.set_is_playing(false, time + Duration::microseconds(1_000_000));
        assert!(!state.is_playing());
    }

    #[tokio::test]
    async fn test_session_state_beat_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let state = link.capture_app_session_state();

        let time = link.clock().micros();
        let beat = state.beat_at_time(time, 4.0);
        // beat should be a finite number
        assert!(beat.is_finite());
    }

    #[tokio::test]
    async fn test_session_state_phase_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let state = link.capture_app_session_state();

        let time = link.clock().micros();
        let phase_val = state.phase_at_time(time, 4.0);
        // Phase should be in [0, 4)
        assert!(phase_val >= 0.0);
        assert!(phase_val < 4.0);
    }

    #[tokio::test]
    async fn test_session_state_force_beat_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let mut state = link.capture_app_session_state();

        let time = link.clock().micros();
        state.force_beat_at_time(0.0, time, 4.0);
        let beat = state.beat_at_time(time, 4.0);
        assert!((beat - 0.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_session_state_request_beat_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let mut state = link.capture_app_session_state();

        let time = link.clock().micros();
        state.request_beat_at_time(0.0, time, 4.0);
        // After requesting, beat and phase should be finite
        let beat = state.beat_at_time(time, 4.0);
        let phase_val = state.phase_at_time(time, 4.0);
        assert!(beat.is_finite(), "beat should be finite after request");
        assert!(
            phase_val.is_finite(),
            "phase should be finite after request"
        );
    }

    #[tokio::test]
    async fn test_to_incoming_client_state_no_change() {
        let _ = tracing_subscriber::fmt::try_init();

        let state = ApiState::default();
        let incoming = to_incoming_client_state(&state, &state, Duration::zero());
        assert!(incoming.timeline.is_none());
        assert!(incoming.start_stop_state.is_none());
    }

    #[tokio::test]
    async fn test_to_incoming_client_state_with_tempo_change() {
        let _ = tracing_subscriber::fmt::try_init();

        let original = ApiState::default();
        let mut modified = original;
        modified.timeline.tempo = Tempo::new(140.0);
        let ts = Duration::microseconds(1_000_000);
        let incoming = to_incoming_client_state(&modified, &original, ts);
        assert!(incoming.timeline.is_some());
        assert_eq!(incoming.timeline.unwrap().tempo.bpm(), 140.0);
        assert!(incoming.start_stop_state.is_none());
        assert_eq!(incoming.timeline_timestamp, ts);
    }

    #[tokio::test]
    async fn test_session_state_commit_and_capture_roundtrip() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await.unwrap();

        // Capture, modify, commit
        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_tempo(160.0, time);
        link.commit_app_session_state(state).await.unwrap();

        // Re-capture and verify
        let state2 = link.capture_app_session_state();
        assert_eq!(state2.tempo(), 160.0);
    }

    #[tokio::test]
    async fn test_session_state_start_stop_commit_roundtrip() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await.unwrap();

        let mut state = link.capture_app_session_state();
        assert!(!state.is_playing());

        let time = link.clock().micros();
        state.set_is_playing(true, time);
        link.commit_app_session_state(state).await.unwrap();

        let state2 = link.capture_app_session_state();
        assert!(state2.is_playing());
    }

    #[tokio::test]
    async fn test_tempo_callback_on_commit() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await.unwrap();

        let recorded_bpm = Arc::new(Mutex::new(0.0f64));
        let recorded_bpm_clone = recorded_bpm.clone();
        link.set_tempo_callback(move |bpm| {
            *recorded_bpm_clone.lock().unwrap() = bpm;
        });

        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_tempo(180.0, time);
        link.commit_app_session_state(state).await.unwrap();

        let bpm = *recorded_bpm.lock().unwrap();
        assert_eq!(bpm, 180.0, "Tempo callback should fire with new BPM");
    }

    #[tokio::test]
    async fn test_start_stop_callback_on_commit() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await.unwrap();

        let recorded_playing = Arc::new(Mutex::new(false));
        let recorded_clone = recorded_playing.clone();
        link.set_start_stop_callback(move |is_playing| {
            *recorded_clone.lock().unwrap() = is_playing;
        });

        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_is_playing(true, time);
        link.commit_app_session_state(state).await.unwrap();

        assert!(
            *recorded_playing.lock().unwrap(),
            "Start/stop callback should fire with true"
        );
    }

    type RecordedEndpoint = (node::NodeId, Option<std::net::SocketAddrV4>);

    #[tokio::test]
    async fn test_audio_endpoint_callback_dispatch() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await.unwrap();

        let recorded: Arc<Mutex<Option<RecordedEndpoint>>> = Arc::new(Mutex::new(None));
        let recorded_clone = recorded.clone();
        link.set_audio_endpoint_callback(move |peer_id, endpoint| {
            *recorded_clone.lock().unwrap() = Some((peer_id, endpoint));
        });

        // Registration must be observable immediately, and the controller's
        // dispatch path must deliver the peer id and endpoint unchanged.
        let peer_id = node::NodeId::new();
        let endpoint = std::net::SocketAddrV4::new(std::net::Ipv4Addr::new(127, 0, 0, 1), 12345);
        crate::link::controller::dispatch_audio_endpoint_change(
            &link.controller().audio_endpoint_callback,
            peer_id,
            Some(endpoint),
        );

        assert_eq!(
            *recorded.lock().unwrap(),
            Some((peer_id, Some(endpoint))),
            "Audio endpoint callback should fire with the peer id and endpoint"
        );
    }

    #[tokio::test]
    async fn test_set_is_playing_and_request_beat_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let mut state = link.capture_app_session_state();

        let time = link.clock().micros();
        state.set_is_playing_and_request_beat_at_time(true, time, 0.0, 4.0);
        assert!(state.is_playing());
        let beat = state.beat_at_time(time, 4.0);
        assert!(beat.is_finite());
    }

    #[tokio::test]
    async fn test_request_beat_at_start_playing_time_not_playing() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let mut state = link.capture_app_session_state();
        // Not playing — should be a no-op
        let tempo_before = state.tempo();
        state.request_beat_at_start_playing_time(0.0, 4.0);
        assert_eq!(state.tempo(), tempo_before);
    }

    #[tokio::test]
    async fn test_session_state_equality() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await.unwrap();
        let state_a = link.capture_app_session_state();
        let state_b = link.capture_app_session_state();

        // Two captures of the same underlying session state must compare equal.
        assert_eq!(state_a, state_b);

        let mut state_c = link.capture_app_session_state();
        state_c.set_tempo(130.0, link.clock().micros());

        // A mutated copy must no longer compare equal to the original.
        assert_ne!(state_a, state_c);
    }
}
