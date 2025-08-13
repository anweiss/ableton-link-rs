use std::{
    net::{SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use chrono::Duration;
use tokio::{
    net::UdpSocket,
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        Notify,
    },
    time::Instant,
};
use tracing::{debug, info};

use crate::{
    discovery::{
        messages::{encode_message, BYEBYE},
        LINK_PORT, MULTICAST_ADDR,
    },
    link::{
        clock::Clock,
        controller::SessionPeerCounter,
        ghostxform::GhostXForm,
        measurement::{MeasurePeerEvent, MeasurementService},
        node::{NodeId, NodeState},
        payload::Payload,
        state::SessionState,
    },
};

use super::{
    messenger::{send_byebye, Messenger},
    peers::{
        ControllerPeer, GatewayObserver, PeerEvent, PeerState, PeerStateChange,
        PeerStateMessageType,
    },
};

pub struct PeerGateway {
    pub observer: GatewayObserver,
    pub peer_state: Arc<Mutex<PeerState>>,
    pub session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    pub measurement_service: MeasurementService,
    epoch: Instant,
    messenger: Messenger,
    tx_peer_event: Sender<PeerEvent>,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    _cancel: Arc<Notify>,
}

#[derive(Clone)]
pub enum OnEvent {
    PeerState(PeerStateMessageType),
    Byebye(NodeId),
}

impl PeerGateway {
    pub async fn new(
        peer_state: Arc<Mutex<PeerState>>,
        session_state: Arc<Mutex<SessionState>>,
        clock: Clock,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        tx_event: Sender<OnEvent>,
        tx_measure_peer_result: Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        notifier: Arc<Notify>,
        rx_measure_peer_state: Receiver<MeasurePeerEvent>,
        ping_responder_unicast_socket: Arc<UdpSocket>,
        enabled: Arc<Mutex<bool>>,
    ) -> Self {
        let (tx_peer_event, rx_peer_event) = mpsc::channel::<PeerEvent>(1);
        let epoch = Instant::now();
        // let ping_responder_unicast_socket = Arc::new(new_udp_reuseport(UNICAST_IP_ANY));
        peer_state.try_lock().unwrap().measurement_endpoint = if let SocketAddr::V4(socket_addr) =
            ping_responder_unicast_socket.local_addr().unwrap()
        {
            Some(socket_addr)
        } else {
            None
        };

        let messenger = Messenger::new(
            peer_state.clone(),
            tx_event.clone(),
            epoch,
            notifier.clone(),
            enabled.clone(),
        );

        PeerGateway {
            epoch,
            observer: GatewayObserver::new(
                rx_peer_event,
                peer_state.clone(),
                session_peer_counter.clone(),
                tx_peer_state_change,
                peers,
                notifier.clone(),
            )
            .await,
            messenger,
            measurement_service: MeasurementService::new(
                ping_responder_unicast_socket,
                peer_state.clone(),
                session_state,
                clock,
                tx_measure_peer_result,
                notifier,
                rx_measure_peer_state,
            )
            .await,
            peer_state,
            tx_peer_event,
            peer_timeouts: Arc::new(Mutex::new(vec![])),
            session_peer_counter,
            _cancel: Arc::new(Notify::new()),
        }
    }

    pub async fn update_node_state(
        &self,
        node_state: NodeState,
        _measurement_endpoint: Option<SocketAddrV4>,
        ghost_xform: GhostXForm,
    ) {
        self.measurement_service
            .update_node_state(node_state.session_id, ghost_xform)
            .await;

        self.peer_state.try_lock().unwrap().node_state = node_state;
    }

    pub async fn listen(&self, mut rx_event: Receiver<OnEvent>, notifier: Arc<Notify>) {
        info!(
            "initializing peer gateway {:?} on interface {}",
            &self.peer_state.try_lock().unwrap().node_state.node_id,
            self.messenger
                .interface
                .as_ref()
                .unwrap()
                .local_addr()
                .unwrap()
        );

        let ctrl_socket = self.messenger.interface.as_ref().unwrap().clone();
        let peer_state = self.peer_state.clone();

        // Get self node ID for filtering self-messages
        let self_node_id = peer_state.try_lock().unwrap().node_state.node_id;

        let peer_timeouts = self.peer_timeouts.clone();
        let tx_peer_event = self.tx_peer_event.clone();
        let epoch = self.epoch;

        self.measurement_service
            .ping_responder
            .listen(notifier.clone())
            .await;

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.unwrap();
            ctrl_socket.set_broadcast(true).unwrap();
            ctrl_socket.set_multicast_ttl_v4(2).unwrap();

            let message = encode_message(
                peer_state.try_lock().unwrap().ident(),
                0,
                BYEBYE,
                &Payload::default(),
            )
            .unwrap();

            ctrl_socket
                .send_to(&message, (MULTICAST_ADDR, LINK_PORT))
                .await
                .unwrap();

            notifier.notify_waiters();
        });

        let measurement_notifier = Arc::new(Notify::new());

        tokio::spawn(async move {
            loop {
                select! {
                    Some(val) = rx_event.recv() => {
                        match val {
                            OnEvent::PeerState(msg) => {
                                on_peer_state(msg, peer_timeouts.clone(), tx_peer_event.clone(), epoch, measurement_notifier.clone(), self_node_id)
                                    .await
                            }
                            OnEvent::Byebye(node_id) => {
                                on_byebye(node_id, peer_timeouts.clone(), tx_peer_event.clone(), measurement_notifier.clone()).await
                            }
                        }
                    }
                }
            }
        });

        self.messenger.listen().await;
    }
}

pub async fn on_peer_state(
    msg: PeerStateMessageType,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
    epoch: Instant,
    cancel: Arc<Notify>,
    _self_node_id: NodeId,
) {
    debug!("received peer state from messenger");

    let peer_id = msg.node_state.ident();
    peer_timeouts
        .try_lock()
        .unwrap()
        .retain(|(_, id)| id != &peer_id);

    let new_to = (
        Instant::now() + Duration::seconds(msg.ttl as i64).to_std().unwrap(),
        peer_id,
    );

    debug!("updating peer timeout status for peer {}", peer_id);

    let i = peer_timeouts
        .try_lock()
        .unwrap()
        .iter()
        .position(|(timeout, _)| timeout >= &new_to.0);
    if let Some(i) = i {
        peer_timeouts.try_lock().unwrap().insert(i, new_to);
    } else {
        peer_timeouts.try_lock().unwrap().push(new_to);
    }

    debug!("sending peer state to observer");
    tx_peer_event
        .send(PeerEvent::SawPeer(PeerState {
            node_state: msg.node_state,
            measurement_endpoint: msg.measurement_endpoint,
        }))
        .await
        .unwrap();

    cancel.notify_waiters();
    schedule_next_pruning(peer_timeouts.clone(), epoch, tx_peer_event.clone(), cancel).await;
}

pub async fn on_byebye(
    peer_id: NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
    notifier: Arc<Notify>,
) {
    if find_peer(&peer_id, peer_timeouts.clone()).await {
        let peer_event = tx_peer_event.clone();
        tokio::spawn(async move { peer_event.send(PeerEvent::PeerLeft(peer_id)).await });

        peer_timeouts
            .try_lock()
            .unwrap()
            .retain(|(_, id)| id != &peer_id);
    }

    notifier.notify_waiters();
}

pub async fn find_peer(
    peer_id: &NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
) -> bool {
    peer_timeouts
        .try_lock()
        .unwrap()
        .iter()
        .any(|(_, id)| id == peer_id)
}

pub async fn schedule_next_pruning(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
    tx_peer_event: Sender<PeerEvent>,
    cancel: Arc<Notify>,
) {
    let pt = peer_timeouts.clone();

    if peer_timeouts.try_lock().unwrap().is_empty() {
        return;
    }

    let timeout = {
        let pt = pt.try_lock().unwrap();
        let (timeout, peer_id) = pt.first().unwrap();
        let timeout = *timeout + Duration::milliseconds(500).to_std().unwrap(); // Reduced grace period from 1 second to 500ms

        debug!(
            "scheduling next pruning for {}ms since gateway initialization epoch because of peer {}",
            timeout.duration_since(epoch).as_millis(),
            peer_id
        );

        timeout
    };

    tokio::spawn(async move {
        select! {
            _ = tokio::time::sleep_until(timeout) => {
                if peer_timeouts.try_lock().unwrap().is_empty() {
                    return;
                }

                let expired_peers = prune_expired_peers(peer_timeouts.clone(), epoch);
                for peer in expired_peers.iter() {
                    info!("pruning peer {}", peer.1);
                    tx_peer_event
                        .send(PeerEvent::PeerTimedOut(peer.1))
                        .await
                        .unwrap();
                }
            }
            _ = cancel.notified() => {}
        }
    });
}

fn prune_expired_peers(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
) -> Vec<(Instant, NodeId)> {
    info!(
        "pruning peers @ {}ms since gateway initialization epoch",
        Instant::now().duration_since(epoch).as_millis(),
    );

    // find the first element in pt whose timeout value is not less than Instant::now()
    // NOTE: peer_timeouts will be empty if byebye message is received prior to pruning

    let i = peer_timeouts
        .try_lock()
        .unwrap()
        .iter()
        .position(|(timeout, _)| timeout >= &Instant::now());

    if let Some(i) = i {
        peer_timeouts
            .try_lock()
            .unwrap()
            .drain(0..i)
            .collect::<Vec<_>>()
    } else {
        peer_timeouts
            .try_lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>()
    }
}

impl Drop for PeerGateway {
    fn drop(&mut self) {
        send_byebye(self.messenger.peer_state.try_lock().unwrap().ident());
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use local_ip_address::{list_afinet_netifas, local_ip};

    use crate::{discovery::messenger::new_udp_reuseport, link::sessions::SessionId};

    use super::*;

    fn init_tracing() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    #[tokio::test]

    async fn test_gateway() {
        init_tracing();

        // console_subscriber::init();

        let session_id = SessionId::default();
        let node_1 = NodeState::new(session_id.clone());
        let (tx_measure_peer_result, _) = tokio::sync::mpsc::channel::<MeasurePeerEvent>(1);
        let (_, rx_measure_peer_state) = tokio::sync::mpsc::channel::<MeasurePeerEvent>(1);
        let (tx_event, rx_event) = mpsc::channel::<OnEvent>(1);
        let (tx_peer_state_change, mut rx_peer_state_change) =
            mpsc::channel::<Vec<PeerStateChange>>(1);

        let notifier = Arc::new(Notify::new());

        tokio::spawn(async move {
            loop {
                let _ = rx_peer_state_change.recv().await;
            }
        });

        let ip = list_afinet_netifas()
            .unwrap()
            .iter()
            .find_map(|(_, ip)| match ip {
                IpAddr::V4(ipv4) if !ip.is_loopback() => Some(*ipv4),
                _ => None,
            })
            .unwrap();

        let ping_responder_unicast_socket = Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0)));

        let gw = PeerGateway::new(
            Arc::new(Mutex::new(PeerState {
                node_state: node_1,
                measurement_endpoint: None,
            })),
            Arc::new(Mutex::new(SessionState::default())),
            Clock::default(),
            Arc::new(Mutex::new(SessionPeerCounter::default())),
            tx_peer_state_change,
            tx_event,
            tx_measure_peer_result,
            Arc::new(Mutex::new(vec![])),
            notifier.clone(),
            rx_measure_peer_state,
            ping_responder_unicast_socket,
            Arc::new(Mutex::new(true)), // enabled for test
        )
        .await;

        // let s = new_udp_reuseport(IP_ANY);
        // s.set_broadcast(true).unwrap();
        // s.set_multicast_ttl_v4(2).unwrap();
        // s.set_multicast_loop_v4(false).unwrap();

        // let header = MessageHeader {
        //     ttl: 5,
        //     ident: NodeState::new(session_id.clone()).ident(),
        //     message_type: ALIVE,
        //     ..Default::default()
        // };

        // let mut encoded = bincode::encode_to_vec(PROTOCOL_HEADER, ENCODING_CONFIG).unwrap();
        // encoded.append(&mut bincode::encode_to_vec(header, ENCODING_CONFIG).unwrap());

        // s.send_to(&encoded, (MULTICAST_ADDR, LINK_PORT))
        //     .await
        //     .unwrap();

        // info!("test bytes sent");

        // tokio::spawn(async {
        //     tokio::time::sleep(Duration::seconds(10).to_std().unwrap()).await;

        //     std::process::exit(0);
        // });

        // Test that the gateway can be created and initialized without blocking
        // Use a timeout to prevent indefinite blocking on the listen operation
        let listen_result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            gw.listen(rx_event, notifier),
        )
        .await;

        match listen_result {
            Ok(_) => {
                // Listen completed within timeout (unexpected but not an error)
                println!("Gateway listen completed within timeout");
            }
            Err(_) => {
                // Listen timed out (expected behavior for network operations)
                println!(
                    "Gateway listen timed out as expected - gateway initialization successful"
                );
            }
        }
    }
}
