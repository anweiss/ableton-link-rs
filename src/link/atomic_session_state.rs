use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use super::{
    error::CommitError,
    safe_rt_session_state::{
        AudioCaptureError, AudioCommitTicket, AudioQueueError, SafeRtSessionStateHandler,
    },
    state::ClientState,
};

#[cfg(test)]
use super::{sessions::SessionId, state::ClientStartStopState, timeline::Timeline};
use crate::link::IncomingClientState;

/// Non-blocking accepted-state capture and a bounded proposal queue.
/// A non-RT consumer must validate each request and acknowledge its ticket.
pub struct AtomicSessionState {
    /// Accepted value snapshots and the acknowledged proposal queue.
    rt_handler: Arc<SafeRtSessionStateHandler>,

    /// Mirrored owner enable state; acceptance belongs to the non-RT consumer.
    is_enabled: AtomicBool,

    /// Last known peer count for determining connection state
    peer_count: AtomicU64,
}

impl AtomicSessionState {
    pub fn new(initial_state: ClientState) -> Self {
        Self {
            rt_handler: Arc::new(SafeRtSessionStateHandler::new(initial_state)),
            is_enabled: AtomicBool::new(false),
            peer_count: AtomicU64::new(0),
        }
    }

    /// Capture with one reader-pin attempt, try-only locks, and no allocation.
    /// On error, skip this capture or reuse a caller-owned accepted value.
    pub fn capture_audio_session_state(&self) -> Result<ClientState, AudioCaptureError> {
        self.rt_handler.get_rt_client_state()
    }

    /// Queue a proposal using try_lock, without allocating or changing capture.
    pub fn commit_audio_session_state(
        &self,
        incoming_state: IncomingClientState,
    ) -> Result<AudioCommitTicket, AudioQueueError> {
        self.rt_handler.update_rt_client_state(incoming_state)
    }

    pub(crate) fn commit_audio_session_state_from_origin(
        &self,
        incoming: IncomingClientState,
        origin: Option<super::sessions::SessionEditOrigin>,
    ) -> Result<AudioCommitTicket, AudioQueueError> {
        self.rt_handler
            .update_rt_client_state_from_origin(incoming, origin)
    }

    pub(crate) fn pending_update_from_origin(
        &self,
    ) -> Option<(
        AudioCommitTicket,
        IncomingClientState,
        Option<super::sessions::SessionEditOrigin>,
    )> {
        self.rt_handler.pending_update_from_origin()
    }

    /// Read the accepted snapshot without applying queued proposals.
    /// May block on accepted publication; use audio capture in callbacks.
    pub fn capture_app_session_state(&self) -> ClientState {
        self.rt_handler.get_app_client_state()
    }

    /// Queue an application proposal under the same explicit acknowledgment contract.
    pub fn commit_app_session_state(
        &self,
        incoming_state: IncomingClientState,
    ) -> Result<AudioCommitTicket, AudioQueueError> {
        self.rt_handler.update_rt_client_state(incoming_state)
    }

    /// Peek at a request; it remains pending until explicitly completed.
    pub fn process_pending_updates(&self) -> Option<IncomingClientState> {
        self.rt_handler.process_pending_updates()
    }

    pub fn pending_update(&self) -> Option<(AudioCommitTicket, IncomingClientState)> {
        self.rt_handler.pending_update()
    }

    pub fn publish_client_state(&self, accepted: ClientState) {
        self.rt_handler.publish_client_state(accepted);
    }

    pub fn complete_pending_update(
        &self,
        ticket: AudioCommitTicket,
        result: Result<(), CommitError>,
    ) -> bool {
        self.rt_handler.complete_pending_update(ticket, result)
    }

    pub fn take_commit_result(
        &self,
        ticket: AudioCommitTicket,
    ) -> Result<Option<Result<(), CommitError>>, AudioQueueError> {
        self.rt_handler.take_commit_result(ticket)
    }

    /// Mirror the owner's enable state; the non-RT consumer decides acceptance.
    pub fn set_enabled(&self, enabled: bool) {
        self.is_enabled.store(enabled, Ordering::Release);
    }

    /// Get current enable state
    pub fn is_enabled(&self) -> bool {
        self.is_enabled.load(Ordering::Acquire)
    }

    /// Update peer count (affects session behavior)
    pub fn set_peer_count(&self, count: usize) {
        self.peer_count.store(count as u64, Ordering::Release);
    }

    /// Get current peer count
    pub fn peer_count(&self) -> usize {
        self.peer_count.load(Ordering::Acquire) as usize
    }

    /// Check if there are pending updates
    pub fn has_pending_updates(&self) -> bool {
        self.rt_handler.has_pending_updates()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{beats::Beats, tempo::Tempo};
    use chrono::Duration;

    fn create_test_client_state() -> ClientState {
        ClientState {
            timeline: Timeline {
                tempo: Tempo::new(120.0),
                beat_origin: Beats::new(0.0),
                time_origin: Duration::zero(),
            },
            timeline_session_id: SessionId::default(),
            start_stop_state: ClientStartStopState {
                is_playing: false,
                time: Duration::zero(),
                timestamp: Duration::zero(),
            },
        }
    }

    #[test]
    fn test_atomic_session_state_creation() {
        let client_state = create_test_client_state();
        let session_state = AtomicSessionState::new(client_state);

        assert!(!session_state.is_enabled());
        assert_eq!(session_state.peer_count(), 0);
    }

    #[test]
    fn test_atomic_session_state_enable() {
        let client_state = create_test_client_state();
        let session_state = AtomicSessionState::new(client_state);

        session_state.set_enabled(true);
        assert!(session_state.is_enabled());

        session_state.set_enabled(false);
        assert!(!session_state.is_enabled());
    }

    #[test]
    fn test_atomic_session_state_peer_count() {
        let client_state = create_test_client_state();
        let session_state = AtomicSessionState::new(client_state);

        session_state.set_peer_count(5);
        assert_eq!(session_state.peer_count(), 5);

        session_state.set_peer_count(0);
        assert_eq!(session_state.peer_count(), 0);
    }

    #[test]
    fn test_atomic_session_state_capture_commit() {
        let client_state = create_test_client_state();
        let session_state = AtomicSessionState::new(client_state);

        // Test audio thread capture
        let captured_audio = session_state.capture_audio_session_state().unwrap();
        assert_eq!(captured_audio.timeline.tempo.bpm(), 120.0);

        // Test app thread capture
        let captured_app = session_state.capture_app_session_state();
        assert_eq!(captured_app.timeline.tempo.bpm(), 120.0);

        // Test commit from audio thread
        let new_timeline = Timeline {
            tempo: Tempo::new(140.0),
            beat_origin: Beats::new(1.0),
            time_origin: Duration::milliseconds(2000),
        };

        let incoming_state = IncomingClientState {
            timeline: Some(new_timeline),
            start_stop_state: None,
            timeline_timestamp: Duration::milliseconds(2000),
        };

        session_state
            .commit_audio_session_state(incoming_state)
            .unwrap();
        assert!(session_state.has_pending_updates());
    }
}
