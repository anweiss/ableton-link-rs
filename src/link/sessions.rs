use std::{
    fmt::{self, Display},
    mem,
    sync::{Arc, Mutex},
};

use bincode::{Decode, Encode};
use chrono::Duration;
use tokio::{
    select,
    sync::{
        mpsc::{Receiver, Sender},
        Notify,
    },
};
use tracing::{debug, info};

use crate::discovery::{
    peers::{ControllerPeer, PeerState},
    ENCODING_CONFIG,
};

use super::{
    ghostxform::GhostXForm,
    measurement::MeasurePeerEvent,
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
    pub session_id: SessionId,
    pub timeline: Timeline,
    pub measurement: SessionMeasurement,
}

#[derive(Clone)]
pub struct Sessions {
    pub sessions: Arc<Mutex<Vec<Session>>>,
    pub current: Option<Arc<Mutex<SessionId>>>,
    pub tx_measure_peer: tokio::sync::broadcast::Sender<MeasurePeerEvent>,
    pub peers: Arc<Mutex<Vec<ControllerPeer>>>,
}

impl Sessions {
    pub fn new(
        init: Session,
        tx_measure_peer: tokio::sync::broadcast::Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        notifier: Arc<Notify>,
    ) -> Self {
        let mut rx_measure_peer = tx_measure_peer.subscribe();
        let sessions = Arc::new(Mutex::new(vec![init.clone()]));

        let sessions_loop = sessions.clone();

        tokio::spawn(async move {
            loop {
                select! {
                    Ok(measure_peer_event) = rx_measure_peer.recv() => {
                        if let MeasurePeerEvent::XForm(session_id, x_form) = measure_peer_event {
                            todo!();
                        }
                    }
                    // _ = notifier.notified() => {
                    //     break;
                    // }
                }
            }
        });

        Self {
            sessions,
            current: Some(Arc::new(Mutex::new(init.session_id.clone()))),
            tx_measure_peer,
            peers,
        }
    }

    pub fn reset_session(&mut self, session: Session) {
        self.current = Some(Arc::new(Mutex::new(session.session_id)));
        self.sessions.lock().unwrap().clear()
    }

    pub fn reset_timeline(&self, timeline: Timeline) {
        if let Some(current) = &self.current {
            if let Some(session) = self
                .sessions
                .lock()
                .unwrap()
                .iter_mut()
                .find(|s| s.session_id == *current.lock().unwrap())
            {
                session.timeline = timeline;
            }
        }
    }

    pub fn saw_session_timeline(
        &self,
        session_id: SessionId,
        timeline: Timeline,
    ) -> Option<Timeline> {
        info!(
            "saw session timeline {:?} for session {}",
            timeline, session_id,
        );

        if let Some(current) = &self.current {
            if *current.lock().unwrap() == session_id {
                self.update_timeline(*current.lock().unwrap(), timeline);
            } else {
                let mut session = Session {
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
                    .any(|s| s.session_id == session_id)
                {
                    self.update_timeline(session.session_id.clone(), timeline);
                } else {
                    self.launch_session_measurement(&mut session);
                    self.sessions.lock().unwrap().push(session);
                }
            }

            return self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.session_id == session_id)
                .map(|s| s.timeline);
        }

        None
    }

    pub fn update_timeline(&self, session_id: SessionId, timeline: Timeline) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap()
            .iter_mut()
            .find(|s| s.session_id == session_id)
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

    pub fn launch_session_measurement(&self, session: &mut Session) {
        info!("launching session measurement");

        let peers = session_peers(self.peers.clone(), session.session_id.clone());

        info!("peers {:?}", peers);

        if let Some(p) = peers
            .iter()
            .find(|p| p.peer_state.ident() == session.session_id.0)
        {
            session.measurement.timestamp = Duration::zero();
            self.tx_measure_peer
                .send(MeasurePeerEvent::PeerState(
                    session.session_id,
                    p.peer_state.clone(),
                ))
                .unwrap();
        } else if let Some(p) = peers.first() {
            session.measurement.timestamp = Duration::zero();
            self.tx_measure_peer
                .send(MeasurePeerEvent::PeerState(
                    session.session_id,
                    p.peer_state.clone(),
                ))
                .unwrap();
        }
    }
}

pub fn session_peers(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_id: SessionId,
) -> Vec<ControllerPeer> {
    let mut peers = peers
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.peer_state.session_id() == session_id)
        .cloned()
        .collect::<Vec<_>>();
    peers.sort_by(|a, b| a.peer_state.ident().cmp(&b.peer_state.ident()));

    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key() {
        assert_eq!(SESSION_MEMBERSHIP_HEADER_KEY, 0x73657373);
    }
}
