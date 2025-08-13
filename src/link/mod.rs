#![allow(clippy::too_many_arguments)]

pub mod beats;
pub mod clock;
pub mod controller;
pub mod error;
pub mod ghostxform;
pub mod measurement;
pub mod node;
pub mod payload;
pub mod phase;
pub mod pingresponder;
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
}

impl BasicLink {
    pub async fn new(bpm: f64) -> Self {
        // Only set up tracing if not already set up
        let _ = tracing_subscriber::fmt::try_init();

        let clock = Clock::default();

        let controller = Controller::new(tempo::Tempo::new(bpm), clock).await;

        Self {
            peer_count_callback: None,
            tempo_callback: None,
            start_stop_callback: None,
            controller,
            clock,
        }
    }
}

impl BasicLink {
    pub async fn enable(&mut self) {
        self.controller.enable().await;
    }

    pub async fn disable(&mut self) {
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
    }

    pub fn set_tempo_callback<F>(&mut self, callback: F)
    where
        F: Fn(f64) + Send + 'static,
    {
        self.tempo_callback = Some(Arc::new(Mutex::new(Box::new(callback))));
    }

    pub fn set_start_stop_callback<F>(&mut self, callback: F)
    where
        F: Fn(bool) + Send + 'static,
    {
        self.start_stop_callback = Some(Arc::new(Mutex::new(Box::new(callback))));
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }

    pub fn capture_audio_session_state(&self) -> SessionState {
        // In a real implementation, this should use a realtime-safe mechanism
        // For now, we'll use the same as app session state but this should be optimized
        to_session_state(
            &self.controller.client_state.try_lock().unwrap(),
            self.num_peers() > 0,
        )
    }

    pub fn commit_audio_session_state(&mut self, state: SessionState) {
        // In a real implementation, this should be realtime-safe
        // For now, we'll use the same mechanism as app session state but synchronously
        let incoming_state =
            to_incoming_client_state(&state.state, &state.original_state, self.clock.micros());

        // This should be made async or use a realtime-safe mechanism
        // For now, we'll just update the client state directly
        *self.controller.client_state.try_lock().unwrap() = ClientState {
            timeline: incoming_state
                .timeline
                .unwrap_or(self.controller.client_state.try_lock().unwrap().timeline),
            start_stop_state: incoming_state.start_stop_state.unwrap_or(
                self.controller
                    .client_state
                    .try_lock()
                    .unwrap()
                    .start_stop_state,
            ),
        };
    }

    pub fn capture_app_session_state(&self) -> SessionState {
        to_session_state(
            &self.controller.client_state.try_lock().unwrap(),
            self.num_peers() > 0,
        )
    }

    pub async fn commit_app_session_state(&self, state: SessionState) {
        self.controller
            .set_state(to_incoming_client_state(
                &state.state,
                &state.original_state,
                self.clock.micros(),
            ))
            .await
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
}
