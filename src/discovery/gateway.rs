use std::{
    ops::DerefMut,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_recursion::async_recursion;
use tokio::{
    select,
    sync::mpsc::{self, Receiver, Sender},
    time::Instant,
};
use tracing::info;

use crate::{
    discovery::{
        messages::{encode_message, BYEBYE},
        payload::Payload,
        LINK_PORT, MULTICAST_ADDR,
    },
    link::{
        ghostxform::GhostXForm,
        measurement::MeasurementService,
        node::{NodeId, NodeState},
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
    tx_peer_event: Sender<PeerEvent>,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
}

#[derive(Clone)]
pub enum OnEvent {
    PeerState(PeerStateMessageType),
    Byebye(NodeId),
}

impl PeerGateway {
    pub async fn new(node_state: NodeState, ghost_xform: GhostXForm) -> Self {
        let (tx_event, rx_event) = mpsc::channel::<OnEvent>(1);
        let (tx_peer_event, rx_peer_event) = mpsc::channel::<PeerEvent>(1);

        PeerGateway {
            epoch: Instant::now(),
            observer: GatewayObserver::new(rx_peer_event).await,
            messenger: Messenger::new(
                PeerState {
                    node_state,
                    endpoint: None,
                },
                tx_event.clone(),
            )
            .await,
            measurement: MeasurementService::default(),
            node_state,
            ghost_xform: Arc::new(Mutex::new(ghost_xform)),
            rx_event: Some(rx_event),
            tx_peer_event,
            peer_timeouts: Arc::new(Mutex::new(vec![])),
        }
    }

    pub async fn update_node_state(&mut self, node_state: NodeState, ghost_xform: GhostXForm) {
        // TODO: measure update node state
        self.messenger
            .update_state(PeerState {
                node_state,
                endpoint: None,
            })
            .await
    }

    pub async fn measure_peer(&mut self, peer: &PeerStateMessageType) {
        todo!()
    }

    pub async fn listen(&mut self) {
        info!(
            "initializing peer gateway on interface {}",
            self.messenger
                .interface
                .as_ref()
                .unwrap()
                .local_addr()
                .unwrap()
        );

        let ctrl_socket = self.messenger.interface.as_ref().unwrap().clone();
        let node_state = self.node_state;

        self.messenger.listen().await;

        loop {
            select! {
                val = self.rx_event.as_mut().unwrap().recv() => {
                    match val.unwrap() {
                        OnEvent::PeerState(peer) => self.on_peer_state(peer.node_state, peer.ttl).await,
                        OnEvent::Byebye(node_id) => self.on_byebye(node_id).await,
                    }
                }
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

    pub async fn on_peer_state(&mut self, node_state: NodeState, ttl: u8) {
        {
            let peer_id = node_state.ident();
            let existing = self.find_peer(&peer_id).await;
            if existing {
                self.peer_timeouts
                    .lock()
                    .unwrap()
                    .retain(|(_, id)| id != &peer_id);
            }

            let new_to = (Instant::now() + Duration::from_secs(ttl as u64), peer_id);

            info!("updating peer timeout status");
            let mut peer_timeouts = self.peer_timeouts.lock().unwrap();
            let i = peer_timeouts
                .iter()
                .position(|(timeout, _)| timeout >= &new_to.0)
                .unwrap_or(peer_timeouts.len());
            peer_timeouts.insert(i, new_to);

            let peer_event = self.tx_peer_event.clone();
            tokio::spawn(async move { peer_event.send(PeerEvent::SawPeer(node_state)).await });
        }

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
    let (timeout, peer_id) = pt
        .first()
        .map(|(timeout, peer_id)| {
            (
                timeout.duration_since(epoch) + Duration::from_secs(1),
                peer_id,
            )
        })
        .unwrap();

    info!(
        "scheduling next pruning for {}ms because of peer {}",
        timeout.as_millis(),
        peer_id
    );

    let tx_peer_event = tx_peer_event.clone();
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
    info!(
        "pruning peers @ {}ms since gateway initialization epoch",
        Instant::now().duration_since(epoch).as_millis(),
    );

    let mut peer_timeouts = peer_timeouts.lock().unwrap();

    // find the first element in pt whose timeout value is not less than Instant::now()
    let i = peer_timeouts
        .iter()
        .position(|(timeout, _)| timeout >= &Instant::now())
        .unwrap_or(peer_timeouts.len());

    peer_timeouts.drain(0..i).collect::<Vec<_>>()
}

impl Drop for PeerGateway {
    fn drop(&mut self) {
        send_byebye(self.messenger.peer_state.lock().unwrap().ident());
    }
}

#[cfg(test)]
mod tests {
    use crate::discovery::{
        messages::{MessageHeader, ALIVE, PROTOCOL_HEADER},
        messenger::new_udp_reuseport,
        ENCODING_CONFIG, IP_ANY, LINK_PORT, MULTICAST_ADDR,
    };

    use super::*;

    fn init_tracing() {
        let subscriber = tracing_subscriber::FmtSubscriber::new();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    }

    #[tokio::test]

    async fn test_gateway() {
        init_tracing();

        let node_1 = NodeState::default();
        let mut gw = PeerGateway::new(node_1, GhostXForm::default()).await;

        let s = new_udp_reuseport(IP_ANY);
        s.set_broadcast(true).unwrap();
        s.set_multicast_ttl_v4(2).unwrap();
        s.set_multicast_loop_v4(false).unwrap();

        let header = MessageHeader {
            ttl: 10,
            ident: NodeState::default().ident(),
            message_type: ALIVE,
            ..Default::default()
        };

        let mut encoded = bincode::encode_to_vec(PROTOCOL_HEADER, ENCODING_CONFIG).unwrap();
        encoded.append(&mut bincode::encode_to_vec(header, ENCODING_CONFIG).unwrap());

        s.send_to(&encoded, (MULTICAST_ADDR, LINK_PORT))
            .await
            .unwrap();

        info!("test bytes sent");

        gw.listen().await;
    }
}
