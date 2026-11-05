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
    sync::lock,
};

use super::{
    clock::Clock,
    controller::{gated_recv, DispatchGate},
    encoding::PayloadEntryHeader,
    ghostxform::GhostXForm,
    measurement::MeasurePeerEvent,
    micros,
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

/// Identity of one local session incarnation, captured when measurement starts.
/// Wire SessionId alone is not sufficient across resets or candidate replacement.
#[derive(Clone, Debug)]
pub struct SessionKey {
    pub session_id: SessionId,
    incarnation: Arc<()>,
}

impl SessionKey {
    /// A standalone incarnation. Managed requests use Session::key instead.
    /// Fresh identity for independently consumed results only. Session handlers
    /// require the key captured from `Session::key()` at request origin instead.
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            incarnation: Arc::new(()),
        }
    }
    pub(crate) fn matches(&self, session: &Session) -> bool {
        self.session_id == session.session_id
            && Arc::ptr_eq(&self.incarnation, &session.incarnation)
    }
}

impl Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.session_id.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub(crate) incarnation: Arc<()>,
    pub session_id: SessionId,
    pub timeline: Timeline,
    pub measurement: SessionMeasurement,
}

impl Session {
    pub fn key(&self) -> SessionKey {
        SessionKey {
            session_id: self.session_id,
            incarnation: self.incarnation.clone(),
        }
    }
    /// A newly observed session incarnation. Cloning preserves that incarnation;
    /// reset_session always replaces it even when the wire SessionId is unchanged.
    pub fn new(session_id: SessionId, timeline: Timeline, measurement: SessionMeasurement) -> Self {
        Self {
            session_id,
            timeline,
            measurement,
            incarnation: Arc::new(()),
        }
    }
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
            let mut retries = std::collections::HashMap::<usize, tokio::task::AbortHandle>::new();
            while let Some((_permit, epoch, event)) =
                gated_recv(&gate, &mut open, &mut rx_measure_peer_result).await
            {
                while remeasurements.try_join_next().is_some() {}
                retries.retain(|_, task| !task.is_finished());
                let (key, x_form, retryable) = match event {
                    MeasurePeerEvent::XForm(key, x_form) => (key, x_form, false),
                    MeasurePeerEvent::Cancelled(key) => (key, GhostXForm::default(), true),
                    MeasurePeerEvent::PeerState(..) => continue,
                };
                // A handler may be blocked sending to another gated consumer.
                // Cancel that await on disable instead of holding stop() up.
                let handle = async {
                    if retryable {
                        let current = lock(&current_loop);
                        if key.matches(&current) {
                            Some(current.clone())
                        } else {
                            lock(&other_sessions_loop)
                                .iter()
                                .find(|session| key.matches(session))
                                .cloned()
                        }
                    } else if x_form == GhostXForm::default() {
                        handle_failed_measurement_inner(
                            key.clone(),
                            other_sessions_loop.clone(),
                            current_loop.clone(),
                            peers_loop.clone(),
                        )
                        .await
                    } else {
                        handle_successful_measurement_inner(
                            key.clone(),
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
                            let identity = Arc::as_ptr(&session.incarnation) as usize;
                            if let Some(old) = retries.remove(&identity) { old.abort(); }
                            let gate = gate.clone();
                            let peers = peers_loop.clone();
                            let sender = tx_measure_peer_state_loop.clone();
                            let current = current_loop.clone();
                            let others = other_sessions_loop.clone();
                            let delay = if retryable { std::time::Duration::from_millis(100) } else { std::time::Duration::from_secs(30) };
                            retries.insert(identity, remeasurements.spawn(async move {
                                gate.run_in_epoch(epoch, async move {
                                    tokio::time::sleep(delay).await;
                                    loop {
                                        let request = owned_measurement_request(&session.key(), &current, &others, &peers);
                                        if let Some(request) = request {
                                            if let Err(error) = sender.send(request).await {
                                                debug!("measurement request receiver closed: {}", error);
                                            }
                                            break;
                                        }
                                        if !session.key().matches(&lock(&current)) { break; }
                                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    }
                                }).await;
                            }));
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

    pub fn reset_session(&mut self, mut session: Session) {
        session.incarnation = Arc::new(());
        let mut current = lock(&self.current);
        let mut others = lock(&self.other_sessions);
        *current = session;
        others.clear();
    }

    pub fn reset_timeline(&self, timeline: Timeline) {
        // Every compound session operation takes current before other_sessions.
        let mut current = lock(&self.current);
        current.timeline = timeline;
        let current_id = current.session_id;
        if let Some(session) = lock(&self.other_sessions)
            .iter_mut()
            .find(|s| s.session_id == current_id)
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

        let launch = {
            let mut current = lock(&self.current);
            let mut others = lock(&self.other_sessions);
            if current.session_id == session_id {
                current.timeline = self.update_timeline(current.clone(), timeline).timeline;
                *lock(&self.has_joined) = true;
                None
            } else if let Some(session) = others.iter_mut().find(|s| s.session_id == session_id) {
                session.timeline = self.update_timeline(session.clone(), timeline).timeline;
                None
            } else {
                let session = Session {
                    incarnation: Arc::new(()),
                    session_id,
                    timeline,
                    measurement: SessionMeasurement::default(),
                };
                others.push(session.clone());
                Some(session)
            }
        };
        if let Some(session) = launch {
            let request = owned_measurement_request(
                &session.key(),
                &self.current,
                &self.other_sessions,
                &self.peers,
            );
            if let Some(request) = request {
                if let Err(error) = self.tx_measure_peer_state.send(request).await {
                    debug!("measurement request receiver closed: {}", error);
                }
            }
        }

        lock(&self.current).timeline
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

fn owned_measurement_request(
    key: &SessionKey,
    current: &Mutex<Session>,
    others: &Mutex<Vec<Session>>,
    peers: &Mutex<Vec<ControllerPeer>>,
) -> Option<MeasurePeerEvent> {
    let current = lock(current);
    let mut others = lock(others);
    let is_current = key.matches(&current);
    if !is_current && !others.iter().any(|session| key.matches(session)) {
        return None;
    }
    let peers = lock(peers);
    let mut candidates = peers
        .iter()
        .filter(|peer| peer.peer_state.session_id() == key.session_id);
    let peer = candidates
        .clone()
        .find(|peer| peer.peer_state.ident() == key.session_id.0)
        .or_else(|| candidates.next());
    if let Some(peer) = peer {
        Some(MeasurePeerEvent::PeerState(
            key.clone(),
            peer.peer_state.clone(),
        ))
    } else {
        // No detached retry or dormant candidate survives peer absence. A later
        // sighting creates a fresh incarnation; old terminal results cannot own it.
        if !is_current {
            others.retain(|session| !key.matches(session));
        }
        None
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
                session.key(),
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
                session.key(),
                p.peer_state.clone(),
            ))
            .await
        {
            debug!("measurement request receiver closed: {}", error);
        }
    }
}

pub async fn handle_successful_measurement(
    key: SessionKey,
    x_form: GhostXForm,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    clock: Clock,
    tx_join_session: tokio::sync::mpsc::Sender<Session>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
) {
    if let Some(session) = handle_successful_measurement_inner(
        key,
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
    key: SessionKey,
    x_form: GhostXForm,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    clock: Clock,
    tx_join_session: tokio::sync::mpsc::Sender<Session>,
) -> Option<Session> {
    let session_id = key.session_id;
    info!(
        "session {} measurement completed with result ({}, {})",
        session_id,
        x_form.slope,
        micros(x_form.intercept),
    );

    let measurement = SessionMeasurement {
        x_form,
        timestamp: clock.micros(),
    };

    // Decide under a consistent snapshot; do not mutate a session that requires
    // a join notification until capacity is reserved. Cancellation while waiting
    // must not commit an undelivered session transition.
    let (current_incarnation, target_incarnation) = {
        let current = lock(&current);
        let mut others = lock(&other_sessions);
        let target_incarnation = if current.session_id != session_id {
            let candidate = others.iter_mut().find(|s| key.matches(s))?;
            if !measurement_wins(&current, session_id, measurement, clock.micros()) {
                candidate.measurement = measurement;
                return None;
            }
            candidate.incarnation.clone()
        } else {
            if !key.matches(&current) {
                return None;
            }
            current.incarnation.clone()
        };
        (current.incarnation.clone(), target_incarnation)
    };
    let Ok(slot) = tx_join_session.reserve().await else {
        return None;
    };
    let mut current = lock(&current);
    let mut others = lock(&other_sessions);
    // Capacity may have been unavailable across a reset or another transition.
    if !Arc::ptr_eq(&current.incarnation, &current_incarnation) {
        return None;
    }
    let target = if current.session_id == session_id {
        Some(&*current)
    } else {
        others
            .iter()
            .find(|candidate| candidate.session_id == session_id)
    };
    if !target.is_some_and(|target| Arc::ptr_eq(&target.incarnation, &target_incarnation)) {
        return None;
    }
    if current.session_id == session_id {
        current.measurement = measurement;
        slot.send(current.clone());
        return Some(current.clone());
    } else if let Some(candidate) = others.iter_mut().find(|s| s.session_id == session_id) {
        candidate.measurement = measurement;
        if measurement_wins(&current, session_id, measurement, clock.micros()) {
            let winner = candidate.clone();
            *candidate = std::mem::replace(&mut *current, winner.clone());
            slot.send(winner.clone());
            return Some(winner);
        }
    }

    None
}

fn measurement_wins(
    current: &Session,
    candidate: SessionId,
    measurement: SessionMeasurement,
    host_time: Duration,
) -> bool {
    const SESSION_EPS: Duration = Duration::microseconds(500_000);
    let diff = measurement.x_form.host_to_ghost(host_time)
        - current.measurement.x_form.host_to_ghost(host_time);
    diff > SESSION_EPS
        || (micros(diff).saturating_abs() < micros(SESSION_EPS) && candidate < current.session_id)
}

pub async fn handle_failed_measurement(
    key: SessionKey,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
) {
    if let Some(session) =
        handle_failed_measurement_inner(key, other_sessions, current, peers.clone()).await
    {
        schedule_remeasurement(peers, tx_measure_peer, session).await;
    }
}

async fn handle_failed_measurement_inner(
    key: SessionKey,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
) -> Option<Session> {
    let current = lock(&current);
    if key.matches(&current) {
        return Some(current.clone());
    }
    if current.session_id == key.session_id {
        return None;
    }
    let mut others = lock(&other_sessions);
    if let Some(index) = others.iter().position(|session| key.matches(session)) {
        others.remove(index);
        lock(&peers).retain(|peer| peer.peer_state.session_id() != key.session_id);
    }
    None
}

/// Schedule one standalone request after 30 seconds; its result handler may rearm it.
pub async fn schedule_remeasurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
) {
    tokio::spawn(remeasurement_once(
        peers,
        tx_measure_peer,
        session,
        None,
        std::time::Duration::from_secs(30),
    ));
}

async fn remeasurement_once(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
    current: Option<Arc<Mutex<Session>>>,
    delay: std::time::Duration,
) {
    let once = async {
        tokio::time::sleep(delay).await;
        let session = if let Some(current) = &current {
            let current = lock(current);
            if !session.key().matches(&current) {
                return;
            }
            current.clone()
        } else {
            session
        };
        launch_session_measurement(peers, tx_measure_peer.clone(), session).await;
    };
    tokio::select! {
        _ = tx_measure_peer.closed() => {}
        _ = once => {}
    }
}

pub fn session_peers(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_id: SessionId,
) -> Vec<ControllerPeer> {
    let mut peers = lock(&peers)
        .iter()
        .filter(|p| p.peer_state.session_id() == session_id)
        .cloned()
        .collect::<Vec<_>>();
    peers.sort_by_key(|a| a.peer_state.ident());

    peers
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn origin_finality_standalone_scheduler_is_one_shot() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let id = SessionId(NodeId::from_array([1; 8]));
            let session = Session::new(id, Timeline::default(), SessionMeasurement::default());
            let peers = Arc::new(Mutex::new(vec![ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(id),
                    ..Default::default()
                },
            }]));
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let mut work = tokio::spawn(remeasurement_once(
                peers,
                tx,
                session.clone(),
                None,
                std::time::Duration::from_millis(5),
            ));
            let MeasurePeerEvent::PeerState(key, _) = rx.recv().await.unwrap() else {
                panic!("request")
            };
            assert!(key.matches(&session));
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut work)
                .await
                .expect("public remeasurement must finish after one request")
                .unwrap();
            assert!(
                rx.recv().await.is_none(),
                "no recurring producer survives the result"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn origin_finality_current_retry_survives_absence_and_remains_unique() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = SessionId(NodeId::from_array([1; 8]));
            let init = Session::new(id, Timeline::default(), SessionMeasurement::default());
            let peer = ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(id),
                    ..Default::default()
                },
            };
            let peers = Arc::new(Mutex::new(Vec::new()));
            let gate = Arc::new(DispatchGate::new_open());
            let (tx_request, mut requests) = tokio::sync::mpsc::channel(8);
            let (tx_result, results) = tokio::sync::mpsc::channel(8);
            let (tx_join, _joins) = tokio::sync::mpsc::channel(1);
            let (sessions, task) = Sessions::with_dispatch_gate(
                init.clone(),
                tx_request,
                peers.clone(),
                Clock::new(),
                tx_join,
                results,
                gate.clone(),
            );
            for _ in 0..4 {
                tx_result
                    .send(MeasurePeerEvent::Cancelled(init.key()))
                    .await
                    .unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            assert!(requests.try_recv().is_err());
            peers.lock().unwrap().push(peer);
            sessions.saw_session_timeline(id, Timeline::default()).await;
            let request =
                tokio::time::timeout(std::time::Duration::from_millis(500), requests.recv())
                    .await
                    .expect("current ownership must survive temporary peer absence")
                    .unwrap();
            assert!(matches!(request, MeasurePeerEvent::PeerState(key, _) if key.matches(&init)));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), requests.recv())
                    .await
                    .is_err(),
                "one owned retry, not one per cancellation result"
            );
            tx_result
                .send(MeasurePeerEvent::Cancelled(init.key()))
                .await
                .unwrap();
            tokio::task::yield_now().await;
            gate.stop().await;
            assert!(requests.try_recv().is_err());
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn originating_incarnation_rejects_old_success_and_failure_after_replacement() {
        for current_target in [false, true] {
            let initial = Session::new(
                SessionId(NodeId::from_array([90; 8])),
                Timeline::default(),
                SessionMeasurement::default(),
            );
            let old = if current_target {
                initial.clone()
            } else {
                Session::new(
                    SessionId(NodeId::from_array([89; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                )
            };
            let replacement = Session::new(
                old.session_id,
                old.timeline,
                SessionMeasurement {
                    x_form: GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(7),
                    },
                    timestamp: Duration::seconds(1),
                },
            );
            let current = Arc::new(Mutex::new(if current_target {
                replacement.clone()
            } else {
                initial
            }));
            let others = Arc::new(Mutex::new(if current_target {
                Vec::new()
            } else {
                vec![replacement.clone()]
            }));
            let peers = Arc::new(Mutex::new(vec![ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(old.session_id),
                    ..Default::default()
                },
            }]));
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            assert!(handle_successful_measurement_inner(
                old.key(),
                GhostXForm {
                    slope: 1.0,
                    intercept: Duration::seconds(100)
                },
                others.clone(),
                current.clone(),
                Clock::new(),
                tx
            )
            .await
            .is_none());
            assert!(handle_failed_measurement_inner(
                old.key(),
                others.clone(),
                current.clone(),
                peers.clone()
            )
            .await
            .is_none());
            let live = if current_target {
                current.lock().unwrap().clone()
            } else {
                others.lock().unwrap()[0].clone()
            };
            assert!(replacement.key().matches(&live));
            assert_eq!(live.measurement.x_form, replacement.measurement.x_form);
            assert_eq!(peers.lock().unwrap().len(), 1);
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn three_gaps_candidate_rearms_after_absent_peer_and_rejects_retired_origin() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = SessionId(NodeId::from_array([1; 8]));
            let peer = ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(id),
                    ..Default::default()
                },
            };
            let peers = Arc::new(Mutex::new(vec![peer.clone()]));
            let (tx_requests, mut requests) = tokio::sync::mpsc::channel(1);
            let (tx_results, results) = tokio::sync::mpsc::channel(1);
            let (tx_joins, mut joins) = tokio::sync::mpsc::channel(1);
            let (sessions, task) = Sessions::with_dispatch_gate(
                Session::new(
                    SessionId(NodeId::from_array([2; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                ),
                tx_requests,
                peers.clone(),
                Clock::new(),
                tx_joins,
                results,
                Arc::new(DispatchGate::new_open()),
            );
            sessions.saw_session_timeline(id, Timeline::default()).await;
            let MeasurePeerEvent::PeerState(old, _) = requests.recv().await.unwrap() else {
                panic!("initial request")
            };
            peers.lock().unwrap().clear();
            tx_results
                .send(MeasurePeerEvent::Cancelled(old.clone()))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while sessions
                    .other_sessions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|session| old.matches(session))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("retry must retire an absent candidate rather than strand it");
            assert!(requests.try_recv().is_err());
            assert!(joins.try_recv().is_err());
            peers.lock().unwrap().push(peer);
            sessions.saw_session_timeline(id, Timeline::default()).await;
            let MeasurePeerEvent::PeerState(fresh, _) = requests.recv().await.unwrap() else {
                panic!("rearmed request")
            };
            assert!(!Arc::ptr_eq(&old.incarnation, &fresh.incarnation));
            tx_results
                .send(MeasurePeerEvent::XForm(
                    old.clone(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(2),
                    },
                ))
                .await
                .unwrap();
            tx_results
                .send(MeasurePeerEvent::XForm(old, GhostXForm::default()))
                .await
                .unwrap();
            tx_results
                .send(MeasurePeerEvent::XForm(
                    fresh.clone(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(1),
                    },
                ))
                .await
                .unwrap();
            let joined = joins.recv().await.unwrap();
            assert!(fresh.matches(&joined));
            assert_eq!(peers.lock().unwrap().len(), 1);
            assert!(joins.try_recv().is_err());
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_candidate_retries_original_key_without_waiting_for_changed_timeline() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let candidate_id = SessionId(NodeId::from_array([1; 8]));
            let peer = ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(candidate_id),
                    ..Default::default()
                },
            };
            let (tx_requests, mut requests) = tokio::sync::mpsc::channel(1);
            let (tx_results, results) = tokio::sync::mpsc::channel(1);
            let (tx_joins, mut joins) = tokio::sync::mpsc::channel(1);
            let (sessions, task) = Sessions::with_dispatch_gate(
                Session::new(
                    SessionId(NodeId::from_array([2; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                ),
                tx_requests,
                Arc::new(Mutex::new(vec![peer])),
                Clock::new(),
                tx_joins,
                results,
                Arc::new(DispatchGate::new_open()),
            );
            sessions
                .saw_session_timeline(candidate_id, Timeline::default())
                .await;
            let MeasurePeerEvent::PeerState(origin, _) = requests.recv().await.unwrap() else {
                panic!("request")
            };
            tx_results
                .send(MeasurePeerEvent::Cancelled(origin.clone()))
                .await
                .unwrap();
            let MeasurePeerEvent::PeerState(retried, _) = requests.recv().await.unwrap() else {
                panic!("retry")
            };
            assert!(Arc::ptr_eq(&origin.incarnation, &retried.incarnation));
            tx_results
                .send(MeasurePeerEvent::XForm(
                    retried,
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(1),
                    },
                ))
                .await
                .unwrap();
            let joined = joins.recv().await.unwrap();
            assert!(origin.matches(&joined));
            assert_eq!(sessions.current.lock().unwrap().session_id, candidate_id);
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    use super::*;
    use crate::link::node::NodeId;

    #[tokio::test]
    async fn blocked_joins_reject_same_id_reset_and_candidate_reinsertion() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for reset_current in [true, false] {
                let a = Session::new(
                    SessionId(NodeId::from_array([1; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                );
                let b = Session::new(
                    SessionId(NodeId::from_array([2; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                );
                let current = Arc::new(Mutex::new(a.clone()));
                let others = Arc::new(Mutex::new(vec![b.clone()]));
                let (tx_request, _rx_request) = tokio::sync::mpsc::channel(1);
                let mut sessions = Sessions {
                    current: current.clone(),
                    other_sessions: others.clone(),
                    is_founding: Arc::new(Mutex::new(false)),
                    has_joined: Arc::new(Mutex::new(false)),
                    tx_measure_peer_state: tx_request,
                    peers: Arc::new(Mutex::new(vec![ControllerPeer {
                        peer_state: crate::discovery::peers::PeerState {
                            node_state: crate::link::node::NodeState::new(b.session_id),
                            ..Default::default()
                        },
                    }])),
                    clock: Clock::new(),
                };
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                tx.send(a.clone()).await.unwrap();
                let target = if reset_current { a.key() } else { b.key() };
                let mut pending = Box::pin(handle_successful_measurement_inner(
                    target,
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(60),
                    },
                    others.clone(),
                    current.clone(),
                    Clock::new(),
                    tx.clone(),
                ));
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                if reset_current {
                    sessions.reset_session(a.clone());
                } else {
                    lock(&others).clear();
                    sessions
                        .saw_session_timeline(b.session_id, b.timeline)
                        .await;
                }
                rx.recv().await.unwrap();
                assert!(pending.await.is_none());
                assert!(
                    rx.try_recv().is_err(),
                    "old incarnation must not publish after same-ID replacement"
                );
                assert_eq!(lock(&current).measurement.x_form, GhostXForm::default());
                if !reset_current {
                    assert_eq!(lock(&others)[0].measurement.x_form, GhostXForm::default());
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn blocked_join_is_cancel_safe_and_rechecks_state_before_commit() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for outcome in 0..3 {
                let a = Session {
                    incarnation: Arc::new(()),
                    session_id: SessionId(NodeId::from_array([10; 8])),
                    timeline: Timeline::default(),
                    measurement: SessionMeasurement::default(),
                };
                let b = Session {
                    incarnation: Arc::new(()),
                    session_id: SessionId(NodeId::from_array([20; 8])),
                    ..a.clone()
                };
                let current = Arc::new(Mutex::new(a.clone()));
                let others = Arc::new(Mutex::new(vec![b.clone()]));
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                tx.send(a.clone()).await.unwrap();
                let mut pending = tokio::spawn(handle_successful_measurement_inner(
                    b.key(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(60),
                    },
                    others.clone(),
                    current.clone(),
                    Clock::new(),
                    tx.clone(),
                ));
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(30), &mut pending)
                        .await
                        .is_err()
                );
                assert_eq!(
                    lock(&current).session_id,
                    a.session_id,
                    "blocked notification must not partially commit a switch"
                );
                assert_eq!(lock(&others)[0].session_id, b.session_id);
                if outcome == 0 {
                    pending.abort();
                    let _ = pending.await;
                    assert_eq!(lock(&current).session_id, a.session_id);
                    rx.recv().await.unwrap();
                    assert!(rx.try_recv().is_err());
                } else if outcome == 1 {
                    {
                        let mut guard = lock(&current);
                        let mut candidates = lock(&others);
                        guard.session_id = SessionId(NodeId::from_array([30; 8]));
                        candidates.clear();
                    }
                    rx.recv().await.unwrap();
                    assert!(pending.await.unwrap().is_none());
                    assert!(
                        rx.try_recv().is_err(),
                        "pre-reset session must not be published after capacity returns"
                    );
                } else {
                    lock(&others)[0].timeline.tempo = crate::link::tempo::Tempo::new(150.0);
                    rx.recv().await.unwrap();
                    let winner = pending.await.unwrap().unwrap();
                    assert_eq!(winner.timeline.tempo.bpm(), 150.0);
                    assert_eq!(rx.recv().await.unwrap().session_id, b.session_id);
                    let guard = lock(&current);
                    let candidates = lock(&others);
                    assert_eq!(guard.session_id, b.session_id);
                    assert_eq!(guard.timeline.tempo.bpm(), 150.0);
                    assert_eq!(candidates.len(), 1);
                    assert_eq!(candidates[0].session_id, a.session_id);
                }
            }
        })
        .await
        .expect("blocked joins must be cancel-safe and use state current at publication");
    }

    #[test]
    fn resetting_timeline_updates_the_current_snapshot_without_an_other_copy() {
        let current = Session {
            incarnation: Arc::new(()),
            session_id: SessionId(NodeId::from_array([1; 8])),
            timeline: Timeline::default(),
            measurement: SessionMeasurement::default(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let sessions = Sessions {
            current: Arc::new(Mutex::new(current)),
            other_sessions: Arc::new(Mutex::new(Vec::new())),
            is_founding: Arc::new(Mutex::new(false)),
            has_joined: Arc::new(Mutex::new(false)),
            peers: Arc::new(Mutex::new(Vec::new())),
            tx_measure_peer_state: tx,
            clock: Clock::new(),
        };
        let timeline = Timeline {
            tempo: crate::link::tempo::Tempo::new(150.0),
            ..Timeline::default()
        };
        sessions.reset_timeline(timeline);
        assert_eq!(lock(&sessions.current).timeline.tempo, timeline.tempo);
        assert!(lock(&sessions.other_sessions).is_empty());
    }

    #[tokio::test]
    async fn failed_other_session_removes_only_its_session_and_peers() {
        let current = Session {
            incarnation: Arc::new(()),
            session_id: SessionId(NodeId::from_array([1; 8])),
            timeline: Timeline::default(),
            measurement: SessionMeasurement::default(),
        };
        let failed = Session {
            incarnation: Arc::new(()),
            session_id: SessionId(NodeId::from_array([2; 8])),
            ..current.clone()
        };
        let other = Session {
            incarnation: Arc::new(()),
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
            failed.key(),
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
            incarnation: Arc::new(()),
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
                        session.key(),
                        GhostXForm::default(),
                    ))
                    .await
                    .unwrap();
            }
            // The following join acknowledges processing of the preceding failures.
            tx_result
                .send(MeasurePeerEvent::XForm(
                    session.key(),
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
