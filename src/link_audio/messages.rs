//! LinkAudio v1 wire messages.
//!
//! Ported from upstream `ableton/link_audio/v1/Messages.hpp`.

use crate::link::node::NodeId;

use super::{
    encoding::{ByteStreamReader, ByteStreamWrite},
    error::{AudioError, Result},
};

/// To avoid fragmentation the message size must stay below the network MTU.
/// Upstream targets 1200 bytes, which fits inside both the IPv4 (1500 byte,
/// up to 60 byte header) and IPv6 (1280 byte, 40 byte header) minimums after
/// accounting for the 8 byte UDP header.
pub const MAX_MESSAGE_SIZE: usize = 1200;
pub const HEADER_SIZE: usize = 24;
pub const MAX_PAYLOAD_SIZE: usize = MAX_MESSAGE_SIZE - HEADER_SIZE;
/// Peer and channel names are truncated to this many bytes on the wire.
pub const MAX_NAME_SIZE: usize = 256;

pub const PROTOCOL_HEADER_SIZE: usize = 8;
pub const PROTOCOL_HEADER: [u8; PROTOCOL_HEADER_SIZE] =
    [b'c', b'h', b'n', b'n', b'l', b's', b'v', 1];

/// Size of the encoded [`MessageHeader`]: message type (1) + ttl (1) +
/// group id (2) + node id (8).
pub const MESSAGE_HEADER_SIZE: usize = 1 + 1 + 2 + 8;

pub type MessageType = u8;
pub type SessionGroupId = u16;

pub const INVALID: MessageType = 0;
pub const PEER_ANNOUNCEMENT: MessageType = 1;
pub const CHANNEL_BYES: MessageType = 2;
pub const PONG: MessageType = 3;
pub const CHANNEL_REQUEST: MessageType = 4;
pub const STOP_CHANNEL_REQUEST: MessageType = 5;
pub const AUDIO_BUFFER: MessageType = 6;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageHeader {
    pub message_type: MessageType,
    pub ttl: u8,
    pub group_id: SessionGroupId,
    pub ident: NodeId,
}

impl MessageHeader {
    pub fn size_in_byte_stream(&self) -> usize {
        MESSAGE_HEADER_SIZE
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.write_u8(self.message_type);
        out.write_u8(self.ttl);
        out.write_u16(self.group_id);
        out.extend_from_slice(&self.ident.0);
    }

    pub fn decode(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        Ok(MessageHeader {
            message_type: reader.read_u8()?,
            ttl: reader.read_u8()?,
            group_id: reader.read_u16()?,
            ident: NodeId(reader.read_array::<8>()?),
        })
    }
}

/// Encodes a complete LinkAudio message: protocol header, message header and
/// the already-encoded payload bytes.
pub fn encode_message(
    from: NodeId,
    ttl: u8,
    message_type: MessageType,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let header = MessageHeader {
        message_type,
        ttl,
        group_id: 0,
        ident: from,
    };

    let message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE + payload.len();
    if message_size > MAX_MESSAGE_SIZE {
        return Err(AudioError::MessageTooLarge(message_size, MAX_MESSAGE_SIZE));
    }

    let mut out = Vec::with_capacity(message_size);
    out.extend_from_slice(&PROTOCOL_HEADER);
    header.encode(&mut out);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Convenience wrapper for [`AUDIO_BUFFER`] messages, which are always sent
/// with a ttl of zero.
pub fn audio_buffer_message(from: NodeId, payload: &[u8]) -> Result<Vec<u8>> {
    encode_message(from, 0, AUDIO_BUFFER, payload)
}

/// Parses the protocol and message headers, returning the header and the
/// offset of the first payload byte.
pub fn parse_message_header(data: &[u8]) -> Result<(MessageHeader, usize)> {
    let min_message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE;

    if data.len() < min_message_size {
        return Err(AudioError::Range("message shorter than header"));
    }

    if !data.starts_with(&PROTOCOL_HEADER) {
        return Err(AudioError::Invalid("unexpected protocol header"));
    }

    let mut reader = ByteStreamReader::new(&data[PROTOCOL_HEADER_SIZE..min_message_size]);
    let header = MessageHeader::decode(&mut reader)?;
    Ok((header, min_message_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_header_is_chnnlsv1() {
        assert_eq!(&PROTOCOL_HEADER[..6], b"chnnls");
        assert_eq!(PROTOCOL_HEADER[6], b'v');
        assert_eq!(PROTOCOL_HEADER[7], 1);
    }

    #[test]
    fn header_size_matches_upstream() {
        // 8 byte protocol header + 12 byte message header + payload key/size
        // headers must all fit in the 24 bytes reserved upstream.
        const { assert!(PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE <= HEADER_SIZE) };
        assert_eq!(MAX_PAYLOAD_SIZE, MAX_MESSAGE_SIZE - HEADER_SIZE);
    }

    #[test]
    fn message_roundtrip() {
        let node_id = NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]);
        let msg = encode_message(node_id, 5, CHANNEL_REQUEST, &[0xaa, 0xbb]).unwrap();

        let (header, offset) = parse_message_header(&msg).unwrap();
        assert_eq!(header.message_type, CHANNEL_REQUEST);
        assert_eq!(header.ttl, 5);
        assert_eq!(header.group_id, 0);
        assert_eq!(header.ident, node_id);
        assert_eq!(&msg[offset..], &[0xaa, 0xbb]);
    }

    #[test]
    fn oversized_message_is_rejected() {
        let payload = vec![0u8; MAX_MESSAGE_SIZE];
        let err = encode_message(NodeId::default(), 1, AUDIO_BUFFER, &payload).unwrap_err();
        assert!(matches!(err, AudioError::MessageTooLarge(_, _)));
    }

    #[test]
    fn wrong_protocol_header_is_rejected() {
        let mut msg = encode_message(NodeId::default(), 1, PONG, &[]).unwrap();
        msg[0] = b'x';
        assert!(parse_message_header(&msg).is_err());
    }

    #[test]
    fn short_message_is_rejected() {
        assert!(parse_message_header(&[0u8; 4]).is_err());
    }

    #[test]
    fn audio_buffer_message_uses_zero_ttl() {
        let msg = audio_buffer_message(NodeId::default(), &[1, 2, 3]).unwrap();
        let (header, _) = parse_message_header(&msg).unwrap();
        assert_eq!(header.message_type, AUDIO_BUFFER);
        assert_eq!(header.ttl, 0);
    }
}
