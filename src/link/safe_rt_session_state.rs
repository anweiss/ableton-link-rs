use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock, TryLockError};

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
    /// [`ClientState::timeline_session_id`].
    pub timeline_session_id: SessionId,
    pub start_stop_state: ClientStartStopState,
}

impl From<ClientState> for RtClientState {
    fn from(client_state: ClientState) -> Self {
        Self {
            timeline: client_state.timeline,
            timeline_session_id: client_state.timeline_session_id,
            start_stop_state: client_state.start_stop_state,
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

struct AcceptedState {
    slots: [RwLock<ClientState>; 2],
    published: AtomicUsize,
    readers: AtomicUsize,
    pending: Mutex<Option<ClientState>>,
}

struct AcceptedRead<'a>(&'a AcceptedState);

impl Drop for AcceptedRead<'_> {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::SeqCst);
    }
}

impl AcceptedState {
    fn new(initial: ClientState) -> Self {
        Self {
            slots: [RwLock::new(initial), RwLock::new(initial)],
            published: AtomicUsize::new(0),
            readers: AtomicUsize::new(0),
            pending: Mutex::new(None),
        }
    }

    fn flush(&self, pending: &mut Option<ClientState>) {
        let Some(state) = *pending else { return };
        if self.readers.load(Ordering::SeqCst) != 0 {
            return;
        }
        let next = 1 - self.published.load(Ordering::SeqCst);
        let mut slot = match self.slots[next].try_write() {
            Ok(slot) => slot,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        *slot = state;
        *pending = None;
        drop(slot);
        self.published.store(next, Ordering::SeqCst);
    }

    fn read(&self) -> ClientState {
        if let Ok(mut pending) = self.pending.try_lock() {
            self.flush(&mut pending);
        }
        // Pin before loading the index. An in-flight writer uses the other
        // slot; another writer cannot start while this reader remains pinned.
        let mut readers = self.readers.load(Ordering::SeqCst);
        loop {
            let next = readers
                .checked_add(1)
                .expect("too many simultaneous accepted-state readers");
            match self.readers.compare_exchange_weak(
                readers,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(current) => readers = current,
            }
        }
        let reader = AcceptedRead(self);
        let slot = self.published.load(Ordering::SeqCst);
        let state = match self.slots[slot].try_read() {
            Ok(state) => *state,
            Err(TryLockError::Poisoned(error)) => *error.into_inner(),
            Err(TryLockError::WouldBlock) => unreachable!("published accepted slot is pinned"),
        };
        drop(reader);
        state
    }

    fn publish(&self, state: ClientState) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        *pending = Some(state);
        self.flush(&mut pending);
    }
}

/// Non-blocking accepted snapshots and a bounded acknowledged proposal queue.
pub struct SafeRtSessionStateHandler {
    accepted: AcceptedState,
    has_pending_updates: AtomicBool,
    pending: Mutex<PendingUpdate>,
}

impl SafeRtSessionStateHandler {
    pub fn new(initial_state: ClientState) -> Self {
        Self {
            accepted: AcceptedState::new(initial_state),
            has_pending_updates: AtomicBool::new(false),
            pending: Mutex::new(PendingUpdate::default()),
        }
    }

    pub fn get_rt_client_state(&self) -> ClientState {
        self.accepted.read()
    }

    /// Queue a proposal without changing accepted state. One outstanding request
    /// is retained until a non-RT consumer completes it and its ticket is read.
    /// Uses try_lock and performs no allocation.
    pub fn update_rt_client_state(
        &self,
        incoming_state: IncomingClientState,
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
                pending.origin,
            )
        })
    }

    /// Publish only a controller-accepted snapshot, never an unvalidated proposal.
    pub fn publish_client_state(&self, accepted: ClientState) {
        self.accepted.publish(accepted);
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
}

// Safe implementations - no unsafe code needed
// The compiler automatically derives Send + Sync for us when all fields are Send + Sync

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
    fn test_safe_rt_session_state_handler_creation() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state);

        assert!(!handler.has_pending_updates());
    }

    #[test]
    fn test_safe_rt_session_state_handler_get_state() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state);

        let retrieved_state = handler.get_rt_client_state();

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
        let handler = SafeRtSessionStateHandler::new(client_state);

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
        let ticket = handler.update_rt_client_state(incoming_state).unwrap();
        assert_eq!(handler.get_rt_client_state(), client_state);

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
            handler.update_rt_client_state(incoming_state),
            Err(AudioQueueError::Busy)
        );
        assert!(handler.complete_pending_update(ticket, Err(CommitError::TimelineOriginExhausted)));
        assert_eq!(handler.get_rt_client_state(), client_state);
        assert_eq!(
            handler.update_rt_client_state(incoming_state),
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
        let ticket = handler.update_rt_client_state(incoming_state).unwrap();
        handler.publish_client_state(ClientState {
            timeline: new_timeline,
            ..client_state
        });
        assert!(handler.complete_pending_update(ticket, Ok(())));
        assert_eq!(handler.take_commit_result(ticket), Ok(Some(Ok(()))));
        assert_eq!(handler.get_rt_client_state().timeline, new_timeline);
    }

    #[test]
    fn ordering_accepted_snapshot_survives_preempted_publication() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state);
        let mut accepted = client_state;
        accepted.timeline.tempo = Tempo::new(150.0);
        handler.publish_client_state(accepted);
        assert_eq!(handler.get_rt_client_state(), accepted);
        let mut pending = handler.accepted.pending.lock().unwrap();
        let next = 1 - handler.accepted.published.load(Ordering::SeqCst);
        let slot = handler.accepted.slots[next].write().unwrap();
        let mut newer = accepted;
        newer.timeline.tempo = Tempo::new(180.0);
        *pending = Some(newer);
        for _ in 0..3 {
            assert_eq!(handler.get_rt_client_state(), accepted);
        }
        drop(slot);
        drop(pending);
        assert_eq!(handler.get_rt_client_state(), newer);
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
        let handler = Arc::new(SafeRtSessionStateHandler::new(client_state));

        let handler_clone = handler.clone();

        // Spawn a thread that simulates RT thread behavior
        let rt_thread = thread::spawn(move || {
            for _ in 0..100 {
                let _state = handler_clone.get_rt_client_state();

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

            let ticket = handler.update_rt_client_state(incoming_state).unwrap();
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
        let final_state = handler.get_rt_client_state();
        assert_eq!(final_state.timeline.tempo.bpm(), 169.0);
    }
}
