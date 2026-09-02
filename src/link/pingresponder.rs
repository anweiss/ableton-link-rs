use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use crate::encoding::{self, Decode, Encode};

use tokio::net::UdpSocket;
use tracing::{debug, info};

use crate::{
    discovery::messages::parse_payload,
    link::{
        payload::{GhostTime, PayloadEntry},
        sessions::SessionMembership,
    },
};

use super::{clock::Clock, ghostxform::GhostXForm, payload::Payload, sessions::SessionId, Result};

pub const MAX_MESSAGE_SIZE: usize = 512;
pub const PROTOCOL_HEADER_SIZE: usize = 8;

pub type MessageType = u8;
pub type ProtocolHeader = [u8; PROTOCOL_HEADER_SIZE];

pub const PING: MessageType = 1;
pub const PONG: MessageType = 2;

pub const MESSAGE_TYPES: [&str; 2] = ["PING", "PONG"];

pub const PROTOCOL_HEADER: ProtocolHeader = [b'_', b'l', b'i', b'n', b'k', b'_', b'v', 1];

pub const MESSAGE_HEADER_SIZE: usize = std::mem::size_of::<MessageType>();

#[derive(Debug)]
pub struct MessageHeader {
    pub message_type: MessageType,
}

impl Encode for MessageHeader {
    fn encode_to(
        &self,
        out: &mut Vec<u8>,
    ) -> core::result::Result<(), crate::encoding::EncodeError> {
        self.message_type.encode_to(out)?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        1
    }
}

impl Decode for MessageHeader {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (message_type, n) = u8::decode_from(bytes)?;
        Ok((Self { message_type }, n))
    }
}

#[derive(Debug, Clone)]
pub struct PingResponder {
    pub session_id: Arc<Mutex<SessionId>>,
    pub ghost_x_form: Arc<Mutex<GhostXForm>>,
    pub clock: Clock,
    pub unicast_socket: Option<Arc<UdpSocket>>,
    ping_message_received: Arc<AtomicBool>,
}

impl PingResponder {
    pub fn new(
        unicast_socket: Arc<UdpSocket>,
        session_id: SessionId,
        ghost_x_form: GhostXForm,
        clock: Clock,
    ) -> Self {
        PingResponder {
            unicast_socket: Some(unicast_socket),
            session_id: Arc::new(Mutex::new(session_id)),
            ghost_x_form: Arc::new(Mutex::new(ghost_x_form)),
            clock,
            ping_message_received: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Handle a single PING datagram received on the shared unicast socket and
    /// reply with a PONG. Datagrams are dispatched by the receive loop owned by
    /// `MeasurementService`, which is the only reader of the shared socket.
    pub async fn handle_ping(&self, data: &[u8], src: SocketAddr) {
        let unicast_socket = match self.unicast_socket.as_ref() {
            Some(socket) => socket,
            None => return,
        };

        let (_, header_len) = match parse_message_header(data) {
            Ok(header) => header,
            Err(e) => {
                debug!("failed to parse ping message header from {}: {}", src, e);
                return;
            }
        };

        let payload_size = data[header_len..].len();
        let max_payload_size = 40;

        if payload_size > max_payload_size {
            debug!("received invalid message from {}", src);
            return;
        }

        let ping_message_received = self.ping_message_received.swap(true, Ordering::Relaxed);

        if !ping_message_received {
            info!("received ping message from {}", src);
        }

        let payload = match parse_payload(&data[header_len..]) {
            Ok(payload) => payload,
            Err(e) => {
                debug!("failed to parse ping payload from {}: {}", src, e);
                return;
            }
        };

        let mut payload_entries = vec![];
        for entry in payload.entries.into_iter() {
            if matches!(
                entry,
                PayloadEntry::HostTime(_) | PayloadEntry::PrevGhostTime(_)
            ) {
                payload_entries.push(entry);
            }
        }

        let id = SessionMembership {
            session_id: *self.session_id.try_lock().unwrap(),
        };
        let current_gt = GhostTime {
            time: self
                .ghost_x_form
                .try_lock()
                .unwrap()
                .host_to_ghost(self.clock.micros()),
        };

        payload_entries.push(PayloadEntry::SessionMembership(id));
        payload_entries.push(PayloadEntry::GhostTime(current_gt));

        let pong_payload = Payload {
            entries: payload_entries,
        };

        if !ping_message_received {
            debug!("pong_payload {:?}", pong_payload);
        }

        let pong_message = match encode_message(PONG, &pong_payload) {
            Ok(message) => message,
            Err(e) => {
                debug!("failed to encode pong message for {}: {}", src, e);
                return;
            }
        };
        if let Err(e) = unicast_socket.send_to(&pong_message, src).await {
            debug!("failed to send pong message to {}: {}", src, e);
            return;
        }

        if !ping_message_received {
            debug!("sent pong message to {}", src);
        }
    }

    pub async fn update_node_state(&self, session_id: SessionId, x_form: GhostXForm) {
        *self.session_id.try_lock().unwrap() = session_id;
        *self.ghost_x_form.try_lock().unwrap() = x_form;
    }
}

pub fn encode_message(message_type: MessageType, payload: &Payload) -> Result<Vec<u8>> {
    let header = MessageHeader { message_type };

    let message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE + payload.size() as usize;

    if message_size > MAX_MESSAGE_SIZE {
        // The ping and pong payloads this crate builds are bounded by
        // construction, so this is unreachable internally. `encode_message` is
        // public, though, and mirrors `discovery::messages::encode_message`: an
        // over-limit message is a recoverable error the caller logs and drops,
        // not a process abort.
        return Err(crate::link::error::Error::Encoding(
            encoding::EncodeError::MessageTooLarge {
                size: message_size,
                max: MAX_MESSAGE_SIZE,
            },
        ));
    }

    let mut encoded = encoding::encode_to_vec(&PROTOCOL_HEADER)?;
    encoded.append(&mut encoding::encode_to_vec(&header)?);
    encoded.append(&mut payload.encode()?);

    Ok(encoded)
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

#[cfg(test)]
mod tests {
    use crate::link::{
        encoding::PAYLOAD_ENTRY_HEADER_SIZE,
        payload::{HostTime, HOST_TIME_SIZE},
    };

    use super::*;

    fn init_tracing() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    #[test]
    fn roundtrip() {
        init_tracing();

        let payload = Payload {
            entries: vec![PayloadEntry::HostTime(HostTime::default())],
        };

        let message = encode_message(PING, &payload).unwrap();
        info!("message: {:?}", message);

        let header = parse_message_header(&message).unwrap();
        info!("header: {:?}", header);
    }

    #[test]
    fn encode_message_rejects_oversized_payload() {
        // The ping/pong payloads this crate builds are bounded by
        // construction, but `encode_message` is public: an over-limit message
        // is reported the way `discovery::messages::encode_message` reports it,
        // as a recoverable error rather than a panic.
        let entry_size = PAYLOAD_ENTRY_HEADER_SIZE + HOST_TIME_SIZE as usize;
        let mut payload = Payload::default();
        for _ in 0..(MAX_MESSAGE_SIZE / entry_size + 1) {
            payload
                .entries
                .push(PayloadEntry::HostTime(HostTime::default()));
        }

        let result = encode_message(PING, &payload);
        // Match only on the error; never debug-print the `Ok` arm, which
        // would write the encoded wire payload into the test log.
        match result {
            Err(crate::link::error::Error::Encoding(
                crate::encoding::EncodeError::MessageTooLarge { size, max },
            )) => {
                assert_eq!(max, MAX_MESSAGE_SIZE);
                assert!(size > MAX_MESSAGE_SIZE);
            }
            Err(other) => panic!("expected MessageTooLarge, got error {}", other),
            Ok(_) => panic!("expected MessageTooLarge, got a successfully encoded message"),
        }
    }
}
