// Real-time safe atomic session state implementation
// Based on C++ Link's atomic and lock-free mechanisms

use chrono::Duration;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use super::{safe_rt_session_state::SafeRtSessionStateHandler, state::ClientState};

#[cfg(test)]
use super::{state::ClientStartStopState, timeline::Timeline};
use crate::link::IncomingClientState;

/// Atomic session state that provides lock-free access for real-time threads
/// This mirrors the C++ Link implementation's clientStateRtSafe() mechanism
pub struct AtomicSessionState {
    /// Real-time session state handler for lock-free access (now using safe implementation)
    rt_handler: Arc<SafeRtSessionStateHandler>,

    /// Enable state for determining if we should respect remote updates
    is_enabled: AtomicBool,

    /// Last known peer count for determining connection state
    peer_count: AtomicU64,
}

impl AtomicSessionState {
    pub fn new(initial_state: ClientState, grace_period: Duration) -> Self {
        Self {
            rt_handler: Arc::new(SafeRtSessionStateHandler::new(initial_state, grace_period)),
            is_enabled: AtomicBool::new(false),
            peer_count: AtomicU64::new(0),
        }
    }

    /// Get current session state for audio thread (real-time safe)
    /// This method is guaranteed to be non-blocking and suitable for audio threads
    pub fn capture_audio_session_state(&self, current_time: Duration) -> ClientState {
        self.rt_handler.get_rt_client_state(current_time)
    }

    /// Update session state from audio thread (real-time safe)
    /// This method is guaranteed to be non-blocking and suitable for audio threads
    pub fn commit_audio_session_state(
        &self,
        incoming_state: IncomingClientState,
        current_time: Duration,
    ) {
        let is_enabled = self.is_enabled.load(Ordering::Acquire);
        self.rt_handler
            .update_rt_client_state(incoming_state, current_time, is_enabled);
    }

    /// Get current session state for application thread (may block briefly)
    /// This is the non-realtime version that can use more expensive operations
    pub fn capture_app_session_state(&self, current_time: Duration) -> ClientState {
        // For application threads, we can afford to get the most up-to-date state
        self.rt_handler.get_rt_client_state(current_time)
    }

    /// Update session state from application thread (may block briefly)
    /// This method can use more expensive operations than the audio version
    pub fn commit_app_session_state(
        &self,
        incoming_state: IncomingClientState,
        current_time: Duration,
    ) {
        let is_enabled = self.is_enabled.load(Ordering::Acquire);
        self.rt_handler
            .update_rt_client_state(incoming_state, current_time, is_enabled);
    }

    /// Process any pending updates from real-time thread
    /// Should be called periodically from a background thread
    pub fn process_pending_updates(&self) -> Option<IncomingClientState> {
        self.rt_handler.process_pending_updates()
    }

    /// Update enable state (affects how updates are processed)
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

    /// Get the current grace period
    pub fn grace_period(&self) -> Duration {
        self.rt_handler.grace_period()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{beats::Beats, tempo::Tempo};

    fn create_test_client_state() -> ClientState {
        ClientState {
            timeline: Timeline {
                tempo: Tempo::new(120.0),
                beat_origin: Beats::new(0.0),
                time_origin: Duration::zero(),
            },
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
        let session_state = AtomicSessionState::new(client_state, Duration::milliseconds(1000));

        assert!(!session_state.is_enabled());
        assert_eq!(session_state.peer_count(), 0);
        assert_eq!(session_state.grace_period(), Duration::milliseconds(1000));
    }

    #[test]
    fn test_atomic_session_state_enable() {
        let client_state = create_test_client_state();
        let session_state = AtomicSessionState::new(client_state, Duration::milliseconds(1000));

        session_state.set_enabled(true);
        assert!(session_state.is_enabled());

        session_state.set_enabled(false);
        assert!(!session_state.is_enabled());
    }

    #[test]
    fn test_atomic_session_state_peer_count() {
        let client_state = create_test_client_state();
        let session_state = AtomicSessionState::new(client_state, Duration::milliseconds(1000));

        session_state.set_peer_count(5);
        assert_eq!(session_state.peer_count(), 5);

        session_state.set_peer_count(0);
        assert_eq!(session_state.peer_count(), 0);
    }

    #[test]
    fn test_atomic_session_state_capture_commit() {
        let client_state = create_test_client_state();
        let session_state = AtomicSessionState::new(client_state, Duration::milliseconds(1000));

        let current_time = Duration::milliseconds(1000);

        // Test audio thread capture
        let captured_audio = session_state.capture_audio_session_state(current_time);
        assert_eq!(captured_audio.timeline.tempo.bpm(), 120.0);

        // Test app thread capture
        let captured_app = session_state.capture_app_session_state(current_time);
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

        session_state.commit_audio_session_state(incoming_state, current_time);
        assert!(session_state.has_pending_updates());
    }
}
