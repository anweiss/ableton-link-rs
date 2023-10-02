use std::mem;

use bincode::{Decode, Encode};

use crate::{
    discovery::ENCODING_CONFIG,
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

#[derive(Debug, Clone, Copy, Default, Encode, Decode, PartialEq, Eq)]
pub struct MessageHeader {
    pub message_type: MessageType,
    pub ttl: u8,
    pub group_id: SessionGroupId,
    pub ident: NodeId,
}

impl MessageHeader {}

pub fn encode_message(
    from: NodeId,
    ttl: u8,
    message_type: MessageType,
    payload: &Payload,
) -> Result<Vec<u8>> {
    let header = MessageHeader {
        message_type,
        ttl,
        group_id: 0,
        ident: from,
    };

    let message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE + payload.size() as usize;

    if message_size > MAX_MESSAGE_SIZE {
        panic!("exceeded maximum message size");
    }

    let mut encoded = bincode::encode_to_vec(PROTOCOL_HEADER, ENCODING_CONFIG)?;
    encoded.append(&mut bincode::encode_to_vec(header, ENCODING_CONFIG)?);
    encoded.append(&mut payload.encode()?);

    Ok(encoded)
}

pub fn alive_message(from: NodeId, ttl: u8, payload: &Payload) -> Result<Vec<u8>> {
    encode_message(from, ttl, ALIVE, payload)
}

pub fn response_message(from: NodeId, ttl: u8, payload: &Payload) -> Result<Vec<u8>> {
    encode_message(from, ttl, RESPONSE, payload)
}

pub fn byebye_message(from: NodeId) -> Result<Vec<u8>> {
    encode_message(from, 0, BYEBYE, &Payload::default())
}

pub fn parse_message_header(data: &[u8]) -> Result<(MessageHeader, usize)> {
    let min_message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE;

    if data.len() < min_message_size {
        panic!("invalid message size");
    }

    if !data.starts_with(&PROTOCOL_HEADER) {
        panic!("invalid protocol header");
    }

    Ok(bincode::decode_from_slice(
        &data[PROTOCOL_HEADER_SIZE..min_message_size],
        ENCODING_CONFIG,
    )
    .map(|header| (header.0, PROTOCOL_HEADER_SIZE + header.1))?)
}

pub fn parse_payload(data: &[u8]) -> Result<Payload> {
    let mut payload = Payload::default();
    payload::decode(&mut payload, data).unwrap();

    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_message_header() {
        let header = MessageHeader {
            ttl: 10,
            ..Default::default()
        };

        let mut encoded = bincode::encode_to_vec(PROTOCOL_HEADER, ENCODING_CONFIG).unwrap();
        encoded.append(&mut bincode::encode_to_vec(header, ENCODING_CONFIG).unwrap());

        let (decoded_header, _) = parse_message_header(&encoded).unwrap();
        assert_eq!(decoded_header, header);
    }
}
