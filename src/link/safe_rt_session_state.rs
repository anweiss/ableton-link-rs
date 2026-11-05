use chrono::Duration;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use triple_buffer::{Input, Output, TripleBuffer};

use super::error::CommitError;
use super::{
    sessions::SessionId,
    state::{ClientStartStopState, ClientState},
    timeline::Timeline,
};
use crate::link::IncomingClientState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioCommitTicket(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AudioQueueError {
    #[error("audio commit queue is busy; drain and acknowledge the outstanding request")]
    Busy,
    #[error("audio commit ticket counter exhausted")]
    TicketExhausted,
    #[error("audio commit ticket is unknown or already acknowledged")]
    UnknownTicket,
}

#[derive(Default)]
struct PendingUpdate {
    sequence: u64,
    incoming: Option<IncomingClientState>,
    result: Option<Result<(), CommitError>>,
    origin: Option<super::sessions::SessionEditOrigin>,
}

/// Accepted client state for non-blocking access from audio threads.
#[derive(Clone, Debug, Default)]
pub struct RtClientState {
    pub timeline: Timeline,
    /// The session `timeline` was captured from; mirrored from
    /// [`ClientState::timeline_session_id`] so grace-period handling can
    /// detect a session change even when the timeline value is unchanged.
    pub timeline_session_id: SessionId,
    pub start_stop_state: ClientStartStopState,
    pub timeline_timestamp: Duration,
    pub start_stop_state_timestamp: Duration,
}

impl From<ClientState> for RtClientState {
    fn from(client_state: ClientState) -> Self {
        Self {
            timeline: client_state.timeline,
            timeline_session_id: client_state.timeline_session_id,
            start_stop_state: client_state.start_stop_state,
            timeline_timestamp: Duration::zero(),
            start_stop_state_timestamp: Duration::zero(),
        }
    }
}

impl From<RtClientState> for ClientState {
    fn from(rt_state: RtClientState) -> Self {
        Self {
            timeline: rt_state.timeline,
            timeline_session_id: rt_state.timeline_session_id,
            start_stop_state: rt_state.start_stop_state,
        }
    }
}

/// Real-time safe session state handler using only safe Rust
/// This provides non-blocking access to session state for real-time audio threads
/// while allowing safe updates from other threads.
///
/// Uses RwLock for cached state (readers won't block each other) and
/// triple_buffer crate for lock-free producer-consumer communication.
pub struct SafeRtSessionStateHandler {
    /// Triple buffer for sharing client state between RT and non-RT threads
    /// This is split into separate input/output handles that are thread-safe
    client_state_input: Arc<Mutex<Input<ClientState>>>,
    client_state_output: Arc<Mutex<Output<ClientState>>>,

    /// Cached RT client state for immediate access
    /// RwLock allows multiple concurrent readers (RT threads) but exclusive writer access
    cached_rt_state: Arc<RwLock<RtClientState>>,

    /// Atomic timestamps for grace period management (in microseconds)
    timeline_timestamp: AtomicU64,
    start_stop_timestamp: AtomicU64,

    /// Flag indicating pending updates
    has_pending_updates: AtomicBool,
    pending: Mutex<PendingUpdate>,

    /// Grace period for local modifications (in microseconds)
    local_mod_grace_period_micros: AtomicU64,
}

impl SafeRtSessionStateHandler {
    pub fn new(initial_state: ClientState, grace_period: Duration) -> Self {
        let grace_period_micros = grace_period.num_microseconds().unwrap_or(1000000) as u64; // Default 1 second

        // Create triple buffer for client state
        let (input, output) = TripleBuffer::new(&initial_state).split();

        Self {
            client_state_input: Arc::new(Mutex::new(input)),
            client_state_output: Arc::new(Mutex::new(output)),
            cached_rt_state: Arc::new(RwLock::new(initial_state.into())),
            timeline_timestamp: AtomicU64::new(0),
            start_stop_timestamp: AtomicU64::new(0),
            has_pending_updates: AtomicBool::new(false),
            pending: Mutex::new(PendingUpdate::default()),
            local_mod_grace_period_micros: AtomicU64::new(grace_period_micros),
        }
    }

    /// Get the current real-time client state
    /// This is optimized for real-time threads - uses try_read to avoid blocking
    pub fn get_rt_client_state(&self, current_time: Duration) -> ClientState {
        let current_time_micros = current_time.num_microseconds().unwrap_or(0) as u64;
        let grace_period = self.local_mod_grace_period_micros.load(Ordering::Acquire);

        // Check if grace period has expired for timeline
        let timeline_grace_expired = current_time_micros
            .saturating_sub(self.timeline_timestamp.load(Ordering::Acquire))
            > grace_period;

        // Check if grace period has expired for start/stop state
        let start_stop_grace_expired = current_time_micros
            .saturating_sub(self.start_stop_timestamp.load(Ordering::Acquire))
            > grace_period;

        // Try to get new state if grace periods have expired and there are updates
        if timeline_grace_expired || start_stop_grace_expired {
            // Try to check for new data from the triple buffer (non-blocking)
            if let Ok(mut output) = self.client_state_output.try_lock() {
                // Check if there's new data available
                if output.updated() {
                    let new_state = *output.read();

                    // Try to update the cached state (non-blocking)
                    if let Ok(mut cached_state) = self.cached_rt_state.try_write() {
                        // Update unconditionally once the grace period has
                        // expired: comparing only the timeline value would
                        // miss a session change whose timeline happens to be
                        // numerically identical to the previous session's.
                        if timeline_grace_expired {
                            cached_state.timeline = new_state.timeline;
                            cached_state.timeline_session_id = new_state.timeline_session_id;
                        }

                        if start_stop_grace_expired {
                            cached_state.start_stop_state = new_state.start_stop_state;
                        }
                    }

                    return new_state;
                }
            }
        }

        // Return current cached state (non-blocking read)
        // If we can't get the lock, we use the last known state stored atomically
        if let Ok(cached_state) = self.cached_rt_state.try_read() {
            cached_state.clone().into()
        } else {
            // Fallback: create a default state if we can't read the cache
            // This should rarely happen in practice
            ClientState::default()
        }
    }

    /// Queue a proposal without changing accepted state. One outstanding request
    /// is retained until a non-RT consumer completes it and its ticket is read.
    /// Uses try_lock and performs no allocation.
    pub fn update_rt_client_state(
        &self,
        incoming_state: IncomingClientState,
        _current_time: Duration,
        _is_enabled: bool,
    ) -> Result<AudioCommitTicket, AudioQueueError> {
        self.update_rt_client_state_from_origin(incoming_state, None)
    }

    pub(crate) fn update_rt_client_state_from_origin(
        &self,
        incoming_state: IncomingClientState,
        origin: Option<super::sessions::SessionEditOrigin>,
    ) -> Result<AudioCommitTicket, AudioQueueError> {
        let mut pending = self.pending.try_lock().map_err(|_| AudioQueueError::Busy)?;
        if pending.incoming.is_some() || pending.result.is_some() {
            return Err(AudioQueueError::Busy);
        }
        pending.sequence = pending
            .sequence
            .checked_add(1)
            .ok_or(AudioQueueError::TicketExhausted)?;
        pending.incoming = Some(incoming_state);
        pending.origin = origin;
        self.has_pending_updates.store(true, Ordering::Release);
        Ok(AudioCommitTicket(pending.sequence))
    }

    /// Peek at the pending request, preserving its changed-field mask.
    /// The non-RT consumer must validate, publish accepted state, and complete
    /// its ticket. Repeated reads do not consume or acknowledge the request.
    pub fn process_pending_updates(&self) -> Option<IncomingClientState> {
        self.pending_update().map(|(_, incoming)| incoming)
    }

    /// Peek without consuming: cancellation of a non-RT drain leaves the request queued.
    pub fn pending_update(&self) -> Option<(AudioCommitTicket, IncomingClientState)> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending
            .incoming
            .map(|incoming| (AudioCommitTicket(pending.sequence), incoming))
    }

    pub(crate) fn pending_update_from_origin(
        &self,
    ) -> Option<(
        AudioCommitTicket,
        IncomingClientState,
        Option<super::sessions::SessionEditOrigin>,
    )> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.incoming.map(|incoming| {
            (
                AudioCommitTicket(pending.sequence),
                incoming,
                pending.origin.clone(),
            )
        })
    }

    /// Publish only a controller-accepted snapshot, never an unvalidated proposal.
    pub fn publish_client_state(&self, accepted: ClientState) {
        self.client_state_input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(accepted);
        *self
            .cached_rt_state
            .write()
            .unwrap_or_else(|e| e.into_inner()) = accepted.into();
    }

    pub fn complete_pending_update(
        &self,
        ticket: AudioCommitTicket,
        result: Result<(), CommitError>,
    ) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if ticket.0 != pending.sequence || pending.incoming.is_none() {
            return false;
        }
        pending.incoming = None;
        pending.origin = None;
        pending.result = Some(result);
        self.has_pending_updates.store(false, Ordering::Release);
        true
    }

    /// Non-blocking acknowledgment; a completed result occupies the slot until read.
    pub fn take_commit_result(
        &self,
        ticket: AudioCommitTicket,
    ) -> Result<Option<Result<(), CommitError>>, AudioQueueError> {
        let mut pending = self.pending.try_lock().map_err(|_| AudioQueueError::Busy)?;
        if ticket.0 != pending.sequence {
            return Err(AudioQueueError::UnknownTicket);
        }
        if pending.incoming.is_some() {
            return Ok(None);
        }
        pending
            .result
            .take()
            .map(Some)
            .ok_or(AudioQueueError::UnknownTicket)
    }

    /// Check if there are pending updates waiting to be processed
    pub fn has_pending_updates(&self) -> bool {
        self.has_pending_updates.load(Ordering::Acquire)
    }

    /// Get the current grace period
    pub fn grace_period(&self) -> Duration {
        Duration::microseconds(self.local_mod_grace_period_micros.load(Ordering::Acquire) as i64)
    }

    /// Set a new grace period for local modifications
    pub fn set_grace_period(&self, grace_period: Duration) {
        let grace_period_micros = grace_period.num_microseconds().unwrap_or(1000000) as u64;
        self.local_mod_grace_period_micros
            .store(grace_period_micros, Ordering::Release);
    }
}

// Safe implementations - no unsafe code needed
// The compiler automatically derives Send + Sync for us when all fields are Send + Sync

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
            timeline_session_id: SessionId::default(),
            start_stop_state: ClientStartStopState {
                is_playing: false,
                time: Duration::zero(),
                timestamp: Duration::zero(),
            },
        }
    }

    #[test]
    fn test_safe_rt_session_state_handler_creation() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        assert_eq!(handler.grace_period(), Duration::milliseconds(1000));
        assert!(!handler.has_pending_updates());
    }

    #[test]
    fn test_safe_rt_session_state_handler_get_state() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        let current_time = Duration::milliseconds(500);
        let retrieved_state = handler.get_rt_client_state(current_time);

        assert_eq!(
            retrieved_state.timeline.tempo.bpm(),
            client_state.timeline.tempo.bpm()
        );
        assert_eq!(
            retrieved_state.start_stop_state.is_playing,
            client_state.start_stop_state.is_playing
        );
    }

    #[test]
    fn test_safe_rt_session_state_handler_updates() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        // Create an incoming state update
        let new_timeline = Timeline {
            tempo: Tempo::new(140.0),
            beat_origin: Beats::new(1.0),
            time_origin: Duration::milliseconds(1000),
        };

        let incoming_state = IncomingClientState {
            timeline: Some(new_timeline),
            start_stop_state: None,
            timeline_timestamp: Duration::milliseconds(2000),
        };

        // Update the real-time state
        let ticket = handler
            .update_rt_client_state(incoming_state, Duration::milliseconds(3000), true)
            .unwrap();
        assert_eq!(
            handler.get_rt_client_state(Duration::seconds(10)),
            client_state
        );

        // Should have pending updates
        assert!(handler.has_pending_updates());

        // Process pending updates
        let processed = handler.process_pending_updates();
        assert!(processed.is_some());

        let processed_state = processed.unwrap();
        assert!(processed_state.timeline.is_some());
        assert_eq!(processed_state.timeline.unwrap().tempo.bpm(), 140.0);
        assert_eq!(processed_state, incoming_state);
        assert_eq!(handler.process_pending_updates(), Some(incoming_state));
        assert_eq!(
            handler.update_rt_client_state(incoming_state, Duration::zero(), true),
            Err(AudioQueueError::Busy)
        );
        assert!(handler.complete_pending_update(ticket, Err(CommitError::TimelineOriginExhausted)));
        assert_eq!(
            handler.get_rt_client_state(Duration::seconds(10)),
            client_state
        );
        assert_eq!(
            handler.update_rt_client_state(incoming_state, Duration::zero(), true),
            Err(AudioQueueError::Busy)
        );
        assert_eq!(
            handler.take_commit_result(ticket),
            Ok(Some(Err(CommitError::TimelineOriginExhausted)))
        );
        assert_eq!(
            handler.take_commit_result(ticket),
            Err(AudioQueueError::UnknownTicket)
        );
        let ticket = handler
            .update_rt_client_state(incoming_state, Duration::zero(), true)
            .unwrap();
        handler.publish_client_state(ClientState {
            timeline: new_timeline,
            ..client_state
        });
        assert!(handler.complete_pending_update(ticket, Ok(())));
        assert_eq!(handler.take_commit_result(ticket), Ok(Some(Ok(()))));
        assert_eq!(
            handler.get_rt_client_state(Duration::seconds(10)).timeline,
            new_timeline
        );
    }

    #[test]
    fn test_safe_rt_session_state_handler_grace_period() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        // Change grace period
        handler.set_grace_period(Duration::milliseconds(2000));
        assert_eq!(handler.grace_period(), Duration::milliseconds(2000));
    }

    #[test]
    fn test_safe_triple_buffer_basic_ops() {
        use triple_buffer::TripleBuffer;

        let (mut input, mut output) = TripleBuffer::new(&42i32).split();

        // Initial read should return the initial value since data is always available
        assert_eq!(*output.read(), 42);
        assert!(!output.updated()); // No new data has been written yet

        // Write a new value
        input.write(100);

        // Should indicate that new data is available
        assert!(output.updated());

        // Read should return the new value
        assert_eq!(*output.read(), 100);
    }

    #[test]
    fn test_safe_concurrent_access() {
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration as StdDuration;

        let client_state = create_test_client_state();
        let handler = Arc::new(SafeRtSessionStateHandler::new(
            client_state,
            Duration::milliseconds(1000),
        ));

        let handler_clone = handler.clone();

        // Spawn a thread that simulates RT thread behavior
        let rt_thread = thread::spawn(move || {
            for i in 0..100 {
                let current_time = Duration::milliseconds(i * 10);
                let _state = handler_clone.get_rt_client_state(current_time);

                // Simulate some RT work
                thread::sleep(StdDuration::from_micros(10));
            }
        });

        // Main thread simulates non-RT updates
        for i in 0..50 {
            let new_timeline = Timeline {
                tempo: Tempo::new(120.0 + i as f64),
                beat_origin: Beats::new(0.0),
                time_origin: Duration::milliseconds(i * 20),
            };

            let incoming_state = IncomingClientState {
                timeline: Some(new_timeline),
                start_stop_state: None,
                timeline_timestamp: Duration::milliseconds(i * 20),
            };

            let ticket = handler
                .update_rt_client_state(incoming_state, Duration::milliseconds(i * 20), true)
                .unwrap();
            assert_eq!(handler.process_pending_updates(), Some(incoming_state));
            handler.publish_client_state(ClientState {
                timeline: new_timeline,
                ..client_state
            });
            assert!(handler.complete_pending_update(ticket, Ok(())));
            assert_eq!(handler.take_commit_result(ticket), Ok(Some(Ok(()))));

            thread::sleep(StdDuration::from_micros(20));
        }

        rt_thread.join().unwrap();

        // Verify we can still read state
        let final_state = handler.get_rt_client_state(Duration::milliseconds(1000));
        assert_eq!(final_state.timeline.tempo.bpm(), 169.0);
    }
}
