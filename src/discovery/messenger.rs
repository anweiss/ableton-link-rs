use std::{
    borrow::BorrowMut,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use bincode::enc;
use tokio::{
    net::UdpSocket,
    runtime::Runtime,
    sync::{
        oneshot::{Receiver, Sender},
        Notify,
    },
};

use crate::{
    discovery::messages::PROTOCOL_HEADER_SIZE,
    link::node::{NodeId, NodeState},
};

use super::{
    gateway::OnEvent,
    messages::{
        encode_message, parse_message_header, parse_payload, MessageHeader, MessageType, ALIVE,
        BYEBYE, RESPONSE,
    },
    payload::Payload,
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
    pub node_state: Arc<Mutex<NodeState>>,
    pub ttl: u8,
    pub ttl_ratio: u8,
    pub tx: Sender<OnEvent>,
}

impl Messenger {
    pub async fn new(node_state: Arc<Mutex<NodeState>>, tx: Sender<OnEvent>) -> Self {
        let socket = new_udp_reuseport(IP_ANY);
        socket
            .join_multicast_v4(MULTICAST_ADDR, Ipv4Addr::new(0, 0, 0, 0))
            .unwrap();
        socket.set_multicast_loop_v4(true).unwrap();

        Messenger {
            interface: Some(Arc::new(socket)),
            node_state,
            ttl: 0,
            ttl_ratio: 0,
            tx,
        }
    }

    pub async fn listen(&self) {
        let socket = self.interface.as_ref().unwrap().clone();
        let node_state = self.node_state.lock().unwrap().clone();
        let ttl = self.ttl;

        tokio::spawn(async move {
            loop {
                let mut buf = [0; 1024];
                let socket = socket.clone();
                let (amt, src) = socket.recv_from(&mut buf).await.unwrap();

                let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();

                println!(
                    "received message type {} from peer {:?}",
                    header.message_type, header.ident
                );

                match header.message_type {
                    ALIVE => {
                        tokio::spawn(async move {
                            send_response(socket, node_state, ttl, src).await;
                        });

                        // empty payload
                        if PROTOCOL_HEADER_SIZE + header_len == amt {
                            continue;
                        }

                        receive_peer_state(&header, &buf[header_len..amt]).await;
                    }
                    _ => todo!(),
                }
            }
        });
    }

    pub async fn update_state(&mut self, state: NodeState) {
        *self.node_state.lock().unwrap() = state;
    }

    pub async fn broadcast_state(&self) {
        todo!()
    }
}

pub async fn send_response(socket: Arc<UdpSocket>, node_state: NodeState, ttl: u8, to: SocketAddr) {
    send_peer_state(socket, node_state, ttl, RESPONSE, to).await
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

    let message = encode_message(from, ttl, message_type, payload).unwrap();

    let sent_bytes = socket.send_to(&message, to).await.unwrap();
    println!("sent {} bytes", sent_bytes)
}

pub async fn send_peer_state(
    socket: Arc<UdpSocket>,
    node_state: NodeState,
    ttl: u8,
    message_type: MessageType,
    to: SocketAddr,
) {
    println!("sending peer state");

    send_message(
        socket,
        node_state.ident(),
        ttl,
        message_type,
        &node_state.into(),
        to,
    )
    .await
}

pub async fn receive_peer_state(header: &MessageHeader, buf: &[u8]) {
    println!("receiving peer state");
    let payload = parse_payload(buf).unwrap();

    todo!()
}

pub fn send_byebye(node_state: NodeId) {
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
