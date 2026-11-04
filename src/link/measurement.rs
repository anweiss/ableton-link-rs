use crate::discovery::messenger::{MeasurementRoute, MeasurementRoutes};
use std::{
    collections::HashMap,
    io, mem,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
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
};
use tracing::{debug, info};

use crate::{
    discovery::{messages::parse_payload, messenger::new_udp_reuseport, peers::PeerState},
    encoding::{self, Decode, Encode},
    link::{
        payload::PrevGhostTime,
        pingresponder::{
            parse_message_header, MessageType, MAX_MESSAGE_SIZE, PONG, PROTOCOL_HEADER,
            PROTOCOL_HEADER_SIZE,
        },
        Result,
    },
    sync::lock,
};

use super::{
    clock::Clock,
    controller::{gated_recv, DispatchGate},
    encoding::PayloadEntryHeader,
    ghostxform::GhostXForm,
    linear_regression::linear_regression,
    node::NodeId,
    payload::{HostTime, Payload, PayloadEntry},
    pingresponder::{encode_message, PingResponder, MESSAGE_HEADER_SIZE, PING},
    sessions::SessionId,
    state::SessionState,
};

pub const MEASUREMENT_ENDPOINT_V4_HEADER_KEY: u32 = u32::from_be_bytes(*b"mep4");
pub const MEASUREMENT_ENDPOINT_V4_SIZE: u32 =
    (mem::size_of::<Ipv4Addr>() + mem::size_of::<u16>()) as u32;
pub const MEASUREMENT_ENDPOINT_V4_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: MEASUREMENT_ENDPOINT_V4_HEADER_KEY,
    size: MEASUREMENT_ENDPOINT_V4_SIZE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementEndpointV4 {
    pub endpoint: Option<SocketAddrV4>,
}

impl Encode for MeasurementEndpointV4 {
    fn encode_to(
        &self,
        out: &mut Vec<u8>,
    ) -> core::result::Result<(), crate::encoding::EncodeError> {
        // Like `AudioEndpointV4`: never panic on a missing endpoint.
        let ep = self
            .endpoint
            .unwrap_or_else(|| SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        u32::from(*ep.ip()).encode_to(out)?;
        ep.port().encode_to(out)?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        6
    }
}

impl Decode for MeasurementEndpointV4 {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (ip_raw, n1) = u32::decode_from(bytes)?;
        let (port, n2) = u16::decode_from(&bytes[n1..])?;
        Ok((
            Self {
                endpoint: (ip_raw != 0 || port != 0)
                    .then(|| SocketAddrV4::new(Ipv4Addr::from(ip_raw), port)),
            },
            n1 + n2,
        ))
    }
}

impl MeasurementEndpointV4 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = MEASUREMENT_ENDPOINT_V4_HEADER.encode()?;
        encoded.append(&mut encoding::encode_to_vec(self)?);
        Ok(encoded)
    }
}

#[derive(Clone, Debug)]
pub enum MeasurePeerEvent {
    PeerState(SessionId, PeerState),
    XForm(SessionId, GhostXForm),
}

/// A datagram received on the shared unicast socket, along with its sender.
pub type PongMessage = (Vec<u8>, SocketAddr);

/// Routing table mapping a peer's measurement endpoint to the measurement
/// currently expecting PONG messages from it.
pub type PongDispatch = Arc<Mutex<HashMap<SocketAddr, Sender<PongMessage>>>>;

#[derive(Debug)]
struct MeasurementServiceTasks {
    gate: Arc<DispatchGate>,
    handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for MeasurementServiceTasks {
    fn drop(&mut self) {
        self.gate.close();
        for task in self
            .handles
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            task.abort();
        }
    }
}

#[derive(Debug, Clone)]
pub struct MeasurementService {
    pub measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    pub clock: Clock,
    pub ping_responder: PingResponder,
    pub tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    pub shared_socket: Arc<UdpSocket>,
    pong_dispatch: PongDispatch,
    tasks: Arc<MeasurementServiceTasks>,
}

impl MeasurementService {
    pub async fn new(
        ping_responder_unicast_socket: Arc<UdpSocket>,
        peer_state: Arc<Mutex<PeerState>>,
        session_state: Arc<Mutex<SessionState>>,
        clock: Clock,
        tx_measure_peer_result: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
        notifier: Arc<Notify>,
        rx_measure_peer_state: tokio::sync::mpsc::Receiver<MeasurePeerEvent>,
    ) -> MeasurementService {
        Self::new_with_routes(
            ping_responder_unicast_socket,
            peer_state,
            session_state,
            clock,
            tx_measure_peer_result,
            notifier,
            rx_measure_peer_state,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_with_routes(
        ping_responder_unicast_socket: Arc<UdpSocket>,
        peer_state: Arc<Mutex<PeerState>>,
        session_state: Arc<Mutex<SessionState>>,
        clock: Clock,
        tx_measure_peer_result: Sender<MeasurePeerEvent>,
        notifier: Arc<Notify>,
        mut rx_measure_peer_state: Receiver<MeasurePeerEvent>,
        routes: Option<MeasurementRoutes>,
    ) -> MeasurementService {
        let measurement_map = Arc::new(Mutex::new(HashMap::new()));
        let pong_dispatch: PongDispatch = Arc::new(Mutex::new(HashMap::new()));

        let m_map = measurement_map.clone();
        let t_peer = tx_measure_peer_result.clone();
        let socket_loop = ping_responder_unicast_socket.clone();
        let dispatch_loop = pong_dispatch.clone();
        let gate = Arc::new(DispatchGate::new_open());
        let task_gate = gate.clone();
        let mut open = task_gate.subscribe();

        let task = tokio::spawn(async move {
            while let Some((_permit, epoch, event)) =
                gated_recv(&task_gate, &mut open, &mut rx_measure_peer_state).await
            {
                if let MeasurePeerEvent::PeerState(session_id, peer) = event {
                    let route = match &routes {
                        Some(routes) => match routes.resolve(&peer) {
                            Some(route) => Some(route),
                            None => {
                                tokio::select! {
                                    biased;
                                    _ = open.closed() => {}
                                    _ = t_peer.send(MeasurePeerEvent::XForm(session_id, GhostXForm::default())) => {}
                                }
                                continue;
                            }
                        },
                        None => None,
                    };
                    let socket = route
                        .as_ref()
                        .map(MeasurementRoute::socket)
                        .unwrap_or_else(|| socket_loop.clone());
                    let dispatch = route
                        .as_ref()
                        .map(MeasurementRoute::dispatch)
                        .unwrap_or_else(|| dispatch_loop.clone());
                    let measurement = measure_peer_in_epoch(
                        clock,
                        m_map.clone(),
                        t_peer.clone(),
                        session_id,
                        peer,
                        notifier.clone(),
                        socket,
                        dispatch,
                        Some((task_gate.clone(), epoch)),
                        route,
                    );
                    tokio::select! {
                        biased;
                        _ = open.closed() => {}
                        _ = measurement => {}
                    }
                }
            }
        });

        MeasurementService {
            measurement_map,
            clock,
            ping_responder: PingResponder::new(
                ping_responder_unicast_socket.clone(),
                lock(&peer_state).session_id(),
                lock(&session_state).ghost_x_form,
                clock,
            ),
            tx_measure_peer: tx_measure_peer_result,
            shared_socket: ping_responder_unicast_socket,
            pong_dispatch,
            tasks: Arc::new(MeasurementServiceTasks {
                gate,
                handles: Mutex::new(vec![task]),
            }),
        }
    }

    pub(crate) async fn start(&self) {
        self.tasks.gate.start().await;
    }

    pub(crate) async fn stop(&self) {
        self.tasks.gate.stop().await;
    }

    /// Receive standalone traffic on the shared socket. Managed discovery
    /// measurements instead use their learned interface registration's socket
    /// and dispatcher, preserving arrival identity even for overlapping peers.
    pub async fn listen(&self, _notifier: Arc<Notify>) {
        let socket = self.shared_socket.clone();
        let ping_responder = self.ping_responder.clone();
        let pong_dispatch = self.pong_dispatch.clone();

        info!(
            "listening for ping and pong messages on {:?}",
            socket.local_addr()
        );

        let task = tokio::spawn(async move {
            loop {
                let mut buf = [0; MAX_MESSAGE_SIZE];

                let (amt, src) = match socket.recv_from(&mut buf).await {
                    Ok(result) => result,
                    Err(e) => {
                        debug!("failed to receive from measurement socket: {}", e);
                        break;
                    }
                };

                match message_type(&buf[..amt], src) {
                    Some(PING) => ping_responder.handle_ping(&buf[..amt], src).await,
                    Some(PONG) => dispatch_pong(&pong_dispatch, &buf[..amt], src),
                    _ => debug!("received invalid message from {}", src),
                }
            }
        });
        self.tasks
            .handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(task);
    }

    pub async fn update_node_state(&self, session_id: SessionId, x_form: GhostXForm) {
        self.ping_responder
            .update_node_state(session_id, x_form)
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn measure_peer(
    clock: Clock,
    measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    tx_measure_peer_result: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session_id: SessionId,
    state: PeerState,
    notifier: Arc<Notify>,
) {
    measure_peer_with_socket_factory(
        clock,
        measurement_map,
        tx_measure_peer_result,
        session_id,
        state,
        notifier,
        new_unicast_socket,
    )
    .await;
}

async fn measure_peer_with_socket_factory(
    clock: Clock,
    measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    tx_measure_peer_result: Sender<MeasurePeerEvent>,
    session_id: SessionId,
    state: PeerState,
    notifier: Arc<Notify>,
    socket_factory: impl FnOnce() -> io::Result<Arc<UdpSocket>>,
) {
    if state.measurement_endpoint.is_none() {
        report_failed_measurement(&tx_measure_peer_result, session_id).await;
        return;
    }
    let socket = match socket_factory() {
        Ok(socket) => socket,
        Err(error) => {
            debug!("can't open a measurement socket: {}", error);
            report_failed_measurement(&tx_measure_peer_result, session_id).await;
            return;
        }
    };
    let pong_dispatch: PongDispatch = Arc::new(Mutex::new(HashMap::new()));
    spawn_pong_dispatch(socket.clone(), pong_dispatch.clone());

    measure_peer_with_socket(
        clock,
        measurement_map,
        tx_measure_peer_result,
        session_id,
        state,
        notifier,
        socket,
        pong_dispatch,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn measure_peer_with_socket(
    clock: Clock,
    measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    tx_measure_peer_result: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session_id: SessionId,
    state: PeerState,
    notifier: Arc<Notify>,
    socket: Arc<UdpSocket>,
    pong_dispatch: PongDispatch,
) {
    measure_peer_in_epoch(
        clock,
        measurement_map,
        tx_measure_peer_result,
        session_id,
        state,
        notifier,
        socket,
        pong_dispatch,
        None,
        None,
    )
    .await;
}

struct MeasurementRegistration {
    node_id: NodeId,
    endpoint: SocketAddr,
    sender: Sender<PongMessage>,
    job: Arc<()>,
    measurements: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    dispatch: PongDispatch,
}

impl Drop for MeasurementRegistration {
    fn drop(&mut self) {
        let mut measurements = self
            .measurements
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Map and route can be replaced independently (including by a different
        // node reusing an endpoint). Only remove entries owned by this job.
        if measurements
            .get(&self.node_id)
            .is_some_and(|measurement| Arc::ptr_eq(&measurement.job, &self.job))
        {
            measurements.remove(&self.node_id);
        }
        if dispatch
            .get(&self.endpoint)
            .is_some_and(|sender| sender.same_channel(&self.sender))
        {
            dispatch.remove(&self.endpoint);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn measure_peer_in_epoch(
    clock: Clock,
    measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    tx_measure_peer_result: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session_id: SessionId,
    state: PeerState,
    notifier: Arc<Notify>,
    socket: Arc<UdpSocket>,
    pong_dispatch: PongDispatch,
    lifecycle: Option<(Arc<DispatchGate>, u64)>,
    route: Option<MeasurementRoute>,
) {
    // A peer that announced no measurement endpoint can't be measured: report
    // it like any other failed measurement.
    let Some(peer_endpoint) = state.measurement_endpoint else {
        info!(
            "peer {} has no measurement endpoint; can't measure session {}",
            state.node_state.node_id, session_id
        );
        report_failed_measurement(&tx_measure_peer_result, session_id).await;
        return;
    };
    info!(
        "measuring peer {} at {} for session {}",
        state.node_state.node_id, peer_endpoint, session_id
    );

    let node_id = state.node_state.node_id;
    let endpoint = SocketAddr::V4(peer_endpoint);

    let (tx_measurement, mut rx_measurement) = mpsc::channel(1);
    let (tx_pong, rx_pong) = mpsc::channel(NUMBER_DATA_POINTS);

    lock(&pong_dispatch).insert(endpoint, tx_pong.clone());
    let registration = MeasurementRegistration {
        node_id,
        endpoint,
        sender: tx_pong,
        job: Arc::new(()),
        measurements: measurement_map.clone(),
        dispatch: pong_dispatch,
    };

    let mut measurement = Measurement::with_route(
        state,
        clock,
        tx_measurement,
        notifier,
        socket,
        rx_pong,
        route,
    )
    .await;
    if measurement.cancelled {
        return;
    }
    measurement.job = registration.job.clone();
    measurement_map
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(node_id, measurement);

    tokio::spawn(async move {
        let _registration = registration;
        let forward = async move {
            let Some(data) = rx_measurement.recv().await else {
                return;
            };
            let x_form = if data.is_empty() {
                GhostXForm::default()
            } else {
                let (slope, intercept) = if data.len() >= 3 {
                    let (reg_slope, reg_intercept) = linear_regression(data.iter().copied());
                    if reg_intercept.is_finite() {
                        (1.0 + reg_slope, reg_intercept)
                    } else {
                        // Fallback to slope=1.0 with median offset
                        let offsets: Vec<f64> = data.iter().map(|(_, y)| *y).collect();
                        (1.0, median(offsets))
                    }
                } else {
                    // Not enough data for regression, use median offset
                    let mut offsets: Vec<f64> = data.iter().map(|(_, y)| *y).collect();
                    offsets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let mid = offsets[offsets.len() / 2];
                    (1.0, mid)
                };
                GhostXForm {
                    slope,
                    intercept: Duration::microseconds(intercept.round() as i64),
                }
            };
            if let Err(error) = tx_measure_peer_result
                .send(MeasurePeerEvent::XForm(session_id, x_form))
                .await
            {
                debug!("measurement result consumer closed: {}", error);
            }
        };
        if let Some((gate, epoch)) = lifecycle {
            gate.run_in_epoch(epoch, forward).await;
        } else {
            forward.await;
        }
    });
}

pub const NUMBER_DATA_POINTS: usize = 20;
pub const NUMBER_MEASUREMENTS: usize = 5;

/// The socket a measurement pings a peer from. Bound to the wildcard address so
/// the OS picks the source address that routes to that peer: binding it to one
/// interface (the first non-loopback one) sent pings to peers on the machine's
/// other networks from an address they could not reach or answer, and
/// panicked on a machine with no non-loopback IPv4 address at all.
fn new_unicast_socket() -> io::Result<Arc<UdpSocket>> {
    new_udp_reuseport(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()).map(Arc::new)
}

/// Tell the session logic a measurement of `session_id` failed (an identity
/// transform), as a measurement that got no answers does.
async fn report_failed_measurement(
    tx_measure_peer_result: &tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session_id: SessionId,
) {
    if let Err(error) = tx_measure_peer_result
        .send(MeasurePeerEvent::XForm(session_id, GhostXForm::default()))
        .await
    {
        debug!("measurement result consumer closed: {}", error);
    }
}

/// Determine the message type of a datagram received on a measurement socket,
/// returning `None` when it is not a well formed Link message.
fn message_type(data: &[u8], src: SocketAddr) -> Option<MessageType> {
    if data.len() < PROTOCOL_HEADER_SIZE + MESSAGE_HEADER_SIZE
        || !data.starts_with(&PROTOCOL_HEADER)
    {
        debug!("protocol header mismatch from {}", src);
        return None;
    }

    match parse_message_header(data) {
        Ok((header, _)) => Some(header.message_type),
        Err(e) => {
            debug!("failed to parse message header from {}: {}", src, e);
            None
        }
    }
}

/// Hand a PONG datagram to the measurement registered for the peer that sent
/// it. `try_send` is used so that a slow or already finished measurement cannot
/// stall the dispatch of datagrams destined for other peers.
pub(crate) fn dispatch_pong(pong_dispatch: &PongDispatch, data: &[u8], src: SocketAddr) {
    let mut dispatch = lock(pong_dispatch);

    let closed = match dispatch.get(&src) {
        Some(tx_pong) => match tx_pong.try_send((data.to_vec(), src)) {
            Ok(()) => false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!("dropping pong from {}: measurement is busy", src);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => true,
        },
        None => {
            debug!("no measurement registered for pong from {}", src);
            false
        }
    };

    if closed {
        dispatch.remove(&src);
    }
}

/// Run the receive loop for a privately owned measurement socket, dispatching
/// PONG messages to the measurement registered for the sending peer.
/// `MeasurementService::listen` performs the same dispatch for the socket it
/// shares with the ping responder.
fn spawn_pong_dispatch(socket: Arc<UdpSocket>, pong_dispatch: PongDispatch) {
    tokio::spawn(async move {
        loop {
            let mut buf = [0; MAX_MESSAGE_SIZE];

            let (amt, src) = match socket.recv_from(&mut buf).await {
                Ok(result) => result,
                Err(e) => {
                    debug!("failed to receive from measurement socket: {}", e);
                    break;
                }
            };

            if let Some(PONG) = message_type(&buf[..amt], src) {
                dispatch_pong(&pong_dispatch, &buf[..amt], src);
            }
        }
    });
}

#[derive(Debug)]
struct MeasurementTasks(Vec<tokio::task::JoinHandle<()>>);

impl MeasurementTasks {
    fn abort(&self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl Drop for MeasurementTasks {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Debug)]
pub struct Measurement {
    route: Option<MeasurementRoute>,
    pub unicast_socket: Option<Arc<UdpSocket>>,
    pub session_id: SessionId,
    pub measurement_endpoint: Option<SocketAddrV4>,
    pub data: Arc<Mutex<Vec<(f64, f64)>>>,
    pub clock: Clock,
    pub measurements_started: Arc<Mutex<usize>>,
    pub success: Arc<Mutex<bool>>,
    pub init_bytes_sent: usize,
    tx_timer: Sender<()>,
    tasks: MeasurementTasks,
    cancelled: bool,
    job: Arc<()>,
}

impl Measurement {
    pub async fn new(
        state: PeerState,
        clock: Clock,
        tx_measurement: Sender<Vec<(f64, f64)>>,
        notifier: Arc<Notify>,
    ) -> io::Result<Self> {
        Self::new_with_socket_factory(state, clock, tx_measurement, notifier, new_unicast_socket)
            .await
    }

    async fn new_with_socket_factory(
        state: PeerState,
        clock: Clock,
        tx_measurement: Sender<Vec<(f64, f64)>>,
        notifier: Arc<Notify>,
        socket_factory: impl FnOnce() -> io::Result<Arc<UdpSocket>>,
    ) -> io::Result<Self> {
        if state.measurement_endpoint.is_none() {
            let (tx_timer, _rx_timer) = mpsc::channel(1);
            let measurement = Self {
                route: None,
                unicast_socket: None,
                session_id: state.node_state.session_id,
                measurement_endpoint: None,
                data: Arc::new(Mutex::new(Vec::new())),
                clock,
                measurements_started: Arc::new(Mutex::new(0)),
                success: Arc::new(Mutex::new(false)),
                init_bytes_sent: 0,
                tx_timer,
                tasks: MeasurementTasks(Vec::new()),
                cancelled: true,
                job: Arc::new(()),
            };
            if let Err(error) = tx_measurement.send(Vec::new()).await {
                debug!("measurement result consumer closed: {}", error);
            }
            return Ok(measurement);
        }
        let socket = socket_factory()?;
        let pong_dispatch: PongDispatch = Arc::new(Mutex::new(HashMap::new()));
        let (tx_pong, rx_pong) = mpsc::channel(NUMBER_DATA_POINTS);

        if let Some(endpoint) = state.measurement_endpoint {
            lock(&pong_dispatch).insert(SocketAddr::V4(endpoint), tx_pong);
            spawn_pong_dispatch(socket.clone(), pong_dispatch);
        }

        Ok(Self::with_socket(state, clock, tx_measurement, notifier, socket, rx_pong).await)
    }

    pub async fn with_socket(
        state: PeerState,
        clock: Clock,
        tx_measurement: Sender<Vec<(f64, f64)>>,
        notifier: Arc<Notify>,
        unicast_socket: Arc<UdpSocket>,
        rx_pong: Receiver<PongMessage>,
    ) -> Self {
        Self::with_route(
            state,
            clock,
            tx_measurement,
            notifier,
            unicast_socket,
            rx_pong,
            None,
        )
        .await
    }

    async fn with_route(
        state: PeerState,
        clock: Clock,
        tx_measurement: Sender<Vec<(f64, f64)>>,
        notifier: Arc<Notify>,
        unicast_socket: Arc<UdpSocket>,
        rx_pong: Receiver<PongMessage>,
        route: Option<MeasurementRoute>,
    ) -> Self {
        // Register before any await or spawned worker can observe disable.
        let cancelled = notifier.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        let (tx_timer, mut rx_timer) = mpsc::channel(1);

        info!(
            "initiating measurement using socket {:?} for measurement_endpoint {:?}",
            unicast_socket.local_addr(),
            state.measurement_endpoint
        );

        let success = Arc::new(Mutex::new(false));
        let data = Arc::new(Mutex::new(vec![]));

        let mut measurement = Measurement {
            route: route.clone(),
            unicast_socket: Some(unicast_socket.clone()),
            session_id: state.node_state.session_id,
            measurement_endpoint: state.measurement_endpoint,
            data: data.clone(),
            clock,
            measurements_started: Arc::new(Mutex::new(0)),
            success: success.clone(),
            tx_timer,
            init_bytes_sent: 0,
            tasks: MeasurementTasks(Vec::new()),
            cancelled: false,
            job: Arc::new(()),
        };

        // Public constructors must complete the result protocol even when no
        // worker starts; another sender may keep the consumer's channel open.
        let Some(endpoint) = state.measurement_endpoint else {
            if let Err(error) = tx_measurement.send(Vec::new()).await {
                debug!("measurement result consumer closed: {}", error);
            }
            measurement.cancelled = true;
            return measurement;
        };

        let ht = HostTime::new(clock.micros());

        let s = success.clone();
        let d = data.clone();
        let t = tx_measurement.clone();

        let finished_notifier = Arc::new(Notify::new());

        let fn_loop = finished_notifier.clone();

        measurement.tasks.0.push(tokio::spawn(async move {
            while rx_timer.recv().await.is_some() {
                fn_loop.notify_one();
                finish(s.clone(), endpoint, d.clone(), t.clone()).await;
            }
        }));

        measurement.listen(rx_pong).await;

        info!("sending initial host time ping {:?}", ht);

        let init_bytes_sent = match send_ping_via(
            unicast_socket.clone(),
            route.clone(),
            endpoint,
            &Payload {
                entries: vec![PayloadEntry::HostTime(ht)],
            },
        )
        .await
        {
            Ok(bytes_sent) => bytes_sent,
            Err(e) => {
                debug!("failed to send initial ping to {}: {}", endpoint, e);
                0
            }
        };

        measurement.init_bytes_sent = init_bytes_sent;

        let timer = reset_timer(
            measurement.measurements_started.clone(),
            clock,
            Some(unicast_socket.clone()),
            endpoint,
            data.clone(),
            tx_measurement.clone(),
            finished_notifier.clone(),
            route,
        );
        tokio::select! {
            biased;
            _ = &mut cancelled => {
                measurement.cancelled = true;
                measurement.tasks.abort();
            }
            _ = timer => {}
        }

        measurement
    }

    pub async fn listen(&mut self, mut rx_pong: Receiver<PongMessage>) {
        let (Some(socket), Some(endpoint)) =
            (self.unicast_socket.clone(), self.measurement_endpoint)
        else {
            debug!("measurement has no socket or endpoint to listen with");
            return;
        };

        let clock = self.clock;
        let route = self.route.clone();
        let s_id = self.session_id;
        let data = self.data.clone();
        let tx_timer = self.tx_timer.clone();

        info!("listening for pong messages from {}", endpoint);

        self.tasks.0.push(tokio::spawn(async move {
            let mut pong_received = false;

            // Pongs are dispatched to this measurement by the single receive
            // loop owned by `MeasurementService`.
            while let Some((buf, src)) = rx_pong.recv().await {
                let (header, header_len) = match parse_message_header(&buf) {
                    Ok(header) => header,
                    Err(e) => {
                        debug!("failed to parse pong message header from {}: {}", src, e);
                        continue;
                    }
                };

                if header.message_type == PONG {
                    if !pong_received {
                        info!("received pong message from {}", src);
                        pong_received = true;
                    }

                    // Inbound data: a malformed pong is dropped, never trusted.
                    let payload = match parse_payload(&buf[header_len..]) {
                        Ok(payload) => payload,
                        Err(e) => {
                            debug!("malformed pong payload from {}: {}", src, e);
                            continue;
                        }
                    };

                    let mut session_id = SessionId::default();
                    let mut ghost_time = Duration::zero();
                    let mut prev_ghost_time = Duration::zero();
                    let mut prev_host_time = Duration::zero();

                    for entry in payload.entries.iter() {
                        match entry {
                            PayloadEntry::SessionMembership(id) => session_id = id.session_id,
                            PayloadEntry::GhostTime(gt) => ghost_time = gt.time,
                            PayloadEntry::PrevGhostTime(gt) => prev_ghost_time = gt.time,
                            PayloadEntry::HostTime(ht) => prev_host_time = ht.time,
                            _ => continue,
                        }
                    }

                    if s_id == session_id {
                        let host_time = clock.micros();

                        let payload = Payload {
                            entries: vec![
                                PayloadEntry::HostTime(HostTime { time: host_time }),
                                PayloadEntry::PrevGhostTime(PrevGhostTime {
                                    time: prev_ghost_time,
                                }),
                            ],
                        };

                        if let Err(e) =
                            send_ping_via(socket.clone(), route.clone(), endpoint, &payload).await
                        {
                            debug!("failed to send ping to {}: {}", endpoint, e);
                        }

                        // Times come from the peer; a value outside chrono's
                        // microsecond range is a bad sample, so it's skipped.
                        let us = |d: Duration| d.num_microseconds().map(|v| v as f64);
                        if ghost_time != Duration::microseconds(0)
                            && prev_host_time != Duration::microseconds(0)
                        {
                            if let (Some(sum_host), Some(ghost)) =
                                (us(host_time + prev_host_time), us(ghost_time))
                            {
                                let avg_host = sum_host * 0.5;
                                lock(&data).push((avg_host, ghost - avg_host));
                            }

                            if prev_ghost_time != Duration::microseconds(0) {
                                if let (Some(sum_ghost), Some(prev_host)) =
                                    (us(ghost_time + prev_ghost_time), us(prev_host_time))
                                {
                                    let avg_ghost = sum_ghost * 0.5;
                                    lock(&data).push((prev_host, avg_ghost - prev_host));
                                }
                            }
                        }

                        let enough = lock(&data).len() > NUMBER_DATA_POINTS;
                        if enough {
                            if let Err(error) = tx_timer.send(()).await {
                                debug!("measurement timer closed during cancellation: {}", error);
                            }
                            break;
                        }
                    }
                }
            }
        }));
    }
}

fn publish_samples(tx: &Sender<Vec<(f64, f64)>>, samples: Vec<(f64, f64)>) {
    // The receiver may not be polled until the timer returns. A queued result
    // wins over a concurrent completion; waiting for capacity would deadlock.
    match tx.try_send(samples) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            debug!("measurement already has a queued result");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            debug!("measurement result receiver closed");
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn reset_timer(
    measurements_started: Arc<Mutex<usize>>,
    clock: Clock,
    unicast_socket: Option<Arc<UdpSocket>>,
    measurement_endpoint: SocketAddrV4,
    data: Arc<Mutex<Vec<(f64, f64)>>>,
    tx_measurement: Sender<Vec<(f64, f64)>>,
    finished_notifier: Arc<Notify>,
    route: Option<MeasurementRoute>,
) {
    loop {
        select! {
            _  = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                let started = *lock(&measurements_started);
                info!("measurements_start {}", started);
                if started < NUMBER_MEASUREMENTS {
                    let ht = HostTime {
                        time: clock.micros(),
                    };

                    let Some(socket) = unicast_socket.as_ref() else {
                        debug!("no socket to ping {} with", measurement_endpoint);
                        break;
                    };
                    if let Err(e) = send_ping_via(
                        socket.clone(), route.clone(),
                        measurement_endpoint,
                        &Payload {
                            entries: vec![PayloadEntry::HostTime(ht)],
                        },
                    )
                    .await {
                        debug!("Failed to send ping to {}: {}", measurement_endpoint, e);
                        publish_samples(&tx_measurement, Vec::new());
                        break;
                    }

                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                    *lock(&measurements_started) += 1;
                } else {
                    lock(&data).clear();
                    info!("measuring {} failed", measurement_endpoint);

                    let data = lock(&data).clone();
                    publish_samples(&tx_measurement, data);
                    break;
                }
            }
            _ = finished_notifier.notified() => {
                break;
            }
        }
    }
}

async fn finish(
    success: Arc<Mutex<bool>>,
    measurement_endpoint: SocketAddrV4,
    data: Arc<Mutex<Vec<(f64, f64)>>>,
    tx_measurement: Sender<Vec<(f64, f64)>>,
) {
    *lock(&success) = true;
    debug!("measuring {} done", measurement_endpoint);

    let d = lock(&data).clone();
    publish_samples(&tx_measurement, d);
    lock(&data).clear();
}

pub async fn send_ping(
    socket: Arc<UdpSocket>,
    measurement_endpoint: SocketAddrV4,
    payload: &Payload,
) -> io::Result<usize> {
    send_ping_via(socket, None, measurement_endpoint, payload).await
}

async fn send_ping_via(
    socket: Arc<UdpSocket>,
    route: Option<MeasurementRoute>,
    measurement_endpoint: SocketAddrV4,
    payload: &Payload,
) -> std::io::Result<usize> {
    let message = match encode_message(PING, payload) {
        Ok(message) => message,
        Err(e) => {
            // Surface the encode failure through the existing `io::Result` so
            // the send paths log and drop it rather than aborting.
            return Err(io::Error::new(io::ErrorKind::InvalidInput, e.to_string()));
        }
    };
    debug!(
        "sending ping message to measurement endpoint {}",
        measurement_endpoint
    );

    if let Some(route) = route {
        route.send(&message, measurement_endpoint).await
    } else {
        socket.send_to(&message, measurement_endpoint).await
    }
}

/// The median, or 0.0 for no numbers. NaNs sort last (`total_cmp`) rather
/// than panicking the comparison.
pub fn median(mut numbers: Vec<f64>) -> f64 {
    numbers.sort_by(|a, b| a.total_cmp(b));
    let length = numbers.len();

    if length == 0 {
        return 0.0;
    }
    if length.is_multiple_of(2) {
        let mid = length / 2;
        (numbers[mid - 1] + numbers[mid]) / 2.0
    } else {
        numbers[length / 2]
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use local_ip_address::list_afinet_netifas;
    use tokio::sync::mpsc::Receiver;

    use crate::{
        discovery::{
            gateway::{OnEvent, PeerGateway},
            messenger::new_udp_reuseport,
            peers::PeerStateChange,
        },
        link::{controller::SessionPeerCounter, node::NodeState},
    };

    use super::*;
    use chrono::Duration;

    fn inert_measurement(job: Arc<()>, endpoint: SocketAddrV4) -> Measurement {
        let (tx_timer, _rx_timer) = mpsc::channel(1);
        Measurement {
            route: None,
            unicast_socket: None,
            session_id: SessionId::default(),
            measurement_endpoint: Some(endpoint),
            data: Arc::new(Mutex::new(Vec::new())),
            clock: Clock::new(),
            measurements_started: Arc::new(Mutex::new(0)),
            success: Arc::new(Mutex::new(false)),
            init_bytes_sent: 0,
            tx_timer,
            tasks: MeasurementTasks(Vec::new()),
            cancelled: false,
            job,
        }
    }

    #[tokio::test]
    async fn public_constructors_report_missing_endpoint_samples() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for use_supplied_socket in [false, true] {
                let (tx, mut rx) = mpsc::channel(1);
                let measurement = if use_supplied_socket {
                    let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
                    let (_tx_pong, rx_pong) = mpsc::channel(1);
                    Measurement::with_socket(
                        PeerState::default(),
                        Clock::new(),
                        tx.clone(),
                        Arc::new(Notify::new()),
                        socket,
                        rx_pong,
                    )
                    .await
                } else {
                    Measurement::new(
                        PeerState::default(),
                        Clock::new(),
                        tx.clone(),
                        Arc::new(Notify::new()),
                    )
                    .await
                    .unwrap()
                };
                assert!(rx.recv().await.unwrap().is_empty());
                if !use_supplied_socket {
                    assert!(measurement.unicast_socket.is_none());
                }
                assert!(measurement.cancelled);
                assert!(measurement.tasks.0.is_empty());
                assert!(
                    !tx.is_closed(),
                    "the retained sender must not mask a missing result"
                );
                assert!(
                    rx.try_recv().is_err(),
                    "only one failure result is reported"
                );
            }
        })
        .await
        .expect("constructors must report failure without waiting for sender closure");
    }

    #[tokio::test]
    async fn public_constructor_missing_endpoint_skips_socket_factory() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for panic_if_called in [true, false] {
                let (tx, mut rx) = mpsc::channel(1);
                let measurement = Measurement::new_with_socket_factory(
                    PeerState::default(),
                    Clock::new(),
                    tx.clone(),
                    Arc::new(Notify::new()),
                    || {
                        if panic_if_called {
                            panic!("missing endpoint must not bind a socket");
                        }
                        Err(io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "injected bind error",
                        ))
                    },
                )
                .await
                .expect("an absent endpoint is reported through samples, not a bind error");
                assert!(rx.recv().await.unwrap().is_empty());
                assert!(measurement.unicast_socket.is_none());
                assert!(measurement.measurement_endpoint.is_none());
                assert!(measurement.cancelled);
                assert!(measurement.tasks.0.is_empty());
                assert!(!tx.is_closed());
                assert!(rx.try_recv().is_err(), "report exactly one empty result");
            }
        })
        .await
        .expect("constructor failure must not wait for socket creation or sender closure");
    }

    #[tokio::test]
    async fn missing_endpoint_reports_failed_session_measurement() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let session_id = SessionId(NodeId::from_array([7; 8]));
            let measurements = Arc::new(Mutex::new(HashMap::new()));
            let dispatch = Arc::new(Mutex::new(HashMap::new()));
            let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
            let (tx, mut rx) = mpsc::channel(1);
            measure_peer_with_socket(
                Clock::new(),
                measurements.clone(),
                tx.clone(),
                session_id,
                PeerState::default(),
                Arc::new(Notify::new()),
                socket,
                dispatch.clone(),
            )
            .await;
            assert!(
                matches!(rx.recv().await, Some(MeasurePeerEvent::XForm(id, xform))
                if id == session_id && xform == GhostXForm::default())
            );
            assert!(lock(&measurements).is_empty());
            assert!(lock(&dispatch).is_empty());
            assert!(!tx.is_closed());
        })
        .await
        .expect("missing endpoint must report a bounded failure event");
    }

    #[tokio::test]
    async fn missing_endpoint_never_allocates_a_socket_or_dispatcher() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let session_id = SessionId(NodeId::from_array([9; 8]));
            let measurements = Arc::new(Mutex::new(HashMap::new()));
            let (tx, mut rx) = mpsc::channel(1);
            measure_peer_with_socket_factory(
                Clock::new(),
                measurements.clone(),
                tx.clone(),
                session_id,
                PeerState::default(),
                Arc::new(Notify::new()),
                || panic!("missing endpoint must fail before socket allocation"),
            )
            .await;
            assert!(
                matches!(rx.recv().await, Some(MeasurePeerEvent::XForm(id, xform))
                if id == session_id && xform == GhostXForm::default())
            );
            assert!(lock(&measurements).is_empty());
            assert!(!tx.is_closed());
            assert!(rx.try_recv().is_err());
        })
        .await
        .expect("missing endpoint must report failure without allocating resources");
    }

    #[tokio::test]
    async fn socket_bind_error_reports_failed_session_measurement() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let session_id = SessionId(NodeId::from_array([8; 8]));
            let measurements = Arc::new(Mutex::new(HashMap::new()));
            let state = PeerState {
                measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808)),
                ..PeerState::default()
            };
            let (tx, mut rx) = mpsc::channel(1);
            measure_peer_with_socket_factory(
                Clock::new(),
                measurements.clone(),
                tx.clone(),
                session_id,
                state,
                Arc::new(Notify::new()),
                || {
                    Err(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "injected bind failure",
                    ))
                },
            )
            .await;
            assert!(
                matches!(rx.recv().await, Some(MeasurePeerEvent::XForm(id, xform))
                if id == session_id && xform == GhostXForm::default())
            );
            assert!(lock(&measurements).is_empty());
            assert!(!tx.is_closed());
        })
        .await
        .expect("socket setup failure must report a bounded failure event");
    }

    #[tokio::test]
    async fn pong_worker_drops_malformed_and_out_of_range_samples_then_continues() {
        use crate::link::payload::GhostTime;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
            let SocketAddr::V4(endpoint) = socket.local_addr().unwrap() else {
                panic!("expected IPv4")
            };
            let mut measurement = inert_measurement(Arc::new(()), endpoint);
            measurement.unicast_socket = Some(socket);
            while measurement.clock.micros() <= Duration::zero() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            let (tx_pong, rx_pong) = mpsc::channel(8);
            measurement.listen(rx_pong).await;
            let packet = |host, ghost, previous| {
                encode_message(
                    PONG,
                    &Payload {
                        entries: vec![
                            PayloadEntry::HostTime(HostTime::new(Duration::microseconds(host))),
                            PayloadEntry::GhostTime(GhostTime::new(Duration::microseconds(ghost))),
                            PayloadEntry::PrevGhostTime(PrevGhostTime::new(
                                Duration::microseconds(previous),
                            )),
                        ],
                    },
                )
                .unwrap()
            };
            let mut malformed = packet(100, 200, 300);
            malformed.pop();
            for bytes in [
                vec![0],
                malformed,
                packet(i64::MAX, 1, 0),
                packet(i64::MAX, i64::MAX, i64::MAX),
                packet(i64::MAX, i64::MIN, i64::MIN),
                packet(100, 200, 300),
            ] {
                tx_pong
                    .send((bytes, SocketAddr::V4(endpoint)))
                    .await
                    .unwrap();
            }
            drop(tx_pong);
            measurement
                .tasks
                .0
                .pop()
                .unwrap()
                .await
                .expect("invalid pongs must not panic the worker");
            let data = lock(&measurement.data);
            assert_eq!(
                data.len(),
                2,
                "only the final valid pong contributes samples"
            );
            assert_eq!(data[0].0 + data[0].1, 200.0);
            assert_eq!(data[1], (100.0, 150.0));
        })
        .await
        .expect("pong worker must process subsequent valid samples and terminate");
    }

    #[tokio::test]
    async fn queued_samples_do_not_block_success_or_timeout_completion() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (tx, mut rx) = mpsc::channel(1);
            let queued = vec![(1.0, 2.0)];
            tx.send(queued.clone()).await.unwrap();
            let data = Arc::new(Mutex::new(vec![(3.0, 4.0)]));
            let endpoint = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808);
            finish(
                Arc::new(Mutex::new(false)),
                endpoint,
                data.clone(),
                tx.clone(),
            )
            .await;
            reset_timer(
                Arc::new(Mutex::new(NUMBER_MEASUREMENTS)),
                Clock::new(),
                None,
                endpoint,
                data,
                tx.clone(),
                Arc::new(Notify::new()),
                None,
            )
            .await;
            assert_eq!(rx.try_recv().unwrap(), queued);
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        })
        .await
        .expect("completion must not wait for the caller to drain queued samples");
    }

    #[tokio::test]
    async fn managed_missing_route_remains_cancelable_with_result_backpressure() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
            let (tx_result, _rx_result) = mpsc::channel(1);
            tx_result
                .send(MeasurePeerEvent::XForm(
                    SessionId::default(),
                    GhostXForm::default(),
                ))
                .await
                .unwrap();
            let (tx_peer, rx_peer) = mpsc::channel(1);
            let service = MeasurementService::new_with_routes(
                socket,
                Arc::new(Mutex::new(PeerState::default())),
                Arc::new(Mutex::new(SessionState::default())),
                Clock::new(),
                tx_result,
                Arc::new(Notify::new()),
                rx_peer,
                Some(MeasurementRoutes::default()),
            )
            .await;
            tx_peer
                .send(MeasurePeerEvent::PeerState(
                    SessionId::default(),
                    PeerState::default(),
                ))
                .await
                .unwrap();
            drop(tx_peer.reserve().await.unwrap());
            service.stop().await;
            assert!(service.measurement_map.lock().unwrap().is_empty());
        })
        .await
        .expect("missing-route failure reporting must not block lifecycle stop");
    }

    #[tokio::test]
    async fn registration_cleans_old_node_without_deleting_replacements() {
        for replace_same_node in [false, true] {
            let endpoint = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808);
            let node = NodeId::from_array([1; 8]);
            let replacement_node = if replace_same_node {
                node
            } else {
                NodeId::from_array([2; 8])
            };
            let old_job = Arc::new(());
            let replacement_job = Arc::new(());
            let measurements = Arc::new(Mutex::new(HashMap::from([(
                node,
                inert_measurement(old_job.clone(), endpoint),
            )])));
            let (old_route, _rx_old) = mpsc::channel(1);
            let (replacement_route, _rx_new) = mpsc::channel(1);
            let dispatch = Arc::new(Mutex::new(HashMap::from([(
                SocketAddr::V4(endpoint),
                replacement_route.clone(),
            )])));
            let registration = MeasurementRegistration {
                node_id: node,
                endpoint: SocketAddr::V4(endpoint),
                sender: old_route,
                job: old_job,
                measurements: measurements.clone(),
                dispatch: dispatch.clone(),
            };
            measurements.lock().unwrap().insert(
                replacement_node,
                inert_measurement(replacement_job.clone(), endpoint),
            );
            drop(registration);
            let entries = measurements.lock().unwrap();
            assert_eq!(entries.len(), 1);
            assert!(Arc::ptr_eq(
                &entries[&replacement_node].job,
                &replacement_job
            ));
            assert!(dispatch.lock().unwrap()[&SocketAddr::V4(endpoint)]
                .same_channel(&replacement_route));
        }
    }

    #[tokio::test]
    async fn workers_tolerate_closed_result_channels() {
        let endpoint = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808);
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let data = Arc::new(Mutex::new(vec![(1.0, 2.0)]));
        finish(
            Arc::new(Mutex::new(false)),
            endpoint,
            data.clone(),
            tx.clone(),
        )
        .await;
        assert!(data.lock().unwrap().is_empty());
        reset_timer(
            Arc::new(Mutex::new(NUMBER_MEASUREMENTS)),
            Clock::new(),
            None,
            endpoint,
            data,
            tx,
            Arc::new(Notify::new()),
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn pong_worker_tolerates_a_closed_timer_channel() {
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        let SocketAddr::V4(endpoint) = socket.local_addr().unwrap() else {
            panic!("expected IPv4");
        };
        let mut measurement = inert_measurement(Arc::new(()), endpoint);
        measurement.unicast_socket = Some(socket);
        *measurement.data.lock().unwrap() = vec![(1.0, 2.0); NUMBER_DATA_POINTS + 1];
        let (tx_pong, rx_pong) = mpsc::channel(1);
        measurement.listen(rx_pong).await;
        let packet = encode_message(PONG, &Payload::default()).unwrap();
        tx_pong
            .send((packet, SocketAddr::V4(endpoint)))
            .await
            .unwrap();
        let worker = measurement.tasks.0.pop().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .unwrap()
            .expect("closed timer must not panic the pong worker");
    }

    #[tokio::test]
    async fn cancelled_registration_preserves_a_replacement_route() {
        let dispatch = Arc::new(Mutex::new(HashMap::new()));
        let measurements = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = SocketAddr::from((Ipv4Addr::LOCALHOST, 20808));
        let (first, _first_rx) = mpsc::channel(1);
        let (second, _second_rx) = mpsc::channel(1);
        dispatch.lock().unwrap().insert(endpoint, first.clone());
        let registration = MeasurementRegistration {
            node_id: NodeId::default(),
            endpoint,
            sender: first,
            job: Arc::new(()),
            measurements,
            dispatch: dispatch.clone(),
        };
        dispatch.lock().unwrap().insert(endpoint, second.clone());
        drop(registration);
        assert!(dispatch.lock().unwrap()[&endpoint].same_channel(&second));
    }

    #[tokio::test]
    async fn cancelled_registration_removes_its_own_route() {
        let dispatch = Arc::new(Mutex::new(HashMap::new()));
        let (sender, mut receiver) = mpsc::channel(1);
        let endpoint = SocketAddr::from((Ipv4Addr::LOCALHOST, 20808));
        let job = Arc::new(());
        let node = NodeId::default();
        let measurements = Arc::new(Mutex::new(HashMap::from([(
            node,
            inert_measurement(job.clone(), SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808)),
        )])));
        dispatch.lock().unwrap().insert(endpoint, sender.clone());
        drop(MeasurementRegistration {
            node_id: node,
            endpoint,
            sender,
            job,
            measurements: measurements.clone(),
            dispatch: dispatch.clone(),
        });
        assert!(dispatch.lock().unwrap().is_empty());
        assert!(measurements.lock().unwrap().is_empty());
        assert!(receiver.recv().await.is_none());
    }

    fn init_tracing() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    async fn init_gateway() -> (PeerGateway, Receiver<OnEvent>, Arc<Notify>) {
        let session_id = SessionId::default();
        let node_1 = NodeState::new(session_id);
        let (tx_measure_peer_result, _) = mpsc::channel::<MeasurePeerEvent>(1);
        let (_, rx_measure_peer_state) = mpsc::channel::<MeasurePeerEvent>(1);
        let (tx_event, rx_event) = mpsc::channel::<OnEvent>(1);
        let (tx_peer_state_change, mut rx_peer_state_change) =
            mpsc::channel::<Vec<PeerStateChange>>(1);

        let notifier = Arc::new(Notify::new());

        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();

        tokio::spawn(async move {
            while (rx_peer_state_change.recv().await).is_some() {
                *c.try_lock().unwrap() += 1;
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

        (
            PeerGateway::new(
                Arc::new(Mutex::new(PeerState {
                    node_state: node_1,
                    measurement_endpoint: None,
                    audio_endpoint: None,
                })),
                Arc::new(Mutex::new(SessionState::default())),
                Clock::default(),
                Arc::new(Mutex::new(SessionPeerCounter::default())),
                tx_peer_state_change.clone(),
                tx_event,
                tx_measure_peer_result,
                Arc::new(Mutex::new(vec![])),
                notifier.clone(),
                rx_measure_peer_state,
                ping_responder_unicast_socket,
                Arc::new(Mutex::new(true)),
            )
            .await
            .unwrap(),
            rx_event,
            notifier,
        )
    }

    #[tokio::test]
    #[ignore] // Requires real UDP multicast; crashes on macOS CI — run locally with --include-ignored
    async fn test_send_ping_on_new() {
        init_tracing();

        let (gw, rx_event, notifier) = init_gateway().await;
        let n = notifier.clone();
        tokio::spawn(async move {
            gw.listen(rx_event, n).await;
        });

        let (tx_measurement, mut rx_measurement) = mpsc::channel::<Vec<(f64, f64)>>(1);

        let ip = list_afinet_netifas()
            .unwrap()
            .iter()
            .find_map(|(_, ip)| match ip {
                IpAddr::V4(ipv4) if !ip.is_loopback() => Some(*ipv4),
                _ => None,
            })
            .unwrap();

        let shared_socket = Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0).into()).unwrap());
        let (_tx_pong, rx_pong) = mpsc::channel::<PongMessage>(NUMBER_DATA_POINTS);

        let measurement = Measurement::with_socket(
            PeerState {
                measurement_endpoint: Some(
                    shared_socket
                        .local_addr()
                        .unwrap()
                        .to_string()
                        .parse()
                        .unwrap(),
                ),
                ..Default::default()
            },
            Clock::default(),
            tx_measurement,
            notifier,
            shared_socket,
            rx_pong,
        )
        .await;

        // Give the measurement some time to attempt to send pings
        tokio::time::sleep(Duration::milliseconds(100).to_std().unwrap()).await;

        // Try to receive a result or timeout
        tokio::select! {
            result = rx_measurement.recv() => {
                // If we get a result, that's fine (measurement completed)
                if let Some(_data) = result {
                    // Test passed - measurement attempted
                }
            }
            _ = tokio::time::sleep(Duration::seconds(1).to_std().unwrap()) => {
                // Timeout is also fine - measurement is running in background
            }
        }

        // Clean up
        drop(measurement);
    }

    // Covers the send path itself, not just `encode_message`. An oversized
    // payload must surface through the existing `io::Result` so the callers
    // log and drop it. If this branch ever regresses to `unwrap()`, the
    // oversized case below panics and this test fails.
    #[tokio::test]
    async fn send_ping_reports_an_oversized_payload_instead_of_panicking() {
        use crate::link::encoding::PAYLOAD_ENTRY_HEADER_SIZE;
        use crate::link::payload::HOST_TIME_SIZE;

        let socket =
            Arc::new(new_udp_reuseport(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into()).unwrap());
        // A real receiver, so the "well-formed payload still sends" half of
        // this test cannot be satisfied by an unconditional early return.
        let receiver = new_udp_reuseport(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let to = match receiver.local_addr().unwrap() {
            SocketAddr::V4(addr) => addr,
            other => panic!("expected an IPv4 receiver address, got {}", other),
        };

        let entry_size = PAYLOAD_ENTRY_HEADER_SIZE + HOST_TIME_SIZE as usize;
        let mut oversized = Payload::default();
        for _ in 0..(MAX_MESSAGE_SIZE / entry_size + 1) {
            oversized
                .entries
                .push(PayloadEntry::HostTime(HostTime::default()));
        }

        let err = send_ping(socket.clone(), to, &oversized)
            .await
            .expect_err("an oversized payload must be reported, not sent");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        // Guard against the inverse regression: an unconditional early error
        // would also satisfy the assertion above, so prove a well-formed
        // payload actually reaches the wire.
        let small = Payload {
            entries: vec![PayloadEntry::HostTime(HostTime::default())],
        };
        let sent = send_ping(socket, to, &small).await.unwrap();
        assert!(sent > 0);

        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("a well-formed ping should have been sent")
        .expect("receiving the well-formed ping should succeed")
        .0;
        assert_eq!(received, sent);
        // Exactly one datagram: the oversized message must not have been sent.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                receiver.recv_from(&mut buf),
            )
            .await
            .is_err(),
            "the oversized message must not be transmitted"
        );
    }
}
