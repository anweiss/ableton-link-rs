//! Audio endpoint payload entries (`aep4` / `aep6`).
//!
//! Upstream Ableton Link announces the UDP endpoint used by the LinkAudio
//! subsystem alongside the measurement endpoint in the `PeerState` payload.
//! Peers that do not understand these entries skip them, but this crate parses
//! them so that it stays forward-compatible with current Ableton Live and
//! Bitwig releases and so that the optional `audio` feature can locate the
//! audio endpoints of session peers.

use std::{
    mem,
    net::{Ipv4Addr, SocketAddrV4},
};

use crate::{link::encoding::PayloadEntryHeader, ENCODING_CONFIG};

use super::Result;

pub const AUDIO_ENDPOINT_V4_HEADER_KEY: u32 = u32::from_be_bytes(*b"aep4");
pub const AUDIO_ENDPOINT_V4_SIZE: u32 = (mem::size_of::<Ipv4Addr>() + mem::size_of::<u16>()) as u32;
pub const AUDIO_ENDPOINT_V4_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: AUDIO_ENDPOINT_V4_HEADER_KEY,
    size: AUDIO_ENDPOINT_V4_SIZE,
};

/// The IPv6 audio endpoint key. IPv6 endpoints are not announced by this crate,
/// but the key is recognized so that the entry is skipped cleanly when parsing
/// payloads from peers that do announce them.
pub const AUDIO_ENDPOINT_V6_HEADER_KEY: u32 = u32::from_be_bytes(*b"aep6");
pub const AUDIO_ENDPOINT_V6_SIZE: u32 = (16 + mem::size_of::<u16>()) as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioEndpointV4 {
    pub endpoint: Option<SocketAddrV4>,
}

impl bincode::Encode for AudioEndpointV4 {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        let endpoint = self
            .endpoint
            .unwrap_or_else(|| SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        bincode::Encode::encode(&(u32::from(*endpoint.ip()), endpoint.port()), encoder)
    }
}

impl bincode::Decode<()> for AudioEndpointV4 {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        let (ip, port): (Ipv4Addr, u16) = bincode::Decode::decode(decoder)?;
        // Upstream announces an unspecified address when audio is disabled.
        let endpoint = if ip.is_unspecified() || port == 0 {
            None
        } else {
            Some(SocketAddrV4::new(ip, port))
        };
        Ok(Self { endpoint })
    }
}

impl AudioEndpointV4 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = AUDIO_ENDPOINT_V4_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(self, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_endpoint_v4_key_is_aep4() {
        assert_eq!(AUDIO_ENDPOINT_V4_HEADER_KEY, 0x61657034);
        assert_eq!(AUDIO_ENDPOINT_V6_HEADER_KEY, 0x61657036);
    }

    #[test]
    fn audio_endpoint_v4_roundtrip() {
        let entry = AudioEndpointV4 {
            endpoint: Some(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 5), 40404)),
        };
        let encoded = entry.encode().unwrap();
        assert_eq!(encoded.len(), 8 + AUDIO_ENDPOINT_V4_SIZE as usize);

        let (decoded, _) =
            bincode::decode_from_slice::<AudioEndpointV4, _>(&encoded[8..], ENCODING_CONFIG)
                .unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn unspecified_endpoint_decodes_to_none() {
        let entry = AudioEndpointV4 { endpoint: None };
        let encoded = entry.encode().unwrap();
        let (decoded, _) =
            bincode::decode_from_slice::<AudioEndpointV4, _>(&encoded[8..], ENCODING_CONFIG)
                .unwrap();
        assert_eq!(decoded.endpoint, None);
    }
}
