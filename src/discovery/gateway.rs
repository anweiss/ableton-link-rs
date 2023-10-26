use std::sync::{Arc, Mutex};

use chrono::Duration;
use tokio::{
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
        messenger::new_udp_reuseport,
        LINK_PORT, MULTICAST_ADDR, UNICAST_IP_ANY,
    },
    link::{
        clock::Clock,
        controller::SessionPeerCounter,
        ghostxform::GhostXForm,
        measurement::{MeasurePeerEvent, MeasurementService},
        node::{NodeId, NodeState},
        payload::Payload,
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
    epoch: Instant,
    messenger: Messenger,
    measurement: MeasurementService,
    node_state: NodeState,
    ghost_xform: Arc<Mutex<GhostXForm>>,
    tx_peer_event: Sender<PeerEvent>,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    tx_measure_peer: tokio::sync::broadcast::Sender<MeasurePeerEvent>,
}

#[derive(Clone)]
pub enum OnEvent {
    PeerState(PeerStateMessageType),
    Byebye(NodeId),
}

impl PeerGateway {
    pub async fn new(
        node_state: NodeState,
        ghost_xform: GhostXForm,
        clock: Clock,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        tx_event: Sender<OnEvent>,
        tx_measure_peer: tokio::sync::broadcast::Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        notifier: Arc<Notify>,
    ) -> Self {
        let (tx_peer_event, rx_peer_event) = mpsc::channel::<PeerEvent>(1);
        let epoch = Instant::now();
        let messenger = Messenger::new(
            PeerState {
                node_state: node_state.clone(),
                measurement_endpoint: None,
            },
            tx_event.clone(),
            epoch,
            notifier.clone(),
        )
        .await;

        info!(
            "initializing peer gateway on interface {}",
            messenger.interface.as_ref().unwrap().local_addr().unwrap()
        );

        let ping_responder_unicast_socket = Arc::new(new_udp_reuseport(UNICAST_IP_ANY));

        PeerGateway {
            epoch,
            observer: GatewayObserver::new(
                rx_peer_event,
                node_state.session_id,
                session_peer_counter.clone(),
                tx_peer_state_change,
                peers,
                notifier.clone(),
            )
            .await,
            messenger,
            measurement: MeasurementService::new(
                ping_responder_unicast_socket,
                node_state.session_id,
                ghost_xform,
                clock,
                tx_measure_peer.clone(),
                notifier,
            )
            .await,
            node_state,
            ghost_xform: Arc::new(Mutex::new(ghost_xform)),
            tx_peer_event,
            peer_timeouts: Arc::new(Mutex::new(vec![])),
            session_peer_counter,
            tx_measure_peer,
        }
    }

    pub async fn update_node_state(&self, node_state: NodeState, _ghost_xform: GhostXForm) {
        self.measurement
            .update_node_state(node_state.session_id, _ghost_xform)
            .await;
        self.messenger
            .update_state(PeerState {
                node_state,
                measurement_endpoint: None,
            })
            .await
    }

    pub async fn listen(&self, mut rx_event: Receiver<OnEvent>, notifier: Arc<Notify>) {
        let ctrl_socket = self.messenger.interface.as_ref().unwrap().clone();
        let node_state = self.node_state.clone();

        let peer_timeouts = self.peer_timeouts.clone();
        let tx_peer_event = self.tx_peer_event.clone();
        let epoch = self.epoch;

        let n = notifier.clone();

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.unwrap();
            ctrl_socket.set_broadcast(true).unwrap();
            ctrl_socket.set_multicast_ttl_v4(2).unwrap();

            let message =
                encode_message(node_state.ident(), 0, BYEBYE, &Payload::default()).unwrap();

            ctrl_socket
                .send_to(&message, (MULTICAST_ADDR, LINK_PORT))
                .await
                .unwrap();

            n.notify_waiters();
        });

        tokio::spawn(async move {
            loop {
                select! {
                    Some(val) = rx_event.recv() => {
                        match val {
                            OnEvent::PeerState(msg) => {
                                on_peer_state(msg, peer_timeouts.clone(), tx_peer_event.clone(), epoch)
                                    .await
                            }
                            OnEvent::Byebye(node_id) => {
                                on_byebye(node_id, peer_timeouts.clone(), tx_peer_event.clone()).await
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
) {
    info!("received peer state from messenger");

    let peer_id = msg.node_state.ident();
    peer_timeouts
        .lock()
        .unwrap()
        .retain(|(_, id)| id != &peer_id);

    let new_to = (
        Instant::now() + Duration::seconds(msg.ttl as i64).to_std().unwrap(),
        peer_id,
    );

    debug!("updating peer timeout status for peer {}", peer_id);

    let i = peer_timeouts
        .lock()
        .unwrap()
        .iter()
        .position(|(timeout, _)| timeout >= &new_to.0);
    if let Some(i) = i {
        peer_timeouts.lock().unwrap().insert(i, new_to);
    } else {
        peer_timeouts.lock().unwrap().push(new_to);
    }

    info!("sending peer state to observer");
    tx_peer_event
        .send(PeerEvent::SawPeer(PeerState {
            node_state: msg.node_state,
            measurement_endpoint: msg.measurement_endpoint,
        }))
        .await
        .unwrap();

    schedule_next_pruning(peer_timeouts.clone(), epoch, tx_peer_event.clone()).await
}

pub async fn on_byebye(
    peer_id: NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
) {
    if find_peer(&peer_id, peer_timeouts.clone()).await {
        let peer_event = tx_peer_event.clone();
        tokio::spawn(async move { peer_event.send(PeerEvent::PeerLeft(peer_id)).await });

        peer_timeouts
            .lock()
            .unwrap()
            .retain(|(_, id)| id != &peer_id);
    }
}

pub async fn find_peer(
    peer_id: &NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
) -> bool {
    peer_timeouts
        .lock()
        .unwrap()
        .iter()
        .any(|(_, id)| id == peer_id)
}

pub async fn schedule_next_pruning(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
    tx_peer_event: Sender<PeerEvent>,
) {
    let pt = peer_timeouts.clone();

    tokio::spawn(async move {
        loop {
            if peer_timeouts.lock().unwrap().is_empty() {
                return;
            }

            let timeout = {
                let pt = pt.lock().unwrap();
                let (timeout, peer_id) = pt.first().unwrap();
                let timeout =
                    timeout.duration_since(epoch) + Duration::seconds(1).to_std().unwrap();

                debug!(
                    "scheduling next pruning for {}ms because of peer {}",
                    timeout.as_millis(),
                    peer_id
                );

                timeout
            };

            tokio::time::sleep(timeout).await;

            let expired_peers = prune_expired_peers(peer_timeouts.clone(), epoch);
            for peer in expired_peers.iter() {
                info!("pruning peer {}", peer.1);
                tx_peer_event
                    .send(PeerEvent::PeerTimedOut(peer.1))
                    .await
                    .unwrap();
            }
        }
    });
}

fn prune_expired_peers(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
) -> Vec<(Instant, NodeId)> {
    debug!(
        "pruning peers @ {}ms since gateway initialization epoch",
        Instant::now().duration_since(epoch).as_millis(),
    );

    // find the first element in pt whose timeout value is not less than Instant::now()

    let i = peer_timeouts
        .lock()
        .unwrap()
        .iter()
        .position(|(timeout, _)| timeout >= &Instant::now());

    if let Some(i) = i {
        peer_timeouts
            .lock()
            .unwrap()
            .drain(0..i)
            .collect::<Vec<_>>()
    } else {
        peer_timeouts.lock().unwrap().drain(..).collect::<Vec<_>>()
    }
}

impl Drop for PeerGateway {
    fn drop(&mut self) {
        send_byebye(self.messenger.peer_state.lock().unwrap().ident());
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        discovery::{
            messages::{MessageHeader, ALIVE, PROTOCOL_HEADER},
            messenger::new_udp_reuseport,
            ENCODING_CONFIG, LINK_PORT, MULTICAST_ADDR, MULTICAST_IP_ANY,
        },
        link::sessions::SessionId,
    };

    use super::*;

    fn init_tracing() {
        let subscriber = tracing_subscriber::FmtSubscriber::new();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    }

    #[tokio::test]

    async fn test_gateway() {
        init_tracing();

        // console_subscriber::init();

        let session_id = SessionId::default();
        let node_1 = NodeState::new(session_id.clone());
        let (tx_measure_peer, _) = tokio::sync::broadcast::channel::<MeasurePeerEvent>(1);
        let (tx_event, rx_event) = mpsc::channel::<OnEvent>(1);
        let (tx_peer_state_change, mut rx_peer_state_change) =
            mpsc::channel::<Vec<PeerStateChange>>(1);

        let notifier = Arc::new(Notify::new());

        tokio::spawn(async move {
            loop {
                let _ = rx_peer_state_change.recv().await;
            }
        });

        let gw = PeerGateway::new(
            node_1,
            GhostXForm::default(),
            Clock::new(),
            Arc::new(Mutex::new(SessionPeerCounter::default())),
            tx_peer_state_change,
            tx_event,
            tx_measure_peer,
            Arc::new(Mutex::new(vec![])),
            notifier.clone(),
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

        gw.listen(rx_event, notifier).await;
    }
}
