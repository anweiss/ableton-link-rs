use std::{
    fmt::{self, Display},
    mem,
    sync::{Arc, Mutex},
};

use bincode::{Decode, Encode};
use chrono::Duration;
use tokio::sync::Notify;
use tracing::{debug, info};

use crate::discovery::{peers::ControllerPeer, ENCODING_CONFIG};

use super::{
    clock::Clock, ghostxform::GhostXForm, measurement::MeasurePeerEvent, node::NodeId,
    payload::PayloadEntryHeader, timeline::Timeline, Result,
};

pub const SESSION_MEMBERSHIP_HEADER_KEY: u32 = u32::from_be_bytes(*b"sess");
pub const SESSION_MEMBERSHIP_SIZE: u32 = mem::size_of::<SessionId>() as u32;
pub const SESSION_MEMBERSHIP_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: SESSION_MEMBERSHIP_HEADER_KEY,
    size: SESSION_MEMBERSHIP_SIZE,
};

#[derive(Clone, Copy, Debug, Encode, Decode, Default, PartialEq, Eq, PartialOrd)]
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
    pub other_sessions: Arc<Mutex<Vec<Session>>>,
    pub current: Arc<Mutex<Session>>,
    pub tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    pub peers: Arc<Mutex<Vec<ControllerPeer>>>,
    pub clock: Clock,
}

impl Sessions {
    pub fn new(
        init: Session,
        tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        clock: Clock,
        tx_join_session: tokio::sync::mpsc::Sender<Session>,
        notifier: Arc<Notify>,
        mut rx_measure_peer_result: tokio::sync::mpsc::Receiver<MeasurePeerEvent>,
    ) -> Self {
        let other_sessions = Arc::new(Mutex::new(vec![init.clone()]));
        let current = Arc::new(Mutex::new(init));

        let other_sessions_loop = other_sessions.clone();
        let current_loop = current.clone();
        let tx_join_session_loop = tx_join_session.clone();
        let peers_loop = peers.clone();
        let tx_measure_peer_state_loop = tx_measure_peer_state.clone();

        let jh = tokio::spawn(async move {
            loop {
                if let Some(MeasurePeerEvent::XForm(session_id, x_form)) =
                    rx_measure_peer_result.recv().await
                {
                    if x_form == GhostXForm::default() {
                        handle_failed_measurement(
                            session_id,
                            other_sessions_loop.clone(),
                            current_loop.clone(),
                            peers_loop.clone(),
                            tx_measure_peer_state_loop.clone(),
                        )
                        .await;
                    } else {
                        handle_successful_measurement(
                            session_id,
                            x_form,
                            other_sessions_loop.clone(),
                            current_loop.clone(),
                            clock,
                            tx_join_session_loop.clone(),
                            peers_loop.clone(),
                            tx_measure_peer_state_loop.clone(),
                        )
                        .await;
                    }
                } else {
                    info!("measure peer event channel closed");
                }
            }
        });

        tokio::spawn(async move {
            notifier.notified().await;

            jh.abort();
        });

        Self {
            other_sessions,
            current,
            tx_measure_peer_state,
            peers,
            clock,
        }
    }

    pub fn reset_session(&mut self, session: Session) {
        *self.current.try_lock().unwrap() = session;
        self.other_sessions.try_lock().unwrap().clear()
    }

    pub fn reset_timeline(&self, timeline: Timeline) {
        if let Some(session) = self
            .other_sessions
            .try_lock()
            .unwrap()
            .iter_mut()
            .find(|s| s.session_id == self.current.try_lock().unwrap().session_id)
        {
            session.timeline = timeline;
        }
    }

    pub async fn saw_session_timeline(
        &self,
        session_id: SessionId,
        timeline: Timeline,
    ) -> Timeline {
        debug!(
            "saw session timeline {:?} for session {}",
            timeline, session_id,
        );

        if self.current.try_lock().unwrap().session_id == session_id {
            self.update_timeline(session_id, timeline);
        } else {
            let session = Session {
                session_id,
                timeline,
                measurement: SessionMeasurement {
                    x_form: GhostXForm::default(),
                    timestamp: Duration::zero(),
                },
            };

            let s = self
                .other_sessions
                .try_lock()
                .unwrap()
                .iter()
                .cloned()
                .find(|s| s.session_id == session_id);

            if let Some(s) = s {
                self.update_timeline(s.session_id, timeline);
            } else {
                info!("adding session {} to other sessions", session_id);
                self.other_sessions
                    .try_lock()
                    .unwrap()
                    .push(session.clone());

                launch_session_measurement(
                    self.peers.clone(),
                    self.tx_measure_peer_state.clone(),
                    session,
                )
                .await;
            }
        }

        self.current.try_lock().unwrap().timeline
    }

    pub fn update_timeline(&self, session_id: SessionId, timeline: Timeline) {
        if self.current.try_lock().unwrap().session_id == session_id {
            if timeline.beat_origin >= self.current.try_lock().unwrap().timeline.beat_origin {
                info!(
                    "adopting peer timeline ({}, {}, {})",
                    timeline.tempo.bpm(),
                    timeline.beat_origin.floating(),
                    timeline.time_origin,
                );
                self.current.try_lock().unwrap().timeline = timeline;
            } else {
                info!("rejecting peer timeline with beat origin: {}. current timeline beat origin: {}", timeline.beat_origin.floating(), self.current.try_lock().unwrap().timeline.beat_origin.floating());
            }
        } else if let Some(session) = self
            .other_sessions
            .try_lock()
            .unwrap()
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            if timeline.beat_origin > session.timeline.beat_origin {
                info!(
                    "adopting peer timeline ({}, {}, {})",
                    timeline.tempo.bpm(),
                    timeline.beat_origin.floating(),
                    timeline.time_origin,
                );
                session.timeline = timeline;
            } else {
                info!("rejecting peer timeline with beat origin: {}. current timeline beat origin: {}", timeline.beat_origin.floating(), session.timeline.beat_origin.floating());
            }
        }
    }
}

pub async fn launch_session_measurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    mut session: Session,
) {
    info!("launching session measurement");

    let peers = session_peers(peers.clone(), session.session_id);

    if let Some(p) = peers
        .iter()
        .find(|p| p.peer_state.ident() == session.session_id.0)
    {
        session.measurement.timestamp = Duration::zero();
        tx_measure_peer_state
            .send(MeasurePeerEvent::PeerState(
                session.session_id,
                p.peer_state.clone(),
            ))
            .await
            .unwrap();
    } else if let Some(p) = peers.first() {
        session.measurement.timestamp = Duration::zero();
        tx_measure_peer_state
            .send(MeasurePeerEvent::PeerState(
                session.session_id,
                p.peer_state.clone(),
            ))
            .await
            .unwrap();
    }
}

pub async fn handle_successful_measurement(
    session_id: SessionId,
    x_form: GhostXForm,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    clock: Clock,
    tx_join_session: tokio::sync::mpsc::Sender<Session>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
) {
    info!(
        "session {} measurement completed with result ({}, {})",
        session_id,
        x_form.slope,
        x_form.intercept.num_microseconds().unwrap(),
    );

    let measurement = SessionMeasurement {
        x_form,
        timestamp: clock.micros(),
    };

    if current.try_lock().unwrap().session_id == session_id {
        current.try_lock().unwrap().measurement = measurement;
        let session = current.try_lock().unwrap().clone();
        tx_join_session.send(session).await.unwrap();
    } else {
        let s = other_sessions
            .try_lock()
            .unwrap()
            .iter()
            .cloned()
            .enumerate()
            .find(|(_, s)| s.session_id == session_id);

        if let Some((idx, mut s)) = s {
            const SESSION_EPS: Duration = Duration::microseconds(500000);

            let host_time = clock.micros();
            let cur_ghost = current
                .try_lock()
                .unwrap()
                .measurement
                .x_form
                .host_to_ghost(host_time);
            let new_ghost = measurement.x_form.host_to_ghost(host_time);

            s.measurement = measurement;
            other_sessions.try_lock().unwrap()[idx] = s.clone();

            let ghost_diff = new_ghost - cur_ghost;

            info!("ghost_diff {:?}", ghost_diff);
            info!("session EPS {:?}", SESSION_EPS);

            if ghost_diff.abs() > SESSION_EPS
                || (ghost_diff.abs().num_microseconds().unwrap()
                    < SESSION_EPS.num_microseconds().unwrap()
                    && session_id != current.try_lock().unwrap().session_id)
            {
                let c = current.try_lock().unwrap().clone();
                *current.try_lock().unwrap() = s.clone();
                other_sessions.try_lock().unwrap().remove(idx);
                other_sessions.try_lock().unwrap().insert(idx, c);

                tx_join_session.send(s.clone()).await.unwrap();

                schedule_remeasurement(peers.clone(), tx_measure_peer_state.clone(), s).await;
            }
        }
    }
}

pub async fn handle_failed_measurement(
    session_id: SessionId,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
) {
    info!("session {} measurement failed", session_id);

    if current.try_lock().unwrap().session_id == session_id {
        let current = current.try_lock().unwrap().clone();
        schedule_remeasurement(peers, tx_measure_peer, current).await;
    } else {
        let s = other_sessions
            .try_lock()
            .unwrap()
            .iter()
            .cloned()
            .enumerate()
            .find(|(_, s)| s.session_id != session_id);

        if let Some((idx, _)) = s {
            other_sessions.try_lock().unwrap().remove(idx);

            let p = peers
                .try_lock()
                .unwrap()
                .iter()
                .cloned()
                .enumerate()
                .filter(|(_, p)| p.peer_state.session_id() == session_id)
                .collect::<Vec<_>>();

            for (idx, _) in p {
                peers.try_lock().unwrap().remove(idx);
            }
        }
    }
}

pub async fn schedule_remeasurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::microseconds(30000000).to_std().unwrap()).await;
            launch_session_measurement(peers.clone(), tx_measure_peer.clone(), session.clone())
                .await;
        }
    });
}

pub fn session_peers(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_id: SessionId,
) -> Vec<ControllerPeer> {
    let mut peers = peers
        .try_lock()
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
