use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock, TryLockError};

use super::error::CommitError;
use super::{
    sessions::SessionId,
    state::{ClientStartStopState, ClientState},
    timeline::Timeline,
};
use crate::link::IncomingClientState;

/// Copyable request identity, recoverable from its owning handler if lost.
/// Dropping copies does not cancel a request or acknowledge its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioCommitTicket {
    queue: core::num::NonZeroU64,
    sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AudioQueueError {
    /// Admission found an occupied slot or an unavailable try-only lock.
    /// Acknowledgment and ticket lookup also return this for an unavailable lock,
    /// including contention or poisoning. This does not
    /// establish that a request or result exists, or validate an existing ticket.
    /// Retry later without spinning in the audio callback; drain or acknowledge
    /// only an actual ticket. Failed admission creates no ticket.
    #[error("audio commit queue is unavailable or occupied")]
    Busy,
    #[error("audio commit ticket counter exhausted")]
    TicketExhausted,
    #[error("audio commit ticket is unknown or already acknowledged")]
    UnknownTicket,
}

/// A failed capture returns no snapshot, but may already have flushed a pending
/// accepted publication before attempting to pin a reader. It is not a rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AudioCaptureError {
    /// The single reader-pin attempt lost a compare-exchange race.
    /// Skip capture or reuse a caller-owned accepted value; do not retry here.
    #[error("accepted-state reader count changed; skip capture until a later callback")]
    Busy,
    #[error("accepted-state reader counter exhausted")]
    ReaderExhausted,
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

    fn read(&self) -> Result<ClientState, AudioCaptureError> {
        self.read_with_pin_hook(|| {})
    }

    fn read_with_pin_hook(
        &self,
        mut before_pin: impl FnMut(),
    ) -> Result<ClientState, AudioCaptureError> {
        if let Ok(mut pending) = self.pending.try_lock() {
            self.flush(&mut pending);
        }
        // Pin before loading the index. An in-flight writer uses the other
        // slot; another writer cannot start while this reader remains pinned.
        let readers = self.readers.load(Ordering::SeqCst);
        let next = readers
            .checked_add(1)
            .ok_or(AudioCaptureError::ReaderExhausted)?;
        before_pin();
        self.readers
            .compare_exchange(readers, next, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| AudioCaptureError::Busy)?;
        let reader = AcceptedRead(self);
        let slot = self.published.load(Ordering::SeqCst);
        let state = match self.slots[slot].try_read() {
            Ok(state) => *state,
            Err(TryLockError::Poisoned(error)) => *error.into_inner(),
            Err(TryLockError::WouldBlock) => unreachable!("published accepted slot is pinned"),
        };
        drop(reader);
        Ok(state)
    }

    fn read_app(&self) -> ClientState {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        self.flush(&mut pending);
        let slot = self.published.load(Ordering::SeqCst);
        *self.slots[slot].read().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, state: ClientState) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        *pending = Some(state);
        self.flush(&mut pending);
    }
}

/// Try-only audio snapshots and a bounded acknowledged proposal queue.
/// Audio capture, admission, ticket lookup and acknowledgment use try-only
/// acquisition. Non-RT peeking, accepted publication, completion and app capture
/// use synchronous locks and may block, including when called by an async task.
///
/// The legacy consuming helper is removed. Use [`Self::pending_update`],
/// publish only accepted state, complete the ticket, then read its result.
///
/// ```compile_fail,E0599
/// use ableton_link_rs::link::safe_rt_session_state::SafeRtSessionStateHandler;
///
/// fn legacy_drain(state: &SafeRtSessionStateHandler) {
///     while state.process_pending_updates().is_some() {}
/// }
/// ```
pub struct SafeRtSessionStateHandler {
    queue_id: Option<core::num::NonZeroU64>,
    accepted: AcceptedState,
    has_pending_updates: AtomicBool,
    pending: Mutex<PendingUpdate>,
}

impl SafeRtSessionStateHandler {
    pub fn new(initial_state: ClientState) -> Self {
        Self {
            queue_id: super::sessions::new_edit_revision(),
            accepted: AcceptedState::new(initial_state),
            has_pending_updates: AtomicBool::new(false),
            pending: Mutex::new(PendingUpdate::default()),
        }
    }

    /// Attempt one reader pin using try-only locks and no allocation.
    /// On contention, skip this capture or reuse a caller-owned accepted value;
    /// do not retry on the audio thread. An error may follow a pending flush.
    pub fn get_rt_client_state(&self) -> Result<ClientState, AudioCaptureError> {
        self.accepted.read()
    }

    pub(crate) fn get_app_client_state(&self) -> ClientState {
        self.accepted.read_app()
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
        let queue = self.queue_id.ok_or(AudioQueueError::TicketExhausted)?;
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
        Ok(AudioCommitTicket {
            queue,
            sequence: pending.sequence,
        })
    }

    /// Peek without consuming: cancellation of a non-RT drain leaves the request queued.
    /// Validate, publish accepted state, and complete its ticket before advancing.
    /// Read its result with `take_commit_result` before reusing the queue slot.
    /// This non-RT operation may block on a synchronous mutex.
    pub fn pending_update(&self) -> Option<(AudioCommitTicket, IncomingClientState)> {
        let queue = self.queue_id?;
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.incoming.map(|incoming| {
            (
                AudioCommitTicket {
                    queue,
                    sequence: pending.sequence,
                },
                incoming,
            )
        })
    }

    pub(crate) fn pending_update_from_origin(
        &self,
    ) -> Option<(
        AudioCommitTicket,
        IncomingClientState,
        Option<super::sessions::SessionEditOrigin>,
    )> {
        let queue = self.queue_id?;
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.incoming.map(|incoming| {
            (
                AudioCommitTicket {
                    queue,
                    sequence: pending.sequence,
                },
                incoming,
                pending.origin,
            )
        })
    }

    /// Publish only a controller-accepted snapshot, never an unvalidated proposal.
    /// This non-RT operation may block on the accepted-publication mutex.
    /// The caller owns snapshot ordering: no version comparison prevents an older
    /// snapshot published later from replacing newer cache contents. Serialize
    /// sampling and publication across publishers, or reject superseded snapshots.
    pub fn publish_client_state(&self, accepted: ClientState) {
        self.accepted.publish(accepted);
    }

    /// Record a terminal result. This non-RT operation may block on the queue mutex.
    pub fn complete_pending_update(
        &self,
        ticket: AudioCommitTicket,
        result: Result<(), CommitError>,
    ) -> bool {
        if self.queue_id != Some(ticket.queue) {
            return false;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if ticket.sequence != pending.sequence || pending.incoming.is_none() {
            return false;
        }
        pending.incoming = None;
        pending.origin = None;
        pending.result = Some(result);
        self.has_pending_updates.store(false, Ordering::Release);
        true
    }

    /// Non-blocking acknowledgment; a completed result occupies the slot until read.
    /// `Busy` means the lock is unavailable, not confirmation that this ticket is valid
    /// or has a result. Nothing was consumed; retain the ticket and retry later.
    pub fn take_commit_result(
        &self,
        ticket: AudioCommitTicket,
    ) -> Result<Option<Result<(), CommitError>>, AudioQueueError> {
        if self.queue_id != Some(ticket.queue) {
            return Err(AudioQueueError::UnknownTicket);
        }
        let mut pending = self.pending.try_lock().map_err(|_| AudioQueueError::Busy)?;
        if ticket.sequence != pending.sequence {
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

    /// Recover a copy of the pending or completed request's ticket.
    /// Uses one try-only lock without allocation, processing, or acknowledgment.
    /// `None` means the slot was empty; `Busy` means its lock was unavailable.
    /// Retain the returned ticket and use `take_commit_result` to acknowledge
    /// completion. Another consumer may acknowledge it first, making it stale.
    pub fn outstanding_commit_ticket(&self) -> Result<Option<AudioCommitTicket>, AudioQueueError> {
        let Some(queue) = self.queue_id else {
            return Ok(None);
        };
        let pending = self.pending.try_lock().map_err(|_| AudioQueueError::Busy)?;
        Ok(
            (pending.incoming.is_some() || pending.result.is_some()).then_some(AudioCommitTicket {
                queue,
                sequence: pending.sequence,
            }),
        )
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
    fn ordering_accepted_capture_bounds_pin_attempts_under_competing_readers() {
        let accepted = AcceptedState::new(create_test_client_state());
        std::thread::scope(|scope| {
            let (commands, receive_commands) = std::sync::mpsc::channel();
            let (finished, receive_finished) = std::sync::mpsc::channel();
            let peer = &accepted;
            scope.spawn(move || {
                let mut pin = None;
                while let Ok(should_pin) = receive_commands.recv() {
                    if should_pin {
                        assert!(pin.is_none());
                        peer.readers.fetch_add(1, Ordering::SeqCst);
                        pin = Some(AcceptedRead(peer));
                    } else {
                        drop(pin.take());
                    }
                    finished.send(()).unwrap();
                }
            });
            let mut attempts = 0;
            let capture = accepted.read_with_pin_hook(|| {
                attempts += 1;
                if attempts <= 64 {
                    commands.send(attempts % 2 == 1).unwrap();
                    receive_finished.recv().unwrap();
                }
            });
            commands.send(false).unwrap();
            receive_finished.recv().unwrap();
            drop(commands);
            assert_eq!(accepted.readers.load(Ordering::SeqCst), 0);
            assert_eq!(attempts, 1, "an audio capture must not retry its pin");
            assert_eq!(capture, Err(AudioCaptureError::Busy));
        });
    }

    #[test]
    fn ordering_accepted_capture_rejects_reader_exhaustion_without_wrapping() {
        let initial = create_test_client_state();
        let accepted = AcceptedState::new(initial);
        accepted.readers.store(usize::MAX, Ordering::SeqCst);
        assert_eq!(accepted.read(), Err(AudioCaptureError::ReaderExhausted));
        assert_eq!(accepted.readers.load(Ordering::SeqCst), usize::MAX);
        accepted.readers.store(0, Ordering::SeqCst);
        assert_eq!(accepted.read(), Ok(initial));
    }

    #[test]
    fn ordering_accepted_capture_preserves_complete_snapshots_while_pinned() {
        let initial = create_test_client_state();
        let accepted = AcceptedState::new(initial);
        accepted.readers.fetch_add(1, Ordering::SeqCst);
        let pin = AcceptedRead(&accepted);
        let mut next = initial;
        next.timeline.tempo = Tempo::new(150.0);
        next.timeline.beat_origin = Beats::new(16.0);
        next.timeline.time_origin = Duration::seconds(4);
        next.start_stop_state.is_playing = true;
        next.start_stop_state.timestamp = Duration::seconds(5);
        accepted.publish(next);
        assert_eq!(accepted.read(), Ok(initial));
        next.timeline.beat_origin = Beats::new(32.0);
        next.start_stop_state.time = Duration::seconds(6);
        accepted.publish(next);
        assert_eq!(accepted.read(), Ok(initial));
        assert_eq!(accepted.read_app(), initial);
        assert_eq!(*accepted.pending.lock().unwrap(), Some(next));
        drop(pin);
        assert_eq!(accepted.read_app(), next);
        assert_eq!(accepted.read(), Ok(next));
    }

    #[test]
    fn ordering_foreign_ticket_cannot_complete_or_acknowledge_another_queue() {
        let first = SafeRtSessionStateHandler::new(create_test_client_state());
        let second = SafeRtSessionStateHandler::new(create_test_client_state());
        let incoming = IncomingClientState {
            timeline: None,
            start_stop_state: None,
            timeline_timestamp: Duration::zero(),
        };
        let first_ticket = first.update_rt_client_state(incoming).unwrap();
        let second_ticket = second.update_rt_client_state(incoming).unwrap();
        assert_ne!(first_ticket, second_ticket);
        assert!(!second.complete_pending_update(first_ticket, Ok(())));
        assert_eq!(second.pending_update(), Some((second_ticket, incoming)));
        assert!(second.complete_pending_update(second_ticket, Ok(())));
        assert_eq!(
            second.take_commit_result(first_ticket),
            Err(AudioQueueError::UnknownTicket)
        );
        assert_eq!(second.take_commit_result(second_ticket), Ok(Some(Ok(()))));
        assert!(first.complete_pending_update(first_ticket, Ok(())));
        assert_eq!(first.take_commit_result(first_ticket), Ok(Some(Ok(()))));
    }

    #[test]
    fn ordering_exhausted_queue_identity_and_sequence_fail_closed() {
        let mut handler = SafeRtSessionStateHandler::new(create_test_client_state());
        let incoming = IncomingClientState {
            timeline: None,
            start_stop_state: None,
            timeline_timestamp: Duration::zero(),
        };
        handler.pending.lock().unwrap().sequence = u64::MAX;
        assert_eq!(
            handler.update_rt_client_state(incoming),
            Err(AudioQueueError::TicketExhausted)
        );
        assert_eq!(handler.outstanding_commit_ticket(), Ok(None));
        handler.pending.lock().unwrap().sequence = 0;
        handler.queue_id = None;
        assert_eq!(
            handler.update_rt_client_state(incoming),
            Err(AudioQueueError::TicketExhausted)
        );
        assert!(!handler.has_pending_updates());
        assert_eq!(handler.outstanding_commit_ticket(), Ok(None));
    }

    #[test]
    fn ordering_lost_ticket_recovery_preserves_result_and_queue_capacity() {
        for result in [Ok(()), Err(CommitError::TimelineOriginExhausted)] {
            let initial = create_test_client_state();
            let handler = SafeRtSessionStateHandler::new(initial);
            let mut accepted = initial;
            accepted.timeline.tempo = Tempo::new(140.0);
            let incoming = IncomingClientState {
                timeline: Some(accepted.timeline),
                start_stop_state: None,
                timeline_timestamp: Duration::zero(),
            };
            assert_eq!(handler.outstanding_commit_ticket(), Ok(None));
            handler.update_rt_client_state(incoming).unwrap();
            {
                let (ticket, proposal) = handler.pending_update().unwrap();
                assert_eq!(proposal, incoming);
                assert_eq!(handler.outstanding_commit_ticket(), Ok(Some(ticket)));
                assert_eq!(handler.pending_update(), Some((ticket, incoming)));
                assert_eq!(handler.take_commit_result(ticket), Ok(None));
                if result.is_ok() {
                    handler.publish_client_state(accepted);
                }
                assert!(handler.complete_pending_update(ticket, result));
            }
            assert!(!handler.has_pending_updates());
            assert_eq!(handler.pending_update(), None);
            assert_eq!(
                handler.update_rt_client_state(incoming),
                Err(AudioQueueError::Busy)
            );
            let recovered = handler.outstanding_commit_ticket().unwrap().unwrap();
            assert_eq!(handler.outstanding_commit_ticket(), Ok(Some(recovered)));
            assert_eq!(
                handler.update_rt_client_state(incoming),
                Err(AudioQueueError::Busy)
            );
            assert_eq!(
                handler.get_rt_client_state(),
                Ok(if result.is_ok() { accepted } else { initial })
            );
            assert_eq!(handler.take_commit_result(recovered), Ok(Some(result)));
            assert_eq!(handler.outstanding_commit_ticket(), Ok(None));
            let next = handler.update_rt_client_state(incoming).unwrap();
            assert_ne!(next, recovered);
            assert_eq!(
                handler.take_commit_result(recovered),
                Err(AudioQueueError::UnknownTicket)
            );
            assert_eq!(handler.pending_update(), Some((next, incoming)));
        }
    }

    #[test]
    fn ordering_ticket_lookup_contention_does_not_consume_the_request() {
        let handler = SafeRtSessionStateHandler::new(create_test_client_state());
        let incoming = IncomingClientState {
            timeline: None,
            start_stop_state: None,
            timeline_timestamp: Duration::zero(),
        };
        let ticket = handler.update_rt_client_state(incoming).unwrap();
        let guard = handler.pending.lock().unwrap();
        assert_eq!(
            handler.outstanding_commit_ticket(),
            Err(AudioQueueError::Busy)
        );
        drop(guard);
        assert_eq!(handler.outstanding_commit_ticket(), Ok(Some(ticket)));
        assert_eq!(handler.pending_update(), Some((ticket, incoming)));
        assert!(handler.complete_pending_update(ticket, Ok(())));
        assert_eq!(handler.take_commit_result(ticket), Ok(Some(Ok(()))));
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

        let retrieved_state = handler.get_rt_client_state().unwrap();

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
        assert_eq!(handler.get_rt_client_state(), Ok(client_state));

        // Should have pending updates
        assert!(handler.has_pending_updates());

        let processed = handler.pending_update();
        assert!(processed.is_some());

        let (processed_ticket, processed_state) = processed.unwrap();
        assert_eq!(processed_ticket, ticket);
        assert!(processed_state.timeline.is_some());
        assert_eq!(processed_state.timeline.unwrap().tempo.bpm(), 140.0);
        assert_eq!(processed_state, incoming_state);
        assert_eq!(handler.pending_update(), Some((ticket, incoming_state)));
        assert_eq!(
            handler.update_rt_client_state(incoming_state),
            Err(AudioQueueError::Busy)
        );
        assert!(handler.complete_pending_update(ticket, Err(CommitError::TimelineOriginExhausted)));
        assert_eq!(handler.pending_update(), None);
        assert_eq!(handler.get_rt_client_state(), Ok(client_state));
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
        assert_eq!(
            handler.get_rt_client_state().unwrap().timeline,
            new_timeline
        );
    }

    #[test]
    fn ordering_accepted_snapshot_survives_preempted_publication() {
        let client_state = create_test_client_state();
        let handler = SafeRtSessionStateHandler::new(client_state);
        let mut accepted = client_state;
        accepted.timeline.tempo = Tempo::new(150.0);
        handler.publish_client_state(accepted);
        assert_eq!(handler.get_rt_client_state(), Ok(accepted));
        let mut pending = handler.accepted.pending.lock().unwrap();
        let next = 1 - handler.accepted.published.load(Ordering::SeqCst);
        let slot = handler.accepted.slots[next].write().unwrap();
        let mut newer = accepted;
        newer.timeline.tempo = Tempo::new(180.0);
        *pending = Some(newer);
        for _ in 0..3 {
            assert_eq!(handler.get_rt_client_state(), Ok(accepted));
        }
        drop(slot);
        drop(pending);
        assert_eq!(handler.get_rt_client_state(), Ok(newer));
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
            assert_eq!(handler.pending_update(), Some((ticket, incoming_state)));
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
        let final_state = handler.get_rt_client_state().unwrap();
        assert_eq!(final_state.timeline.tempo.bpm(), 169.0);
    }
}
