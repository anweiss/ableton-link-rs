use std::sync::{Arc, Mutex};

use bincode::{Decode, Encode};

use tokio::{net::UdpSocket, sync::Notify};
use tracing::{debug, info};

use crate::{
    discovery::{messages::parse_payload, ENCODING_CONFIG},
    link::{
        payload::{GhostTime, PayloadEntry},
        sessions::SessionMembership,
    },
};

use super::{
    clock::Clock, ghostxform::GhostXForm, payload::Payload, sessions::SessionId, Result,
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
    pub session_id: Arc<Mutex<SessionId>>,
    pub ghost_x_form: Arc<Mutex<GhostXForm>>,
    pub clock: Clock,
    pub unicast_socket: Option<Arc<UdpSocket>>,
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
        }
    }

    pub async fn listen(&self, _notifier: Arc<Notify>) {
        let unicast_socket = self.unicast_socket.as_ref().unwrap().clone();
        let session_id = self.session_id.clone();
        let ghost_x_form = self.ghost_x_form.clone();
        let clock = self.clock;

        info!(
            "listening for ping messages on {}",
            unicast_socket.local_addr().unwrap()
        );

        let mut ping_message_received = false;

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
                        if !ping_message_received {
                            info!("received ping message from {}", src);
                        }

                        let payload = parse_payload(&buf[header_len..amt]).unwrap();

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
                            session_id: *session_id.try_lock().unwrap(),
                        };
                        let current_gt = GhostTime {
                            time: ghost_x_form
                                .try_lock()
                                .unwrap()
                                .host_to_ghost(clock.micros()),
                        };

                        payload_entries.push(PayloadEntry::SessionMembership(id));
                        payload_entries.push(PayloadEntry::GhostTime(current_gt));

                        let pong_payload = Payload {
                            entries: payload_entries,
                        };

                        if !ping_message_received {
                            debug!("pong_payload {:?}", pong_payload);
                        }

                        let pong_message = encode_message(PONG, &pong_payload).unwrap();
                        unicast_socket.send_to(&pong_message, src).await.unwrap();
                        if !ping_message_received {
                            debug!("sent pong message to {}", src);
                        }

                        ping_message_received = true;
                    } else {
                        debug!("received invalid message from {}", src);
                    }
                }
            }
        });
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
}
