#![allow(clippy::too_many_arguments)]

pub mod atomic_session_state;
pub mod beats;
pub mod clock;
pub mod controller;
pub mod error;
pub mod ghostxform;
pub mod host_time_filter;
pub mod linear_regression;
pub mod measurement;
pub mod median;
pub mod node;
pub mod payload;
pub mod phase;
pub mod pingresponder;
pub mod safe_rt_session_state;
pub mod sessions;
pub mod state;
pub mod tempo;
pub mod timeline;

use std::{
    result,
    sync::{Arc, Mutex},
};

use chrono::Duration;

use self::{
    atomic_session_state::AtomicSessionState,
    beats::Beats,
    clock::Clock,
    controller::Controller,
    phase::{force_beat_at_time_impl, from_phase_encoded_beats, phase, to_phase_encoded_beats},
    state::{ClientStartStopState, ClientState},
    tempo::Tempo,
    timeline::{clamp_tempo, Timeline},
};

pub type Result<T> = result::Result<T, error::Error>;

pub type PeerCountCallback = Arc<Mutex<Box<dyn Fn(usize) + Send>>>;
pub type TempoCallback = Arc<Mutex<Box<dyn Fn(f64) + Send>>>;
pub type StartStopCallback = Arc<Mutex<Box<dyn Fn(bool) + Send>>>;

pub struct BasicLink {
    peer_count_callback: Option<PeerCountCallback>,
    tempo_callback: Option<TempoCallback>,
    start_stop_callback: Option<StartStopCallback>,
    controller: Controller,
    clock: Clock,
    atomic_session_state: AtomicSessionState,
    last_is_playing_for_callback: bool,
}

impl BasicLink {
    pub async fn new(bpm: f64) -> Self {
        // Only set up tracing if not already set up
        let _ = tracing_subscriber::fmt::try_init();

        let clock = Clock::default();
        let controller = Controller::new(tempo::Tempo::new(bpm), clock).await;

        // Create initial client state for atomic session state
        let initial_client_state =
            if let Ok(client_state_guard) = controller.client_state.try_lock() {
                *client_state_guard
            } else {
                // Fallback to default state if we can't get the lock
                ClientState {
                    timeline: Timeline {
                        tempo: Tempo::new(bpm),
                        beat_origin: Beats::new(0.0),
                        time_origin: Duration::zero(),
                    },
                    start_stop_state: ClientStartStopState {
                        is_playing: false,
                        time: Duration::zero(),
                        timestamp: Duration::zero(),
                    },
                }
            };

        let atomic_session_state =
            AtomicSessionState::new(initial_client_state, controller::LOCAL_MOD_GRACE_PERIOD);

        Self {
            peer_count_callback: None,
            tempo_callback: None,
            start_stop_callback: None,
            controller,
            clock,
            atomic_session_state,
            last_is_playing_for_callback: false,
        }
    }
}

impl BasicLink {
    pub async fn enable(&mut self) {
        self.controller.enable().await;

        // Update the atomic session state to reflect the new enable state
        self.atomic_session_state.set_enabled(true);
    }

    pub async fn disable(&mut self) {
        self.controller.disable().await;

        // Update the atomic session state to reflect the new enable state
        self.atomic_session_state.set_enabled(false);
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

        // Trigger initial callback with current tempo
        if let Ok(client_state) = self.controller.client_state.try_lock() {
            if let Some(ref callback) = self.tempo_callback {
                if let Ok(callback) = callback.try_lock() {
                    callback(client_state.timeline.tempo.bpm());
                }
            }
        }
    }

    pub fn set_start_stop_callback<F>(&mut self, callback: F)
    where
        F: Fn(bool) + Send + 'static,
    {
        self.start_stop_callback = Some(Arc::new(Mutex::new(Box::new(callback))));

        // Update our cached playing state and trigger initial callback
        if let Ok(client_state) = self.controller.client_state.try_lock() {
            self.last_is_playing_for_callback = client_state.start_stop_state.is_playing;
            if let Some(ref callback) = self.start_stop_callback {
                if let Ok(callback) = callback.try_lock() {
                    callback(self.last_is_playing_for_callback);
                }
            }
        }
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }

    pub fn capture_audio_session_state(&self) -> SessionState {
        // Real-time safe capture using atomic session state
        let current_time = self.clock.micros();
        let client_state = self
            .atomic_session_state
            .capture_audio_session_state(current_time);
        to_session_state(&client_state, self.num_peers() > 0)
    }

    pub fn commit_audio_session_state(&self, state: SessionState) {
        // Real-time safe commit using atomic session state
        let current_time = self.clock.micros();
        let incoming_state =
            to_incoming_client_state(&state.state, &state.original_state, current_time);
        self.atomic_session_state
            .commit_audio_session_state(incoming_state, current_time);
    }

    pub fn capture_app_session_state(&self) -> SessionState {
        if let Ok(client_state_guard) = self.controller.client_state.try_lock() {
            to_session_state(&client_state_guard, self.num_peers() > 0)
        } else {
            // Return a default state if we can't get the lock
            SessionState::default()
        }
    }

    pub async fn commit_app_session_state(&mut self, state: SessionState) {
        let incoming_state =
            to_incoming_client_state(&state.state, &state.original_state, self.clock.micros());

        // Check if start/stop state changed for callback
        let should_invoke_callback = if let Some(start_stop_state) = incoming_state.start_stop_state
        {
            let changed = self.last_is_playing_for_callback != start_stop_state.is_playing;
            if changed {
                self.last_is_playing_for_callback = start_stop_state.is_playing;
            }
            changed
        } else {
            false
        };

        // Update the controller state
        self.controller.set_state(incoming_state).await;

        // Invoke start/stop callback if needed
        if should_invoke_callback {
            if let Some(ref callback) = self.start_stop_callback {
                if let Ok(callback) = callback.try_lock() {
                    callback(self.last_is_playing_for_callback);
                }
            }
        }

        // Check for tempo changes and invoke callback
        if let Some(ref callback) = self.tempo_callback {
            if let Ok(client_state) = self.controller.client_state.try_lock() {
                if let Ok(callback) = callback.try_lock() {
                    callback(client_state.timeline.tempo.bpm());
                }
            }
        }
    }
}

pub fn to_session_state(state: &ClientState, _is_connected: bool) -> SessionState {
    SessionState::new(
        ApiState {
            timeline: state.timeline,
            start_stop_state: ApiStartStopState {
                is_playing: state.start_stop_state.is_playing,
                time: state.start_stop_state.time,
            },
        },
        true, // Always respect quantum like C++ implementation
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IncomingClientState {
    pub timeline: Option<Timeline>,
    pub start_stop_state: Option<ClientStartStopState>,
    pub timeline_timestamp: Duration,
}

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

#[derive(Clone, Copy, Debug, Default)]
pub struct ApiState {
    timeline: Timeline,
    start_stop_state: ApiStartStopState,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
struct ApiStartStopState {
    is_playing: bool,
    time: Duration,
}

impl Default for ApiStartStopState {
    fn default() -> Self {
        Self {
            is_playing: false,
            time: Duration::zero(),
        }
    }
}

impl ApiStartStopState {
    fn new(is_playing: bool, time: Duration) -> Self {
        Self { is_playing, time }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionState {
    original_state: ApiState,
    state: ApiState,
    respect_quantum: bool,
}

impl SessionState {
    pub fn new(state: ApiState, respect_quantum: bool) -> Self {
        Self {
            original_state: state,
            state,
            respect_quantum,
        }
    }

    pub fn tempo(&self) -> f64 {
        self.state.timeline.tempo.bpm()
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
                time + Duration::microseconds(1),
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
    async fn test_basic_link() {
        let _ = tracing_subscriber::fmt::try_init();

        let bpm = 140.0;
        let mut link = BasicLink::new(bpm).await;

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
        let mut link = BasicLink::new(bpm).await;

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

        let link = BasicLink::new(120.0).await;
        let mut state = link.capture_app_session_state();
        assert_eq!(state.tempo(), 120.0);

        let time = link.clock().micros();
        state.set_tempo(140.0, time);
        assert_eq!(state.tempo(), 140.0);
    }

    #[tokio::test]
    async fn test_session_state_clamped_tempo() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await;
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

        let link = BasicLink::new(120.0).await;
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

        let link = BasicLink::new(120.0).await;
        let state = link.capture_app_session_state();

        let time = link.clock().micros();
        let beat = state.beat_at_time(time, 4.0);
        // beat should be a finite number
        assert!(beat.is_finite());
    }

    #[tokio::test]
    async fn test_session_state_phase_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await;
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

        let link = BasicLink::new(120.0).await;
        let mut state = link.capture_app_session_state();

        let time = link.clock().micros();
        state.force_beat_at_time(0.0, time, 4.0);
        let beat = state.beat_at_time(time, 4.0);
        assert!((beat - 0.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_session_state_request_beat_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await;
        let mut state = link.capture_app_session_state();

        let time = link.clock().micros();
        state.request_beat_at_time(0.0, time, 4.0);
        // After requesting, beat and phase should be finite
        let beat = state.beat_at_time(time, 4.0);
        let phase_val = state.phase_at_time(time, 4.0);
        assert!(beat.is_finite(), "beat should be finite after request");
        assert!(
            phase_val >= 0.0 && phase_val < 4.0,
            "phase should be in [0, 4)"
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

        let mut link = BasicLink::new(120.0).await;

        // Capture, modify, commit
        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_tempo(160.0, time);
        link.commit_app_session_state(state).await;

        // Re-capture and verify
        let state2 = link.capture_app_session_state();
        assert_eq!(state2.tempo(), 160.0);
    }

    #[tokio::test]
    async fn test_session_state_start_stop_commit_roundtrip() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await;

        let mut state = link.capture_app_session_state();
        assert!(!state.is_playing());

        let time = link.clock().micros();
        state.set_is_playing(true, time);
        link.commit_app_session_state(state).await;

        let state2 = link.capture_app_session_state();
        assert!(state2.is_playing());
    }

    #[tokio::test]
    async fn test_tempo_callback_on_commit() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await;

        let recorded_bpm = Arc::new(Mutex::new(0.0f64));
        let recorded_bpm_clone = recorded_bpm.clone();
        link.set_tempo_callback(move |bpm| {
            *recorded_bpm_clone.lock().unwrap() = bpm;
        });

        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_tempo(180.0, time);
        link.commit_app_session_state(state).await;

        let bpm = *recorded_bpm.lock().unwrap();
        assert_eq!(bpm, 180.0, "Tempo callback should fire with new BPM");
    }

    #[tokio::test]
    async fn test_start_stop_callback_on_commit() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut link = BasicLink::new(120.0).await;

        let recorded_playing = Arc::new(Mutex::new(false));
        let recorded_clone = recorded_playing.clone();
        link.set_start_stop_callback(move |is_playing| {
            *recorded_clone.lock().unwrap() = is_playing;
        });

        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_is_playing(true, time);
        link.commit_app_session_state(state).await;

        assert!(
            *recorded_playing.lock().unwrap(),
            "Start/stop callback should fire with true"
        );
    }

    #[tokio::test]
    async fn test_set_is_playing_and_request_beat_at_time() {
        let _ = tracing_subscriber::fmt::try_init();

        let link = BasicLink::new(120.0).await;
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

        let link = BasicLink::new(120.0).await;
        let mut state = link.capture_app_session_state();
        // Not playing — should be a no-op
        let tempo_before = state.tempo();
        state.request_beat_at_start_playing_time(0.0, 4.0);
        assert_eq!(state.tempo(), tempo_before);
    }
}
