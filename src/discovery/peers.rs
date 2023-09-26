use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    vec,
};

use tokio::sync::mpsc::Receiver;
use tracing::info;

use crate::link::{
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
    pub async fn new(mut on_peer_event: Receiver<PeerEvent>) -> Self {
        let peers = Arc::new(Mutex::new(vec![]));
        let gwo = GatewayObserver { peers };

        let peers = gwo.peers.clone();

        tokio::spawn(async move {
            loop {
                match on_peer_event.recv().await {
                    Some(PeerEvent::SawPeer(node_state)) => {
                        saw_peer(node_state, peers.clone()).await
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
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct PeerState {
    pub node_state: NodeState,
    pub endpoint: Option<SocketAddr>,
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

async fn session_timeline_exists(session_id: SessionId, timeline: Timeline) -> bool {
    todo!()
}

async fn saw_peer(node_state: NodeState, peers: Arc<Mutex<Vec<ControllerPeer>>>) {
    info!("saw peer {}", node_state.ident());

    let ps = PeerState {
        node_state,
        endpoint: None,
    };

    let peer_session = ps.session_id();
    let peer_timeline = ps.timeline();
    let peer_start_stop_state = ps.start_stop_state();

    let mut peers = peers.lock().unwrap();

    let is_new_session_timeline = !peers.iter().any(|p| {
        p.peer_state.session_id() == peer_session && p.peer_state.timeline() == peer_timeline
    });

    let is_new_session_start_stop_state = !peers
        .iter()
        .any(|p| p.peer_state.start_stop_state() == peer_start_stop_state);

    let peer = ControllerPeer { peer_state: ps };

    let mut did_session_membership_change = false;
    if !peers
        .iter()
        .any(|p| p.peer_state.ident() == peer.peer_state.ident())
    {
        did_session_membership_change = true;
        peers.push(peer);
    } else {
        did_session_membership_change = peers
            .iter()
            .any(|p| p.peer_state.session_id() != peer_session);
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

async fn peer_timed_out(peer_id: SocketAddr, new_state: PeerStateMessageType) {
    todo!()
}

#[derive(Debug, Default)]
pub struct ControllerPeer {
    pub peer_state: PeerState,
}
