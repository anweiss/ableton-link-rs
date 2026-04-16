use std::mem;

use crate::encoding::{self, Decode, Encode};

use crate::{
    link::{
        node::NodeId,
        payload::{self, Payload},
        Result,
    },
};

pub const MAX_MESSAGE_SIZE: usize = 512;
pub const PROTOCOL_HEADER_SIZE: usize = 8;

pub type MessageType = u8;
pub type SessionGroupId = u16;

pub type ProtocolHeader = [u8; PROTOCOL_HEADER_SIZE];

pub const INVALID: MessageType = 0;
pub const ALIVE: MessageType = 1;
pub const RESPONSE: MessageType = 2;
pub const BYEBYE: MessageType = 3;

pub const MESSAGE_TYPES: [&str; 4] = ["INVALID", "ALIVE", "RESPONSE", "BYEBYE"];

pub const PROTOCOL_HEADER: ProtocolHeader = [b'_', b'a', b's', b'd', b'p', b'_', b'v', 1];

pub const MESSAGE_HEADER_SIZE: usize = mem::size_of::<MessageType>()
    + mem::size_of::<u8>()
    + mem::size_of::<SessionGroupId>()
    + mem::size_of::<NodeId>();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageHeader {
    pub message_type: MessageType,
    pub ttl: u8,
    pub group_id: SessionGroupId,
    pub ident: NodeId,
}

impl Encode for MessageHeader {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.message_type.encode_to(out);
        self.ttl.encode_to(out);
        self.group_id.encode_to(out);
        self.ident.encode_to(out);
    }
    fn encoded_size(&self) -> usize {
        1 + 1 + 2 + 8
    }
}

impl Decode for MessageHeader {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (message_type, n1) = u8::decode_from(bytes)?;
        let (ttl, n2) = u8::decode_from(&bytes[n1..])?;
        let (group_id, n3) = u16::decode_from(&bytes[n1 + n2..])?;
        let (ident, n4) = NodeId::decode_from(&bytes[n1 + n2 + n3..])?;
        Ok((
            Self {
                message_type,
                ttl,
                group_id,
                ident,
            },
            n1 + n2 + n3 + n4,
        ))
    }
}

impl MessageHeader {}

pub fn encode_message(
    from: NodeId,
    ttl: u8,
    message_type: MessageType,
    payload: &Payload,
    group_id: SessionGroupId,
) -> Result<Vec<u8>> {
    let header = MessageHeader {
        message_type,
        ttl,
        group_id,
        ident: from,
    };

    let message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE + payload.size() as usize;

    if message_size > MAX_MESSAGE_SIZE {
        panic!("exceeded maximum message size");
    }

    let mut encoded = encoding::encode_to_vec(&PROTOCOL_HEADER)?;
    encoded.append(&mut encoding::encode_to_vec(&header)?);
    encoded.append(&mut payload.encode()?);

    Ok(encoded)
}

pub fn alive_message(
    from: NodeId,
    ttl: u8,
    payload: &Payload,
    group_id: SessionGroupId,
) -> Result<Vec<u8>> {
    encode_message(from, ttl, ALIVE, payload, group_id)
}

pub fn response_message(
    from: NodeId,
    ttl: u8,
    payload: &Payload,
    group_id: SessionGroupId,
) -> Result<Vec<u8>> {
    encode_message(from, ttl, RESPONSE, payload, group_id)
}

pub fn byebye_message(from: NodeId) -> Result<Vec<u8>> {
    encode_message(from, 0, BYEBYE, &Payload::default(), 0)
}

pub fn parse_message_header(data: &[u8]) -> Result<(MessageHeader, usize)> {
    let min_message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE;

    if data.len() < min_message_size {
        panic!("invalid message size");
    }

    if !data.starts_with(&PROTOCOL_HEADER) {
        panic!("invalid protocol header");
    }

    let (header, consumed) = encoding::decode_from_slice::<MessageHeader>(
        &data[PROTOCOL_HEADER_SIZE..min_message_size],
    )?;
    Ok((header, PROTOCOL_HEADER_SIZE + consumed))
}

pub fn parse_payload(data: &[u8]) -> Result<Payload> {
    let mut payload = Payload::default();
    payload::decode(&mut payload, data).unwrap();

    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{
        beats::Beats,
        node::{NodeId, NodeState},
        sessions::SessionId,
        state::StartStopState,
        tempo::Tempo,
        timeline::Timeline,
    };
    use chrono::Duration;

    #[test]
    fn test_parse_message_header() {
        let header = MessageHeader {
            ttl: 10,
            ..Default::default()
        };

        let mut encoded = encoding::encode_to_vec(&PROTOCOL_HEADER).unwrap();
        encoded.append(&mut encoding::encode_to_vec(&header).unwrap());

        let (decoded_header, _) = parse_message_header(&encoded).unwrap();
        assert_eq!(decoded_header, header);
    }

    #[test]
    fn alive_message_roundtrip() {
        let node_id = NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]);
        let session_id = SessionId(node_id);
        let node_state = NodeState {
            node_id,
            session_id,
            timeline: Timeline {
                tempo: Tempo::new(120.0),
                beat_origin: Beats::new(0.0),
                time_origin: Duration::zero(),
            },
            start_stop_state: StartStopState::default(),
        };
        let payload = Payload::from(node_state);
        let msg = alive_message(node_id, 5, &payload, 0).unwrap();

        let (header, offset) = parse_message_header(&msg).unwrap();
        assert_eq!(header.message_type, ALIVE);
        assert_eq!(header.ttl, 5);
        assert_eq!(header.ident, node_id);

        let decoded_payload = parse_payload(&msg[offset..]).unwrap();
        assert!(!decoded_payload.entries.is_empty());
    }

    #[test]
    fn response_message_roundtrip() {
        let node_id = NodeId::from_array([10, 20, 30, 40, 50, 60, 70, 80]);
        let payload = Payload::default();
        let msg = response_message(node_id, 3, &payload, 42).unwrap();

        let (header, _) = parse_message_header(&msg).unwrap();
        assert_eq!(header.message_type, RESPONSE);
        assert_eq!(header.ttl, 3);
        assert_eq!(header.group_id, 42);
        assert_eq!(header.ident, node_id);
    }

    #[test]
    fn byebye_message_roundtrip() {
        let node_id = NodeId::from_array([99, 88, 77, 66, 55, 44, 33, 22]);
        let msg = byebye_message(node_id).unwrap();

        let (header, _) = parse_message_header(&msg).unwrap();
        assert_eq!(header.message_type, BYEBYE);
        assert_eq!(header.ttl, 0);
        assert_eq!(header.ident, node_id);
    }

    #[test]
    #[should_panic(expected = "invalid message size")]
    fn parse_message_header_too_short() {
        let data = [0u8; 4]; // Way too short
        let _ = parse_message_header(&data);
    }

    #[test]
    #[should_panic(expected = "invalid protocol header")]
    fn parse_message_header_bad_protocol() {
        // Create data of correct length but wrong protocol header
        let mut data = vec![0u8; PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE];
        data[0] = 0xFF; // Wrong protocol
        let _ = parse_message_header(&data);
    }

    #[test]
    fn message_type_constants() {
        assert_eq!(INVALID, 0);
        assert_eq!(ALIVE, 1);
        assert_eq!(RESPONSE, 2);
        assert_eq!(BYEBYE, 3);
    }

    #[test]
    fn protocol_header_format() {
        assert_eq!(&PROTOCOL_HEADER[..6], b"_asdp_");
        assert_eq!(PROTOCOL_HEADER[6], b'v');
        assert_eq!(PROTOCOL_HEADER[7], 1); // Version 1
    }

    #[test]
    fn message_types_labels() {
        assert_eq!(MESSAGE_TYPES[INVALID as usize], "INVALID");
        assert_eq!(MESSAGE_TYPES[ALIVE as usize], "ALIVE");
        assert_eq!(MESSAGE_TYPES[RESPONSE as usize], "RESPONSE");
        assert_eq!(MESSAGE_TYPES[BYEBYE as usize], "BYEBYE");
    }

    #[test]
    fn parse_payload_empty() {
        let payload = parse_payload(&[]).unwrap();
        assert!(payload.entries.is_empty());
    }
}
