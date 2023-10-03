use core::fmt;
use std::{
    fmt::Display,
    sync::{Arc, Mutex},
};

use bincode::{Decode, Encode};
use rand::Rng;

use super::{
    payload::{Payload, PayloadEntry},
    sessions::SessionId,
    state::StartStopState,
    timeline::Timeline,
};

pub type NodeIdArray = [u8; 8];

#[derive(Default, Clone, Copy, Debug, Encode, Decode, Eq, PartialEq, PartialOrd, Ord)]
pub struct NodeId(NodeIdArray);

impl Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl NodeId {
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        NodeId::random(&mut rng)
    }

    pub fn from_array(rhs: NodeIdArray) -> Self {
        NodeId(rhs)
    }

    pub fn random<R: Rng>(mut rng: R) -> Self {
        let mut node_id = [0; 8];
        rng.fill(&mut node_id);
        NodeId(node_id)
    }
}

#[derive(Debug, Clone)]
pub struct NodeState {
    pub node_id: NodeId,
    pub session_id: Arc<Mutex<SessionId>>,
    pub timeline: Timeline,
    pub start_stop_state: StartStopState,
}

impl NodeState {
    pub fn new(session_id: Arc<Mutex<SessionId>>) -> Self {
        let node_id = NodeId::new();

        NodeState {
            node_id,
            session_id,
            timeline: Timeline::default(),
            start_stop_state: StartStopState::default(),
        }
    }

    pub fn ident(&self) -> NodeId {
        self.node_id
    }

    pub fn from_payload(node_id: NodeId, payload: &Payload) -> Self {
        let mut node_state = NodeState {
            node_id,
            session_id: Arc::new(Mutex::new(SessionId::default())),
            timeline: Timeline::default(),
            start_stop_state: StartStopState::default(),
        };

        for entry in &payload.entries {
            match entry {
                PayloadEntry::Timeline(tl) => {
                    node_state.timeline = *tl;
                }
                PayloadEntry::SessionMembership(sm) => {
                    node_state.session_id = Arc::new(Mutex::new(sm.session_id));
                }
                PayloadEntry::StartStopState(ststst) => {
                    node_state.start_stop_state = *ststst;
                }
                _ => continue,
            }
        }

        node_state
    }
}
