use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use tokio::{
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
    time::{Instant, Interval},
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
    peers::{GatewayObserver, PeerEvent, PeerState},
};

pub struct PeerGateway {
    epoch: Instant,
    observer: GatewayObserver,
    messenger: Messenger,
    measurement: MeasurementService,
    node_state: Arc<std::sync::Mutex<NodeState>>,
    ghost_xform: Arc<Mutex<GhostXForm>>,
    on_event: Option<Receiver<OnEvent>>,
    peer_event: Sender<PeerEvent>,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    prune_receiver: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[derive(Clone, Copy)]
pub enum OnEvent {
    PeerState(PeerState),
    Byebye(NodeId),
}

impl PeerGateway {
    pub async fn new(node_state: NodeState, ghost_xform: GhostXForm) -> Self {
        let node_state = Arc::new(std::sync::Mutex::new(node_state));

        let (send_event, on_event) = mpsc::channel::<OnEvent>(1);
        let (peer_event, on_peer_event) = mpsc::channel::<PeerEvent>(1);

        PeerGateway {
            epoch: Instant::now(),
            observer: GatewayObserver::new(on_peer_event).await,
            messenger: Messenger::new(node_state.clone(), send_event.clone()).await,
            measurement: MeasurementService::default(),
            node_state,
            ghost_xform: Arc::new(Mutex::new(ghost_xform)),
            on_event: Some(on_event),
            peer_event,
            peer_timeouts: Arc::new(Mutex::new(vec![])),
            prune_receiver: None,
        }
    }

    pub async fn update_node_state(&mut self, node_state: NodeState, ghost_x_form: GhostXForm) {
        todo!()
    }

    pub async fn measure_peer(&mut self, peer: &PeerState) {
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
        let node_state = self.node_state.clone();

        self.messenger.listen().await;

        loop {
            select! {
                val = self.on_event.as_mut().unwrap().recv() => {
                    match val.unwrap() {
                        OnEvent::PeerState(peer) => self.on_peer_state(peer.node_state, peer.ttl).await,
                        OnEvent::Byebye(node_id) => self.on_byebye(&node_id).await,
                    }
                }
                // byebye upon program termination
                _ = tokio::signal::ctrl_c() => {
                    ctrl_socket.set_broadcast(true).unwrap();
                    ctrl_socket.set_multicast_ttl_v4(2).unwrap();

                    let message =
                        encode_message(node_state.lock().unwrap().ident(), 0, BYEBYE, &Payload::default()).unwrap();

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
                    .await
                    .retain(|(_, id)| id != &peer_id);
            }

            let new_to = (Instant::now() + Duration::from_secs(ttl as u64), peer_id);

            info!("updating peer timeout status");
            let mut peer_timeouts = self.peer_timeouts.lock().await;
            let i = peer_timeouts
                .iter()
                .position(|(timeout, _)| timeout >= &new_to.0)
                .unwrap_or(peer_timeouts.len());
            peer_timeouts.insert(i, new_to);

            let peer_event = self.peer_event.clone();
        }

        // tokio::spawn(async move { peer_event.send(PeerEvent::SawPeer(node_state)).await });
        self.schedule_next_pruning().await;
    }

    pub async fn on_byebye(&self, peer_id: &NodeId) {
        todo!()
    }

    pub async fn prune_expired_peers(&self) {
        let _ = &self.prune_receiver.as_ref().unwrap();
        todo!();
    }

    pub async fn schedule_next_pruning(&mut self) {
        let peer_timeouts = self.peer_timeouts.lock().await;

        let (timeout, peer_id) = peer_timeouts
            .first()
            .map(|(timeout, peer_id)| {
                (
                    timeout.duration_since(self.epoch) + Duration::from_secs(1),
                    peer_id,
                )
            })
            .unwrap();

        info!(
            "scheduling next pruning for {}ms because of peer {}",
            timeout.as_millis(),
            peer_id
        );

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        self.prune_receiver = Some(rx);

        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            tx.send(())
        });

        self.prune_expired_peers().await;
    }

    pub async fn find_peer(&self, peer_id: &NodeId) -> bool {
        self.peer_timeouts
            .lock()
            .await
            .iter()
            .any(|(_, id)| id == peer_id)
    }
}

impl Drop for PeerGateway {
    fn drop(&mut self) {
        send_byebye(self.messenger.node_state.lock().unwrap().ident());
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::UdpSocket;

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
