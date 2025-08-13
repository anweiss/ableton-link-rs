use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

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

    // Try to set reuse address for better socket sharing
    udp_sock.set_reuse_address(true).unwrap();

    // Set SO_REUSEPORT on Unix systems for better multicast support
    #[cfg(unix)]
    {
        let fd = udp_sock.as_raw_fd();
        unsafe {
            let optval: libc::c_int = 1;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );
        }
    }

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
    pub enabled: Arc<Mutex<bool>>,
}

impl Messenger {
    pub fn new(
        peer_state: Arc<Mutex<PeerState>>,
        tx_event: Sender<OnEvent>,
        epoch: Instant,
        notifier: Arc<Notify>,
        enabled: Arc<Mutex<bool>>,
    ) -> Self {
        let socket = new_udp_reuseport(MULTICAST_IP_ANY);
        socket
            .join_multicast_v4(MULTICAST_ADDR, Ipv4Addr::new(0, 0, 0, 0))
            .unwrap();
        socket.set_multicast_loop_v4(true).unwrap();

        Messenger {
            interface: Some(Arc::new(socket)),
            peer_state,
            ttl: 2, // Reduced from 5 to 2 seconds for faster peer timeout detection
            ttl_ratio: 20,
            last_broadcast_time: Arc::new(Mutex::new(epoch)),
            tx_event,
            notifier,
            enabled,
        }
    }

    pub async fn listen(&self) {
        let socket = self.interface.as_ref().unwrap().clone();
        let peer_state = self.peer_state.clone();
        let ttl = self.ttl;
        let tx_event = self.tx_event.clone();
        let last_broadcast_time = self.last_broadcast_time.clone();
        let enabled = self.enabled.clone();

        let _n = self.notifier.clone();

        tokio::spawn(async move {
            loop {
                let mut buf = [0; MAX_MESSAGE_SIZE];

                let (amt, src) = socket.recv_from(&mut buf).await.unwrap();
                let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();

                // TODO figure out how to encode group ID
                let should_ignore = match peer_state.try_lock() {
                    Ok(guard) => header.ident == guard.ident() && header.group_id == 0,
                    Err(_) => false, // If we can't get the lock, don't ignore
                };

                if should_ignore {
                    debug!("ignoring messages from self (peer {})", header.ident);
                    continue;
                } else {
                    debug!(
                        "received message type {} from peer {} at {}",
                        MESSAGE_TYPES[header.message_type as usize], header.ident, src
                    );
                }

                // Check if Link is enabled before processing ALIVE and RESPONSE messages
                // BYEBYE messages should still be processed even when disabled to properly clean up peers
                let is_enabled = if let Ok(enabled_guard) = enabled.try_lock() {
                    *enabled_guard
                } else {
                    false
                };

                if let SocketAddr::V4(src) = src {
                    debug!(
                        "Received message type {} from peer {}",
                        header.message_type, header.ident
                    );
                    match header.message_type {
                        ALIVE => {
                            if !is_enabled {
                                debug!(
                                    "ignoring ALIVE message from peer {} because Link is disabled",
                                    header.ident
                                );
                                continue;
                            }

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
                            if !is_enabled {
                                debug!("ignoring RESPONSE message from peer {} because Link is disabled", header.ident);
                                continue;
                            }

                            receive_peer_state(tx_event.clone(), header, &buf[header_len..amt])
                                .await;
                        }
                        BYEBYE => {
                            info!("Received BYEBYE message from peer {}", header.ident);
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
            self.enabled.clone(),
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
    enabled: Arc<Mutex<bool>>,
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

                let time_since_last_broadcast = match lbt.try_lock() {
                    Ok(last_time) => {
                        if *last_time > Instant::now() {
                            0
                        } else {
                            Instant::now()
                                .duration_since(*last_time)
                                .as_millis()
                        }
                    }
                    Err(_) => {
                        // If we can't get the lock, use a conservative value
                        0
                    }
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
                    // Only broadcast if Link is enabled
                    let should_broadcast = if let Ok(enabled_guard) = enabled.try_lock() {
                        *enabled_guard
                    } else {
                        false
                    };

                    if should_broadcast {
                        send_peer_state(s.clone(), peer_state.clone(), ttl, ALIVE, to, lbt).await;
                    }
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
    let (ident, peer_state_clone) = match peer_state.try_lock() {
        Ok(guard) => (guard.ident(), guard.clone()),
        Err(_) => {
            // If we can't get the lock, skip this broadcast
            return;
        }
    };

    send_message(
        socket,
        ident,
        ttl,
        message_type,
        &peer_state_clone.into(),
        to,
    )
    .await;

    if let Ok(mut last_time) = last_broadcast_time.try_lock() {
        *last_time = Instant::now();
    }
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
    info!("Received BYEBYE message from peer {}", node_id);
    tokio::spawn(async move {
        if let Err(e) = tx.send(OnEvent::Byebye(node_id)).await {
            debug!("Failed to send BYEBYE event: {:?}", e);
        } else {
            info!("Successfully forwarded BYEBYE event for peer {}", node_id);
        }
    });
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
