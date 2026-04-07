use core::fmt;
use core::fmt::Display;

use bincode::{Decode, Encode};
use rand::{
    distributions::{Distribution, Uniform},
    Rng,
};

#[cfg(feature = "std")]
use super::payload::{Payload, PayloadEntry};
#[cfg(feature = "std")]
use super::sessions::SessionId;
#[cfg(feature = "std")]
use super::state::StartStopState;
#[cfg(feature = "std")]
use super::timeline::Timeline;

pub type NodeIdArray = [u8; 8];

#[derive(Default, Clone, Copy, Debug, Encode, Decode, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub NodeIdArray);

impl Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter) -> core::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl NodeId {
    #[cfg(feature = "std")]
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        NodeId::random(&mut rng)
    }

    pub fn from_array(rhs: NodeIdArray) -> Self {
        NodeId(rhs)
    }

    pub fn random<R: Rng>(mut rng: R) -> Self {
        let dist = Uniform::from(33..127);
        let arr: [u8; 8] = core::array::from_fn(|_| dist.sample(&mut rng));
        NodeId(arr)
    }
}

#[cfg(feature = "std")]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeState {
    pub node_id: NodeId,
    pub session_id: SessionId,
    pub timeline: Timeline,
    pub start_stop_state: StartStopState,
}

#[cfg(feature = "std")]
impl NodeState {
    pub fn new(session_id: SessionId) -> Self {
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
            session_id: SessionId::default(),
            timeline: Timeline::default(),
            start_stop_state: StartStopState::default(),
        };

        for entry in &payload.entries {
            match entry {
                PayloadEntry::Timeline(tl) => {
                    node_state.timeline = *tl;
                }
                PayloadEntry::SessionMembership(sm) => {
                    node_state.session_id = sm.session_id;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{
        beats::Beats,
        sessions::{SessionId, SessionMembership},
        state::StartStopState,
        tempo::Tempo,
        timeline::Timeline,
    };
    use chrono::Duration;

    #[test]
    fn node_id_from_array() {
        let arr = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let id = NodeId::from_array(arr);
        assert_eq!(id.0, arr);
    }

    #[test]
    fn node_id_new_is_random() {
        let id1 = NodeId::new();
        let id2 = NodeId::new();
        // Extremely unlikely to be equal
        assert_ne!(id1, id2);
    }

    #[test]
    fn node_id_display_is_hex() {
        let id = NodeId::from_array([0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48]);
        assert_eq!(format!("{}", id), "4142434445464748");
    }

    #[test]
    fn node_state_new_has_defaults() {
        let session_id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let ns = NodeState::new(session_id);
        assert_eq!(ns.session_id, session_id);
        assert_eq!(ns.timeline, Timeline::default());
        assert_eq!(ns.start_stop_state, StartStopState::default());
        // node_id should be randomly generated
        assert_ne!(ns.node_id, NodeId::default());
    }

    #[test]
    fn node_state_ident_returns_node_id() {
        let session_id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let ns = NodeState::new(session_id);
        assert_eq!(ns.ident(), ns.node_id);
    }

    #[test]
    fn node_state_from_payload_extracts_all_fields() {
        let node_id = NodeId::from_array([10, 20, 30, 40, 50, 60, 70, 80]);
        let session_id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let timeline = Timeline {
            tempo: Tempo::new(130.0),
            beat_origin: Beats::new(2.0),
            time_origin: Duration::microseconds(500_000),
        };
        let sss = StartStopState {
            is_playing: true,
            beats: Beats::new(3.0),
            timestamp: Duration::microseconds(1_000_000),
        };

        let payload = Payload {
            entries: vec![
                PayloadEntry::Timeline(timeline),
                PayloadEntry::SessionMembership(SessionMembership::from(session_id)),
                PayloadEntry::StartStopState(sss),
            ],
        };

        let ns = NodeState::from_payload(node_id, &payload);
        assert_eq!(ns.node_id, node_id);
        assert_eq!(ns.session_id, session_id);
        assert_eq!(ns.timeline.tempo.bpm(), 130.0);
        assert!(ns.start_stop_state.is_playing);
    }

    #[test]
    fn node_state_from_payload_ignores_extra_entries() {
        let node_id = NodeId::from_array([1, 1, 1, 1, 1, 1, 1, 1]);
        let payload = Payload {
            entries: vec![PayloadEntry::HostTime(crate::link::payload::HostTime::new(
                Duration::zero(),
            ))],
        };
        let ns = NodeState::from_payload(node_id, &payload);
        assert_eq!(ns.node_id, node_id);
        assert_eq!(ns.session_id, SessionId::default());
        assert_eq!(ns.timeline, Timeline::default());
    }
}
