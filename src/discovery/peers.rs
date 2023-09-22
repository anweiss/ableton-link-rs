use std::{collections::HashSet, io::Error, net::SocketAddr, result::Result, sync::Arc};

use async_trait::async_trait;
use tokio::sync::{mpsc::Receiver, Mutex};

use crate::link::node::NodeState;

#[derive(Clone, Copy)]
pub struct PeerState {
    pub node_state: NodeState,
    pub ttl: u8,
}

pub enum PeerEvent {
    SawPeer(NodeState),
    PeerLeft(NodeState),
    PeerTimedOut(NodeState),
}

pub struct GatewayObserver {
    peers: Arc<Mutex<ControllerPeers>>,
}

impl GatewayObserver {
    pub async fn new(mut on_peer_event: Receiver<PeerEvent>) -> Self {
        let peers = Arc::new(Mutex::new(ControllerPeers::default()));
        let gwo = GatewayObserver {
            peers: peers.clone(),
        };

        let peers = peers.clone();

        tokio::spawn(async move {
            loop {
                match on_peer_event.recv().await {
                    Some(PeerEvent::SawPeer(node_state)) => {
                        saw_peer(&node_state, peers.clone()).await
                    }
                    Some(PeerEvent::PeerLeft(node_state)) => todo!(),
                    Some(PeerEvent::PeerTimedOut(node_state)) => todo!(),
                    None => continue,
                }
            }
        });

        gwo
    }
}

async fn saw_peer(peer_id: &NodeState, peers: Arc<Mutex<ControllerPeers>>) {
    todo!()
}

async fn peer_left(peer_id: SocketAddr) {
    todo!()
}

async fn peer_timed_out(peer_id: SocketAddr, new_state: PeerState) {
    todo!()
}

#[derive(Default)]
pub struct ControllerPeers {}
