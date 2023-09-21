use std::{collections::HashSet, io::Error, net::SocketAddr, result::Result};

use crate::link::node::NodeState;

pub trait PeerObserver {
    fn saw_peer(&mut self, peer_id: SocketAddr);
    fn peer_left(&mut self, peer_id: &SocketAddr);
    fn peer_timed_out(&mut self, peer_id: SocketAddr, new_state: PeerState);
}

#[derive(Clone, Copy)]
pub struct PeerState {
    pub node_state: NodeState,
    pub ttl: u8,
}

#[derive(Default)]
pub struct GatewayObserver {
    peers: ControllerPeers,
}

impl PeerObserver for GatewayObserver {
    fn saw_peer(&mut self, peer_id: SocketAddr) {
        todo!()
    }

    fn peer_left(&mut self, peer_id: &SocketAddr) {
        todo!()
    }

    fn peer_timed_out(&mut self, peer_id: SocketAddr, new_state: PeerState) {
        todo!()
    }
}

#[derive(Default)]
pub struct ControllerPeers {}
