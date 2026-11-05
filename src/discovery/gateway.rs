use std::{
    net::{SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
};

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
#[cfg(test)]
use crate::sync::lock;

use super::{
    messenger::{send_byebye, Messenger, PeerObservation},
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
    #[cfg(test)]
    pub(crate) fn hold_discovery_boundary_fixture(
        &self,
        hints: bool,
        ready: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> std::thread::JoinHandle<()> {
        let map = self.messenger.interface_sockets.clone();
        let routes = self.messenger.measurement_routes.clone();
        std::thread::spawn(move || {
            if hints {
                routes.hold_hints_fixture(ready, release);
            } else {
                let _guard = map.lock().unwrap();
                ready.send(()).unwrap();
                release
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
            }
        })
    }

    pub(crate) fn try_discard_queued_datagrams(&self) -> Option<()> {
        self.messenger.try_discard_queued_datagrams()
    }

    pub(crate) async fn stop(&self) {
        self.gate.stop().await;
        crate::platform::io_context::retry_state(|| {
            try_timeouts(&self.peer_timeouts)?.clear();
            Some(())
        })
        .await;
    }

    #[cfg(test)]
    pub(crate) fn event_sender(&self) -> Sender<OnEvent> {
        self.messenger.tx_event.clone()
    }

    #[cfg(test)]
    pub(crate) async fn receive_peer_state_fixture(&self, local: std::net::Ipv4Addr, bytes: &[u8]) {
        self.messenger
            .receive_peer_state_fixture(local, bytes)
            .await;
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
        sessions: Option<crate::link::sessions::Sessions>,
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
        let measurement_service = MeasurementService::new_with_routes(
            ping_responder_unicast_socket,
            peer_state.clone(),
            session_state,
            clock,
            tx_measure_peer_result,
            notifier,
            rx_measure_peer_state,
            Some(messenger.measurement_routes.clone()),
        )
        .await;
        messenger.set_measurement_responder(measurement_service.ping_responder.clone());

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
                sessions,
            )
            .await,
            messenger,
            measurement_service,
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
        crate::platform::io_context::retry_state(|| {
            let mut peer = crate::platform::io_context::try_state(&self.peer_state)?;
            let responder = &self.measurement_service.ping_responder;
            let mut id = crate::platform::io_context::try_state(&responder.session_id)?;
            let mut form = crate::platform::io_context::try_state(&responder.ghost_x_form)?;
            peer.node_state = node_state.clone();
            *id = node_state.session_id;
            *form = ghost_xform;
            Some(())
        })
        .await;
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
            "initializing peer gateway {:?} on interface {:?}",
            node_id,
            self.messenger
                .interface
                .as_ref()
                .and_then(|socket| socket.local_addr().ok())
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
                            if let Some(state) = crate::platform::io_context::try_state(&peer_state) { send_byebye(state.ident()); }
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
    mut msg: PeerStateMessageType,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
    epoch: Instant,
    cancel: Arc<Notify>,
    _self_node_id: NodeId,
    lifecycle: Option<(Arc<DispatchGate>, u64)>,
) {
    debug!("received peer state from messenger");

    let deadline = msg
        .expires_at
        .unwrap_or_else(|| Instant::now() + std::time::Duration::from_secs(u64::from(msg.ttl)));
    msg.expires_at = Some(deadline);
    let slot = select! {
        biased;
        _ = PeerObservation::invalidated(msg.observation.as_deref()) => return,
        _ = tokio::time::sleep_until(deadline), if msg.ttl > 0 => return,
        result = tx_peer_event.reserve() => match result { Ok(slot) => slot, Err(_) => return },
    };
    if msg.ttl > 0 && deadline <= Instant::now() {
        return;
    }
    let peer_id = msg.node_state.ident();
    let ttl = msg.ttl;
    let observation = msg.observation.clone();
    let mut msg = Some(msg);
    let mut slot = Some(slot);
    let commit = crate::platform::io_context::retry_state(|| {
        let mut timeouts = try_timeouts(&peer_timeouts)?;
        let mut publish = || {
            let now = Instant::now();
            if ttl > 0 && deadline <= now {
                return false;
            }
            if ttl == 0 {
                if !timeouts.iter().any(|(_, id)| *id == peer_id) {
                    return false;
                }
                timeouts.retain(|(_, id)| *id != peer_id);
                // Once removed, cleanup remains an irrevocable FIFO obligation.
                slot.take().unwrap().send(PeerEvent::PeerLeft(peer_id));
            } else {
                let new_to = (
                    deadline.min(now + std::time::Duration::from_secs(3)),
                    peer_id,
                );
                timeouts.retain(|(_, id)| *id != peer_id);
                let position = timeouts
                    .iter()
                    .position(|(time, _)| *time >= new_to.0)
                    .unwrap_or(timeouts.len());
                timeouts.insert(position, new_to);
                slot.take()
                    .unwrap()
                    .send(PeerEvent::Observed(msg.take().unwrap()));
            }
            true
        };
        if let Some(observation) = &observation {
            observation
                .try_while_current(publish)
                .map(|result| result.unwrap_or(false))
        } else {
            Some(publish())
        }
    });
    let committed = select! {
        biased;
        _ = PeerObservation::invalidated(observation.as_deref()) => false,
        _ = tokio::time::sleep_until(deadline), if ttl > 0 => false,
        committed = commit => committed,
    };
    if !committed {
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

fn try_timeouts(
    timeouts: &Mutex<Vec<(Instant, NodeId)>>,
) -> Option<std::sync::MutexGuard<'_, Vec<(Instant, NodeId)>>> {
    let guard = crate::platform::io_context::try_state(timeouts)?;
    timeouts.clear_poison();
    Some(guard)
}

pub async fn on_byebye(
    peer_id: NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
    notifier: Arc<Notify>,
) {
    if !find_peer(&peer_id, peer_timeouts.clone()).await {
        return;
    }
    let Ok(slot) = tx_peer_event.reserve().await else {
        return;
    };
    let mut slot = Some(slot);
    crate::platform::io_context::retry_state(|| {
        let mut timeouts = try_timeouts(&peer_timeouts)?;
        if timeouts.iter().any(|(_, id)| *id == peer_id) {
            timeouts.retain(|(_, id)| *id != peer_id);
            slot.take().unwrap().send(PeerEvent::PeerLeft(peer_id));
        }
        if timeouts.is_empty() {
            notifier.notify_waiters();
        }
        Some(())
    })
    .await;
}

pub async fn find_peer(
    peer_id: &NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
) -> bool {
    crate::platform::io_context::retry_state(|| {
        Some(
            try_timeouts(&peer_timeouts)?
                .iter()
                .any(|(_, id)| id == peer_id),
        )
    })
    .await
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
    if crate::platform::io_context::retry_state(|| Some(try_timeouts(&peer_timeouts)?.is_empty()))
        .await
    {
        return;
    }
    // Register before spawning so a renewal cannot be missed before the worker polls.
    let mut cancelled = Box::pin(cancel.notified_owned());
    cancelled.as_mut().enable();
    tokio::spawn(async move {
        let prune = async {
            loop {
                let deadline = crate::platform::io_context::retry_state(|| {
                    Some(try_timeouts(&peer_timeouts)?.first().map(|(time, _)| *time))
                })
                .await;
                let Some(deadline) = deadline else {
                    return;
                };
                tokio::time::sleep_until(deadline + std::time::Duration::from_millis(100)).await;
                let Ok(slot) = tx_peer_event.reserve().await else {
                    return;
                };
                // A renewal can win while capacity is unavailable. Removal and
                // publication share one guard with the announcement commit.
                let mut slot = Some(slot);
                crate::platform::io_context::retry_state(|| {
                    let mut timeouts = try_timeouts(&peer_timeouts)?;
                    if timeouts
                        .first()
                        .is_some_and(|(time, _)| *time <= Instant::now())
                    {
                        let (_, peer) = timeouts.remove(0);
                        debug!(
                            "pruning peer {} after {:?}",
                            peer,
                            Instant::now().duration_since(epoch)
                        );
                        slot.take().unwrap().send(PeerEvent::PeerTimedOut(peer));
                    }
                    Some(())
                })
                .await;
            }
        };
        let run = async {
            select! { biased; _ = &mut cancelled => {}, _ = prune => {} }
        };
        if let Some((gate, epoch)) = lifecycle {
            gate.run_in_epoch(epoch, run).await;
        } else {
            run.await;
        }
    });
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
    #[tokio::test]
    async fn byebye_does_not_cancel_pruning_of_other_peers() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let first = NodeId::from_array([71; 8]);
            let second = NodeId::from_array([72; 8]);
            let now = Instant::now();
            let timeouts = Arc::new(Mutex::new(vec![
                (now + std::time::Duration::from_millis(10), first),
                (now + std::time::Duration::from_millis(20), second),
            ]));
            let (tx, mut rx) = mpsc::channel(1);
            let cancel = Arc::new(Notify::new());
            schedule_next_pruning(timeouts.clone(), now, tx.clone(), cancel.clone()).await;
            on_byebye(first, timeouts.clone(), tx, cancel).await;
            assert!(matches!(rx.recv().await, Some(PeerEvent::PeerLeft(id)) if id == first));
            assert!(matches!(rx.recv().await, Some(PeerEvent::PeerTimedOut(id)) if id == second));
            assert!(timeouts.lock().unwrap().is_empty());
        })
        .await
        .unwrap();
    }

    fn announcement(id: NodeId, ttl: u8) -> PeerStateMessageType {
        PeerStateMessageType {
            node_state: NodeState {
                node_id: id,
                ..NodeState::default()
            },
            ttl,
            expires_at: None,
            observation: None,
            measurement_endpoint: None,
            audio_endpoint: None,
        }
    }

    #[tokio::test]
    async fn poisoned_timeouts_remain_findable_removable_and_prunable() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = NodeId::from_array([80; 8]);
            let timeouts = Arc::new(Mutex::new(Vec::new()));
            let poisoned = timeouts.clone();
            assert!(std::thread::spawn(move || {
                let _guard = poisoned.lock().unwrap();
                panic!("fixture poison");
            })
            .join()
            .is_err());
            let (tx, mut rx) = mpsc::channel(4);
            let cancel = Arc::new(Notify::new());
            on_peer_state(
                announcement(id, 1),
                timeouts.clone(),
                tx.clone(),
                Instant::now(),
                cancel.clone(),
                NodeId::default(),
            )
            .await;
            assert!(!timeouts.is_poisoned());
            assert!(find_peer(&id, timeouts.clone()).await);
            assert!(matches!(rx.recv().await, Some(PeerEvent::Observed(_))));
            on_byebye(id, timeouts.clone(), tx.clone(), cancel.clone()).await;
            assert!(matches!(rx.recv().await, Some(PeerEvent::PeerLeft(node)) if node == id));
            assert!(!find_peer(&id, timeouts.clone()).await);
            on_peer_state(
                announcement(id, 1),
                timeouts.clone(),
                tx,
                Instant::now(),
                cancel,
                NodeId::default(),
            )
            .await;
            assert!(matches!(rx.recv().await, Some(PeerEvent::Observed(_))));
            assert!(matches!(rx.recv().await, Some(PeerEvent::PeerTimedOut(node)) if node == id));
            assert!(timeouts.lock().unwrap().is_empty());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_departure_keeps_timeout_and_zero_ttl_withdraws_existing_peer() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = NodeId::from_array([79; 8]);
            let original = vec![(Instant::now() + std::time::Duration::from_secs(2), id)];
            let timeouts = Arc::new(Mutex::new(original.clone()));
            let (tx, mut rx) = mpsc::channel(1);
            tx.send(PeerEvent::PeerLeft(NodeId::default()))
                .await
                .unwrap();
            let mut departure = Box::pin(on_byebye(
                id,
                timeouts.clone(),
                tx.clone(),
                Arc::new(Notify::new()),
            ));
            assert!(std::future::Future::poll(
                departure.as_mut(),
                &mut std::task::Context::from_waker(std::task::Waker::noop())
            )
            .is_pending());
            drop(departure);
            assert_eq!(*timeouts.lock().unwrap(), original);
            rx.recv().await.unwrap();
            on_peer_state(
                announcement(id, 0),
                timeouts.clone(),
                tx.clone(),
                Instant::now(),
                Arc::new(Notify::new()),
                NodeId::default(),
            )
            .await;
            assert!(matches!(rx.recv().await, Some(PeerEvent::PeerLeft(node)) if node == id));
            assert!(timeouts.lock().unwrap().is_empty());
            on_peer_state(
                announcement(id, 0),
                timeouts.clone(),
                tx,
                Instant::now(),
                Arc::new(Notify::new()),
                NodeId::default(),
            )
            .await;
            assert!(
                rx.try_recv().is_err(),
                "unknown zero-TTL messages must not add peers or emit departures"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pruning_rechecks_after_backpressured_renewal() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = NodeId::from_array([78; 8]);
            let deadline = Instant::now() + std::time::Duration::from_millis(80);
            let timeouts = Arc::new(Mutex::new(vec![(deadline, id)]));
            let (tx, mut rx) = mpsc::channel(1);
            tx.send(PeerEvent::PeerLeft(NodeId::default()))
                .await
                .unwrap();
            let epoch = Instant::now();
            let cancel = Arc::new(Notify::new());
            let mut renewal = Box::pin(on_peer_state(
                announcement(id, 30),
                timeouts.clone(),
                tx.clone(),
                epoch,
                cancel.clone(),
                NodeId::default(),
            ));
            assert!(std::future::Future::poll(
                renewal.as_mut(),
                &mut std::task::Context::from_waker(std::task::Waker::noop())
            )
            .is_pending());
            schedule_next_pruning(timeouts.clone(), epoch, tx.clone(), cancel.clone()).await;
            tokio::time::sleep_until(deadline + std::time::Duration::from_millis(180)).await;
            assert_eq!(
                timeouts.lock().unwrap().len(),
                1,
                "pruning must not remove before reserving notification capacity"
            );
            rx.recv().await.unwrap();
            renewal.await;
            assert!(matches!(rx.recv().await, Some(PeerEvent::Observed(_))));
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            assert!(
                rx.try_recv().is_err(),
                "obsolete pruning must not follow the renewal"
            );
            assert!(timeouts.lock().unwrap()[0].0 > deadline);
            cancel.notify_waiters();
        })
        .await
        .unwrap();
    }

    use std::net::IpAddr;

    use local_ip_address::list_afinet_netifas;

    use crate::{discovery::messenger::new_udp_reuseport, link::sessions::SessionId};

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gateway_rechecks_receive_deadline_after_timeout_lock_contention() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let id = NodeId::from_array([81; 8]);
            let other = NodeId::from_array([82; 8]);
            let original = vec![
                (Instant::now() + std::time::Duration::from_secs(100), id),
                (Instant::now() + std::time::Duration::from_secs(200), other),
            ];
            let timeouts = Arc::new(Mutex::new(original.clone()));
            let held = timeouts.clone();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _guard = held.lock().unwrap();
                ready_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(std::time::Duration::from_secs(4));
            });
            ready_rx.await.unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(1);
            let (tx, mut rx) = mpsc::channel(1);
            let work = tokio::spawn(on_peer_state(
                PeerStateMessageType {
                    node_state: NodeState {
                        node_id: id,
                        ..NodeState::default()
                    },
                    ttl: 30,
                    expires_at: Some(deadline),
                    observation: None,
                    measurement_endpoint: None,
                    audio_endpoint: None,
                },
                timeouts.clone(),
                tx.clone(),
                Instant::now(),
                Arc::new(Notify::new()),
                other,
            ));
            while tx.capacity() != 0 {
                tokio::task::yield_now().await;
            }
            assert!(
                Instant::now() < deadline,
                "capacity must be reserved before expiry while the mutex is held"
            );
            tokio::time::sleep_until(deadline + std::time::Duration::from_millis(20)).await;
            release_tx.send(()).unwrap();
            holder.join().unwrap();
            work.await.unwrap();
            assert!(
                rx.try_recv().is_err(),
                "expired input must not publish after a lock wait"
            );
            assert_eq!(
                *timeouts.lock().unwrap(),
                original,
                "expired input must not replace even an existing same-peer timeout"
            );
        })
        .await
        .expect("lock contention must not renew an expired announcement");
    }

    #[tokio::test]
    async fn gateway_preserves_receive_deadlines_and_rejects_expired_state() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for expired in [true, false] {
                let (tx, mut rx) = mpsc::channel(2);
                let timeouts = Arc::new(Mutex::new(Vec::new()));
                let deadline = if expired {
                    Instant::now() - std::time::Duration::from_secs(1)
                } else {
                    Instant::now() + std::time::Duration::from_millis(300)
                };
                let state = PeerStateMessageType {
                    node_state: NodeState::default(),
                    ttl: if expired { 2 } else { 1 },
                    expires_at: Some(deadline),
                    observation: None,
                    measurement_endpoint: None,
                    audio_endpoint: None,
                };
                on_peer_state(
                    state,
                    timeouts.clone(),
                    tx,
                    Instant::now(),
                    Arc::new(Notify::new()),
                    NodeId::default(),
                )
                .await;
                if expired {
                    assert!(rx.try_recv().is_err());
                    assert!(timeouts.lock().unwrap().is_empty());
                } else {
                    assert_eq!(timeouts.lock().unwrap()[0].0, deadline);
                    assert!(matches!(rx.recv().await, Some(PeerEvent::Observed(_))));
                    assert!(matches!(rx.recv().await, Some(PeerEvent::PeerTimedOut(_))));
                }
            }
        })
        .await
        .expect("gateway must honor original deadlines, including subsecond remaining life");
    }

    fn init_tracing() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    #[tokio::test]
    async fn poisoned_timeout_replacement_is_atomic_and_releases_before_event_send() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let id = NodeId::from_array([71; 8]);
            let other = NodeId::from_array([72; 8]);
            let epoch = Instant::now();
            let timeouts = Arc::new(Mutex::new(vec![
                (epoch - std::time::Duration::from_secs(1), id),
                (epoch, id),
                (epoch + std::time::Duration::from_secs(20), other),
            ]));
            let poisoned = timeouts.clone();
            assert!(std::thread::spawn(move || {
                let _guard = poisoned.lock().unwrap();
                panic!("poison timeout fixture");
            })
            .join()
            .is_err());
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.send(PeerEvent::PeerLeft(other)).await.unwrap();
            let cancel = Arc::new(Notify::new());
            let mut work = Box::pin(on_peer_state(
                PeerStateMessageType {
                    expires_at: None,
                    observation: None,
                    node_state: crate::link::node::NodeState {
                        node_id: id,
                        ..Default::default()
                    },
                    ttl: 30,
                    measurement_endpoint: None,
                    audio_endpoint: None,
                },
                timeouts.clone(),
                tx.clone(),
                epoch,
                cancel.clone(),
                other,
            ));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(work.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert_eq!(
                lock(&timeouts).len(),
                3,
                "waiting for capacity must not prematurely renew a timeout"
            );
            assert!(find_peer(&id, timeouts.clone()).await);
            rx.recv().await.unwrap();
            work.await;
            {
                let current = lock(&timeouts);
                assert_eq!(current.len(), 2);
                assert_eq!(current[0].1, id);
                assert!(current[0].0 <= Instant::now() + std::time::Duration::from_secs(3));
                assert_eq!(current[1].1, other);
            }
            assert!(matches!(rx.recv().await, Some(PeerEvent::Observed(state)) if state.node_state.ident() == id && state.ttl == 30 && state.expires_at.is_some() && state.observation.is_none()));
            on_byebye(id, timeouts.clone(), tx, cancel).await;
            assert!(matches!(rx.recv().await, Some(PeerEvent::PeerLeft(peer)) if peer == id));
            assert!(!find_peer(&id, timeouts).await);
        })
        .await
        .expect(
            "timeout replacement must recover poison and release its guard before awaiting events",
        );
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
            None,
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
