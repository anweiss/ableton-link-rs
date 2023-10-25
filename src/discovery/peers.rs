use std::{
    net::{SocketAddr, SocketAddrV4},
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
        session_id: SessionId,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        notifier: Arc<Notify>,
    ) -> Self {
        let gwo = GatewayObserver { peers };

        let peers = gwo.peers.clone();
        let session_id = session_id.clone();

        tokio::spawn(async move {
            loop {
                select! {
                    peer_event = on_peer_event.recv() => {
                        match peer_event {
                            Some(PeerEvent::SawPeer(peer_state)) => {
                                saw_peer(
                                    peer_state.node_state,
                                    peer_state.measurement_endpoint,
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
                    // _ = notifier.notified() => {
                    //     break;
                    // }
                }
            }
        });

        gwo
    }

    pub fn set_session_timeline(&mut self, session_id: SessionId, timeline: Timeline) {
        self.peers.lock().unwrap().iter_mut().for_each(|peer| {
            if peer.peer_state.session_id() == session_id {
                peer.peer_state.node_state.timeline = timeline;
            }
        });
    }

    pub fn forget_session(&mut self, session_id: Arc<Mutex<SessionId>>) {
        self.peers
            .lock()
            .unwrap()
            .retain(|peer| peer.peer_state.session_id() != *session_id.lock().unwrap());
    }

    pub fn reset_peers(&self) {
        self.peers.lock().unwrap().clear();
    }
}

#[derive(Debug, Clone)]
pub struct PeerState {
    pub node_state: NodeState,
    pub measurement_endpoint: Option<SocketAddrV4>,
}

impl PeerState {
    pub fn ident(&self) -> NodeId {
        self.node_state.ident()
    }

    pub fn session_id(&self) -> SessionId {
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
    session_id: SessionId,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
) -> usize {
    let mut peers = peers
        .lock()
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
}

async fn saw_peer(
    node_state: NodeState,
    endpoint: Option<SocketAddrV4>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_id: SessionId,
    session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    tx_peer_state_change: Sender<Vec<PeerStateChange>>,
) {
    info!("saw peer {}", node_state.ident());

    let ps = PeerState {
        node_state,
        measurement_endpoint: endpoint,
    };

    let peer_session = ps.session_id();
    let peer_timeline = ps.timeline();
    let peer_start_stop_state = ps.start_stop_state();

    let is_new_session_timeline = !peers.lock().unwrap().iter().any(|p| {
        p.peer_state.session_id() == peer_session && p.peer_state.timeline() == peer_timeline
    });

    let is_new_session_start_stop_state = !peers
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.peer_state.start_stop_state() == peer_start_stop_state);

    let peer = ControllerPeer { peer_state: ps };

    let did_session_membership_change = if !peers
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.peer_state.ident() == peer.peer_state.ident())
    {
        peers.lock().unwrap().push(peer);
        true
    } else {
        peers
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.peer_state.session_id() != peer_session)
    };

    let mut peer_state_changes = vec![];

    if is_new_session_timeline {
        info!("session timeline changed");
        peer_state_changes.push(PeerStateChange::SessionTimeline(
            peer_session.clone(),
            peer_timeline,
        ));
    }

    if is_new_session_start_stop_state {
        info!("session start stop changed");
        peer_state_changes.push(PeerStateChange::SessionStartStopState(
            peer_session,
            peer_start_stop_state,
        ));
    }

    if did_session_membership_change {
        info!("session membership changed");
        let count = unique_session_peer_count(session_id, peers.clone());
        let old_count = session_peer_counter.lock().unwrap().session_peer_count;
        if old_count != count {
            if count == 0 {
                peer_state_changes.push(PeerStateChange::SessionMembership);
            }
        }
    }

    info!("sending peer state changes to controller");
    let _ = tx_peer_state_change.send(peer_state_changes).await.unwrap();
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
