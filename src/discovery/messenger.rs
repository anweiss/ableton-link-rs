use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_recursion::async_recursion;
use tokio::{net::UdpSocket, sync::mpsc::Sender, time::Instant};
use tracing::{debug, info};

use crate::{
    discovery::{messages::MESSAGE_TYPES, peers::PeerStateMessageType},
    link::{
        node::{NodeId, NodeState},
        payload::Payload,
    },
};

use super::{
    gateway::OnEvent,
    messages::{
        encode_message, parse_message_header, parse_payload, MessageHeader, MessageType, ALIVE,
        BYEBYE, MAX_MESSAGE_SIZE, RESPONSE,
    },
    peers::PeerState,
    IP_ANY, LINK_PORT, MULTICAST_ADDR,
};

pub fn new_udp_reuseport(addr: SocketAddr) -> UdpSocket {
    let udp_sock = socket2::Socket::new(
        if addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        },
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
    pub peer_state: Arc<std::sync::Mutex<PeerState>>,
    pub ttl: u8,
    pub ttl_ratio: u8,
    pub last_broadcast_time: Arc<Mutex<Instant>>,
    pub tx_event: Sender<OnEvent>,
}

impl Messenger {
    pub async fn new(peer_state: PeerState, tx_event: Sender<OnEvent>, epoch: Instant) -> Self {
        let socket = new_udp_reuseport(IP_ANY);
        socket
            .join_multicast_v4(MULTICAST_ADDR, Ipv4Addr::new(0, 0, 0, 0))
            .unwrap();
        socket.set_multicast_loop_v4(true).unwrap();

        Messenger {
            interface: Some(Arc::new(socket)),
            peer_state: Arc::new(std::sync::Mutex::new(peer_state)),
            ttl: 5,
            ttl_ratio: 20,
            last_broadcast_time: Arc::new(Mutex::new(epoch)),
            tx_event,
        }
    }

    pub async fn listen(&self) {
        let socket = self.interface.as_ref().unwrap().clone();
        let peer_state = self.peer_state.clone();
        let ttl = self.ttl;
        let tx_event = self.tx_event.clone();
        let last_broadcast_time = self.last_broadcast_time.clone();

        tokio::spawn(async move {
            loop {
                let tx = tx_event.clone();
                let peer_state = peer_state.clone();

                let last_broadcast_time = last_broadcast_time.clone();

                let mut buf = [0; MAX_MESSAGE_SIZE];
                let socket = socket.clone();

                let (amt, src) = socket.recv_from(&mut buf).await.unwrap();

                let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();

                if header.ident == peer_state.lock().unwrap().ident() && header.group_id == 0 {
                    debug!("ignoring messages from self (peer {})", header.ident);
                    continue;
                } else {
                    info!(
                        "received message type {} from peer {}",
                        MESSAGE_TYPES[header.message_type as usize], header.ident
                    );
                }

                match header.message_type {
                    ALIVE => {
                        tokio::spawn(async move {
                            send_response(
                                socket,
                                peer_state.clone(),
                                ttl,
                                src,
                                last_broadcast_time.clone(),
                            )
                            .await;
                        });

                        receive_peer_state(tx, header, &buf[header_len..amt]).await;
                    }
                    RESPONSE => {
                        receive_peer_state(tx, header, &buf[header_len..amt]).await;
                    }
                    BYEBYE => {
                        receive_bye_bye(tx, header.ident).await;
                    }
                    _ => todo!(),
                }
            }
        });

        broadcast_state(
            self.ttl,
            self.ttl_ratio,
            self.last_broadcast_time.clone(),
            self.interface.as_ref().unwrap().clone(),
            self.peer_state.clone(),
            SocketAddr::new(MULTICAST_ADDR.into(), LINK_PORT),
        )
        .await;
    }

    pub async fn update_state(&self, state: PeerState) {
        *self.peer_state.lock().unwrap() = state;

        broadcast_state(
            self.ttl,
            self.ttl_ratio,
            self.last_broadcast_time.clone(),
            self.interface.as_ref().unwrap().clone(),
            self.peer_state.clone(),
            SocketAddr::new(MULTICAST_ADDR.into(), LINK_PORT),
        )
        .await;
    }
}

#[async_recursion]
pub async fn broadcast_state(
    ttl: u8,
    ttl_ratio: u8,
    last_broadcast_time: Arc<Mutex<Instant>>,
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    to: SocketAddr,
) {
    let min_broadcast_period = Duration::from_millis(50);
    let nominal_broadcast_period = Duration::from_millis(ttl as u64 * 1000 / ttl_ratio as u64);

    let time_since_last_broadcast = if *last_broadcast_time.lock().unwrap() > Instant::now() {
        0
    } else {
        Instant::now()
            .duration_since(*last_broadcast_time.lock().unwrap())
            .as_millis()
    };

    let tslb = Duration::from_millis(time_since_last_broadcast as u64);
    let delay = if tslb > min_broadcast_period {
        Duration::default()
    } else {
        min_broadcast_period - tslb
    };

    let lbt = last_broadcast_time.clone();
    let s = socket.clone();

    let ps = peer_state.clone();

    tokio::spawn(async move {
        let sleep_time = if delay > Duration::from_millis(0) {
            delay
        } else {
            nominal_broadcast_period
        };

        tokio::time::sleep(sleep_time).await;
        broadcast_state(ttl, ttl_ratio, lbt, s, ps, to).await;
    });

    if delay < Duration::from_millis(1) {
        debug!("broadcasting state");
        send_peer_state(
            socket,
            peer_state.clone(),
            ttl,
            ALIVE,
            to,
            last_broadcast_time,
        )
        .await;
    }
}

pub async fn send_response(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    to: SocketAddr,
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
    to: SocketAddr,
) {
    socket.set_broadcast(true).unwrap();
    socket.set_multicast_ttl_v4(2).unwrap();
    socket.set_multicast_loop_v4(false).unwrap();

    let message = encode_message(from, ttl, message_type, payload).unwrap();

    let _sent_bytes = socket.send_to(&message, to).await.unwrap();
}

pub async fn send_peer_state(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    message_type: MessageType,
    to: SocketAddr,
    last_broadcast_time: Arc<Mutex<Instant>>,
) {
    let ident = peer_state.lock().unwrap().ident();
    let node_state = peer_state.lock().unwrap().node_state.clone();

    send_message(socket, ident, ttl, message_type, &node_state.into(), to).await;

    *last_broadcast_time.lock().unwrap() = Instant::now();
}

pub async fn receive_peer_state(tx: Sender<OnEvent>, header: MessageHeader, buf: &[u8]) {
    let payload = parse_payload(buf).unwrap();
    let node_state: NodeState = NodeState::from_payload(header.ident, &payload);

    tokio::spawn(async move {
        tx.send(OnEvent::PeerState(PeerStateMessageType {
            node_state,
            ttl: header.ttl,
        }))
        .await
    });
}

pub async fn receive_bye_bye(tx: Sender<OnEvent>, node_id: NodeId) {
    debug!("receiving bye bye from peer {}", node_id);
    tokio::spawn(async move { tx.send(OnEvent::Byebye(node_id)).await });
}

pub fn send_byebye(node_state: NodeId) {
    info!("sending bye bye");

    let socket = new_udp_reuseport(IP_ANY);
    socket.set_broadcast(true).unwrap();
    socket.set_multicast_ttl_v4(2).unwrap();

    let message = encode_message(node_state, 0, BYEBYE, &Payload::default()).unwrap();

    let _ = socket
        .into_std()
        .unwrap()
        .send_to(&message, (MULTICAST_ADDR, LINK_PORT))
        .unwrap();
}
