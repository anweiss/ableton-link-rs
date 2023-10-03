use std::{
    net::{SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    vec,
};

use tokio::sync::mpsc::{Receiver, Sender};
use tracing::info;

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
}

pub enum PeerEvent {
    SawPeer(NodeState),
    PeerLeft(NodeId),
    PeerTimedOut(NodeId),
}

pub struct GatewayObserver {
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
}

impl GatewayObserver {
    pub async fn new(
        mut on_peer_event: Receiver<PeerEvent>,
        session_id: Arc<Mutex<SessionId>>,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<PeerStateChange>,
    ) -> Self {
        let peers = Arc::new(Mutex::new(vec![]));
        let gwo = GatewayObserver { peers };

        let peers = gwo.peers.clone();
        let session_id = session_id.clone();

        tokio::spawn(async move {
            loop {
                match on_peer_event.recv().await {
                    Some(PeerEvent::SawPeer(node_state)) => {
                        saw_peer(
                            node_state,
                            peers.clone(),
                            session_id.clone(),
                            session_peer_counter.clone(),
                            tx_peer_state_change.clone(),
                        )
                        .await
                    }
                    Some(PeerEvent::PeerLeft(node_id)) => peer_left(node_id, peers.clone()).await,
                    Some(PeerEvent::PeerTimedOut(node_id)) => {
                        peer_left(node_id, peers.clone()).await
                    }
                    None => continue,
                }
            }
        });

        gwo
    }

    pub fn session_peers(&self, session_id: Arc<Mutex<SessionId>>) -> Vec<ControllerPeer> {
        let mut peers = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p.peer_state.session_id().lock().unwrap() == *session_id.lock().unwrap())
            .cloned()
            .collect::<Vec<_>>();
        peers.sort_by(|a, b| a.peer_state.ident().cmp(&b.peer_state.ident()));

        peers
    }

    pub fn set_session_timeline(&mut self, session_id: Arc<Mutex<SessionId>>, timeline: Timeline) {
        self.peers.lock().unwrap().iter_mut().for_each(|peer| {
            if *peer.peer_state.session_id().lock().unwrap() == *session_id.lock().unwrap() {
                peer.peer_state.node_state.timeline = timeline;
            }
        });
    }

    pub fn forget_session(&mut self, session_id: Arc<Mutex<SessionId>>) {
        self.peers.lock().unwrap().retain(|peer| {
            *peer.peer_state.session_id().lock().unwrap() != *session_id.lock().unwrap()
        });
    }

    pub fn reset_peers(&self) {
        self.peers.lock().unwrap().clear();
    }
}

#[derive(Debug, Clone)]
pub struct PeerState {
    pub node_state: NodeState,
    pub endpoint: Option<SocketAddrV4>,
}

impl PeerState {
    pub fn ident(&self) -> NodeId {
        self.node_state.ident()
    }

    pub fn session_id(&self) -> Arc<Mutex<SessionId>> {
        self.node_state.session_id.clone()
    }

    pub fn timeline(&self) -> Timeline {
        self.node_state.timeline
    }

    pub fn start_stop_state(&self) -> StartStopState {
        self.node_state.start_stop_state
    }
}

async fn session_timeline_exists(_session_id: SessionId, _timeline: Timeline) -> bool {
    todo!()
}

pub fn unique_session_peer_count(
    session_id: Arc<Mutex<SessionId>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
) -> usize {
    let mut peers = peers
        .lock()
        .unwrap()
        .iter()
        .filter(|p| *p.peer_state.session_id().lock().unwrap() == *session_id.lock().unwrap())
        .cloned()
        .collect::<Vec<_>>();
    peers.dedup_by(|a, b| a.peer_state.ident() == b.peer_state.ident());

    peers.len()
}

#[derive(Clone, Copy)]
pub enum PeerStateChange {
    SessionTimeline,
    SessionStartStopState,
    SessionMembership,
}

async fn saw_peer(
    node_state: NodeState,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_id: Arc<Mutex<SessionId>>,
    session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    tx_peer_state_change: Sender<PeerStateChange>,
) {
    info!("saw peer {}", node_state.ident());

    let ps = PeerState {
        node_state,
        endpoint: None,
    };

    let peer_session = ps.session_id();
    let peer_timeline = ps.timeline();
    let peer_start_stop_state = ps.start_stop_state();

    let is_new_session_timeline = !peers.lock().unwrap().iter().any(|p| {
        *p.peer_state.session_id().lock().unwrap() == *peer_session.lock().unwrap()
            && p.peer_state.timeline() == peer_timeline
    });

    let is_new_session_start_stop_state = !peers
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.peer_state.start_stop_state() == peer_start_stop_state);

    let peer = ControllerPeer { peer_state: ps };

    let did_session_membership_change =
        if !peers
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.peer_state.ident() == peer.peer_state.ident())
        {
            peers.lock().unwrap().push(peer);
            true
        } else {
            peers.lock().unwrap().iter().any(|p| {
                *p.peer_state.session_id().lock().unwrap() != *peer_session.lock().unwrap()
            })
        };

    if is_new_session_timeline {
        let _ = tx_peer_state_change
            .send(PeerStateChange::SessionTimeline)
            .await;
    }

    if did_session_membership_change {
        let count = unique_session_peer_count(session_id, peers);
        let old_count = session_peer_counter.lock().unwrap().session_peer_count;
        if old_count != count {
            if count == 0 {
                let _ = tx_peer_state_change
                    .send(PeerStateChange::SessionMembership)
                    .await;
            }
        }
    }
}

async fn peer_left(node_id: NodeId, peers: Arc<Mutex<Vec<ControllerPeer>>>) {
    info!("peer {} left", node_id);

    let mut did_session_membership_change = false;
    peers.lock().unwrap().retain(|peer| {
        if peer.peer_state.ident() == node_id {
            did_session_membership_change = true;
            false
        } else {
            true
        }
    })
}

async fn peer_timed_out(_peer_id: SocketAddr, _new_state: PeerStateMessageType) {
    todo!()
}

#[derive(Debug, Clone)]
pub struct ControllerPeer {
    pub peer_state: PeerState,
}
