use std::sync::{Arc, Mutex};

use async_recursion::async_recursion;
use chrono::Duration;
use tokio::{
    select,
    sync::mpsc::{self, Receiver, Sender},
    time::Instant,
};
use tracing::{debug, info};

use crate::{
    discovery::{
        messages::{encode_message, BYEBYE},
        messenger::new_udp_reuseport,
        IP_ANY, LINK_PORT, MULTICAST_ADDR,
    },
    link::{
        clock::Clock,
        controller::SessionPeerCounter,
        ghostxform::GhostXForm,
        measurement::MeasurementService,
        node::{self, NodeId, NodeState},
        payload::Payload,
    },
};

use super::{
    messenger::{send_byebye, Messenger},
    peers::{GatewayObserver, PeerEvent, PeerState, PeerStateMessageType},
};

pub struct PeerGateway {
    epoch: Instant,
    observer: GatewayObserver,
    messenger: Messenger,
    measurement: MeasurementService,
    node_state: NodeState,
    ghost_xform: Arc<Mutex<GhostXForm>>,
    rx_event: Option<Receiver<OnEvent>>,
    rx_measure_peer: Option<Receiver<PeerState>>,
    tx_peer_event: Sender<PeerEvent>,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
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
        rx_measure_peer: Receiver<PeerState>,
        clock: Clock,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    ) -> Self {
        let (tx_event, rx_event) = mpsc::channel::<OnEvent>(1);
        let (tx_peer_event, rx_peer_event) = mpsc::channel::<PeerEvent>(1);
        let epoch = Instant::now();
        let messenger = Messenger::new(
            PeerState {
                node_state: node_state.clone(),
                endpoint: None,
            },
            tx_event.clone(),
            epoch,
        )
        .await;

        info!(
            "initializing peer gateway on interface {}",
            messenger.interface.as_ref().unwrap().local_addr().unwrap()
        );

        let unicast_socket = Arc::new(new_udp_reuseport(IP_ANY));

        PeerGateway {
            epoch,
            observer: GatewayObserver::new(rx_peer_event).await,
            messenger,
            measurement: MeasurementService::new(
                unicast_socket,
                node_state.session_id.clone(),
                ghost_xform,
                clock,
            )
            .await,
            node_state,
            ghost_xform: Arc::new(Mutex::new(ghost_xform)),
            rx_event: Some(rx_event),
            rx_measure_peer: Some(rx_measure_peer),
            tx_peer_event,
            peer_timeouts: Arc::new(Mutex::new(vec![])),
            session_peer_counter,
        }
    }

    pub async fn update_node_state(&mut self, node_state: NodeState, _ghost_xform: GhostXForm) {
        self.measurement
            .update_node_state(node_state.session_id.clone(), _ghost_xform)
            .await;
        self.messenger
            .update_state(PeerState {
                node_state,
                endpoint: None,
            })
            .await
    }

    pub async fn measure_peer(&mut self, _peer: &PeerStateMessageType) {
        todo!()
    }

    pub async fn listen(&mut self) {
        let ctrl_socket = self.messenger.interface.as_ref().unwrap().clone();
        let node_state = self.node_state.clone();

        self.messenger.listen().await;

        loop {
            select! {
                val = self.rx_event.as_mut().unwrap().recv() => {
                    match val.unwrap() {
                        OnEvent::PeerState(msg) => self.on_peer_state(msg).await,
                        OnEvent::Byebye(node_id) => self.on_byebye(node_id).await,
                    }
                }
                // peer = self.rx_measure_peer.as_mut().unwrap().recv() => {
                //     todo!()
                // }
                // byebye upon program termination
                _ = tokio::signal::ctrl_c() => {
                    ctrl_socket.set_broadcast(true).unwrap();
                    ctrl_socket.set_multicast_ttl_v4(2).unwrap();

                    let message =
                        encode_message(node_state.ident(), 0, BYEBYE, &Payload::default()).unwrap();

                    ctrl_socket
                        .send_to(&message, (MULTICAST_ADDR, LINK_PORT))
                        .await.unwrap();

                    break;
                }
            }
        }
    }

    pub async fn on_peer_state(&mut self, msg: PeerStateMessageType) {
        let peer_id = msg.node_state.ident();
        self.peer_timeouts
            .lock()
            .unwrap()
            .retain(|(_, id)| id != &peer_id);

        let new_to = (
            Instant::now() + Duration::seconds(msg.ttl as i64).to_std().unwrap(),
            peer_id,
        );

        debug!("updating peer timeout status for peer {}", peer_id);

        let i = self
            .peer_timeouts
            .lock()
            .unwrap()
            .iter()
            .position(|(timeout, _)| timeout >= &new_to.0);
        if let Some(i) = i {
            self.peer_timeouts.lock().unwrap().insert(i, new_to);
        } else {
            self.peer_timeouts.lock().unwrap().push(new_to);
        }

        let peer_event = self.tx_peer_event.clone();
        tokio::spawn(async move { peer_event.send(PeerEvent::SawPeer(msg.node_state)).await });

        schedule_next_pruning(
            self.peer_timeouts.clone(),
            self.epoch,
            self.tx_peer_event.clone(),
        )
        .await
    }

    pub async fn on_byebye(&self, peer_id: NodeId) {
        if self.find_peer(&peer_id).await {
            let peer_event = self.tx_peer_event.clone();
            tokio::spawn(async move { peer_event.send(PeerEvent::PeerLeft(peer_id)).await });

            self.peer_timeouts
                .lock()
                .unwrap()
                .retain(|(_, id)| id != &peer_id);
        }
    }

    pub async fn find_peer(&self, peer_id: &NodeId) -> bool {
        self.peer_timeouts
            .lock()
            .unwrap()
            .iter()
            .any(|(_, id)| id == peer_id)
    }
}

#[async_recursion]
pub async fn schedule_next_pruning(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
    tx_peer_event: Sender<PeerEvent>,
) {
    if peer_timeouts.lock().unwrap().is_empty() {
        return;
    }

    let pt = peer_timeouts.lock().unwrap();
    let (timeout, peer_id) = pt.first().unwrap();
    let timeout = timeout.duration_since(epoch) + Duration::seconds(1).to_std().unwrap();

    debug!(
        "scheduling next pruning for {}ms because of peer {}",
        timeout.as_millis(),
        peer_id
    );

    let peer_timeouts = peer_timeouts.clone();

    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;

        let expired_peers = prune_expired_peers(peer_timeouts.clone(), epoch);
        for peer in expired_peers.iter() {
            info!("pruning peer {}", peer.1);
            tx_peer_event
                .send(PeerEvent::PeerTimedOut(peer.1))
                .await
                .unwrap();
        }

        schedule_next_pruning(peer_timeouts, epoch, tx_peer_event).await;
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
            ENCODING_CONFIG, IP_ANY, LINK_PORT, MULTICAST_ADDR,
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

        let session_id = Arc::new(Mutex::new(SessionId::default()));
        let node_1 = NodeState::new(session_id.clone());
        let (_, rx) = mpsc::channel::<PeerState>(1);
        let mut gw = PeerGateway::new(
            node_1,
            GhostXForm::default(),
            rx,
            Clock::new(),
            Arc::new(Mutex::new(SessionPeerCounter::default())),
        )
        .await;

        let s = new_udp_reuseport(IP_ANY);
        s.set_broadcast(true).unwrap();
        s.set_multicast_ttl_v4(2).unwrap();
        s.set_multicast_loop_v4(false).unwrap();

        let header = MessageHeader {
            ttl: 5,
            ident: NodeState::new(session_id.clone()).ident(),
            message_type: ALIVE,
            ..Default::default()
        };

        let mut encoded = bincode::encode_to_vec(PROTOCOL_HEADER, ENCODING_CONFIG).unwrap();
        encoded.append(&mut bincode::encode_to_vec(header, ENCODING_CONFIG).unwrap());

        s.send_to(&encoded, (MULTICAST_ADDR, LINK_PORT))
            .await
            .unwrap();

        info!("test bytes sent");

        tokio::spawn(async {
            tokio::time::sleep(Duration::seconds(10).to_std().unwrap()).await;

            std::process::exit(0);
        });

        gw.listen().await;
    }
}
