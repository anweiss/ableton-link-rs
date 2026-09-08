use std::{
    fmt::{self, Display},
    mem,
    sync::{Arc, Mutex},
};

use chrono::Duration;
use tokio::sync::Notify;
use tracing::{debug, info};

use crate::{
    discovery::peers::ControllerPeer,
    encoding::{self, Decode, Encode},
};

use super::{
    clock::Clock,
    controller::{gated_recv, DispatchGate},
    encoding::PayloadEntryHeader,
    ghostxform::GhostXForm,
    measurement::MeasurePeerEvent,
    node::NodeId,
    timeline::Timeline,
    Result,
};

pub const SESSION_MEMBERSHIP_HEADER_KEY: u32 = u32::from_be_bytes(*b"sess");
pub const SESSION_MEMBERSHIP_SIZE: u32 = mem::size_of::<SessionId>() as u32;
pub const SESSION_MEMBERSHIP_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: SESSION_MEMBERSHIP_HEADER_KEY,
    size: SESSION_MEMBERSHIP_SIZE,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd)]
pub struct SessionId(pub NodeId);

impl Encode for SessionId {
    fn encode_to(
        &self,
        out: &mut Vec<u8>,
    ) -> core::result::Result<(), crate::encoding::EncodeError> {
        self.0.encode_to(out)?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        self.0.encoded_size()
    }
}

impl Decode for SessionId {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (node_id, n) = NodeId::decode_from(bytes)?;
        Ok((SessionId(node_id), n))
    }
}

impl Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionMembership {
    pub session_id: SessionId,
}

impl Encode for SessionMembership {
    fn encode_to(
        &self,
        out: &mut Vec<u8>,
    ) -> core::result::Result<(), crate::encoding::EncodeError> {
        self.session_id.encode_to(out)?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        self.session_id.encoded_size()
    }
}

impl Decode for SessionMembership {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (session_id, n) = SessionId::decode_from(bytes)?;
        Ok((Self { session_id }, n))
    }
}

impl From<SessionId> for SessionMembership {
    fn from(session_id: SessionId) -> Self {
        SessionMembership { session_id }
    }
}

impl SessionMembership {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = SESSION_MEMBERSHIP_HEADER.encode()?;
        encoded.append(&mut encoding::encode_to_vec(&self.session_id)?);
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
    pub is_founding: Arc<Mutex<bool>>,
    pub tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    pub peers: Arc<Mutex<Vec<ControllerPeer>>>,
    pub clock: Clock,
    pub has_joined: Arc<Mutex<bool>>,
}

impl Sessions {
    pub fn new(
        init: Session,
        tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        clock: Clock,
        tx_join_session: tokio::sync::mpsc::Sender<Session>,
        notifier: Arc<Notify>,
        rx_measure_peer_result: tokio::sync::mpsc::Receiver<MeasurePeerEvent>,
    ) -> Self {
        let gate = Arc::new(DispatchGate::new_open());
        let (sessions, task) = Self::with_dispatch_gate(
            init,
            tx_measure_peer_state,
            peers,
            clock,
            tx_join_session,
            rx_measure_peer_result,
            gate,
        );
        tokio::spawn(async move {
            notifier.notified().await;
            task.abort();
        });
        sessions
    }

    pub(crate) fn with_dispatch_gate(
        init: Session,
        tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        clock: Clock,
        tx_join_session: tokio::sync::mpsc::Sender<Session>,
        mut rx_measure_peer_result: tokio::sync::mpsc::Receiver<MeasurePeerEvent>,
        gate: Arc<DispatchGate>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let other_sessions = Arc::new(Mutex::new(vec![init.clone()]));
        let current = Arc::new(Mutex::new(init));

        let other_sessions_loop = other_sessions.clone();
        let current_loop = current.clone();
        let tx_join_session_loop = tx_join_session.clone();
        let peers_loop = peers.clone();
        let tx_measure_peer_state_loop = tx_measure_peer_state.clone();

        let mut open = gate.subscribe();
        let task = tokio::spawn(async move {
            let mut remeasurements = tokio::task::JoinSet::new();
            while let Some((_permit, epoch, event)) =
                gated_recv(&gate, &mut open, &mut rx_measure_peer_result).await
            {
                let MeasurePeerEvent::XForm(session_id, x_form) = event else {
                    continue;
                };
                // A handler may be blocked sending to another gated consumer.
                // Cancel that await on disable instead of holding stop() up.
                let handle = async {
                    if x_form == GhostXForm::default() {
                        handle_failed_measurement_inner(
                            session_id,
                            other_sessions_loop.clone(),
                            current_loop.clone(),
                            peers_loop.clone(),
                        )
                        .await
                    } else {
                        handle_successful_measurement_inner(
                            session_id,
                            x_form,
                            other_sessions_loop.clone(),
                            current_loop.clone(),
                            clock,
                            tx_join_session_loop.clone(),
                        )
                        .await
                    }
                };
                tokio::select! {
                    biased;
                    _ = open.closed() => {}
                    session = handle => {
                        if let Some(session) = session {
                            // Only one current-session retry loop is useful. Its
                            // permit cancels sleep/send on disable, and the task
                            // set cancels it when this consumer is dropped.
                            remeasurements.shutdown().await;
                            let gate = gate.clone();
                            let peers = peers_loop.clone();
                            let sender = tx_measure_peer_state_loop.clone();
                            remeasurements.spawn(async move {
                                gate.run_in_epoch(epoch, remeasurement_loop(peers, sender, session)).await;
                            });
                        }
                    }
                }
            }
            debug!("measure peer event channel closed");
        });

        (
            Self {
                other_sessions,
                current,
                tx_measure_peer_state,
                peers,
                clock,
                is_founding: Arc::new(Mutex::new(false)),
                has_joined: Arc::new(Mutex::new(false)),
            },
            task,
        )
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
            let session = self.update_timeline(self.current.try_lock().unwrap().clone(), timeline);
            self.current.try_lock().unwrap().timeline = session.timeline;
            if !*self.has_joined.try_lock().unwrap() {
                debug!(
                    "updating current session {} with timeline {:?}",
                    session_id, session.timeline
                );

                *self.has_joined.try_lock().unwrap() = true;
            }
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
                .enumerate()
                .find(|(_, s)| s.session_id == session_id);

            if let Some((idx, s)) = s {
                let session = self.update_timeline(s, timeline);
                info!(
                    "updating already seen session {} with timeline {:?}",
                    session_id, session.timeline
                );
                self.other_sessions.try_lock().unwrap()[idx].timeline = session.timeline;
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

    pub fn update_timeline(&self, mut session: Session, timeline: Timeline) -> Session {
        if timeline.beat_origin > session.timeline.beat_origin {
            info!(
                "[adopting] updating peer timeline for session {} (bpm: {}, beat origin: {}, time: origin: {})",
                session.session_id,
                timeline.tempo.bpm().round(),
                timeline.beat_origin.floating(),
                timeline.time_origin,
            );
            session.timeline = timeline;
        } else {
            debug!(
                "[rejecting] updating peer timeline with beat origin: {}. current timeline beat origin: {}",
                timeline.beat_origin.floating(),
                session.timeline.beat_origin.floating()
            );
        }

        session
    }
}

pub async fn launch_session_measurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    mut session: Session,
) {
    info!(
        "launching session measurement for session {}",
        session.session_id
    );

    let peers = session_peers(peers.clone(), session.session_id);

    if let Some(p) = peers
        .iter()
        .find(|p| p.peer_state.ident() == session.session_id.0)
    {
        session.measurement.timestamp = Duration::zero();
        if let Err(error) = tx_measure_peer_state
            .send(MeasurePeerEvent::PeerState(
                session.session_id,
                p.peer_state.clone(),
            ))
            .await
        {
            debug!("measurement request receiver closed: {}", error);
        }
    } else if let Some(p) = peers.first() {
        session.measurement.timestamp = Duration::zero();
        if let Err(error) = tx_measure_peer_state
            .send(MeasurePeerEvent::PeerState(
                session.session_id,
                p.peer_state.clone(),
            ))
            .await
        {
            debug!("measurement request receiver closed: {}", error);
        }
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
    if let Some(session) = handle_successful_measurement_inner(
        session_id,
        x_form,
        other_sessions,
        current,
        clock,
        tx_join_session,
    )
    .await
    {
        schedule_remeasurement(peers, tx_measure_peer_state, session).await;
    }
}

async fn handle_successful_measurement_inner(
    session_id: SessionId,
    x_form: GhostXForm,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    clock: Clock,
    tx_join_session: tokio::sync::mpsc::Sender<Session>,
) -> Option<Session> {
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

    let current_session_id = current.try_lock().unwrap().session_id;
    debug!(
        "Current session: {}, measured session: {}",
        current_session_id, session_id
    );

    if current_session_id == session_id {
        current.try_lock().unwrap().measurement = measurement;
        let session = current.try_lock().unwrap().clone();
        if let Err(e) = tx_join_session.send(session).await {
            debug!("Failed to send session join event: {}", e);
        }
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
            debug!(
                "Ghost time comparison: current={} us, new={} us, diff={} us, eps={} us",
                cur_ghost.num_microseconds().unwrap(),
                new_ghost.num_microseconds().unwrap(),
                ghost_diff.num_microseconds().unwrap(),
                SESSION_EPS.num_microseconds().unwrap()
            );

            // Session switching logic, matching upstream `Sessions::
            // handleSuccessfulMeasurement`:
            // 1. Join if the other session's ghost time is significantly ahead
            //    of ours, which is what makes a freshly started peer adopt an
            //    established session (and its tempo) rather than impose its own.
            // 2. If the two are within an epsilon, fall back to session id order
            //    so that both sides break the tie the same way.
            let current_session_id = current.try_lock().unwrap().session_id;

            let should_switch = ghost_diff > SESSION_EPS
                || (ghost_diff.num_microseconds().unwrap().abs()
                    < SESSION_EPS.num_microseconds().unwrap()
                    && session_id < current_session_id);

            if should_switch {
                info!(
                    "Session {} wins over current session (ghost_diff={} us, tempo={}), switching!",
                    session_id,
                    ghost_diff.num_microseconds().unwrap(),
                    s.timeline.tempo.bpm()
                );
                let c = current.try_lock().unwrap().clone();

                *current.try_lock().unwrap() = s.clone();
                other_sessions.try_lock().unwrap().remove(idx);
                other_sessions.try_lock().unwrap().insert(idx, c);

                if let Err(e) = tx_join_session.send(s.clone()).await {
                    debug!("Failed to send session join event: {}", e);
                }

                return Some(s);
            } else {
                debug!("Session {} does not win over current session (ghost_diff={} us), staying with current",
                       session_id,
                       ghost_diff.num_microseconds().unwrap());
            }
        }
    }
    None
}

pub async fn handle_failed_measurement(
    session_id: SessionId,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
) {
    if let Some(session) =
        handle_failed_measurement_inner(session_id, other_sessions, current, peers.clone()).await
    {
        schedule_remeasurement(peers, tx_measure_peer, session).await;
    }
}

async fn handle_failed_measurement_inner(
    session_id: SessionId,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
) -> Option<Session> {
    info!("session {} measurement failed", session_id);

    if current.try_lock().unwrap().session_id == session_id {
        let current = current.try_lock().unwrap().clone();
        return Some(current);
    } else {
        other_sessions
            .lock()
            .unwrap()
            .retain(|session| session.session_id != session_id);
        peers
            .lock()
            .unwrap()
            .retain(|peer| peer.peer_state.session_id() != session_id);
    }
    None
}

pub async fn schedule_remeasurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
) {
    tokio::spawn(remeasurement_loop(peers, tx_measure_peer, session));
}

async fn remeasurement_loop(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
) {
    let repeat = async {
        loop {
            tokio::time::sleep(Duration::microseconds(30000000).to_std().unwrap()).await;
            launch_session_measurement(peers.clone(), tx_measure_peer.clone(), session.clone())
                .await;
        }
    };
    tokio::select! {
        _ = tx_measure_peer.closed() => {}
        _ = repeat => {}
    }
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
    peers.sort_by_key(|a| a.peer_state.ident());

    peers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::node::NodeId;

    #[tokio::test]
    async fn failed_other_session_removes_only_its_session_and_peers() {
        let current = Session {
            session_id: SessionId(NodeId::from_array([1; 8])),
            timeline: Timeline::default(),
            measurement: SessionMeasurement::default(),
        };
        let failed = Session {
            session_id: SessionId(NodeId::from_array([2; 8])),
            ..current.clone()
        };
        let other = Session {
            session_id: SessionId(NodeId::from_array([3; 8])),
            ..current.clone()
        };
        let sessions = Arc::new(Mutex::new(vec![failed.clone(), other.clone()]));
        let peers = Arc::new(Mutex::new(
            vec![failed.session_id, failed.session_id, other.session_id]
                .into_iter()
                .map(|session_id| ControllerPeer {
                    peer_state: crate::discovery::peers::PeerState {
                        node_state: crate::link::node::NodeState::new(session_id),
                        ..Default::default()
                    },
                })
                .collect(),
        ));
        assert!(handle_failed_measurement_inner(
            failed.session_id,
            sessions.clone(),
            Arc::new(Mutex::new(current)),
            peers.clone()
        )
        .await
        .is_none());
        assert_eq!(sessions.lock().unwrap().len(), 1);
        assert_eq!(sessions.lock().unwrap()[0].session_id, other.session_id);
        assert_eq!(peers.lock().unwrap().len(), 1);
        assert_eq!(
            peers.lock().unwrap()[0].peer_state.session_id(),
            other.session_id
        );
    }

    #[tokio::test]
    async fn remeasurement_scheduler_releases_peers_on_disable_and_final_drop() {
        let gate = Arc::new(DispatchGate::new());
        let peers = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::downgrade(&peers);
        let (tx_request, _rx_request) = tokio::sync::mpsc::channel(1);
        let (tx_result, rx_result) = tokio::sync::mpsc::channel(2);
        let (tx_join, mut rx_join) = tokio::sync::mpsc::channel(1);
        let session = Session {
            session_id: SessionId::default(),
            timeline: Timeline::default(),
            measurement: SessionMeasurement::default(),
        };
        let (sessions, task) = Sessions::with_dispatch_gate(
            session.clone(),
            tx_request,
            peers.clone(),
            Clock::new(),
            tx_join,
            rx_result,
            gate.clone(),
        );
        let baseline = Arc::strong_count(&peers);
        for cycle in 0..2 {
            gate.start().await;
            for _ in 0..3 {
                tx_result
                    .send(MeasurePeerEvent::XForm(
                        session.session_id,
                        GhostXForm::default(),
                    ))
                    .await
                    .unwrap();
            }
            // The following join acknowledges processing of the preceding failures.
            tx_result
                .send(MeasurePeerEvent::XForm(
                    session.session_id,
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(1),
                    },
                ))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), rx_join.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Arc::strong_count(&peers), baseline + 1);
            if cycle == 0 {
                gate.stop().await;
                assert_eq!(Arc::strong_count(&peers), baseline);
            }
        }
        drop(sessions);
        drop(peers);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while probe.strong_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn test_key() {
        assert_eq!(SESSION_MEMBERSHIP_HEADER_KEY, 0x73657373);
    }

    #[test]
    fn session_id_equality() {
        let id1 = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let id2 = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let id3 = SessionId(NodeId::from_array([8, 7, 6, 5, 4, 3, 2, 1]));
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn session_id_ordering() {
        let id_low = SessionId(NodeId::from_array([0, 0, 0, 0, 0, 0, 0, 1]));
        let id_high = SessionId(NodeId::from_array([0, 0, 0, 0, 0, 0, 0, 2]));
        assert!(id_low < id_high);
    }

    #[test]
    fn session_id_display() {
        let id = SessionId(NodeId::from_array([
            0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44,
        ]));
        let display = format!("{}", id);
        assert_eq!(display, "0xaabbccdd11223344");
    }

    #[test]
    fn session_membership_from_session_id() {
        let id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let membership = SessionMembership::from(id);
        assert_eq!(membership.session_id, id);
    }

    #[test]
    fn session_membership_roundtrip_encode() {
        let id = SessionId(NodeId::from_array([10, 20, 30, 40, 50, 60, 70, 80]));
        let membership = SessionMembership::from(id);
        let encoded = membership.encode().unwrap();
        // Should include the header + SessionId bytes
        assert!(!encoded.is_empty());
    }

    #[test]
    fn session_measurement_default() {
        let sm = SessionMeasurement::default();
        assert_eq!(sm.x_form, GhostXForm::default());
        assert_eq!(sm.timestamp, Duration::zero());
    }

    #[test]
    fn session_peers_empty_when_no_match() {
        let peers = Arc::new(Mutex::new(vec![]));
        let session_id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let result = session_peers(peers, session_id);
        assert!(result.is_empty());
    }
}
