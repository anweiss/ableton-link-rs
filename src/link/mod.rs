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
    time::Duration,
};

use self::{clock::Clock, controller::Controller, state::ApiState};

pub type Result<T> = result::Result<T, error::Error>;

pub type PeerCountCallback = Arc<Mutex<Box<dyn Fn(usize) + Send>>>;
pub type TempoCallback = Arc<Mutex<Box<dyn Fn(tempo::Tempo) + Send>>>;
pub type StartStopCallback = Arc<Mutex<Box<dyn Fn(bool) + Send>>>;

pub struct BasicLink {
    peer_count_callback: Option<PeerCountCallback>,
    tempo_callback: Option<TempoCallback>,
    start_stop_callback: Option<StartStopCallback>,
    controller: Controller,
}

impl BasicLink {
    pub async fn new(bpm: f64) -> Self {
        let subscriber = tracing_subscriber::FmtSubscriber::new();
        tracing::subscriber::set_global_default(subscriber).unwrap();

        let controller =
            Controller::new(tempo::Tempo::new(bpm), None, None, None, Clock::new()).await;

        Self {
            peer_count_callback: None,
            tempo_callback: None,
            start_stop_callback: None,
            controller,
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
        todo!()
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
        todo!()
    }

    pub fn commit_app_session_state(&mut self, _state: SessionState) {
        todo!()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionState {
    original_state: ApiState,
    state: ApiState,
    respect_quantum: bool,
}

impl SessionState {
    pub fn new(_state: ApiState, _respect_quantum: bool) -> Self {
        todo!()
    }

    pub fn tempo(&self) -> f64 {
        todo!()
    }

    pub fn set_tempo(&mut self, _bpm: f64, _at_time: Duration) {
        todo!()
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
    use super::*;

    #[tokio::test]
    async fn test_basic_link() {
        let mut link = BasicLink::new(120.0).await;
        link.enable().await;
    }
}
