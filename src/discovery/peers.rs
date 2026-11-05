use std::{
    net::SocketAddrV4,
    sync::{Arc, Mutex},
    vec,
};

use tokio::sync::{
    mpsc::{Receiver, Sender},
    Notify,
};
use tracing::debug;

use crate::link::{
    controller::{gated_recv, DispatchGate, SessionPeerCounter},
    node::{NodeId, NodeState},
    sessions::{SessionId, Sessions},
    state::StartStopState,
    timeline::Timeline,
};

#[derive(Clone)]
pub struct PeerStateMessageType {
    pub node_state: NodeState,
    pub ttl: u8,
    /// Original receive deadline. None preserves standalone caller TTL semantics.
    pub expires_at: Option<tokio::time::Instant>,
    /// Preserve managed ordering/ingress ownership when forwarding. None for standalone messages.
    pub observation: Option<Arc<super::messenger::PeerObservation>>,
    pub measurement_endpoint: Option<SocketAddrV4>,
    pub audio_endpoint: Option<SocketAddrV4>,
}

pub enum PeerEvent {
    /// Gateway announcement with its original deadline and optional receive revision.
    Observed(PeerStateMessageType),
    SawPeer(PeerState),
    PeerLeft(NodeId),
    PeerTimedOut(NodeId),
}

pub struct GatewayObserver {
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    sessions: Option<Sessions>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for GatewayObserver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl GatewayObserver {
    pub async fn new(
        on_peer_event: Receiver<PeerEvent>,
        peer_state: Arc<Mutex<PeerState>>,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        _notifier: Arc<Notify>,
        sessions: Option<Sessions>,
    ) -> Self {
        Self::with_dispatch_gate(
            on_peer_event,
            peer_state,
            session_peer_counter,
            tx_peer_state_change,
            peers,
            Arc::new(DispatchGate::new_open()),
            sessions,
        )
        .await
    }

    pub(crate) async fn with_dispatch_gate(
        mut on_peer_event: Receiver<PeerEvent>,
        peer_state: Arc<Mutex<PeerState>>,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        gate: Arc<DispatchGate>,
        sessions: Option<Sessions>,
    ) -> Self {
        if let Some(sessions) = &sessions {
            assert!(
                Arc::ptr_eq(&peers, &sessions.peers),
                "observer and sessions must share the peer registry"
            );
        }
        let observer_sessions = sessions.clone();
        let observer_peers = peers.clone();
        let peer_state_loop = peer_state.clone();

        let mut open = gate.subscribe();
        let task = tokio::spawn(async move {
            while let Some((_permit, _, peer_event)) =
                gated_recv(&gate, &mut open, &mut on_peer_event).await
            {
                let handle = async {
                    match peer_event {
                        PeerEvent::Observed(message) => {
                            observed_peer(
                                message,
                                peers.clone(),
                                peer_state_loop.clone(),
                                tx_peer_state_change.clone(),
                                sessions.as_ref(),
                            )
                            .await;
                        }
                        PeerEvent::SawPeer(peer_state) => {
                            saw_peer(
                                peer_state,
                                peers.clone(),
                                peer_state_loop.clone(),
                                session_peer_counter.clone(),
                                tx_peer_state_change.clone(),
                                sessions.as_ref(),
                            )
                            .await
                        }
                        PeerEvent::PeerLeft(node_id) => {
                            peer_left(
                                node_id,
                                peers.clone(),
                                tx_peer_state_change.clone(),
                                sessions.as_ref(),
                            )
                            .await
                        }
                        PeerEvent::PeerTimedOut(node_id) => {
                            peer_left(
                                node_id,
                                peers.clone(),
                                tx_peer_state_change.clone(),
                                sessions.as_ref(),
                            )
                            .await
                        }
                    }
                };
                tokio::select! {
                    biased;
                    _ = open.closed() => {}
                    _ = handle => {}
                }
            }
        });

        Self {
            peers: observer_peers,
            sessions: observer_sessions,
            task,
        }
    }

    pub fn set_session_timeline(&mut self, session_id: SessionId, timeline: Timeline) {
        if let Ok(mut peers) = self.peers.try_lock() {
            peers.iter_mut().for_each(|peer| {
                if peer.peer_state.session_id() == session_id {
                    peer.peer_state.node_state.timeline = timeline;
                }
            });
        } else {
            debug!("Could not acquire peers lock in set_session_timeline");
        }
    }

    pub fn forget_session(&mut self, session_id: Arc<Mutex<SessionId>>) {
        let session_id_val = match session_id.try_lock() {
            Ok(guard) => *guard,
            Err(_) => {
                debug!("Could not acquire session_id lock in forget_session");
                return;
            }
        };

        with_observer_state(&self.peers, None, self.sessions.as_ref(), |_, peers| {
            peers.retain(|peer| peer.peer_state.session_id() != session_id_val);
        });
    }

    pub fn reset_peers(&self) {
        with_observer_state(&self.peers, None, self.sessions.as_ref(), |_, peers| {
            peers.clear()
        });
    }

    // Controller reset already owns current and has cleared all candidates.
    pub(crate) fn reset_peers_after_session_reset(&self) {
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerState {
    pub node_state: NodeState,
    pub measurement_endpoint: Option<SocketAddrV4>,
    pub audio_endpoint: Option<SocketAddrV4>,
}

impl PeerState {
    pub fn ident(&self) -> NodeId {
        self.node_state.ident()
    }

    pub fn session_id(&self) -> SessionId {
        self.node_state.session_id
    }

    pub fn timeline(&self) -> Timeline {
        self.node_state.timeline
    }

    pub fn start_stop_state(&self) -> StartStopState {
        self.node_state.start_stop_state
    }
}

pub fn unique_session_peer_count(
    session_id: SessionId,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    self_node_id: NodeId,
) -> usize {
    let all_peers = match peers.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            debug!("Could not acquire peers lock in unique_session_peer_count, returning 0");
            return 0;
        }
    };
    debug!(
        "unique_session_peer_count: looking for session_id={}, self_node_id={}",
        session_id, self_node_id
    );
    debug!(
        "unique_session_peer_count: total peers in list: {}",
        all_peers.len()
    );

    for (i, peer) in all_peers.iter().enumerate() {
        debug!(
            "  peer[{}]: node_id={}, session_id={}",
            i,
            peer.peer_state.ident(),
            peer.peer_state.session_id()
        );
    }

    let mut peers = all_peers
        .iter()
        .filter(|p| {
            let matches_session = p.peer_state.session_id() == session_id;
            let is_not_self = p.peer_state.ident() != self_node_id;
            debug!(
                "  filtering peer {}: matches_session={}, is_not_self={}",
                p.peer_state.ident(),
                matches_session,
                is_not_self
            );
            matches_session && is_not_self
        })
        .cloned()
        .collect::<Vec<_>>();

    debug!(
        "unique_session_peer_count: before dedup, filtered peers count: {}",
        peers.len()
    );
    peers.dedup_by(|a, b| {
        let is_duplicate = a.peer_state.ident() == b.peer_state.ident();
        if is_duplicate {
            debug!("  removing duplicate peer: {}", a.peer_state.ident());
        }
        is_duplicate
    });

    debug!(
        "unique_session_peer_count: after dedup, final peers count: {}",
        peers.len()
    );
    peers.len()
}

#[derive(Clone)]
pub enum PeerStateChange {
    SessionMembership,
    SessionTimeline(SessionId, Timeline),
    SessionStartStopState(SessionId, StartStopState),
    PeerLeft,
    /// A peer's discovered audio endpoint was seen for the first time or has
    /// since changed. Rust analogue of upstream's `Peers`' `AudioEndpointCallback`,
    /// but edge-triggered: emitted once when a peer is first added (carrying
    /// `None` if it advertises nothing) and thereafter only when the endpoint
    /// differs from the last one recorded, whereas upstream emits on every
    /// `sawPeerOnGateway`. Carries `None` when the peer no longer advertises an
    /// endpoint. Peer departure is reported as `PeerLeft`, not as an endpoint
    /// change — mirroring upstream, which does not invoke the callback from
    /// `peerLeftGateway`.
    AudioEndpoint(NodeId, Option<SocketAddrV4>),
}

fn with_observer_state<R>(
    peers: &Mutex<Vec<ControllerPeer>>,
    self_state: Option<&Mutex<PeerState>>,
    sessions: Option<&Sessions>,
    action: impl FnOnce(Option<&PeerState>, &mut Vec<ControllerPeer>) -> R,
) -> R {
    let current =
        sessions.map(|sessions| sessions.current.lock().unwrap_or_else(|e| e.into_inner()));
    let mut others = sessions.map(|sessions| {
        sessions
            .other_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    });
    let self_state = self_state.map(|state| state.lock().unwrap_or_else(|e| e.into_inner()));
    let mut peers = peers.lock().unwrap_or_else(|e| e.into_inner());
    let result = action(self_state.as_deref(), &mut peers);
    if let (Some(current), Some(others)) = (current, others.as_mut()) {
        // Current-session retry ownership survives temporary absence. A candidate
        // must not survive its last peer, even if its measurement is still running.
        others.retain(|session| {
            session.key().matches(&current)
                || peers
                    .iter()
                    .any(|peer| peer.peer_state.session_id() == session.session_id)
        });
    }
    result
}

async fn observed_peer(
    message: PeerStateMessageType,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    self_peer_state: Arc<Mutex<PeerState>>,
    tx: Sender<Vec<PeerStateChange>>,
    sessions: Option<&Sessions>,
) {
    let deadline = message.expires_at.unwrap_or_else(|| {
        tokio::time::Instant::now() + std::time::Duration::from_secs(u64::from(message.ttl))
    });
    let slot = tokio::select! {
        biased;
        _ = super::messenger::PeerObservation::invalidated(message.observation.as_deref()) => return,
        _ = tokio::time::sleep_until(deadline), if message.ttl > 0 => return,
        result = tx.reserve() => match result { Ok(slot) => slot, Err(_) => return },
    };
    let observation = message.observation.clone();
    with_observer_state(
        &peers,
        Some(&self_peer_state),
        sessions,
        |self_state, peers| {
            let commit = || {
                if message.ttl > 0 && deadline <= tokio::time::Instant::now() {
                    return;
                }
                let changes = if message.ttl == 0 {
                    remove_peer(message.node_state.ident(), peers)
                } else {
                    apply_peer(
                        PeerState {
                            node_state: message.node_state,
                            measurement_endpoint: message.measurement_endpoint,
                            audio_endpoint: message.audio_endpoint,
                        },
                        peers,
                        self_state.unwrap(),
                        Some(deadline),
                    )
                };
                if !changes.is_empty() {
                    slot.send(changes);
                }
            };
            if let Some(observation) = observation {
                observation.while_current(commit);
            } else {
                commit();
            }
        },
    );
}

async fn saw_peer(
    state: PeerState,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    self_peer_state: Arc<Mutex<PeerState>>,
    _session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    tx: Sender<Vec<PeerStateChange>>,
    sessions: Option<&Sessions>,
) {
    let Ok(slot) = tx.reserve().await else {
        return;
    };
    with_observer_state(
        &peers,
        Some(&self_peer_state),
        sessions,
        |self_state, peers| {
            let changes = apply_peer(state, peers, self_state.unwrap(), None);
            if !changes.is_empty() {
                slot.send(changes);
            }
        },
    );
}

fn apply_peer(
    state: PeerState,
    peers: &mut Vec<ControllerPeer>,
    self_peer_state: &PeerState,
    deadline: Option<tokio::time::Instant>,
) -> Vec<PeerStateChange> {
    if state.ident() == self_peer_state.ident() {
        return Vec::new();
    }
    let timeline_changed = !peers.iter().any(|peer| {
        peer.peer_state.session_id() == state.session_id()
            && peer.peer_state.timeline() == state.timeline()
    });
    let start_stop_changed = !peers
        .iter()
        .any(|peer| peer.peer_state.start_stop_state() == state.start_stop_state());
    let previous = peers
        .iter()
        .position(|peer| peer.peer_state.ident() == state.ident());
    let membership_changed =
        previous.is_none_or(|index| peers[index].peer_state.session_id() != state.session_id());
    let audio_changed =
        previous.is_none_or(|index| peers[index].peer_state.audio_endpoint != state.audio_endpoint);
    let mut changes = Vec::new();
    if timeline_changed {
        changes.push(PeerStateChange::SessionTimeline(
            state.session_id(),
            state.timeline(),
        ));
    }
    if start_stop_changed {
        changes.push(PeerStateChange::SessionStartStopState(
            state.session_id(),
            state.start_stop_state(),
        ));
    }
    if membership_changed {
        changes.push(PeerStateChange::SessionMembership);
    }
    // Membership must precede audio callbacks so consumers observe the updated count.
    if audio_changed {
        changes.push(PeerStateChange::AudioEndpoint(
            state.ident(),
            state.audio_endpoint,
        ));
    }
    // State guards may have blocked beyond the receive deadline. Recheck after
    // both guards and the change calculation, immediately before publication.
    if deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
        return Vec::new();
    }
    let peer = ControllerPeer { peer_state: state };
    if let Some(index) = previous {
        peers[index] = peer;
    } else {
        peers.push(peer);
    }
    changes
}

fn remove_peer(node: NodeId, peers: &mut Vec<ControllerPeer>) -> Vec<PeerStateChange> {
    let before = peers.len();
    peers.retain(|peer| peer.peer_state.ident() != node);
    if before != peers.len() {
        vec![PeerStateChange::PeerLeft]
    } else {
        Vec::new()
    }
}

async fn peer_left(
    node_id: NodeId,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx: Sender<Vec<PeerStateChange>>,
    sessions: Option<&Sessions>,
) {
    let Ok(slot) = tx.reserve().await else {
        return;
    };
    with_observer_state(&peers, None, sessions, |_, peers| {
        let changes = remove_peer(node_id, peers);
        if !changes.is_empty() {
            slot.send(changes);
        }
    });
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControllerPeer {
    pub peer_state: PeerState,
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn origin_finality_last_departure_retires_candidate_before_result_or_reappearance() {
        use crate::link::{
            ghostxform::GhostXForm,
            node::NodeState,
            sessions::{Session, SessionMeasurement},
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for timed_out in [false, true] {
                let id = SessionId(NodeId::from_array([1; 8]));
                let first = PeerState { node_state: NodeState::new(id), ..Default::default() };
                let mut second = first.clone(); second.node_state.node_id = NodeId::from_array([3; 8]);
                let peers = Arc::new(Mutex::new(vec![ControllerPeer { peer_state: first.clone() }, ControllerPeer { peer_state: second.clone() }]));
                let (tx_request, mut requests) = mpsc::channel(1);
                let (tx_result, results) = mpsc::channel(1);
                let (tx_join, mut joins) = mpsc::channel(1);
                let (sessions, task) = Sessions::with_dispatch_gate(Session::new(SessionId(NodeId::from_array([2; 8])), Timeline::default(), SessionMeasurement::default()), tx_request, peers.clone(), Clock::new(), tx_join, results, Arc::new(DispatchGate::new_open()));
                let (events, rx_events) = mpsc::channel(1);
                let (changes, mut rx_changes) = mpsc::channel(1);
                let _observer = GatewayObserver::new(rx_events, Arc::new(Mutex::new(PeerState::default())), Arc::new(Mutex::new(SessionPeerCounter::default())), changes, peers.clone(), Arc::new(Notify::new()), Some(sessions.clone())).await;
                sessions.saw_session_timeline(id, Timeline::default()).await;
                let MeasurePeerEvent::PeerState(old, _) = requests.recv().await.unwrap() else { panic!("initial request") };
                events.send(PeerEvent::PeerLeft(first.ident())).await.unwrap(); rx_changes.recv().await.unwrap();
                assert!(sessions.other_sessions.lock().unwrap().iter().any(|session| old.matches(session)), "another live peer preserves the candidate");
                let departure = if timed_out { PeerEvent::PeerTimedOut(second.ident()) } else { PeerEvent::PeerLeft(second.ident()) };
                events.send(departure).await.unwrap(); rx_changes.recv().await.unwrap();
                assert!(!sessions.other_sessions.lock().unwrap().iter().any(|session| old.matches(session)), "last departure retires ownership before its notification is consumed by a controller");
                tx_result.send(MeasurePeerEvent::XForm(old.clone(), GhostXForm { slope: 1.0, intercept: Duration::seconds(3) })).await.unwrap();
                tokio::task::yield_now().await;
                assert!(joins.try_recv().is_err(), "in-flight result cannot join an absent candidate");
                events.send(PeerEvent::SawPeer(first)).await.unwrap(); rx_changes.recv().await.unwrap();
                sessions.saw_session_timeline(id, Timeline::default()).await;
                let MeasurePeerEvent::PeerState(fresh, _) = requests.recv().await.unwrap() else { panic!("fresh request") };
                tx_result.send(MeasurePeerEvent::XForm(old.clone(), GhostXForm::default())).await.unwrap();
                tx_result.send(MeasurePeerEvent::XForm(old, GhostXForm { slope: 1.0, intercept: Duration::seconds(3) })).await.unwrap();
                tx_result.send(MeasurePeerEvent::XForm(fresh.clone(), GhostXForm { slope: 1.0, intercept: Duration::seconds(1) })).await.unwrap();
                let joined = joins.recv().await.unwrap(); assert!(fresh.matches(&joined));
                assert_eq!(peers.lock().unwrap().len(), 1);
                assert!(joins.try_recv().is_err());
                task.abort(); let _ = task.await;
            }
        }).await.unwrap();
    }

    use std::net::IpAddr;

    use chrono::Duration;
    use local_ip_address::list_afinet_netifas;
    use tokio::sync::mpsc;

    use crate::{
        discovery::{
            gateway::{OnEvent, PeerGateway},
            messenger::new_udp_reuseport,
        },
        link::{
            beats::Beats, clock::Clock, measurement::MeasurePeerEvent, sessions::session_peers,
            state::SessionState, tempo::Tempo,
        },
    };

    use super::*;

    #[tokio::test]
    async fn three_gaps_standalone_gateway_preserves_deadline_through_observer_wait() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for explicit in [false, true] {
                let (events, rx_events) = mpsc::channel(1);
                let (changes, mut rx_changes) = mpsc::channel(1);
                changes.send(Vec::new()).await.unwrap();
                let peers = Arc::new(Mutex::new(Vec::new()));
                let _observer = GatewayObserver::new(
                    rx_events,
                    Arc::new(Mutex::new(PeerState::default())),
                    Arc::new(Mutex::new(SessionPeerCounter::default())),
                    changes,
                    peers.clone(),
                    Arc::new(Notify::new()),
                    None,
                )
                .await;
                let start = tokio::time::Instant::now();
                let deadline =
                    start + std::time::Duration::from_millis(if explicit { 100 } else { 1000 });
                super::super::gateway::on_peer_state(
                    PeerStateMessageType {
                        node_state: crate::link::node::NodeState {
                            node_id: NodeId::from_array([83; 8]),
                            ..Default::default()
                        },
                        ttl: 1,
                        expires_at: explicit.then_some(deadline),
                        observation: None,
                        measurement_endpoint: None,
                        audio_endpoint: None,
                    },
                    Arc::new(Mutex::new(Vec::new())),
                    events.clone(),
                    start,
                    Arc::new(Notify::new()),
                    NodeId::default(),
                )
                .await;
                while events.capacity() == 0 {
                    tokio::task::yield_now().await;
                }
                tokio::time::sleep_until(deadline + std::time::Duration::from_millis(30)).await;
                rx_changes.recv().await.unwrap();
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(40), rx_changes.recv())
                        .await
                        .is_err(),
                    "expired standalone gateway input must not publish observer changes"
                );
                assert!(peers.lock().unwrap().is_empty());
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_gaps_observer_rechecks_deadline_after_each_state_lock() {
        for hold_self in [false, true] {
            let mut original = PeerState::default();
            original.node_state.node_id = NodeId::from_array([81; 8]);
            let mut other = original.clone();
            other.node_state.node_id = NodeId::from_array([82; 8]);
            let before = vec![
                ControllerPeer {
                    peer_state: original.clone(),
                },
                ControllerPeer { peer_state: other },
            ];
            let peers = Arc::new(Mutex::new(before.clone()));
            let self_state = Arc::new(Mutex::new(PeerState::default()));
            let held_peers = peers.clone();
            let held_self = self_state.clone();
            let (ready, ready_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let peer_guard = (!hold_self).then(|| held_peers.lock().unwrap());
                let self_guard = hold_self.then(|| held_self.lock().unwrap());
                ready.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
                drop((peer_guard, self_guard));
            });
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let (tx, mut rx) = mpsc::channel(1);
            let probe = tx.clone();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
            let work = tokio::spawn(observed_peer(
                PeerStateMessageType {
                    node_state: original.node_state,
                    ttl: 30,
                    expires_at: Some(deadline),
                    observation: None,
                    measurement_endpoint: Some("127.0.0.1:12345".parse().unwrap()),
                    audio_endpoint: None,
                },
                peers.clone(),
                self_state,
                tx,
                None,
            ));
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while probe.capacity() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep_until(deadline + std::time::Duration::from_millis(20)).await;
            release.send(()).unwrap();
            holder.join().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), work)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                *peers.lock().unwrap(),
                before,
                "expired state must not mutate either peer"
            );
            assert!(
                rx.try_recv().is_err(),
                "expired state must not emit changes"
            );
        }
    }

    #[tokio::test]
    async fn observer_drains_disabled_events_before_admitting_fresh_peers() {
        let gate = Arc::new(DispatchGate::new());
        let (tx_event, rx_event) = mpsc::channel(2);
        let (tx_changes, mut rx_changes) = mpsc::channel(2);
        let peers = Arc::new(Mutex::new(vec![]));
        let observer = GatewayObserver::with_dispatch_gate(
            rx_event,
            Arc::new(Mutex::new(PeerState::default())),
            Arc::new(Mutex::new(SessionPeerCounter::default())),
            tx_changes,
            peers.clone(),
            gate.clone(),
            None,
        )
        .await;
        let (stale, fresh, _) = init_peers();
        for _ in 0..3 {
            tx_event
                .send(PeerEvent::SawPeer(stale.clone()))
                .await
                .unwrap();
            gate.start().await;
            assert!(peers.lock().unwrap().is_empty());
            assert!(rx_changes.try_recv().is_err());
            tx_event
                .send(PeerEvent::SawPeer(fresh.clone()))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), rx_changes.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(peers.lock().unwrap()[0].peer_state.ident(), fresh.ident());
            gate.stop().await;
            observer.reset_peers();
        }
    }

    #[tokio::test]
    async fn observer_exits_when_event_channel_closes() {
        let (tx_event, rx_event) = mpsc::channel(1);
        let (tx_changes, _rx_changes) = mpsc::channel(1);
        let state = Arc::new(Mutex::new(PeerState::default()));
        let probe = Arc::downgrade(&state);
        let mut observer = GatewayObserver::new(
            rx_event,
            state,
            Arc::new(Mutex::new(SessionPeerCounter::default())),
            tx_changes,
            Arc::new(Mutex::new(vec![])),
            Arc::new(Notify::new()),
            None,
        )
        .await;
        drop(tx_event);
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut observer.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(probe.strong_count(), 0);
    }

    #[tokio::test]
    async fn dropping_observer_cancels_a_blocked_state_change() {
        let (tx_event, rx_event) = mpsc::channel(1);
        let (tx_changes, mut rx_changes) = mpsc::channel(1);
        tx_changes.send(vec![]).await.unwrap();
        let peers = Arc::new(Mutex::new(vec![]));
        let state = Arc::new(Mutex::new(PeerState::default()));
        let probe = Arc::downgrade(&state);
        let observer = GatewayObserver::new(
            rx_event,
            state,
            Arc::new(Mutex::new(SessionPeerCounter::default())),
            tx_changes,
            peers.clone(),
            Arc::new(Notify::new()),
            None,
        )
        .await;
        tx_event
            .send(PeerEvent::SawPeer(init_peers().0))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while tx_event.capacity() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            peers.lock().unwrap().is_empty(),
            "capacity must precede peer mutation"
        );
        let task = observer.task.abort_handle();
        drop(observer);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(probe.strong_count(), 0);
        assert!(tx_event.is_closed());
        assert!(rx_changes.recv().await.unwrap().is_empty());
        assert!(rx_changes.recv().await.is_none());
    }

    // fn init_tracing() {
    //     let subscriber = tracing_subscriber::FmtSubscriber::new();
    //     tracing::subscriber::set_global_default(subscriber).unwrap();
    // }

    fn init_peers() -> (PeerState, PeerState, PeerState) {
        (
            PeerState {
                node_state: NodeState {
                    node_id: NodeId::new(),
                    session_id: SessionId(NodeId::new()),
                    timeline: Timeline {
                        tempo: Tempo::new(60.0),
                        beat_origin: Beats::new(1.0),
                        time_origin: Duration::microseconds(1234),
                    },
                    start_stop_state: StartStopState {
                        is_playing: false,
                        beats: Beats::new(0.0),
                        timestamp: Duration::microseconds(2345),
                    },
                },
                measurement_endpoint: None,
                audio_endpoint: None,
            },
            PeerState {
                node_state: NodeState {
                    node_id: NodeId::new(),
                    session_id: SessionId(NodeId::new()),
                    timeline: Timeline {
                        tempo: Tempo::new(120.0),
                        beat_origin: Beats::new(10.0),
                        time_origin: Duration::microseconds(500),
                    },
                    start_stop_state: StartStopState::default(),
                },
                measurement_endpoint: None,
                audio_endpoint: None,
            },
            PeerState {
                node_state: NodeState {
                    node_id: NodeId::new(),
                    session_id: SessionId(NodeId::new()),
                    timeline: Timeline {
                        tempo: Tempo::new(100.0),
                        beat_origin: Beats::new(4.0),
                        time_origin: Duration::microseconds(100),
                    },
                    start_stop_state: StartStopState::default(),
                },
                measurement_endpoint: None,
                audio_endpoint: None,
            },
        )
    }

    async fn init_gateway() -> (
        PeerGateway,
        mpsc::Sender<Vec<PeerStateChange>>,
        Arc<Mutex<u8>>,
    ) {
        let session_id = SessionId::default();
        let node_1 = NodeState::new(session_id);
        let (tx_measure_peer_result, _) = mpsc::channel::<MeasurePeerEvent>(1);
        let (_, rx_measure_peer_state) = mpsc::channel::<MeasurePeerEvent>(1);
        let (tx_event, _) = mpsc::channel::<OnEvent>(1);
        let (tx_peer_state_change, mut rx_peer_state_change) =
            mpsc::channel::<Vec<PeerStateChange>>(1);

        let notifier = Arc::new(Notify::new());

        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();

        tokio::spawn(async move {
            while (rx_peer_state_change.recv().await).is_some() {
                *c.try_lock().unwrap() += 1;
            }
        });

        let ip = list_afinet_netifas()
            .unwrap()
            .iter()
            .find_map(|(_, ip)| match ip {
                IpAddr::V4(ipv4) if !ip.is_loopback() => Some(*ipv4),
                _ => None,
            })
            .unwrap();

        let ping_responder_unicast_socket =
            Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0).into()).unwrap());

        (
            PeerGateway::new(
                Arc::new(Mutex::new(PeerState {
                    node_state: node_1,
                    measurement_endpoint: None,
                    audio_endpoint: None,
                })),
                Arc::new(Mutex::new(SessionState::default())),
                Clock::default(),
                Arc::new(Mutex::new(SessionPeerCounter::default())),
                tx_peer_state_change.clone(),
                tx_event,
                tx_measure_peer_result,
                Arc::new(Mutex::new(vec![])),
                notifier.clone(),
                rx_measure_peer_state,
                ping_responder_unicast_socket,
                Arc::new(Mutex::new(true)), // enabled for test
                None,
            )
            .await
            .unwrap(),
            tx_peer_state_change,
            calls,
        )
    }

    #[tokio::test]
    #[ignore] // Requires multicast networking; "No route to host" on macOS CI
    async fn add_find_peer() {
        // init_tracing();

        let (foo_peer, _, _) = init_peers();

        let (gw, tx_peer_state_change, calls) = init_gateway().await;

        saw_peer(
            foo_peer.clone(),
            gw.observer.peers.clone(),
            gw.peer_state.clone(),
            gw.session_peer_counter.clone(),
            tx_peer_state_change.clone(),
            None,
        )
        .await;

        tokio::time::sleep(Duration::milliseconds(50).to_std().unwrap()).await;

        assert_eq!(
            session_peers(gw.observer.peers.clone(), foo_peer.session_id()),
            vec![ControllerPeer {
                peer_state: foo_peer
            }]
        );
        assert!(*calls.try_lock().unwrap() == 1);
    }

    #[tokio::test]
    #[ignore] // Requires multicast networking; "No route to host" on macOS CI
    async fn add_remove_peer() {
        // init_tracing();

        let (foo_peer, _, _) = init_peers();

        let (gw, tx_peer_state_change, calls) = init_gateway().await;

        saw_peer(
            foo_peer.clone(),
            gw.observer.peers.clone(),
            gw.peer_state.clone(),
            gw.session_peer_counter.clone(),
            tx_peer_state_change.clone(),
            None,
        )
        .await;

        peer_left(
            foo_peer.ident(),
            gw.observer.peers.clone(),
            tx_peer_state_change.clone(),
            None,
        )
        .await;

        tokio::time::sleep(Duration::milliseconds(50).to_std().unwrap()).await;

        assert!(gw.observer.peers.try_lock().unwrap().is_empty());
        assert!(*calls.try_lock().unwrap() == 2);
    }

    #[tokio::test]
    #[ignore] // Requires multicast networking; "No route to host" on macOS CI
    async fn add_two_peers_remove_one_peer() {
        // init_tracing();

        let (foo_peer, bar_peer, _) = init_peers();

        let (gw, tx_peer_state_change, _) = init_gateway().await;

        for peer in [foo_peer.clone(), bar_peer.clone()].iter() {
            saw_peer(
                peer.clone(),
                gw.observer.peers.clone(),
                gw.peer_state.clone(),
                gw.session_peer_counter.clone(),
                tx_peer_state_change.clone(),
                None,
            )
            .await;
        }

        peer_left(
            foo_peer.ident(),
            gw.observer.peers.clone(),
            tx_peer_state_change.clone(),
            None,
        )
        .await;

        tokio::time::sleep(Duration::milliseconds(50).to_std().unwrap()).await;

        assert!(session_peers(gw.observer.peers.clone(), foo_peer.session_id()).is_empty());

        assert_eq!(
            session_peers(gw.observer.peers.clone(), bar_peer.clone().session_id()),
            vec![ControllerPeer {
                peer_state: bar_peer,
            }]
        );
    }

    #[tokio::test]
    async fn saw_peer_notifies_on_audio_endpoint_change() {
        let (mut foo_peer, _, _) = init_peers();

        let peers = Arc::new(Mutex::new(vec![]));
        let self_peer_state = Arc::new(Mutex::new(PeerState {
            node_state: NodeState::new(SessionId::default()),
            measurement_endpoint: None,
            audio_endpoint: None,
        }));
        let session_peer_counter = Arc::new(Mutex::new(SessionPeerCounter::default()));
        let (tx_peer_state_change, mut rx_peer_state_change) =
            mpsc::channel::<Vec<PeerStateChange>>(4);

        // First sighting: no audio endpoint yet.
        saw_peer(
            foo_peer.clone(),
            peers.clone(),
            self_peer_state.clone(),
            session_peer_counter.clone(),
            tx_peer_state_change.clone(),
            None,
        )
        .await;

        let first_changes = rx_peer_state_change.recv().await.unwrap();
        assert!(first_changes.iter().any(
            |c| matches!(c, PeerStateChange::AudioEndpoint(id, None) if *id == foo_peer.ident())
        ));

        // Upstream's `sawPeerOnGateway` invokes `mSessionMembershipCallback()`
        // before `mAudioEndpointCallback(...)`, and that order matters: the
        // membership change is what updates the peer counter and drives any
        // session reset, so a consumer reacting to the endpoint must not see
        // pre-membership state. A first sighting emits both, so it is the case
        // that pins the order.
        let membership_at = first_changes
            .iter()
            .position(|c| matches!(c, PeerStateChange::SessionMembership))
            .expect("a first sighting must report session membership");
        let endpoint_at = first_changes
            .iter()
            .position(|c| matches!(c, PeerStateChange::AudioEndpoint(..)))
            .expect("a first sighting must report the audio endpoint");
        assert!(
            membership_at < endpoint_at,
            "session membership must be queued before the audio endpoint, \
             got membership at {membership_at} and endpoint at {endpoint_at}"
        );

        // The peer starts advertising an audio endpoint: expect a notification
        // carrying the new endpoint.
        let endpoint = SocketAddrV4::new(std::net::Ipv4Addr::new(127, 0, 0, 1), 12345);
        foo_peer.audio_endpoint = Some(endpoint);

        saw_peer(
            foo_peer.clone(),
            peers.clone(),
            self_peer_state.clone(),
            session_peer_counter.clone(),
            tx_peer_state_change.clone(),
            None,
        )
        .await;

        let second_changes = rx_peer_state_change.recv().await.unwrap();
        assert!(second_changes.iter().any(|c| matches!(
            c,
            PeerStateChange::AudioEndpoint(id, Some(ep)) if *id == foo_peer.ident() && *ep == endpoint
        )));

        // Seeing the same peer state again must not re-fire the callback.
        saw_peer(
            foo_peer.clone(),
            peers.clone(),
            self_peer_state.clone(),
            session_peer_counter,
            tx_peer_state_change,
            None,
        )
        .await;

        assert!(rx_peer_state_change.try_recv().is_err());
    }
}
