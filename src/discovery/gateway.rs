use std::sync::{Arc, Mutex};

use tokio::{
    select,
    sync::mpsc::{self, Receiver},
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
    peers::{GatewayObserver, PeerObserver, PeerState},
};

pub struct PeerGateway<O: PeerObserver> {
    observer: O,
    messenger: Messenger,
    measurement: MeasurementService,
    node_state: Arc<Mutex<NodeState>>,
    ghost_xform: Arc<Mutex<GhostXForm>>,
    rx: Receiver<OnEvent>,
}

#[derive(Clone, Copy)]
pub enum OnEvent {
    PeerState(PeerState),
    Byebye(NodeId),
}

impl PeerGateway<GatewayObserver> {
    pub async fn new(node_state: NodeState, ghost_xform: GhostXForm) -> Self {
        let node_state = Arc::new(Mutex::new(node_state));

        let (tx, rx) = mpsc::channel::<OnEvent>(1);

        PeerGateway {
            observer: GatewayObserver::default(),
            messenger: Messenger::new(node_state.clone(), tx).await,
            measurement: MeasurementService::default(),
            node_state,
            ghost_xform: Arc::new(Mutex::new(ghost_xform)),
            rx,
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
                val = self.rx.recv() => {
                    match val.unwrap() {
                        OnEvent::PeerState(peer) => self.on_peer_state(&peer.node_state, peer.ttl).await,
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

    pub async fn on_peer_state(&self, node_state: &NodeState, ttl: u8) {
        todo!()
    }

    pub async fn on_byebye(&self, peer_id: &NodeId) {
        todo!()
    }

    pub async fn prune_expired_peers(&mut self) {
        todo!()
    }

    pub async fn schedule_next_pruning(&mut self) {
        todo!()
    }
}

impl<O: PeerObserver> Drop for PeerGateway<O> {
    fn drop(&mut self) {
        send_byebye(self.messenger.node_state.lock().unwrap().ident());
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

        let node_state = NodeState::default();
        let mut gw = PeerGateway::new(node_state, GhostXForm::default()).await;

        let s = new_udp_reuseport(IP_ANY);
        s.set_broadcast(true).unwrap();
        s.set_multicast_ttl_v4(2).unwrap();

        tokio::spawn(async move {
            let header = MessageHeader {
                ttl: 10,
                ident: node_state.node_id,
                message_type: ALIVE,
                ..Default::default()
            };

            let mut encoded = bincode::encode_to_vec(PROTOCOL_HEADER, ENCODING_CONFIG).unwrap();
            encoded.append(&mut bincode::encode_to_vec(header, ENCODING_CONFIG).unwrap());

            s.send_to(&encoded, (MULTICAST_ADDR, LINK_PORT))
                .await
                .unwrap();

            info!("test bytes sent");
        });

        gw.listen().await;
    }
}
