use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use crate::encoding::{self, Decode, Encode};
use crate::sync::lock;

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

#[cfg(test)]
type SnapshotPause = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[derive(Debug, Clone)]
pub struct PingResponder {
    pub session_id: Arc<Mutex<SessionId>>,
    pub ghost_x_form: Arc<Mutex<GhostXForm>>,
    pub clock: Clock,
    pub unicast_socket: Option<Arc<UdpSocket>>,
    ping_message_received: Arc<AtomicBool>,
    #[cfg(test)]
    snapshot_pause: Arc<Mutex<Option<SnapshotPause>>>,
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
            #[cfg(test)]
            snapshot_pause: Arc::new(Mutex::new(None)),
        }
    }

    /// Handle a single PING datagram received on the shared unicast socket and
    /// reply with a PONG. Datagrams are dispatched by the receive loop owned by
    /// `MeasurementService`, which is the only reader of the shared socket.
    pub async fn handle_ping(&self, data: &[u8], src: SocketAddr) {
        let Some(socket) = self.unicast_socket.as_ref() else {
            return;
        };
        let Some(message) = self.pong_message(data, src) else {
            return;
        };
        if let Err(error) = socket.send_to(&message, src).await {
            debug!("failed to send pong message to {}: {}", src, error);
        }
    }

    /// Build a reply independently of its transport. Managed discovery sends
    /// this through the same bound registration it advertises for measurement.
    pub(crate) fn pong_message(&self, data: &[u8], src: SocketAddr) -> Option<Vec<u8>> {
        if data.len() < PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE
            || !data.starts_with(&PROTOCOL_HEADER)
            || data[PROTOCOL_HEADER_SIZE] != PING
        {
            return None;
        }
        let (_, header_len) = match parse_message_header(data) {
            Ok(header) => header,
            Err(e) => {
                debug!("failed to parse ping message header from {}: {}", src, e);
                return None;
            }
        };

        let payload_size = data[header_len..].len();
        let max_payload_size = 40;

        if payload_size > max_payload_size {
            debug!("received invalid message from {}", src);
            return None;
        }

        let ping_message_received = self.ping_message_received.swap(true, Ordering::Relaxed);

        if !ping_message_received {
            info!("received ping message from {}", src);
        }

        let payload = match parse_payload(&data[header_len..]) {
            Ok(payload) => payload,
            Err(e) => {
                debug!("failed to parse ping payload from {}: {}", src, e);
                return None;
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

        // A contended PING is skipped, not queued: the sender's bounded retry
        // owns recovery. Read membership and clock in the writer's lock order.
        let (session_id, time) = {
            let id = crate::platform::io_context::try_state(&self.session_id)?;
            #[cfg(test)]
            self.pause_snapshot();
            let form = crate::platform::io_context::try_state(&self.ghost_x_form)?;
            (*id, form.host_to_ghost(self.clock.micros()))
        };
        let id = SessionMembership { session_id };
        let current_gt = GhostTime { time };

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
                return None;
            }
        };
        Some(pong_message)
    }

    #[cfg(test)]
    fn pause_snapshot(&self) {
        if let Some((reached, release)) = self.snapshot_pause.lock().unwrap().take() {
            reached.send(()).unwrap();
            release
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
        }
    }

    pub async fn update_node_state(&self, session_id: SessionId, x_form: GhostXForm) {
        crate::platform::io_context::retry_state(|| {
            let mut id = crate::platform::io_context::try_state(&self.session_id)?;
            let mut form = crate::platform::io_context::try_state(&self.ghost_x_form)?;
            *id = session_id;
            *form = x_form;
            Some(())
        })
        .await;
    }

    pub(crate) fn update_node_state_now(&self, session_id: SessionId, x_form: GhostXForm) {
        let mut current_id = lock(&self.session_id);
        let mut current_form = lock(&self.ghost_x_form);
        *current_id = session_id;
        *current_form = x_form;
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
        return Err(crate::link::error::Error::Protocol("invalid message size"));
    }

    if !data.starts_with(&PROTOCOL_HEADER) {
        return Err(crate::link::error::Error::Protocol(
            "invalid protocol header",
        ));
    }

    let (header, consumed) = encoding::decode_from_slice::<MessageHeader>(
        &data[PROTOCOL_HEADER_SIZE..min_message_size],
    )?;
    Ok((header, PROTOCOL_HEADER_SIZE + consumed))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn admission_owned_reader_skips_contended_ping_and_recovers() {
        use crate::{
            discovery::peers::PeerState,
            link::{measurement::MeasurementService, state::SessionState},
        };
        use std::time::Duration;
        for hold_form in [false, true] {
            let io = crate::platform::io_context::IoContext::new().unwrap();
            let service = io
                .spawn(async {
                    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                    let (results, _result_rx) = tokio::sync::mpsc::channel(1);
                    let (_requests, request_rx) = tokio::sync::mpsc::channel(1);
                    let notifier = Arc::new(tokio::sync::Notify::new());
                    let service = MeasurementService::new(
                        socket,
                        Arc::new(Mutex::new(PeerState::default())),
                        Arc::new(Mutex::new(SessionState::default())),
                        Clock::new(),
                        results,
                        notifier.clone(),
                        request_rx,
                    )
                    .await;
                    service.listen(notifier).await;
                    service
                })
                .await
                .unwrap();
            let responder = &service.ping_responder;
            let id = responder.session_id.clone();
            let form = responder.ghost_x_form.clone();
            let (ready, ready_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            fn hold<T>(
                state: Arc<Mutex<T>>,
                ready: std::sync::mpsc::Sender<()>,
                release: std::sync::mpsc::Receiver<()>,
            ) {
                let _guard = state.lock().unwrap();
                ready.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            let holder = std::thread::spawn(move || {
                if hold_form {
                    hold(form, ready, release_rx)
                } else {
                    hold(id, ready, release_rx)
                }
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let ping = encode_message(
                PING,
                &Payload {
                    entries: vec![PayloadEntry::HostTime(HostTime::default())],
                },
            )
            .unwrap();
            peer.send_to(&ping, service.shared_socket.local_addr().unwrap())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while !responder.ping_message_received.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            let heartbeat = io.spawn(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
            });
            let responsive = matches!(
                tokio::time::timeout(Duration::from_millis(100), heartbeat).await,
                Ok(Ok(()))
            );
            let mut bytes = [0u8; MAX_MESSAGE_SIZE];
            let skipped =
                tokio::time::timeout(Duration::from_millis(20), peer.recv_from(&mut bytes))
                    .await
                    .is_err();
            release.send(()).unwrap();
            holder.join().unwrap();
            assert!(
                responsive && skipped,
                "held public responder guard must not park real shared reader: {hold_form}"
            );
            peer.send_to(&ping, service.shared_socket.local_addr().unwrap())
                .await
                .unwrap();
            let (size, _) =
                tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                parse_message_header(&bytes[..size]).unwrap().0.message_type,
                PONG
            );
            service.stop().await;
            drop(service);
            drop(io);
        }
    }

    #[tokio::test]
    async fn admission_pong_forced_publication_interleave_keeps_pair_coherent() {
        use crate::link::node::NodeId;
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let first = SessionId(NodeId::from_array([1; 8]));
        let second = SessionId(NodeId::from_array([2; 8]));
        let responder = PingResponder::new(
            socket,
            first,
            GhostXForm {
                slope: 0.0,
                intercept: chrono::Duration::microseconds(1),
            },
            Clock::new(),
        );
        let (ready, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::channel();
        *responder.snapshot_pause.lock().unwrap() = Some((ready, release_rx));
        let reader = responder.clone();
        let task = std::thread::spawn(move || {
            let ping = encode_message(
                PING,
                &Payload {
                    entries: vec![PayloadEntry::HostTime(HostTime::default())],
                },
            )
            .unwrap();
            reader
                .pong_message(&ping, "127.0.0.1:12345".parse().unwrap())
                .unwrap()
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let mut writer = Box::pin(responder.update_node_state(
            second,
            GhostXForm {
                slope: 0.0,
                intercept: chrono::Duration::microseconds(2),
            },
        ));
        let pending = writer
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending();
        release.send(()).unwrap();
        let bytes = task.join().unwrap();
        let (_, offset) = parse_message_header(&bytes).unwrap();
        let payload = parse_payload(&bytes[offset..]).unwrap();
        let id = payload
            .entries
            .iter()
            .find_map(|entry| {
                if let PayloadEntry::SessionMembership(value) = entry {
                    Some(value.session_id)
                } else {
                    None
                }
            })
            .unwrap();
        let time = payload
            .entries
            .iter()
            .find_map(|entry| {
                if let PayloadEntry::GhostTime(value) = entry {
                    Some(value.time)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(
            (id, time),
            (first, chrono::Duration::microseconds(1)),
            "writer ran between PONG membership and clock reads"
        );
        assert!(
            pending,
            "publication must wait for the whole paired snapshot"
        );
        writer.await;
        assert_eq!(*responder.session_id.lock().unwrap(), second);
    }

    #[tokio::test]
    async fn admission_pong_membership_and_transform_are_one_publication() {
        use crate::link::{node::NodeId, sessions::SessionId};
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let responder = PingResponder::new(
            socket,
            SessionId(NodeId::from_array([1; 8])),
            GhostXForm {
                slope: 0.0,
                intercept: chrono::Duration::microseconds(1),
            },
            Clock::new(),
        );
        let writer = responder.clone();
        let worker = std::thread::spawn(move || {
            for n in 0..20_000 {
                let marker = (n % 2 + 1) as u8;
                writer.update_node_state_now(
                    SessionId(NodeId::from_array([marker; 8])),
                    GhostXForm {
                        slope: 0.0,
                        intercept: chrono::Duration::microseconds(i64::from(marker)),
                    },
                );
                std::thread::yield_now();
            }
        });
        let ping = encode_message(
            PING,
            &Payload {
                entries: vec![PayloadEntry::HostTime(HostTime::default())],
            },
        )
        .unwrap();
        let mut observed = 0;
        for _ in 0..20_000 {
            if let Some(bytes) = responder.pong_message(&ping, "127.0.0.1:12345".parse().unwrap()) {
                let (_, offset) = parse_message_header(&bytes).unwrap();
                let payload = parse_payload(&bytes[offset..]).unwrap();
                let id = payload
                    .entries
                    .iter()
                    .find_map(|entry| {
                        if let PayloadEntry::SessionMembership(value) = entry {
                            Some(value.session_id)
                        } else {
                            None
                        }
                    })
                    .unwrap();
                let time = payload
                    .entries
                    .iter()
                    .find_map(|entry| {
                        if let PayloadEntry::GhostTime(value) = entry {
                            Some(value.time)
                        } else {
                            None
                        }
                    })
                    .unwrap();
                assert!(matches!(time.num_microseconds(), Some(1 | 2)));
                assert_eq!(
                    id,
                    SessionId(NodeId::from_array(
                        [time.num_microseconds().unwrap() as u8; 8]
                    )),
                    "PONG paired membership with another publication's clock"
                );
                observed += 1;
            }
        }
        worker.join().unwrap();
        assert!(observed > 0);
    }

    use crate::link::{
        encoding::PAYLOAD_ENTRY_HEADER_SIZE,
        payload::{HostTime, HOST_TIME_SIZE},
    };

    use super::*;

    fn init_tracing() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    #[tokio::test]
    async fn pong_snapshot_keeps_session_and_transform_together_during_update() {
        use crate::link::node::NodeId;
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let old_id = SessionId(NodeId::from_array([1; 8]));
        let new_id = SessionId(NodeId::from_array([2; 8]));
        let responder = PingResponder {
            session_id: Arc::new(Mutex::new(old_id)),
            ghost_x_form: Arc::new(Mutex::new(GhostXForm {
                slope: 0.0,
                intercept: chrono::Duration::microseconds(100),
            })),
            clock: Clock::new(),
            unicast_socket: None,
            ping_message_received: Arc::new(AtomicBool::new(false)),
            snapshot_pause: Arc::new(Mutex::new(None)),
        };
        let ping = encode_message(PING, &Payload::default()).unwrap();
        let src = "127.0.0.1:12345".parse().unwrap();
        let first = responder.pong_message(&ping, src).unwrap();
        let mut writer = Box::pin(responder.update_node_state(
            new_id,
            GhostXForm {
                slope: 0.0,
                intercept: chrono::Duration::microseconds(200),
            },
        ));
        {
            let _held_transform = lock(&responder.ghost_x_form);
            assert!(
                responder.pong_message(&ping, src).is_none(),
                "a contended snapshot skips the PING instead of blocking"
            );
            assert!(writer
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending());
            assert!(
                responder.session_id.try_lock().is_ok(),
                "cooperative publication must release partial guards"
            );
        }
        writer.await;
        let second = responder.pong_message(&ping, src).unwrap();
        for (message, expected_id, expected_time) in [(first, old_id, 100), (second, new_id, 200)] {
            let (_, header) = parse_message_header(&message).unwrap();
            let payload = parse_payload(&message[header..]).unwrap();
            assert!(payload.entries.iter().any(|entry| matches!(entry, PayloadEntry::SessionMembership(id) if id.session_id == expected_id)));
            assert!(payload.entries.iter().any(|entry| matches!(entry, PayloadEntry::GhostTime(time) if time.time == chrono::Duration::microseconds(expected_time))));
        }
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
