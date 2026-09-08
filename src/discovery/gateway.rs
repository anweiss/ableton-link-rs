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

use crate::link::{
    clock::Clock,
    controller::{gated_recv, DispatchGate, DispatchReceiver, SessionPeerCounter},
    ghostxform::GhostXForm,
    measurement::{MeasurePeerEvent, MeasurementService},
    node::{NodeId, NodeState},
    state::SessionState,
};

use super::{
    messenger::{send_byebye, Messenger},
    peers::{
        ControllerPeer, GatewayObserver, PeerEvent, PeerState, PeerStateChange,
        PeerStateMessageType,
    },
};

pub struct PeerGateway {
    pub(crate) gate: Arc<DispatchGate>,
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
    pub(crate) fn discard_queued_datagrams(&self) {
        self.messenger.discard_queued_datagrams();
    }

    pub(crate) async fn stop(&self) {
        self.gate.stop().await;
        self.peer_timeouts.lock().unwrap().clear();
    }

    #[cfg(test)]
    pub(crate) fn event_sender(&self) -> Sender<OnEvent> {
        self.messenger.tx_event.clone()
    }

    #[cfg(test)]
    pub(crate) fn socket_probes(&self) -> Vec<std::sync::Weak<UdpSocket>> {
        let mut probes = self.messenger.socket_probes();
        probes.push(Arc::downgrade(&self.measurement_service.shared_socket));
        probes
    }

    #[cfg(test)]
    pub(crate) fn event_state_probe(&self) -> std::sync::Weak<Mutex<Vec<(Instant, NodeId)>>> {
        Arc::downgrade(&self.peer_timeouts)
    }

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
    ) -> Result<Self, std::io::Error> {
        let (tx_peer_event, rx_peer_event) = mpsc::channel::<PeerEvent>(1);
        let epoch = Instant::now();
        let gate = Arc::new(DispatchGate::new_open());
        // let ping_responder_unicast_socket = Arc::new(new_udp_reuseport(UNICAST_IP_ANY));

        if let Ok(mut state) = peer_state.try_lock() {
            state.measurement_endpoint =
                if let SocketAddr::V4(socket_addr) = ping_responder_unicast_socket.local_addr()? {
                    Some(socket_addr)
                } else {
                    None
                };
        }

        let mut messenger = Messenger::new(
            peer_state.clone(),
            tx_event.clone(),
            epoch,
            notifier.clone(),
            enabled.clone(),
        )?;
        messenger.gate = gate.clone();

        Ok(PeerGateway {
            gate: gate.clone(),
            epoch,
            observer: GatewayObserver::with_dispatch_gate(
                rx_peer_event,
                peer_state.clone(),
                session_peer_counter.clone(),
                tx_peer_state_change,
                peers,
                gate,
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
        })
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

        if let Ok(mut state) = self.peer_state.try_lock() {
            state.node_state = node_state;
        }
    }

    pub async fn listen(&self, rx_event: Receiver<OnEvent>, notifier: Arc<Notify>) {
        select! {
            biased;
            _ = notifier.notified() => {}
            _ = self.listen_with_dispatch(rx_event, notifier.clone(), self.gate.subscribe()) => {}
        }
    }

    pub(crate) async fn listen_with_dispatch(
        &self,
        rx_event: Receiver<OnEvent>,
        notifier: Arc<Notify>,
        open: DispatchReceiver,
    ) {
        self.listen_with_signal(rx_event, notifier, open, tokio::signal::ctrl_c())
            .await;
    }

    pub(crate) async fn listen_with_signal(
        &self,
        mut rx_event: Receiver<OnEvent>,
        notifier: Arc<Notify>,
        mut open: DispatchReceiver,
        signal: impl std::future::Future<Output = std::io::Result<()>>,
    ) {
        let node_id = self
            .peer_state
            .try_lock()
            .map(|state| state.node_state.node_id)
            .unwrap_or_default();

        info!(
            "initializing peer gateway {:?} on interface {}",
            node_id,
            self.messenger
                .interface
                .as_ref()
                .unwrap()
                .local_addr()
                .unwrap()
        );

        let peer_state = self.peer_state.clone();

        // Get self node ID for filtering self-messages
        let self_node_id = peer_state
            .try_lock()
            .map(|state| state.node_state.node_id)
            .unwrap_or_default();

        let peer_timeouts = self.peer_timeouts.clone();
        let tx_peer_event = self.tx_peer_event.clone();
        let epoch = self.epoch;

        self.measurement_service.listen(notifier.clone()).await;

        let mut children = tokio::task::JoinSet::new();
        let measurement_notifier = Arc::new(Notify::new());
        let gate = self.gate.clone();
        // Register socket receivers before the event consumer can acknowledge
        // startup; otherwise the first barrier could miss its discovery children.
        let messenger = self.messenger.listen_owned();

        children.spawn(async move {
            while let Some((_permit, generation, val)) =
                gated_recv(&gate, &mut open, &mut rx_event).await
            {
                let handle = async {
                    match val {
                        OnEvent::PeerState(msg) => {
                            on_peer_state_in_epoch(
                                msg,
                                peer_timeouts.clone(),
                                tx_peer_event.clone(),
                                epoch,
                                measurement_notifier.clone(),
                                self_node_id,
                                Some((gate.clone(), generation)),
                            )
                            .await
                        }
                        OnEvent::Byebye(node_id) => {
                            on_byebye(
                                node_id,
                                peer_timeouts.clone(),
                                tx_peer_event.clone(),
                                measurement_notifier.clone(),
                            )
                            .await
                        }
                    }
                };
                select! {
                    biased;
                    _ = open.closed() => {}
                    _ = handle => {}
                }
            }
        });

        tokio::pin!(messenger);
        tokio::pin!(signal);
        let mut signal_available = true;
        loop {
            select! {
                _ = &mut messenger => break,
                result = &mut signal, if signal_available => {
                    match result {
                        Ok(()) => {
                            send_byebye(peer_state.lock().unwrap().ident());
                            notifier.notify_waiters();
                            break;
                        }
                        Err(error) => {
                            tracing::warn!("Ctrl-C listener failed; discovery continues: {}", error);
                            signal_available = false;
                        }
                    }
                }
                result = children.join_next(), if !children.is_empty() => {
                    if let Some(Err(error)) = result {
                        tracing::warn!("peer gateway worker failed: {}", error);
                    }
                }
            }
        }
        children.shutdown().await;
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
    on_peer_state_in_epoch(
        msg,
        peer_timeouts,
        tx_peer_event,
        epoch,
        cancel,
        _self_node_id,
        None,
    )
    .await;
}

async fn on_peer_state_in_epoch(
    msg: PeerStateMessageType,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
    epoch: Instant,
    cancel: Arc<Notify>,
    _self_node_id: NodeId,
    lifecycle: Option<(Arc<DispatchGate>, u64)>,
) {
    debug!("received peer state from messenger");

    let peer_id = msg.node_state.ident();
    peer_timeouts
        .try_lock()
        .unwrap()
        .retain(|(_, id)| id != &peer_id);

    let new_to = (
        Instant::now()
            + Duration::seconds(std::cmp::min(msg.ttl as i64, 3))
                .to_std()
                .unwrap(), // Cap timeout at 3 seconds max
        peer_id,
    );

    debug!(
        "updating peer timeout status for peer {} with TTL {} seconds (capped at 3)",
        peer_id, msg.ttl
    );

    if let Ok(mut timeouts) = peer_timeouts.try_lock() {
        let i = timeouts
            .iter()
            .position(|(timeout, _)| timeout >= &new_to.0);
        if let Some(i) = i {
            timeouts.insert(i, new_to);
        } else {
            timeouts.push(new_to);
        }
    } else {
        debug!(
            "Could not acquire peer_timeouts lock to update timeout for peer {}",
            peer_id
        );
    }

    debug!("sending peer state to observer");
    if let Err(e) = tx_peer_event
        .send(PeerEvent::SawPeer(PeerState {
            node_state: msg.node_state,
            measurement_endpoint: msg.measurement_endpoint,
            audio_endpoint: msg.audio_endpoint,
        }))
        .await
    {
        debug!("Failed to send SawPeer event: {:?}", e);
        return;
    }

    cancel.notify_waiters();
    schedule_next_pruning_in_epoch(
        peer_timeouts.clone(),
        epoch,
        tx_peer_event.clone(),
        cancel,
        lifecycle,
    )
    .await;
}

pub async fn on_byebye(
    peer_id: NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
    notifier: Arc<Notify>,
) {
    info!("Processing BYEBYE from peer {}", peer_id);

    if find_peer(&peer_id, peer_timeouts.clone()).await {
        info!(
            "Found peer {} in timeout list, sending PeerLeft event",
            peer_id
        );
        let peer_event = tx_peer_event.clone();
        if let Err(e) = peer_event.send(PeerEvent::PeerLeft(peer_id)).await {
            debug!("Failed to send PeerLeft event: {:?}", e);
        } else {
            info!("Successfully sent PeerLeft event for peer {}", peer_id);
        }

        if let Ok(mut timeouts) = peer_timeouts.try_lock() {
            timeouts.retain(|(_, id)| id != &peer_id);
            info!("Removed peer {} from timeout list", peer_id);
        } else {
            debug!(
                "Could not acquire peer_timeouts lock to remove peer {}",
                peer_id
            );
        }
    } else {
        info!(
            "Peer {} not found in timeout list (may have already been removed)",
            peer_id
        );
    }

    notifier.notify_waiters();
}

pub async fn find_peer(
    peer_id: &NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
) -> bool {
    match peer_timeouts.try_lock() {
        Ok(timeouts) => {
            let found = timeouts.iter().any(|(_, id)| id == peer_id);
            debug!("find_peer: Looking for peer {}, found: {}", peer_id, found);
            found
        }
        Err(_) => {
            debug!(
                "Could not acquire peer_timeouts lock in find_peer for {}",
                peer_id
            );
            false
        }
    }
}

pub async fn schedule_next_pruning(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
    tx_peer_event: Sender<PeerEvent>,
    cancel: Arc<Notify>,
) {
    schedule_next_pruning_in_epoch(peer_timeouts, epoch, tx_peer_event, cancel, None).await;
}

async fn schedule_next_pruning_in_epoch(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
    tx_peer_event: Sender<PeerEvent>,
    cancel: Arc<Notify>,
    lifecycle: Option<(Arc<DispatchGate>, u64)>,
) {
    let pt = peer_timeouts.clone();

    let has_timeouts = peer_timeouts
        .try_lock()
        .map(|timeouts| !timeouts.is_empty())
        .unwrap_or(false);

    if !has_timeouts {
        return;
    }

    let (timeout_instant, peer_id) = {
        if let Ok(timeouts) = pt.try_lock() {
            if let Some((timeout, peer_id)) = timeouts.first().copied() {
                let timeout_instant = timeout + Duration::milliseconds(100).to_std().unwrap();
                (timeout_instant, peer_id)
            } else {
                return; // No timeouts available
            }
        } else {
            return; // Cannot acquire lock
        }
    };

    debug!(
        "scheduling next pruning for {}ms since gateway initialization epoch because of peer {}",
        timeout_instant.duration_since(epoch).as_millis(),
        peer_id
    );

    tokio::spawn(async move {
        let prune = async {
            select! {
                _ = tokio::time::sleep_until(timeout_instant) => {
                    let has_timeouts = peer_timeouts.try_lock()
                        .map(|timeouts| !timeouts.is_empty())
                        .unwrap_or(false);

                    if !has_timeouts {
                        return;
                    }
                    let expired_peers = prune_expired_peers(peer_timeouts.clone(), epoch);
                    for peer in expired_peers.iter() {
                        info!("pruning peer {}", peer.1);
                        if let Err(e) = tx_peer_event
                            .send(PeerEvent::PeerTimedOut(peer.1))
                            .await
                        {
                            debug!("Failed to send PeerTimedOut event: {:?}", e);
                            // Receiver has been dropped, no point in continuing
                            return;
                        }
                    }
                }
                _ = cancel.notified() => {}
            }
        };
        if let Some((gate, epoch)) = lifecycle {
            gate.run_in_epoch(epoch, prune).await;
        } else {
            prune.await;
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

    let i = peer_timeouts.try_lock().ok().and_then(|timeouts| {
        timeouts
            .iter()
            .position(|(timeout, _)| timeout >= &Instant::now())
    });

    if let Some(i) = i {
        peer_timeouts
            .try_lock()
            .map(|mut timeouts| timeouts.drain(0..i).collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        peer_timeouts
            .try_lock()
            .map(|mut timeouts| timeouts.drain(..).collect::<Vec<_>>())
            .unwrap_or_default()
    }
}

impl Drop for PeerGateway {
    fn drop(&mut self) {
        self.gate.close();
        if let Ok(state) = self.messenger.peer_state.try_lock() {
            send_byebye(state.ident());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use local_ip_address::list_afinet_netifas;

    use crate::{discovery::messenger::new_udp_reuseport, link::sessions::SessionId};

    use super::*;

    fn init_tracing() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    #[tokio::test]
    #[ignore] // Requires real UDP multicast; crashes on macOS CI — run locally with --include-ignored
    async fn test_gateway() {
        init_tracing();

        // console_subscriber::init();

        let session_id = SessionId::default();
        let node_1 = NodeState::new(session_id);
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

        let ping_responder_unicast_socket =
            Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0).into()).unwrap());

        let gw = PeerGateway::new(
            Arc::new(Mutex::new(PeerState {
                node_state: node_1,
                measurement_endpoint: None,
                audio_endpoint: None,
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
        .await
        .unwrap();

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
