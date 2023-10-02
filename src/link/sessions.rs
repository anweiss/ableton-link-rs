use std::{
    fmt::{self, Display},
    mem,
    sync::{Arc, Mutex},
};

use bincode::{Decode, Encode};
use chrono::Duration;
use tokio::sync::mpsc::Sender;
use tracing::debug;

use crate::discovery::{
    peers::{ControllerPeer, PeerState},
    ENCODING_CONFIG,
};

use super::{
    ghostxform::GhostXForm,
    node::NodeId,
    payload::PayloadEntryHeader,
    state::{ClientStartStopState, StartStopState},
    timeline::Timeline,
    Result,
};

pub const SESSION_MEMBERSHIP_HEADER_KEY: u32 = u32::from_be_bytes(*b"sess");
pub const SESSION_MEMBERSHIP_SIZE: u32 = mem::size_of::<SessionId>() as u32;
pub const SESSION_MEMBERSHIP_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: SESSION_MEMBERSHIP_HEADER_KEY,
    size: SESSION_MEMBERSHIP_SIZE,
};

#[derive(Clone, Copy, Debug, Encode, Decode, Default, PartialEq, Eq)]
pub struct SessionId(pub NodeId);

impl Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, Encode, Decode)]
pub struct SessionMembership {
    pub session_id: SessionId,
}

impl From<SessionId> for SessionMembership {
    fn from(session_id: SessionId) -> Self {
        SessionMembership { session_id }
    }
}

impl SessionMembership {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = SESSION_MEMBERSHIP_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(
            self.session_id,
            ENCODING_CONFIG,
        )?);
        Ok(encoded)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionMeasurement {
    pub x_form: GhostXForm,
    pub timestamp: Duration,
}

impl Default for SessionMeasurement {
    fn default() -> Self {
        Self {
            x_form: GhostXForm::default(),
            timestamp: Duration::zero(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub session_id: Arc<Mutex<SessionId>>,
    pub timeline: Timeline,
    pub measurement: SessionMeasurement,
}

pub struct Sessions {
    pub sessions: Arc<Mutex<Vec<Session>>>,
    pub current: Option<Arc<Mutex<SessionId>>>,
    pub tx_measure_peer: Sender<PeerState>,
    pub peers: Arc<Mutex<Vec<ControllerPeer>>>,
}

impl Sessions {
    pub fn new(
        init: Session,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        tx_measure_peer: Sender<PeerState>,
    ) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(vec![init.clone()])),
            current: Some(init.session_id.clone()),
            tx_measure_peer,
            peers,
        }
    }

    pub fn reset_session(&mut self, session: Session) {
        self.current = Some(session.session_id);
        self.sessions.lock().unwrap().clear()
    }

    pub fn reset_timeline(&self, timeline: Timeline) {
        if let Some(current) = &self.current {
            if let Some(session) = self
                .sessions
                .lock()
                .unwrap()
                .iter_mut()
                .find(|s| *s.session_id.lock().unwrap() == *current.lock().unwrap())
            {
                session.timeline = timeline;
            }
        }
    }

    pub fn saw_session_timeline(
        &self,
        session_id: Arc<Mutex<SessionId>>,
        timeline: Timeline,
    ) -> Option<Timeline> {
        if let Some(current) = &self.current {
            if *current.lock().unwrap() == *session_id.lock().unwrap() {
                self.update_timeline(current.clone(), timeline);
            } else {
                let session = Session {
                    session_id: session_id.clone(),
                    timeline,
                    measurement: SessionMeasurement {
                        x_form: GhostXForm::default(),
                        timestamp: Duration::zero(),
                    },
                };

                if self
                    .sessions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|s| *s.session_id.lock().unwrap() == *session_id.lock().unwrap())
                {
                    self.update_timeline(session.session_id.clone(), timeline);
                } else {
                    self.launch_session_measurement(&session);
                    self.sessions.lock().unwrap().push(session);
                }
            }

            return self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .find(|s| *s.session_id.lock().unwrap() == *session_id.lock().unwrap())
                .map(|s| s.timeline);
        }

        None
    }

    pub fn update_timeline(&self, session_id: Arc<Mutex<SessionId>>, timeline: Timeline) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap()
            .iter_mut()
            .find(|s| *s.session_id.lock().unwrap() == *session_id.lock().unwrap())
        {
            if timeline.beat_origin > session.timeline.beat_origin {
                debug!(
                    "adopting peer timeline ({}, {}, {})",
                    timeline.tempo.bpm(),
                    timeline.beat_origin.floating(),
                    timeline.time_origin.num_microseconds().unwrap(),
                );

                session.timeline = timeline;
            } else {
                debug!("rejecting peer timeline with beat origin: {}. current timeline beat origin: {}",
                    timeline.beat_origin.floating(),
                    session.timeline.beat_origin.floating(),
                );
            }
        }
    }

    pub fn launch_session_measurement(&self, _session: &Session) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key() {
        assert_eq!(SESSION_MEMBERSHIP_HEADER_KEY, 0x73657373);
    }
}
