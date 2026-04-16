use std::mem;

use chrono::Duration;
use tracing::{debug, warn};

use crate::{
    discovery::peers::PeerState,
    encoding::{self, Decode},
    link::{
        encoding::{PayloadEntryHeader, PAYLOAD_ENTRY_HEADER_SIZE},
        measurement::{
            MeasurementEndpointV4, MEASUREMENT_ENDPOINT_V4_HEADER_KEY, MEASUREMENT_ENDPOINT_V4_SIZE,
        },
        node::NodeState,
        sessions::{SessionMembership, SESSION_MEMBERSHIP_HEADER_KEY, SESSION_MEMBERSHIP_SIZE},
        state::{StartStopState, START_STOP_STATE_HEADER_KEY, START_STOP_STATE_SIZE},
        timeline::{Timeline, TIMELINE_HEADER_KEY, TIMELINE_SIZE},
    },
};

use super::Result;

pub const HOST_TIME_HEADER_KEY: u32 = u32::from_be_bytes(*b"__ht");
pub const HOST_TIME_SIZE: u32 = mem::size_of::<u64>() as u32;
pub const HOST_TIME_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: HOST_TIME_HEADER_KEY,
    size: HOST_TIME_SIZE,
};

pub const GHOST_TIME_HEADER_KEY: u32 = u32::from_be_bytes(*b"__gt");
pub const GHOST_TIME_SIZE: u32 = mem::size_of::<u64>() as u32;
pub const GHOST_TIME_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: GHOST_TIME_HEADER_KEY,
    size: GHOST_TIME_SIZE,
};

pub const PREV_GHOST_TIME_HEADER_KEY: u32 = u32::from_be_bytes(*b"_pgt");
pub const PREV_GHOST_TIME_SIZE: u32 = mem::size_of::<u64>() as u32;
pub const PREV_GHOST_TIME_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: PREV_GHOST_TIME_HEADER_KEY,
    size: PREV_GHOST_TIME_SIZE,
};

#[derive(Default, Debug)]
pub struct Payload {
    pub entries: Vec<PayloadEntry>,
}

impl Payload {
    pub fn size(&self) -> u32 {
        let mut size = 0;
        for entry in &self.entries {
            size += PAYLOAD_ENTRY_HEADER_SIZE as u32 + entry.size();
        }

        size
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = Vec::new();

        for entry in &self.entries {
            let mut encoded_entry = entry.encode()?;
            encoded.append(&mut encoded_entry);
        }

        Ok(encoded)
    }
}

impl From<NodeState> for Payload {
    fn from(value: NodeState) -> Self {
        Payload {
            entries: vec![
                PayloadEntry::Timeline(value.timeline),
                PayloadEntry::SessionMembership((value.session_id).into()),
                PayloadEntry::StartStopState(value.start_stop_state),
            ],
        }
    }
}

impl From<PeerState> for Payload {
    fn from(value: PeerState) -> Self {
        let mut payload = Payload::from(value.node_state);
        payload
            .entries
            .push(PayloadEntry::MeasurementEndpointV4(MeasurementEndpointV4 {
                endpoint: value.measurement_endpoint,
            }));
        payload
    }
}

pub fn decode(payload: &mut Payload, data: &[u8]) -> Result<()> {
    if PAYLOAD_ENTRY_HEADER_SIZE > data.len() {
        return Ok(());
    }

    let (payload_entry_header, _) =
        encoding::decode_from_slice::<PayloadEntryHeader>(&data[..PAYLOAD_ENTRY_HEADER_SIZE])
            .unwrap();

    match payload_entry_header.key {
        HOST_TIME_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + HOST_TIME_SIZE as usize;
            let (entry, _) = encoding::decode_from_slice::<HostTime>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
            )?;

            debug!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::HostTime(entry));
            decode(payload, &data[decode_len..])?;
        }
        TIMELINE_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + TIMELINE_SIZE as usize;
            let (entry, _) = encoding::decode_from_slice::<Timeline>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
            )
            .unwrap();

            debug!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::Timeline(entry));
            decode(payload, &data[decode_len..])?;
        }
        SESSION_MEMBERSHIP_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + SESSION_MEMBERSHIP_SIZE as usize;
            let (entry, _) = encoding::decode_from_slice::<SessionMembership>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
            )?;

            debug!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::SessionMembership(entry));
            decode(payload, &data[decode_len..])?;
        }
        START_STOP_STATE_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + START_STOP_STATE_SIZE as usize;
            let (entry, _) = encoding::decode_from_slice::<StartStopState>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
            )?;

            debug!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::StartStopState(entry));
            decode(payload, &data[decode_len..])?;
        }
        MEASUREMENT_ENDPOINT_V4_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + MEASUREMENT_ENDPOINT_V4_SIZE as usize;
            let (entry, _) = encoding::decode_from_slice::<MeasurementEndpointV4>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
            )?;

            debug!("decoded payload entry {:?}", entry);

            payload
                .entries
                .push(PayloadEntry::MeasurementEndpointV4(entry));
            decode(payload, &data[decode_len..])?;
        }
        GHOST_TIME_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + GHOST_TIME_SIZE as usize;
            let (entry, _) = encoding::decode_from_slice::<GhostTime>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
            )?;

            debug!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::GhostTime(entry));
            decode(payload, &data[decode_len..])?;
        }
        PREV_GHOST_TIME_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + PREV_GHOST_TIME_SIZE as usize;
            let (entry, _) = encoding::decode_from_slice::<PrevGhostTime>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
            )?;

            debug!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::PrevGhostTime(entry));
            decode(payload, &data[decode_len..])?;
        }
        _ => {
            warn!("unknown payload entry key {:x}", payload_entry_header.key);
            let skip_len = PAYLOAD_ENTRY_HEADER_SIZE + payload_entry_header.size as usize;
            if skip_len <= data.len() {
                decode(payload, &data[skip_len..])?;
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
pub enum PayloadEntry {
    HostTime(HostTime),
    GhostTime(GhostTime),
    PrevGhostTime(PrevGhostTime),
    Timeline(Timeline),
    SessionMembership(SessionMembership),
    StartStopState(StartStopState),
    MeasurementEndpointV4(MeasurementEndpointV4),
}

impl PayloadEntry {
    pub fn size(&self) -> u32 {
        match self {
            PayloadEntry::HostTime(_) => HOST_TIME_SIZE,
            PayloadEntry::GhostTime(_) => GHOST_TIME_SIZE,
            PayloadEntry::PrevGhostTime(_) => PREV_GHOST_TIME_SIZE,
            PayloadEntry::Timeline(_) => TIMELINE_SIZE,
            PayloadEntry::SessionMembership(_) => SESSION_MEMBERSHIP_SIZE,
            PayloadEntry::StartStopState(_) => START_STOP_STATE_SIZE,
            PayloadEntry::MeasurementEndpointV4(_) => MEASUREMENT_ENDPOINT_V4_SIZE,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            PayloadEntry::HostTime(host_time) => host_time.encode(),
            PayloadEntry::GhostTime(ghost_time) => ghost_time.encode(),
            PayloadEntry::PrevGhostTime(prev_ghost_time) => prev_ghost_time.encode(),
            PayloadEntry::Timeline(timeline) => timeline.encode(),
            PayloadEntry::SessionMembership(session_membership) => session_membership.encode(),
            PayloadEntry::StartStopState(start_stop_state) => start_stop_state.encode(),
            PayloadEntry::MeasurementEndpointV4(measurement_endpoint_v4) => {
                measurement_endpoint_v4.encode()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostTime {
    pub time: Duration,
}

impl HostTime {
    pub fn new(time: Duration) -> Self {
        Self { time }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = HOST_TIME_HEADER.encode()?;
        encoded.append(&mut encoding::encode_to_vec(
            &self.time.num_microseconds().unwrap(),
        )?);
        Ok(encoded)
    }
}

impl Decode for HostTime {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (time, n) = i64::decode_from(bytes)?;
        Ok((
            Self {
                time: Duration::microseconds(time),
            },
            n,
        ))
    }
}

impl Default for HostTime {
    fn default() -> Self {
        Self {
            time: Duration::zero(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GhostTime {
    pub time: Duration,
}

impl GhostTime {
    pub fn new(time: Duration) -> Self {
        Self { time }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = GHOST_TIME_HEADER.encode()?;
        encoded.append(&mut encoding::encode_to_vec(
            &self.time.num_microseconds().unwrap(),
        )?);
        Ok(encoded)
    }
}

impl Decode for GhostTime {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (time, n) = i64::decode_from(bytes)?;
        Ok((
            Self {
                time: Duration::microseconds(time),
            },
            n,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct PrevGhostTime {
    pub time: Duration,
}

impl PrevGhostTime {
    pub fn new(time: Duration) -> Self {
        Self { time }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = PREV_GHOST_TIME_HEADER.encode()?;
        encoded.append(&mut encoding::encode_to_vec(
            &self.time.num_microseconds().unwrap(),
        )?);
        Ok(encoded)
    }
}

impl Decode for PrevGhostTime {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (time, n) = i64::decode_from(bytes)?;
        Ok((
            Self {
                time: Duration::microseconds(time),
            },
            n,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{
        beats::Beats,
        node::{NodeId, NodeState},
        sessions::{SessionId, SessionMembership},
        state::StartStopState,
        tempo::Tempo,
        timeline::Timeline,
    };

    #[test]
    fn host_time_header() {
        assert_eq!(HOST_TIME_HEADER_KEY, 0x5f5f6874, "unexpected byte order");
        assert_eq!(HOST_TIME_SIZE, 8, "unexpected size");
    }

    #[test]
    fn ghost_time_header() {
        assert_eq!(GHOST_TIME_HEADER_KEY, 0x5f5f6774, "unexpected byte order");
        assert_eq!(GHOST_TIME_SIZE, 8, "unexpected size");
    }

    #[test]
    fn prev_ghost_time_header() {
        assert_eq!(
            PREV_GHOST_TIME_HEADER_KEY, 0x5f706774,
            "unexpected byte order"
        );
        assert_eq!(PREV_GHOST_TIME_SIZE, 8, "unexpected size");
    }

    #[test]
    fn roundtrip_timeline_payload() {
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(4.0),
            time_origin: Duration::microseconds(1_000_000),
        };
        let payload = Payload {
            entries: vec![PayloadEntry::Timeline(timeline)],
        };
        let encoded = payload.encode().unwrap();
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &encoded).unwrap();

        assert_eq!(decoded_payload.entries.len(), 1);
        if let PayloadEntry::Timeline(tl) = &decoded_payload.entries[0] {
            assert_eq!(tl.tempo.bpm(), 120.0);
            assert_eq!(tl.beat_origin, Beats::new(4.0));
            assert_eq!(tl.time_origin, Duration::microseconds(1_000_000));
        } else {
            panic!("expected Timeline entry");
        }
    }

    #[test]
    fn roundtrip_start_stop_state_payload() {
        let sss = StartStopState {
            is_playing: true,
            beats: Beats::new(8.5),
            timestamp: Duration::microseconds(2_000_000),
        };
        let payload = Payload {
            entries: vec![PayloadEntry::StartStopState(sss)],
        };
        let encoded = payload.encode().unwrap();
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &encoded).unwrap();

        assert_eq!(decoded_payload.entries.len(), 1);
        if let PayloadEntry::StartStopState(decoded) = &decoded_payload.entries[0] {
            assert!(decoded.is_playing);
            assert_eq!(decoded.beats, Beats::new(8.5));
            assert_eq!(decoded.timestamp, Duration::microseconds(2_000_000));
        } else {
            panic!("expected StartStopState entry");
        }
    }

    #[test]
    fn roundtrip_host_time_payload() {
        let ht = HostTime::new(Duration::microseconds(9_876_543));
        let payload = Payload {
            entries: vec![PayloadEntry::HostTime(ht)],
        };
        let encoded = payload.encode().unwrap();
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &encoded).unwrap();

        assert_eq!(decoded_payload.entries.len(), 1);
        if let PayloadEntry::HostTime(decoded) = &decoded_payload.entries[0] {
            assert_eq!(decoded.time, Duration::microseconds(9_876_543));
        } else {
            panic!("expected HostTime entry");
        }
    }

    #[test]
    fn roundtrip_ghost_time_payload() {
        let gt = GhostTime::new(Duration::microseconds(1_234_567));
        let payload = Payload {
            entries: vec![PayloadEntry::GhostTime(gt)],
        };
        let encoded = payload.encode().unwrap();
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &encoded).unwrap();

        assert_eq!(decoded_payload.entries.len(), 1);
        if let PayloadEntry::GhostTime(decoded) = &decoded_payload.entries[0] {
            assert_eq!(decoded.time, Duration::microseconds(1_234_567));
        } else {
            panic!("expected GhostTime entry");
        }
    }

    #[test]
    fn roundtrip_prev_ghost_time_payload() {
        let pgt = PrevGhostTime::new(Duration::microseconds(7_654_321));
        let payload = Payload {
            entries: vec![PayloadEntry::PrevGhostTime(pgt)],
        };
        let encoded = payload.encode().unwrap();
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &encoded).unwrap();

        assert_eq!(decoded_payload.entries.len(), 1);
        if let PayloadEntry::PrevGhostTime(decoded) = &decoded_payload.entries[0] {
            assert_eq!(decoded.time, Duration::microseconds(7_654_321));
        } else {
            panic!("expected PrevGhostTime entry");
        }
    }

    #[test]
    fn roundtrip_multi_entry_payload() {
        let timeline = Timeline {
            tempo: Tempo::new(140.0),
            beat_origin: Beats::new(1.0),
            time_origin: Duration::microseconds(500_000),
        };
        let sss = StartStopState {
            is_playing: false,
            beats: Beats::new(0.0),
            timestamp: Duration::zero(),
        };
        let session_membership = SessionMembership {
            session_id: SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8])),
        };
        let payload = Payload {
            entries: vec![
                PayloadEntry::Timeline(timeline),
                PayloadEntry::SessionMembership(session_membership),
                PayloadEntry::StartStopState(sss),
            ],
        };
        let encoded = payload.encode().unwrap();
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &encoded).unwrap();

        assert_eq!(decoded_payload.entries.len(), 3);
        assert!(matches!(
            &decoded_payload.entries[0],
            PayloadEntry::Timeline(_)
        ));
        assert!(matches!(
            &decoded_payload.entries[1],
            PayloadEntry::SessionMembership(_)
        ));
        assert!(matches!(
            &decoded_payload.entries[2],
            PayloadEntry::StartStopState(_)
        ));
    }

    #[test]
    fn roundtrip_node_state_payload() {
        let node_id = NodeId::from_array([10, 20, 30, 40, 50, 60, 70, 80]);
        let session_id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let node_state = NodeState {
            node_id,
            session_id,
            timeline: Timeline {
                tempo: Tempo::new(100.0),
                beat_origin: Beats::new(2.0),
                time_origin: Duration::microseconds(3_000_000),
            },
            start_stop_state: StartStopState {
                is_playing: true,
                beats: Beats::new(5.0),
                timestamp: Duration::microseconds(4_000_000),
            },
        };
        let payload = Payload::from(node_state.clone());
        let encoded = payload.encode().unwrap();
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &encoded).unwrap();

        let recovered = NodeState::from_payload(node_id, &decoded_payload);
        assert_eq!(recovered.node_id, node_id);
        assert_eq!(recovered.session_id, session_id);
        assert_eq!(recovered.timeline.tempo.bpm(), 100.0);
        assert!(recovered.start_stop_state.is_playing);
    }

    #[test]
    fn unknown_entry_type_skipped_gracefully() {
        // Build a valid HostTime entry followed by a fake unknown entry, then a GhostTime entry
        let ht = HostTime::new(Duration::microseconds(42));
        let gt = GhostTime::new(Duration::microseconds(99));

        let mut data = Vec::new();
        data.extend_from_slice(&ht.encode().unwrap());

        // Fabricate an unknown entry: key=0xDEADBEEF, size=4, payload=4 zero bytes
        let unknown_header = PayloadEntryHeader {
            key: 0xDEADBEEF,
            size: 4,
        };
        data.extend_from_slice(&unknown_header.encode().unwrap());
        data.extend_from_slice(&[0u8; 4]);

        data.extend_from_slice(&gt.encode().unwrap());

        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &data).unwrap();

        // Should have decoded the HostTime and GhostTime, skipping the unknown entry
        assert_eq!(decoded_payload.entries.len(), 2);
        assert!(matches!(
            &decoded_payload.entries[0],
            PayloadEntry::HostTime(_)
        ));
        assert!(matches!(
            &decoded_payload.entries[1],
            PayloadEntry::GhostTime(_)
        ));
    }

    #[test]
    fn empty_payload_decode() {
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &[]).unwrap();
        assert!(decoded_payload.entries.is_empty());
    }

    #[test]
    fn payload_too_short_for_header_is_ok() {
        // Data shorter than PAYLOAD_ENTRY_HEADER_SIZE should just return Ok with no entries
        let mut decoded_payload = Payload::default();
        decode(&mut decoded_payload, &[0u8; 3]).unwrap();
        assert!(decoded_payload.entries.is_empty());
    }

    #[test]
    fn payload_size_matches_encoded_length() {
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        let payload = Payload {
            entries: vec![PayloadEntry::Timeline(timeline)],
        };
        let encoded = payload.encode().unwrap();
        assert_eq!(payload.size() as usize, encoded.len());
    }

    #[test]
    fn empty_payload_encode() {
        let payload = Payload::default();
        let encoded = payload.encode().unwrap();
        assert!(encoded.is_empty());
        assert_eq!(payload.size(), 0);
    }
}
