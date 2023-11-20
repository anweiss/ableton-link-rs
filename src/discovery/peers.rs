use std::{
    net::SocketAddrV4,
    sync::{Arc, Mutex},
    vec,
};

use tokio::{
    select,
    sync::{
        mpsc::{Receiver, Sender},
        Notify,
    },
};
use tracing::{debug, info};

use crate::link::{
    controller::SessionPeerCounter,
    node::{NodeId, NodeState},
    sessions::SessionId,
    state::StartStopState,
    timeline::Timeline,
};

#[derive(Clone)]
pub struct PeerStateMessageType {
    pub node_state: NodeState,
    pub ttl: u8,
    pub measurement_endpoint: Option<SocketAddrV4>,
}

pub enum PeerEvent {
    SawPeer(PeerState),
    PeerLeft(NodeId),
    PeerTimedOut(NodeId),
}

pub struct GatewayObserver {
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
}

impl GatewayObserver {
    pub async fn new(
        mut on_peer_event: Receiver<PeerEvent>,
        peer_state: Arc<Mutex<PeerState>>,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        _notifier: Arc<Notify>,
    ) -> Self {
        let gwo = GatewayObserver { peers };

        let peers = gwo.peers.clone();
        let peer_state_loop = peer_state.clone();

        tokio::spawn(async move {
            loop {
                select! {
                    peer_event = on_peer_event.recv() => {
                        match peer_event {
                            Some(PeerEvent::SawPeer(peer_state)) => {
                                saw_peer(
                                    peer_state,
                                    peers.clone(),
                                    peer_state_loop.clone(),
                                    session_peer_counter.clone(),
                                    tx_peer_state_change.clone(),
                                )
                                .await
                            }
                            Some(PeerEvent::PeerLeft(node_id)) => peer_left(node_id, peers.clone(), tx_peer_state_change.clone()).await,
                            Some(PeerEvent::PeerTimedOut(node_id)) => {
                                peer_left(node_id, peers.clone(), tx_peer_state_change.clone()).await
                            }
                            None => continue,
                        }
                    }
                    // _ = notifier.notified() => {
                    //     break;
                    // }
                }
            }
        });

        gwo
    }

    pub fn set_session_timeline(&mut self, session_id: SessionId, timeline: Timeline) {
        self.peers.try_lock().unwrap().iter_mut().for_each(|peer| {
            if peer.peer_state.session_id() == session_id {
                peer.peer_state.node_state.timeline = timeline;
            }
        });
    }

    pub fn forget_session(&mut self, session_id: Arc<Mutex<SessionId>>) {
        self.peers
            .try_lock()
            .unwrap()
            .retain(|peer| peer.peer_state.session_id() != *session_id.try_lock().unwrap());
    }

    pub fn reset_peers(&self) {
        self.peers.try_lock().unwrap().clear();
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerState {
    pub node_state: NodeState,
    pub measurement_endpoint: Option<SocketAddrV4>,
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
) -> usize {
    let mut peers = peers
        .try_lock()
        .unwrap()
        .iter()
        .filter(|p| p.peer_state.session_id() == session_id)
        .cloned()
        .collect::<Vec<_>>();
    peers.dedup_by(|a, b| a.peer_state.ident() == b.peer_state.ident());

    peers.len()
}

#[derive(Clone)]
pub enum PeerStateChange {
    SessionMembership,
    SessionTimeline(SessionId, Timeline),
    SessionStartStopState(SessionId, StartStopState),
    PeerLeft,
}

async fn saw_peer(
    peer_seen_peer_state: PeerState,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    _self_peer_state: Arc<Mutex<PeerState>>,
    _session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    tx_peer_state_change: Sender<Vec<PeerStateChange>>,
) {
    let ps = PeerState {
        node_state: peer_seen_peer_state.node_state,
        measurement_endpoint: peer_seen_peer_state.measurement_endpoint,
    };

    let peer_session = ps.session_id();
    let peer_timeline = ps.timeline();
    let peer_start_stop_state = ps.start_stop_state();

    let is_new_session_timeline = !peers.try_lock().unwrap().iter().any(|p| {
        p.peer_state.session_id() == peer_session && p.peer_state.timeline() == peer_timeline
    });

    let is_new_session_start_stop_state = !peers
        .try_lock()
        .unwrap()
        .iter()
        .any(|p| p.peer_state.start_stop_state() == peer_start_stop_state);

    let peer = ControllerPeer { peer_state: ps };

    let did_session_membership_change = if !peers
        .try_lock()
        .unwrap()
        .iter()
        .any(|p| p.peer_state.ident() == peer.peer_state.ident())
    {
        peers.try_lock().unwrap().push(peer.clone());
        true
    } else {
        peers
            .try_lock()
            .unwrap()
            .iter()
            .all(|p| p.peer_state.session_id() != peer_session)
    };

    let mut peer_state_changes = vec![];

    if is_new_session_timeline {
        debug!("session timeline changed");
        peer_state_changes.push(PeerStateChange::SessionTimeline(
            peer_session,
            peer_timeline,
        ));
    }

    if is_new_session_start_stop_state {
        debug!("session start stop changed");
        peer_state_changes.push(PeerStateChange::SessionStartStopState(
            peer_session,
            peer_start_stop_state,
        ));
    }

    if did_session_membership_change {
        debug!("session membership changed");
        peer_state_changes.push(PeerStateChange::SessionMembership);
    }

    if !peer_state_changes.is_empty() {
        debug!("sending peer state changes to controller");
        tx_peer_state_change.send(peer_state_changes).await.unwrap();
    }
}

async fn peer_left(
    node_id: NodeId,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_peer_state_change: Sender<Vec<PeerStateChange>>,
) {
    info!("peer {} left", node_id);

    let mut did_session_membership_change = false;
    peers.try_lock().unwrap().retain(|peer| {
        if peer.peer_state.ident() == node_id {
            did_session_membership_change = true;
            false
        } else {
            true
        }
    });

    if did_session_membership_change {
        tx_peer_state_change
            .send(vec![PeerStateChange::PeerLeft])
            .await
            .unwrap();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControllerPeer {
    pub peer_state: PeerState,
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use tokio::sync::mpsc;

    use crate::{
        discovery::gateway::{OnEvent, PeerGateway},
        link::{
            beats::Beats, clock::Clock, ghostxform::GhostXForm, measurement::MeasurePeerEvent,
            sessions::session_peers, state::SessionState, tempo::Tempo,
        },
    };

    use super::*;

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
            },
        )
    }

    async fn init_gateway() -> (
        PeerGateway,
        mpsc::Sender<Vec<PeerStateChange>>,
        Arc<Mutex<u8>>,
    ) {
        let session_id = SessionId::default();
        let node_1 = NodeState::new(session_id.clone());
        let (tx_measure_peer_result, _) = mpsc::channel::<MeasurePeerEvent>(1);
        let (_, rx_measure_peer_state) = mpsc::channel::<MeasurePeerEvent>(1);
        let (tx_event, _) = mpsc::channel::<OnEvent>(1);
        let (tx_peer_state_change, mut rx_peer_state_change) =
            mpsc::channel::<Vec<PeerStateChange>>(1);

        let notifier = Arc::new(Notify::new());

        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();

        tokio::spawn(async move {
            while let Some(_) = rx_peer_state_change.recv().await {
                *c.try_lock().unwrap() += 1;
            }
        });

        (
            PeerGateway::new(
                Arc::new(Mutex::new(PeerState {
                    node_state: node_1,
                    measurement_endpoint: None,
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
            )
            .await,
            tx_peer_state_change,
            calls,
        )
    }

    #[tokio::test]
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
        )
        .await;

        peer_left(
            foo_peer.ident(),
            gw.observer.peers.clone(),
            tx_peer_state_change.clone(),
        )
        .await;

        tokio::time::sleep(Duration::milliseconds(50).to_std().unwrap()).await;

        assert!(gw.observer.peers.try_lock().unwrap().is_empty());
        assert!(*calls.try_lock().unwrap() == 2);
    }

    #[tokio::test]
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
            )
            .await;
        }

        peer_left(
            foo_peer.ident(),
            gw.observer.peers.clone(),
            tx_peer_state_change.clone(),
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
}
