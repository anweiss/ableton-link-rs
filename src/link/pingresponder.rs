use std::sync::{Arc, Mutex};

use bincode::{Decode, Encode};
use tokio::{net::UdpSocket, sync::Notify};
use tracing::{debug, info};

use crate::{
    discovery::{peers::PeerState, ENCODING_CONFIG},
    link::{
        payload::{GhostTime, PayloadEntry},
        sessions::SessionMembership,
    },
};

use super::{
    clock::Clock,
    ghostxform::GhostXForm,
    payload::{Payload, HOST_TIME_SIZE, PREV_GHOST_TIME_SIZE},
    sessions::SessionId,
    state::SessionState,
    Result,
};

pub const MAX_MESSAGE_SIZE: usize = 512;
pub const PROTOCOL_HEADER_SIZE: usize = 8;

pub type MessageType = u8;
pub type ProtocolHeader = [u8; PROTOCOL_HEADER_SIZE];

pub const PING: MessageType = 1;
pub const PONG: MessageType = 2;

pub const MESSAGE_TYPES: [&str; 2] = ["PING", "PONG"];

pub const PROTOCOL_HEADER: ProtocolHeader = [b'_', b'l', b'i', b'n', b'k', b'_', b'v', 1];

pub const MESSAGE_HEADER_SIZE: usize = std::mem::size_of::<MessageType>();

#[derive(Debug, Encode, Decode)]
pub struct MessageHeader {
    pub message_type: MessageType,
}

#[derive(Debug, Clone)]
pub struct PingResponder {
    pub peer_state: Arc<Mutex<PeerState>>,
    pub session_state: Arc<Mutex<SessionState>>,
    pub clock: Clock,
    pub unicast_socket: Option<Arc<UdpSocket>>,
}

impl PingResponder {
    pub fn new(
        unicast_socket: Arc<UdpSocket>,
        peer_state: Arc<Mutex<PeerState>>,
        session_state: Arc<Mutex<SessionState>>,
        clock: Clock,
    ) -> Self {
        PingResponder {
            unicast_socket: Some(unicast_socket),
            peer_state,
            session_state,
            clock,
        }
    }

    pub async fn listen(&self, _notifier: Arc<Notify>) {
        let unicast_socket = self.unicast_socket.as_ref().unwrap().clone();
        let peer_state = self.peer_state.clone();
        let ghost_x_form = self.session_state.try_lock().unwrap().ghost_x_form;
        let clock = self.clock;

        info!(
            "listening for ping messages on {}",
            unicast_socket.local_addr().unwrap()
        );

        tokio::spawn(async move {
            loop {
                let mut buf = [0; MAX_MESSAGE_SIZE];

                if let Ok((amt, src)) = unicast_socket.recv_from(&mut buf).await {
                    if !buf.starts_with(&PROTOCOL_HEADER) {
                        info!("protocol header mismatch");
                        continue;
                    }

                    let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();
                    let payload_size = buf[header_len..amt].len();
                    let max_payload_size = 40;

                    if header.message_type == PING && payload_size <= max_payload_size as usize {
                        debug!("received ping message from {}", src);

                        let id = SessionMembership {
                            session_id: peer_state.try_lock().unwrap().session_id(),
                        };
                        let current_gt = GhostTime {
                            time: ghost_x_form.host_to_ghost(clock.micros()),
                        };
                        let pong_payload = Payload {
                            entries: vec![
                                PayloadEntry::SessionMembership(id),
                                PayloadEntry::GhostTime(current_gt),
                            ],
                        };

                        let pong_message = encode_message(PONG, &pong_payload).unwrap();
                        unicast_socket.send_to(&pong_message, src).await.unwrap();
                        debug!("sent pong message to {}", src);
                    } else {
                        info!("received invalid message from {}", src);
                    }
                }
            }
        });
    }

    pub async fn update_node_state(&self, session_id: SessionId, x_form: GhostXForm) {
        self.peer_state.try_lock().unwrap().node_state.session_id = session_id;
        self.session_state.try_lock().unwrap().ghost_x_form = x_form;
    }
}

pub fn encode_message(message_type: MessageType, payload: &Payload) -> Result<Vec<u8>> {
    let header = MessageHeader { message_type };

    let message_size = PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE + payload.size() as usize;

    if message_size > MAX_MESSAGE_SIZE {
        panic!("exceeded maximum message size");
    }

    let mut encoded = bincode::encode_to_vec(PROTOCOL_HEADER, ENCODING_CONFIG)?;
    encoded.append(&mut bincode::encode_to_vec(header, ENCODING_CONFIG)?);
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

    Ok(bincode::decode_from_slice(
        &data[PROTOCOL_HEADER_SIZE..min_message_size],
        ENCODING_CONFIG,
    )
    .map(|header| (header.0, PROTOCOL_HEADER_SIZE + header.1))?)
}

#[cfg(test)]
mod tests {
    use crate::link::payload::HostTime;

    use super::*;

    fn init_tracing() {
        let subscriber = tracing_subscriber::FmtSubscriber::new();
        tracing::subscriber::set_global_default(subscriber).unwrap();
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
}
