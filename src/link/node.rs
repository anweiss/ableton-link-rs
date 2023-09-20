use bincode::{Decode, Encode};
use rand::Rng;

use super::{sessions::SessionId, state::StartStopState, timeline::Timeline};

pub type NodeIdArray = [u8; 8];

#[derive(Default, Clone, Copy, Debug, Encode, Decode, Eq, PartialEq)]
pub struct NodeId(NodeIdArray);

impl NodeId {
    pub fn from_array(rhs: NodeIdArray) -> Self {
        NodeId(rhs)
    }

    pub fn random<R: Rng>(mut rng: R) -> Self {
        let mut node_id = [0; 8];
        rng.fill(&mut node_id);
        NodeId(node_id)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NodeState {
    pub node_id: NodeId,
    pub session_id: SessionId,
    pub timeline: Timeline,
    pub start_stop_state: StartStopState,
}

impl Default for NodeState {
    fn default() -> Self {
        let mut rng = rand::thread_rng();
        let node_id = NodeId::random(&mut rng);
        let session_id = node_id;

        NodeState {
            node_id,
            session_id,
            timeline: Timeline::default(),
            start_stop_state: StartStopState::default(),
        }
    }
}

impl NodeState {
    pub fn ident(&self) -> NodeId {
        self.node_id
    }
}
