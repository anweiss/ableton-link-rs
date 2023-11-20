#![allow(clippy::too_many_arguments)]

pub mod beats;
pub mod clock;
pub mod controller;
pub mod error;
pub mod ghostxform;
pub mod measurement;
pub mod node;
pub mod payload;
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
    clock::Clock,
    controller::Controller,
    state::{ClientStartStopState, ClientState},
    tempo::Tempo,
    timeline::{clamp_tempo, Timeline},
};

pub type Result<T> = result::Result<T, error::Error>;

pub type PeerCountCallback = Arc<Mutex<Box<dyn Fn(usize) + Send>>>;
pub type TempoCallback = Arc<Mutex<Box<dyn Fn(tempo::Tempo) + Send>>>;
pub type StartStopCallback = Arc<Mutex<Box<dyn Fn(bool) + Send>>>;

pub struct BasicLink {
    // peer_count_callback: Option<PeerCountCallback>,
    // tempo_callback: Option<TempoCallback>,
    // start_stop_callback: Option<StartStopCallback>,
    controller: Controller,
    clock: Clock,
}

impl BasicLink {
    pub async fn new(bpm: f64) -> Self {
        let subscriber = tracing_subscriber::FmtSubscriber::new();
        tracing::subscriber::set_global_default(subscriber).unwrap();

        let clock = Clock::default();

        let controller = Controller::new(tempo::Tempo::new(bpm), clock).await;

        Self {
            // peer_count_callback: None,
            // tempo_callback: None,
            // start_stop_callback: None,
            controller,
            clock,
        }
    }
}

impl BasicLink {
    pub async fn enable(&mut self) {
        self.controller.enable().await;
    }

    pub fn is_start_stop_sync_enabled(&self) -> bool {
        todo!()
    }

    pub fn enable_start_stop_sync(&mut self, _enable: bool) {
        todo!()
    }

    pub fn num_peers(&self) -> usize {
        self.controller.num_peers()
    }

    pub fn set_num_peers_callback<F>(&mut self, _callback: F)
    where
        F: FnMut(usize) + Send + 'static,
    {
        todo!()
    }

    pub fn set_tempo_callback<F>(&mut self, _callback: F)
    where
        F: FnMut(f64) + Send + 'static,
    {
        todo!()
    }

    pub fn set_start_stop_callback<F>(&mut self, _callback: F)
    where
        F: FnMut(bool) + Send + 'static,
    {
        todo!()
    }

    pub fn capture_audio_session_state(&self) -> SessionState {
        todo!()
    }

    pub fn commit_audio_session_state(&mut self, _state: SessionState) {
        todo!()
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

pub fn to_session_state(state: &ClientState, is_connected: bool) -> SessionState {
    SessionState::new(
        ApiState {
            timeline: state.timeline,
            start_stop_state: ApiStartStopState {
                is_playing: state.start_stop_state.is_playing,
                time: state.start_stop_state.time,
            },
        },
        is_connected,
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
        todo!()
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

    pub fn beat_at_time(&self, _time: Duration, _quantum: f64) -> f64 {
        todo!()
    }

    pub fn phase_at_time(&self, _time: Duration, _quantum: f64) -> f64 {
        todo!()
    }

    pub fn time_at_beat(&self, _beat: f64, _quantum: f64) -> Duration {
        todo!()
    }

    pub fn request_beat_at_time(&mut self, _beat: f64, _time: Duration, _quantum: f64) {
        todo!()
    }

    pub fn force_beat_at_time(&mut self, _beat: f64, _time: Duration, _quantum: f64) {
        todo!()
    }

    pub fn set_is_playing(&mut self, _is_playing: bool, _time: Duration) {
        todo!()
    }

    pub fn is_playing(&self) -> bool {
        todo!()
    }

    pub fn time_for_is_playing(&self) -> Duration {
        todo!()
    }

    pub fn request_beat_at_start_playing_time(&mut self, _beat: f64, _quantum: f64) {
        todo!()
    }

    pub fn set_is_playing_and_request_beat_at_time(
        &mut self,
        _is_playing: bool,
        _time: Duration,
        _beat: f64,
        _quantum: f64,
    ) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use tracing::info;

    use super::*;

    #[tokio::test]
    async fn test_basic_link() {
        // console_subscriber::init();

        let bpm = 140.0;

        let mut link = BasicLink::new(bpm).await;

        info!("initializing basic link at {} bpm", bpm);

        link.enable().await;

        // tokio::time::sleep(Duration::seconds(10).to_std().unwrap()).await;

        // let mut state = link.capture_app_session_state();

        // info!("captured app session state: {:?}", state);

        // tokio::time::sleep(Duration::seconds(10).to_std().unwrap()).await;

        // info!("changing tempo to {}", bpm);

        // state.set_tempo(bpm, Duration::zero());

        // info!("commiting new app session state");

        // link.commit_app_session_state(state).await;

        // tokio::time::sleep(Duration::seconds(10).to_std().unwrap()).await;
    }
}
