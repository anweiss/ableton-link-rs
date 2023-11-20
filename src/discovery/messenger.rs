use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    net::UdpSocket,
    select,
    sync::{mpsc::Sender, Notify},
    time::Instant,
};
use tracing::{debug, info};

use crate::{
    discovery::{messages::MESSAGE_TYPES, peers::PeerStateMessageType},
    link::{
        node::{NodeId, NodeState},
        payload::{Payload, PayloadEntry},
    },
};

use super::{
    gateway::OnEvent,
    messages::{
        encode_message, parse_message_header, parse_payload, MessageHeader, MessageType, ALIVE,
        BYEBYE, MAX_MESSAGE_SIZE, RESPONSE,
    },
    peers::PeerState,
    LINK_PORT, MULTICAST_ADDR, MULTICAST_IP_ANY,
};

pub fn new_udp_reuseport(addr: SocketAddrV4) -> UdpSocket {
    let udp_sock = socket2::Socket::new(
        // if addr.is_ipv4() {
        //     socket2::Domain::IPV4
        // } else {
        //     socket2::Domain::IPV6
        // },
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        None,
    )
    .unwrap();
    udp_sock.set_reuse_port(true).unwrap();
    // from tokio-rs/mio/blob/master/src/sys/unix/net.rs
    udp_sock.set_cloexec(true).unwrap();
    udp_sock.set_nonblocking(true).unwrap();
    udp_sock.bind(&socket2::SockAddr::from(addr)).unwrap();
    let udp_sock: std::net::UdpSocket = udp_sock.into();
    udp_sock.try_into().unwrap()
}

pub struct Messenger {
    pub interface: Option<Arc<UdpSocket>>,
    pub peer_state: Arc<Mutex<PeerState>>,
    pub ttl: u8,
    pub ttl_ratio: u8,
    pub last_broadcast_time: Arc<Mutex<Instant>>,
    pub tx_event: Sender<OnEvent>,
    pub notifier: Arc<Notify>,
}

impl Messenger {
    pub fn new(
        peer_state: Arc<Mutex<PeerState>>,
        tx_event: Sender<OnEvent>,
        epoch: Instant,
        notifier: Arc<Notify>,
    ) -> Self {
        let socket = new_udp_reuseport(MULTICAST_IP_ANY);
        socket
            .join_multicast_v4(MULTICAST_ADDR, Ipv4Addr::new(0, 0, 0, 0))
            .unwrap();
        socket.set_multicast_loop_v4(true).unwrap();

        Messenger {
            interface: Some(Arc::new(socket)),
            peer_state,
            ttl: 5,
            ttl_ratio: 20,
            last_broadcast_time: Arc::new(Mutex::new(epoch)),
            tx_event,
            notifier,
        }
    }

    pub async fn listen(&self) {
        let socket = self.interface.as_ref().unwrap().clone();
        let peer_state = self.peer_state.clone();
        let ttl = self.ttl;
        let tx_event = self.tx_event.clone();
        let last_broadcast_time = self.last_broadcast_time.clone();

        let _n = self.notifier.clone();

        tokio::spawn(async move {
            loop {
                let mut buf = [0; MAX_MESSAGE_SIZE];

                let (amt, src) = socket.recv_from(&mut buf).await.unwrap();
                let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();

                // TODO figure out how to encode group ID
                if header.ident == peer_state.try_lock().unwrap().ident() && header.group_id == 0 {
                    debug!("ignoring messages from self (peer {})", header.ident);
                    continue;
                } else {
                    debug!(
                        "received message type {} from peer {} at {}",
                        MESSAGE_TYPES[header.message_type as usize], header.ident, src
                    );
                }

                if let SocketAddr::V4(src) = src {
                    match header.message_type {
                        ALIVE => {
                            send_response(
                                socket.clone(),
                                peer_state.clone(),
                                ttl,
                                src,
                                last_broadcast_time.clone(),
                            )
                            .await;

                            receive_peer_state(tx_event.clone(), header, &buf[header_len..amt])
                                .await;
                        }
                        RESPONSE => {
                            receive_peer_state(tx_event.clone(), header, &buf[header_len..amt])
                                .await;
                        }
                        BYEBYE => {
                            receive_bye_bye(tx_event.clone(), header.ident).await;
                        }
                        _ => todo!(),
                    }
                }
            }
            // _ = n.notified() => {
            //     break;
            // }
        });

        broadcast_state(
            self.ttl,
            self.ttl_ratio,
            self.last_broadcast_time.clone(),
            self.interface.as_ref().unwrap().clone(),
            self.peer_state.clone(),
            SocketAddrV4::new(MULTICAST_ADDR, LINK_PORT),
            self.notifier.clone(),
        )
        .await;
    }
}

pub async fn broadcast_state(
    ttl: u8,
    ttl_ratio: u8,
    last_broadcast_time: Arc<Mutex<Instant>>,
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    to: SocketAddrV4,
    n: Arc<Notify>,
) {
    let lbt = last_broadcast_time.clone();
    let s = socket.clone();

    let mut sleep_time = Duration::default();

    loop {
        select! {
            _ = tokio::time::sleep(sleep_time) => {
                let min_broadcast_period = Duration::from_millis(50);
                let nominal_broadcast_period =
                    Duration::from_millis(ttl as u64 * 1000 / ttl_ratio as u64);

                let lbt = lbt.clone();

                let time_since_last_broadcast = if *lbt.try_lock().unwrap() > Instant::now() {
                    0
                } else {
                    Instant::now()
                        .duration_since(*lbt.try_lock().unwrap())
                        .as_millis()
                };

                let tslb = Duration::from_millis(time_since_last_broadcast as u64);
                let delay = if tslb > min_broadcast_period {
                    Duration::default()
                } else {
                    min_broadcast_period - tslb
                };

                sleep_time = if delay > Duration::from_millis(0) {
                    delay
                } else {
                    nominal_broadcast_period
                };

                if delay < Duration::from_millis(1) {
                    send_peer_state(s.clone(), peer_state.clone(), ttl, ALIVE, to, lbt).await;
                }
            }
            _ = n.notified() => {
                break;
            }
        }
    }
}

pub async fn send_response(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    to: SocketAddrV4,
    last_broadcast_time: Arc<Mutex<Instant>>,
) {
    send_peer_state(socket, peer_state, ttl, RESPONSE, to, last_broadcast_time).await
}

pub async fn send_message(
    socket: Arc<UdpSocket>,
    from: NodeId,
    ttl: u8,
    message_type: MessageType,
    payload: &Payload,
    to: SocketAddrV4,
) {
    socket.set_broadcast(true).unwrap();
    socket.set_multicast_ttl_v4(2).unwrap();
    socket.set_multicast_loop_v4(true).unwrap();

    let message = encode_message(from, ttl, message_type, payload).unwrap();

    let _sent_bytes = socket.send_to(&message, to).await.unwrap();
}

pub async fn send_peer_state(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    message_type: MessageType,
    to: SocketAddrV4,
    last_broadcast_time: Arc<Mutex<Instant>>,
) {
    let ident = peer_state.try_lock().unwrap().ident();
    let peer_state = peer_state.try_lock().unwrap().clone();

    send_message(socket, ident, ttl, message_type, &peer_state.into(), to).await;

    *last_broadcast_time.try_lock().unwrap() = Instant::now();
}

pub async fn receive_peer_state(tx: Sender<OnEvent>, header: MessageHeader, buf: &[u8]) {
    let payload = parse_payload(buf).unwrap();
    let measurement_endpoint = payload.entries.iter().find_map(|e| {
        if let PayloadEntry::MeasurementEndpointV4(me) = e {
            me.endpoint
        } else {
            None
        }
    });

    let node_state: NodeState = NodeState::from_payload(header.ident, &payload);

    debug!("sending peer state to gateway {}", node_state.ident());
    let _ = tx
        .send(OnEvent::PeerState(PeerStateMessageType {
            node_state,
            ttl: header.ttl,
            measurement_endpoint,
        }))
        .await;

    // info!("peer state sent")
}

pub async fn receive_bye_bye(tx: Sender<OnEvent>, node_id: NodeId) {
    debug!("receiving bye bye from peer {}", node_id);
    tokio::spawn(async move { tx.send(OnEvent::Byebye(node_id)).await });
}

pub fn send_byebye(node_state: NodeId) {
    info!("sending bye bye");

    let socket = new_udp_reuseport(MULTICAST_IP_ANY);
    socket.set_broadcast(true).unwrap();
    socket.set_multicast_ttl_v4(2).unwrap();

    let message = encode_message(node_state, 0, BYEBYE, &Payload::default()).unwrap();

    let _ = socket
        .into_std()
        .unwrap()
        .send_to(&message, (MULTICAST_ADDR, LINK_PORT))
        .unwrap();
}
