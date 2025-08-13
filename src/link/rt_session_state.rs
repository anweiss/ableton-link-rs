use chrono::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
use triple_buffer::{TripleBuffer, Input, Output};

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

/// Real-time safe session state handler
/// This provides lock-free access to session state for real-time audio threads
/// while allowing safe updates from other threads.
pub struct RtSessionStateHandler {
    /// Real-time client state accessible without blocking
    rt_client_state: RtClientState,
    
    /// Triple buffer for timeline updates (split into input/output)
    timeline_input: Input<(Duration, Timeline)>,
    timeline_output: Output<(Duration, Timeline)>,
    
    /// Triple buffer for start/stop state updates (split into input/output)
    start_stop_input: Input<ClientStartStopState>,
    start_stop_output: Output<ClientStartStopState>,
    
    /// Flag indicating pending updates
    has_pending_updates: AtomicBool,
    
    /// Grace period for local modifications
    local_mod_grace_period: Duration,
}

impl RtSessionStateHandler {
    pub fn new(initial_state: ClientState, grace_period: Duration) -> Self {
        // Create default values for the buffers
        let default_timeline_entry = (Duration::zero(), initial_state.timeline);
        let default_start_stop_state = initial_state.start_stop_state;
        
        // Create the triple buffers and split them
        let timeline_buffer = TripleBuffer::new(&default_timeline_entry);
        let (timeline_input, timeline_output) = timeline_buffer.split();
        
        let start_stop_buffer = TripleBuffer::new(&default_start_stop_state);
        let (start_stop_input, start_stop_output) = start_stop_buffer.split();
        
        Self {
            rt_client_state: initial_state.into(),
            timeline_input,
            timeline_output,
            start_stop_input,
            start_stop_output,
            has_pending_updates: AtomicBool::new(false),
            local_mod_grace_period: grace_period,
        }
    }

    /// Get the current real-time client state
    /// This is lock-free and safe to call from audio threads
    pub fn get_rt_client_state(&self, current_time: Duration) -> ClientState {
        // Check if grace period has expired for timeline
        let timeline_grace_expired = 
            current_time - self.rt_client_state.timeline_timestamp > self.local_mod_grace_period;
            
        // Check if grace period has expired for start/stop state
        let start_stop_grace_expired = 
            current_time - self.rt_client_state.start_stop_state_timestamp > self.local_mod_grace_period;

        // Only update if grace periods have expired to avoid disrupting local modifications
        let updated_state = self.rt_client_state.clone();

        if timeline_grace_expired {
            // Try to get new timeline data without blocking
            // This would require a mutable reference, so we'd need a different approach
            // For now, we'll return the cached state
        }

        if start_stop_grace_expired {
            // Try to get new start/stop state without blocking
            // This would require a mutable reference, so we'd need a different approach
            // For now, we'll return the cached state
        }

        updated_state.into()
    }

    /// Update the real-time client state with new data
    /// This should only be called from the real-time thread
    pub fn update_rt_client_state(
        &mut self,
        incoming_state: IncomingClientState,
        current_time: Duration,
        is_enabled: bool,
    ) {
        if incoming_state.timeline.is_none() && incoming_state.start_stop_state.is_none() {
            return;
        }

        // Update timeline if provided
        if let Some(timeline) = incoming_state.timeline {
            self.timeline_input.write((incoming_state.timeline_timestamp, timeline));
            self.rt_client_state.timeline = timeline;
            self.rt_client_state.timeline_timestamp = if is_enabled {
                current_time
            } else {
                Duration::zero()
            };
        }

        // Update start/stop state if provided
        if let Some(start_stop_state) = incoming_state.start_stop_state {
            self.start_stop_input.write(start_stop_state);
            self.rt_client_state.start_stop_state = start_stop_state;
            self.rt_client_state.start_stop_state_timestamp = if is_enabled {
                current_time
            } else {
                Duration::zero()
            };
        }

        // Mark that we have pending updates
        self.has_pending_updates.store(true, Ordering::Release);
    }

    /// Process any pending updates from the triple buffers
    /// This should be called periodically from a non-realtime thread
    pub fn process_pending_updates(&mut self) -> Option<IncomingClientState> {
        if !self.has_pending_updates.load(Ordering::Acquire) {
            return None;
        }

        let mut result = IncomingClientState {
            timeline: None,
            start_stop_state: None,
            timeline_timestamp: Duration::zero(),
        };

        // Check for new timeline data
        if self.timeline_output.update() {
            let (timestamp, timeline) = *self.timeline_output.read();
            result.timeline = Some(timeline);
            result.timeline_timestamp = timestamp;
        }

        // Check for new start/stop state data
        if self.start_stop_output.update() {
            let start_stop_state = *self.start_stop_output.read();
            result.start_stop_state = Some(start_stop_state);
        }

        // Clear the pending flag if we processed any updates
        if result.timeline.is_some() || result.start_stop_state.is_some() {
            self.has_pending_updates.store(false, Ordering::Release);
            Some(result)
        } else {
            None
        }
    }

    /// Check if there are pending updates waiting to be processed
    pub fn has_pending_updates(&self) -> bool {
        self.has_pending_updates.load(Ordering::Acquire)
    }

    /// Get the current grace period
    pub fn grace_period(&self) -> Duration {
        self.local_mod_grace_period
    }

    /// Set a new grace period for local modifications
    pub fn set_grace_period(&mut self, grace_period: Duration) {
        self.local_mod_grace_period = grace_period;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{
        beats::Beats,
        tempo::Tempo,
    };

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
    fn test_rt_session_state_handler_creation() {
        let client_state = create_test_client_state();
        let handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));
        
        assert_eq!(handler.grace_period(), Duration::milliseconds(1000));
        assert!(!handler.has_pending_updates());
    }

    #[test]
    fn test_rt_session_state_handler_get_state() {
        let client_state = create_test_client_state();
        let handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));
        
        let current_time = Duration::milliseconds(500);
        let retrieved_state = handler.get_rt_client_state(current_time);
        
        assert_eq!(retrieved_state.timeline.tempo.bpm(), client_state.timeline.tempo.bpm());
        assert_eq!(retrieved_state.start_stop_state.is_playing, client_state.start_stop_state.is_playing);
    }

    #[test]
    fn test_rt_session_state_handler_updates() {
        let client_state = create_test_client_state();
        let mut handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));
        
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
    fn test_rt_session_state_handler_grace_period() {
        let client_state = create_test_client_state();
        let mut handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));
        
        // Change grace period
        handler.set_grace_period(Duration::milliseconds(2000));
        assert_eq!(handler.grace_period(), Duration::milliseconds(2000));
    }

    #[test]
    fn test_rt_client_state_conversion() {
        let client_state = create_test_client_state();
        let rt_state: RtClientState = client_state.into();
        let converted_back: ClientState = rt_state.into();
        
        assert_eq!(client_state.timeline.tempo.bpm(), converted_back.timeline.tempo.bpm());
        assert_eq!(client_state.start_stop_state.is_playing, converted_back.start_stop_state.is_playing);
    }
}
