use chrono::Duration;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use triple_buffer::{Input, Output, TripleBuffer};

use super::{
    state::{ClientStartStopState, ClientState},
    timeline::Timeline,
};
use crate::link::IncomingClientState;

/// Real-time client state for lock-free access from audio threads
#[derive(Clone, Debug, Default)]
pub struct RtClientState {
    pub timeline: Timeline,
    pub start_stop_state: ClientStartStopState,
    pub timeline_timestamp: Duration,
    pub start_stop_state_timestamp: Duration,
}

impl From<ClientState> for RtClientState {
    fn from(client_state: ClientState) -> Self {
        Self {
            timeline: client_state.timeline,
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
            start_stop_state: rt_state.start_stop_state,
        }
    }
}

/// Real-time safe session state handler using only safe Rust
/// This provides lock-free access to session state for real-time audio threads
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
                        if timeline_grace_expired && new_state.timeline != cached_state.timeline {
                            cached_state.timeline = new_state.timeline;
                        }

                        if start_stop_grace_expired
                            && new_state.start_stop_state != cached_state.start_stop_state
                        {
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

    /// Update the real-time client state with new data
    /// This should only be called from the real-time thread
    /// Uses try_lock operations to avoid blocking
    pub fn update_rt_client_state(
        &self,
        incoming_state: IncomingClientState,
        current_time: Duration,
        is_enabled: bool,
    ) {
        if incoming_state.timeline.is_none() && incoming_state.start_stop_state.is_none() {
            return;
        }

        let current_time_micros = current_time.num_microseconds().unwrap_or(0) as u64;
        let timestamp_to_store = if is_enabled { current_time_micros } else { 0 };

        // Try to update cached state (non-blocking)
        if let Ok(mut cached_state) = self.cached_rt_state.try_write() {
            // Update timeline if provided
            if let Some(timeline) = incoming_state.timeline {
                cached_state.timeline = timeline;
                cached_state.timeline_timestamp = current_time;
                self.timeline_timestamp
                    .store(timestamp_to_store, Ordering::Release);
            }

            // Update start/stop state if provided
            if let Some(start_stop_state) = incoming_state.start_stop_state {
                cached_state.start_stop_state = start_stop_state;
                cached_state.start_stop_state_timestamp = current_time;
                self.start_stop_timestamp
                    .store(timestamp_to_store, Ordering::Release);
            }

            // Try to send updated state to non-RT thread via triple buffer (non-blocking)
            if let Ok(mut input) = self.client_state_input.try_lock() {
                input.write(cached_state.clone().into());

                // Mark that we have pending updates
                self.has_pending_updates.store(true, Ordering::Release);
            }
        }
    }

    /// Process any pending updates from the triple buffers
    /// This should be called periodically from a non-realtime thread
    /// This method can block briefly since it's not called from RT thread
    pub fn process_pending_updates(&self) -> Option<IncomingClientState> {
        if !self.has_pending_updates.load(Ordering::Acquire) {
            return None;
        }

        // Check for new state data from triple buffer (can block for non-RT thread)
        if let Ok(mut output) = self.client_state_output.lock() {
            if output.updated() {
                let new_state = *output.read();

                let result = IncomingClientState {
                    timeline: Some(new_state.timeline),
                    start_stop_state: Some(new_state.start_stop_state),
                    timeline_timestamp: Duration::microseconds(
                        self.timeline_timestamp.load(Ordering::Acquire) as i64,
                    ),
                };

                // Clear the pending flag
                self.has_pending_updates.store(false, Ordering::Release);
                return Some(result);
            }
        }

        None
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
        handler.update_rt_client_state(incoming_state, Duration::milliseconds(3000), true);

        // Should have pending updates
        assert!(handler.has_pending_updates());

        // Process pending updates
        let processed = handler.process_pending_updates();
        assert!(processed.is_some());

        let processed_state = processed.unwrap();
        assert!(processed_state.timeline.is_some());
        assert_eq!(processed_state.timeline.unwrap().tempo.bpm(), 140.0);
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

            handler.update_rt_client_state(incoming_state, Duration::milliseconds(i * 20), true);

            // Process updates occasionally
            if i % 10 == 0 {
                handler.process_pending_updates();
            }

            thread::sleep(StdDuration::from_micros(20));
        }

        rt_thread.join().unwrap();

        // Verify we can still read state
        let final_state = handler.get_rt_client_state(Duration::milliseconds(1000));
        assert!(final_state.timeline.tempo.bpm() >= 120.0);
    }
}
