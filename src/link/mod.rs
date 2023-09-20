pub mod beats;
pub mod controller;
pub mod error;
pub mod ghostxform;
pub mod measurement;
pub mod node;
pub mod sessions;
pub mod state;
pub mod tempo;
pub mod timeline;

use std::{
    result,
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Duration,
};

use crate::clock::Clock;

use self::{controller::Controller, state::ApiState};

pub type Result<T> = result::Result<T, error::Error>;

pub type PeerCountCallback = Arc<Mutex<Box<dyn Fn(usize) + Send>>>;
pub type TempoCallback = Arc<Mutex<Box<dyn Fn(tempo::Tempo) + Send>>>;
pub type StartStopCallback = Arc<Mutex<Box<dyn Fn(bool) + Send>>>;

pub struct BasicLink {
    peer_count_callback: Option<PeerCountCallback>,
    tempo_callback: Option<TempoCallback>,
    start_stop_callback: Option<StartStopCallback>,
    clock: Clock,
    controller: Controller,
}

impl BasicLink {
    pub async fn new(bpm: f64) -> Self {
        let clock = Clock::default();

        let controller = Controller::new(tempo::Tempo::new(bpm), None, None, None, clock).await;

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

    pub fn is_start_stop_sync_enabled(&self) -> bool {
        todo!()
    }

    pub fn enable_start_stop_sync(&mut self, enable: bool) {
        todo!()
    }

    pub fn num_peers(&self) -> usize {
        todo!()
    }

    pub fn set_num_peers_callback<F>(&mut self, callback: F)
    where
        F: FnMut(usize) + Send + 'static,
    {
        todo!()
    }

    pub fn set_tempo_callback<F>(&mut self, callback: F)
    where
        F: FnMut(f64) + Send + 'static,
    {
        todo!()
    }

    pub fn set_start_stop_callback<F>(&mut self, callback: F)
    where
        F: FnMut(bool) + Send + 'static,
    {
        todo!()
    }

    pub fn clock(&self) -> Clock {
        todo!()
    }

    pub fn capture_audio_session_state(&self) -> SessionState {
        todo!()
    }

    pub fn commit_audio_session_state(&mut self, state: SessionState) {
        todo!()
    }

    pub fn capture_app_session_state(&self) -> SessionState {
        todo!()
    }

    pub fn commit_app_session_state(&mut self, state: SessionState) {
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
    pub fn new(state: ApiState, respect_quantum: bool) -> Self {
        todo!()
    }

    pub fn tempo(&self) -> f64 {
        todo!()
    }

    pub fn set_tempo(&mut self, bpm: f64, at_time: Duration) {
        todo!()
    }

    pub fn beat_at_time(&self, time: Duration, quantum: f64) -> f64 {
        todo!()
    }

    pub fn phase_at_time(&self, time: Duration, quantum: f64) -> f64 {
        todo!()
    }

    pub fn time_at_beat(&self, beat: f64, quantum: f64) -> Duration {
        todo!()
    }

    pub fn request_beat_at_time(&mut self, beat: f64, time: Duration, quantum: f64) {
        todo!()
    }

    pub fn force_beat_at_time(&mut self, beat: f64, time: Duration, quantum: f64) {
        todo!()
    }

    pub fn set_is_playing(&mut self, is_playing: bool, time: Duration) {
        todo!()
    }

    pub fn is_playing(&self) -> bool {
        todo!()
    }

    pub fn time_for_is_playing(&self) -> Duration {
        todo!()
    }

    pub fn request_beat_at_start_playing_time(&mut self, beat: f64, quantum: f64) {
        todo!()
    }

    pub fn set_is_playing_and_request_beat_at_time(
        &mut self,
        is_playing: bool,
        time: Duration,
        beat: f64,
        quantum: f64,
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
