use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use tokio::{
    net::UdpSocket,
    select,
    sync::{mpsc::Sender, Notify},
    time::Instant,
};
use tracing::{debug, info, warn};

use crate::{
    discovery::{messages::MESSAGE_TYPES, peers::PeerStateMessageType},
    link::{
        node::{NodeId, NodeState},
        payload::{Payload, PayloadEntry},
    },
};

use super::{
    gateway::OnEvent,
    ingress::{set_membership, PacketSocket},
    messages::{
        encode_message, parse_message_header, parse_payload, MessageHeader, MessageType,
        SessionGroupId, ALIVE, BYEBYE, MAX_MESSAGE_SIZE, RESPONSE,
    },
    peers::PeerState,
    topology::Topology,
    LINK_PORT, MULTICAST_ADDR, MULTICAST_IP_ANY,
};
use crate::platform::network::{scan_discovery_interfaces, Ipv4Interface};

// Safe UDP socket creation using socket2 and safe options
pub fn new_udp_reuseport(addr: SocketAddr) -> Result<UdpSocket, std::io::Error> {
    new_std_udp_reuseport(addr)?.try_into()
}

fn new_std_udp_reuseport(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };

    let udp_sock = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;

    // Preserve sharing for explicit ports, including the discovery listener.
    // Ephemeral measurement and reply sockets must not opt into port sharing.
    if addr.port() != 0 {
        udp_sock.set_reuse_address(true)?;

        // Set SO_REUSEPORT on Unix systems so multiple sockets (discovery
        // listener, send_byebye, etc.) can bind to the same multicast port
        // concurrently.
        #[cfg(unix)]
        udp_sock.set_reuse_port(true)?;
    }

    // On Linux, a socket bound to a port receives datagrams for *any* multicast
    // group joined by any socket on the host, including groups this socket never
    // joined itself. The socket this matters for is the discovery listener, which
    // binds the Link port wildcard and with `SO_REUSEADDR`/`SO_REUSEPORT` shares it
    // with anything else on that port - including a program that binds it and joins
    // a multicast group of its own. That program's traffic lands in this port's
    // receive path to be parsed and discarded.
    //
    // Other Link instances are not that case: they join the same discovery group,
    // so their packets are addressed to a membership this listener holds and remain
    // deliverable. Only traffic for groups this socket never joined is excluded.
    //
    // It is deliberately *not* about the per-interface sockets created below: those
    // bind an ephemeral port, so port demultiplexing already keeps discovery
    // multicast away from them regardless of this option.
    //
    // `IP_MULTICAST_ALL=0` (upstream `c5574eee4d03`) narrows delivery to this
    // socket's own memberships. That is a filter on *group*, not on interface: the
    // listener joins the discovery group on every interface, so Link traffic
    // arriving on any of them still matches, and the option reports nothing about
    // which interface a datagram came in on. Messenger uses PacketSocket's
    // ancillary metadata for that separate responsibility.
    //
    // The option is IPv4-only, so it is applied only to IPv4 sockets.
    #[cfg(target_os = "linux")]
    if addr.is_ipv4() {
        udp_sock.set_multicast_all_v4(false)?;
    }

    // When binding to a concrete interface address, make sure outgoing multicast
    // traffic leaves through that very interface.
    if let SocketAddr::V4(addr) = addr {
        if !addr.ip().is_unspecified() {
            udp_sock.set_multicast_if_v4(addr.ip())?;
        }
    }

    udp_sock.set_nonblocking(true)?;
    udp_sock.bind(&socket2::SockAddr::from(addr))?;

    Ok(udp_sock.into())
}

/// How often the set of usable network interfaces is re-scanned.
const INTERFACE_SCAN_PERIOD: Duration = Duration::from_secs(5);

/// Cancellation handle for a per-interface receive loop.
#[derive(Clone, Default)]
struct Cancel {
    notify: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
}

impl Cancel {
    async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Default)]
struct LocalIdentity {
    loopback_indices: HashSet<u32>,
    addresses: HashSet<IpAddr>,
    subnets: HashMap<(u32, Ipv4Addr), Ipv4Addr>,
}

#[derive(Default)]
struct LocalDelivery(Mutex<LocalIdentity>);

impl LocalDelivery {
    fn identity() -> LocalIdentity {
        let Ok(interfaces) = if_addrs::get_if_addrs() else {
            return LocalIdentity::default();
        };
        LocalIdentity {
            loopback_indices: interfaces
                .iter()
                .filter(|i| i.is_loopback())
                .filter_map(|i| i.index)
                .collect(),
            addresses: interfaces.iter().map(|i| i.ip()).collect(),
            subnets: interfaces
                .iter()
                .filter_map(|interface| match &interface.addr {
                    if_addrs::IfAddr::V4(address) => {
                        Some(((interface.index?, address.ip), address.netmask))
                    }
                    _ => None,
                })
                .collect(),
        }
    }
    #[cfg(test)]
    fn snapshot() -> Self {
        Self(Mutex::new(Self::identity()))
    }
    fn refresh(&self, identity: &LocalIdentity) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = identity.clone();
    }
    fn same_subnet(&self, index: u32, address: Ipv4Addr, source: IpAddr) -> bool {
        let IpAddr::V4(source) = source else {
            return false;
        };
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subnets
            .get(&(index, address))
            .is_some_and(|mask| {
                u32::from(address) & u32::from(*mask) == u32::from(source) & u32::from(*mask)
            })
    }
    fn accepts(&self, index: u64, source: IpAddr) -> bool {
        let identity = self.0.lock().unwrap_or_else(|e| e.into_inner());
        u32::try_from(index)
            .ok()
            .is_some_and(|i| identity.loopback_indices.contains(&i))
            && identity.addresses.contains(&source)
    }
}

/// A send/receive socket bound to a single network interface.
#[derive(Clone)]
pub struct InterfaceSocket {
    socket: Arc<UdpSocket>,
    receiver: Arc<PacketSocket>,
    identity: Ipv4Interface,
    cancel: Cancel,
    topology: Option<Arc<Topology>>,
    generation: u64,
    measurement_responder: bool,
    measurement_dispatch: crate::link::measurement::PongDispatch,
    local_delivery: Arc<LocalDelivery>,
    // Preserve the public address-keyed map while retaining every adapter with
    // that address. Only map roots contain siblings; captured leases do not.
    siblings: Vec<InterfaceSocket>,
}

impl InterfaceSocket {
    fn registrations(&self) -> impl Iterator<Item = &Self> {
        std::iter::once(self).chain(self.siblings.iter())
    }

    fn lease(&self) -> Self {
        Self {
            socket: self.socket.clone(),
            receiver: self.receiver.clone(),
            identity: self.identity.clone(),
            cancel: self.cancel.clone(),
            topology: self.topology.clone(),
            generation: self.generation,
            measurement_responder: self.measurement_responder,
            measurement_dispatch: self.measurement_dispatch.clone(),
            local_delivery: self.local_delivery.clone(),
            siblings: Vec::new(),
        }
    }

    fn is_current(&self) -> std::io::Result<bool> {
        Ok(!self.cancel.is_cancelled()
            && match &self.topology {
                Some(topology) => topology.check()? == self.generation,
                None => true,
            })
    }

    async fn invalidated(&self) {
        select! {
            _ = self.cancel.cancelled() => {}
            result = async {
                match &self.topology {
                    Some(topology) => topology.changed(self.generation).await,
                    None => std::future::pending().await,
                }
            } => {
                if let Err(error) = result { warn!("discovery topology check failed: {}", error); }
            }
        }
    }
}

/// Address-keyed groups of per-interface sockets. A value privately retains all
/// adapters sharing its address; managed broadcasts visit every registration.
pub type InterfaceSockets = Arc<Mutex<HashMap<Ipv4Addr, InterfaceSocket>>>;

#[derive(Clone)]
pub(crate) struct MeasurementRoute {
    #[cfg(test)]
    retire_before_samples: Arc<AtomicBool>,
    entry: InterfaceSocket,
    sockets: InterfaceSockets,
}

impl std::fmt::Debug for MeasurementRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeasurementRoute")
            .field("interface", &self.entry.identity)
            .finish()
    }
}

impl MeasurementRoute {
    #[cfg(test)]
    pub(crate) fn test_fixture() -> (Self, impl FnOnce()) {
        let entry = tests::interface_socket();
        let route = Self {
            retire_before_samples: Default::default(),
            entry: entry.lease(),
            sockets: Arc::new(Mutex::new(HashMap::from([(
                entry.identity.addr,
                entry.lease(),
            )]))),
        };
        (route, move || entry.cancel.cancel())
    }
    #[cfg(test)]
    pub(crate) fn retire_at_sample_publication(&self) {
        self.retire_before_samples.store(true, Ordering::SeqCst);
    }
    #[cfg(test)]
    pub(crate) fn before_sample_publication(&self) {
        if self.retire_before_samples.swap(false, Ordering::SeqCst) {
            self.entry.cancel.cancel();
        }
    }
    pub(crate) fn socket(&self) -> Arc<UdpSocket> {
        self.entry.socket.clone()
    }
    pub(crate) fn dispatch(&self) -> crate::link::measurement::PongDispatch {
        self.entry.measurement_dispatch.clone()
    }
    pub(crate) async fn invalidated(&self) {
        self.entry.invalidated().await;
    }
    pub(crate) fn while_current<T>(&self, publish: impl FnOnce() -> T) -> Option<T> {
        let sockets = self.sockets.lock().ok()?;
        let registered = sockets.get(&self.entry.identity.addr).is_some_and(|group| {
            group
                .registrations()
                .any(|entry| Arc::ptr_eq(&entry.socket, &self.entry.socket))
        });
        if registered && self.entry.is_current().unwrap_or(false) {
            Some(publish())
        } else {
            None
        }
    }
    pub(crate) async fn send(&self, bytes: &[u8], to: SocketAddrV4) -> std::io::Result<usize> {
        send_registered(&self.entry, &self.sockets, bytes, to).await?;
        Ok(bytes.len())
    }
}

const MAX_MEASUREMENT_ROUTES: usize = 1024;
const MAX_ROUTES_PER_PEER: usize = 16;

struct RouteHint {
    expires: Instant,
    observed: Instant,
    route: MeasurementRoute,
}
type MeasurementRouteMap = HashMap<(NodeId, SocketAddrV4), RouteHint>;
type TopologySnapshot = HashMap<usize, Option<u64>>;

fn current_in_snapshot(entry: &InterfaceSocket, generations: &mut TopologySnapshot) -> bool {
    !entry.cancel.is_cancelled()
        && entry.topology.as_ref().is_none_or(|topology| {
            *generations
                .entry(Arc::as_ptr(topology) as usize)
                .or_insert_with(|| topology.check().ok())
                == Some(entry.generation)
        })
}

struct EventRevision {
    received: Instant,
    cancel: Cancel,
}
type PendingRevisions = HashMap<NodeId, Weak<EventRevision>>;

#[derive(Clone, Default)]
pub(crate) struct MeasurementRoutes(
    Arc<Mutex<MeasurementRouteMap>>,
    Arc<Mutex<PendingRevisions>>,
);

impl MeasurementRoutes {
    #[cfg(test)]
    pub(crate) fn test_record(&self, peer: &PeerState, route: MeasurementRoute) {
        self.record(peer.ident(), peer.measurement_endpoint, 30, route);
    }

    fn observe(
        &self,
        peer: NodeId,
        _endpoint: Option<SocketAddrV4>,
        received: Instant,
        admitted: bool,
    ) -> Option<Arc<EventRevision>> {
        let mut pending = self.1.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, revision| revision.strong_count() > 0);
        if pending
            .get(&peer)
            .and_then(Weak::upgrade)
            .is_some_and(|revision| revision.received > received)
        {
            return None;
        }
        if let Some(revision) = pending
            .remove(&peer)
            .and_then(|revision| revision.upgrade())
        {
            revision.cancel.cancel();
        }
        // Only bounded queued work owns revisions. Dropped traffic never allocates
        // a tombstone; a cancelled item's own token survives cache hint removal.
        if !admitted {
            return None;
        }
        let revision = Arc::new(EventRevision {
            received,
            cancel: Cancel::default(),
        });
        pending.insert(peer, Arc::downgrade(&revision));
        Some(revision)
    }

    fn sweep(routes: &mut MeasurementRouteMap, now: Instant) -> TopologySnapshot {
        let mut generations = HashMap::new();
        routes.retain(|_, hint| {
            hint.expires > now && current_in_snapshot(&hint.route.entry, &mut generations)
        });
        generations
    }
    fn prune(&self) {
        Self::sweep(
            &mut self.0.lock().unwrap_or_else(|e| e.into_inner()),
            Instant::now(),
        );
    }
    fn record(
        &self,
        peer: NodeId,
        endpoint: Option<SocketAddrV4>,
        ttl: u8,
        route: MeasurementRoute,
    ) {
        let now = Instant::now();
        self.record_received(
            peer,
            endpoint,
            now,
            now + Duration::from_secs(u64::from(ttl)),
            route,
            false,
        );
    }
    fn record_received(
        &self,
        peer: NodeId,
        endpoint: Option<SocketAddrV4>,
        observed: Instant,
        expires: Instant,
        route: MeasurementRoute,
        existing_only: bool,
    ) {
        let now = Instant::now();
        let mut routes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut generations = Self::sweep(&mut routes, now);
        if !current_in_snapshot(&route.entry, &mut generations) {
            return;
        }
        if let Some(endpoint) = endpoint {
            let key = (peer, endpoint);
            if routes
                .get(&key)
                .is_some_and(|hint| hint.observed > observed)
            {
                return;
            }
            if expires <= now {
                routes.remove(&key);
                return;
            }
            if existing_only && !routes.contains_key(&key) {
                return;
            }
            if !routes.contains_key(&key) {
                if routes.keys().filter(|(node, _)| *node == peer).count() >= MAX_ROUTES_PER_PEER {
                    if let Some(old) = routes
                        .iter()
                        .filter(|((node, _), _)| *node == peer)
                        .min_by_key(|(_, hint)| hint.expires)
                        .map(|(key, _)| *key)
                    {
                        routes.remove(&old);
                    }
                }
                if routes.len() >= MAX_MEASUREMENT_ROUTES {
                    if let Some(old) = routes
                        .iter()
                        .min_by_key(|(_, hint)| hint.expires)
                        .map(|(key, _)| *key)
                    {
                        routes.remove(&old);
                    }
                }
            }
            routes.insert(
                key,
                RouteHint {
                    expires,
                    observed,
                    route,
                },
            );
        } else {
            routes.retain(|(node, _), hint| *node != peer || hint.observed > observed);
        }
    }
    pub(crate) fn resolve(&self, peer: &PeerState) -> Option<MeasurementRoute> {
        let endpoint = peer.measurement_endpoint?;
        let mut routes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Self::sweep(&mut routes, Instant::now());
        routes
            .get(&(peer.ident(), endpoint))
            .map(|hint| hint.route.clone())
    }
    fn remove_peer_before(&self, node: NodeId, received: Instant) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(peer, _), hint| *peer != node || hint.observed > received);
    }
}

/// Opaque ownership of a managed observation. Keep this value when forwarding
/// a peer message: it preserves node ordering and its original ingress lease.
#[derive(Clone)]
pub struct PeerObservation {
    revision: Arc<EventRevision>,
    ingress: MeasurementRoute,
    routes: MeasurementRoutes,
}

impl PeerObservation {
    pub(crate) async fn invalidated(observation: Option<&Self>) {
        if let Some(observation) = observation {
            select! {
                _ = observation.revision.cancel.cancelled() => {},
                _ = observation.ingress.invalidated() => {},
            }
        } else {
            std::future::pending::<()>().await;
        }
    }

    pub(crate) fn while_current<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        self.ingress
            .while_current(|| {
                // Same order as receive/admission: interface map, then revision index.
                let _pending = self.routes.1.lock().unwrap_or_else(|e| e.into_inner());
                (!self.revision.cancel.is_cancelled()).then(commit)
            })
            .flatten()
    }
}

pub struct Messenger {
    multicast_receiver: Arc<PacketSocket>,
    topology: Arc<Topology>,
    pub(crate) gate: Arc<crate::link::controller::DispatchGate>,
    pub interface: Option<Arc<UdpSocket>>,
    /// One ephemeral socket per usable interface, used to send discovery messages
    /// and to listen for the unicast responses they trigger.
    pub interface_sockets: InterfaceSockets,
    pub peer_state: Arc<Mutex<PeerState>>,
    pub ttl: u8,
    pub ttl_ratio: u8,
    pub last_broadcast_time: Arc<Mutex<Instant>>,
    pub tx_event: Sender<OnEvent>,
    pub notifier: Arc<Notify>,
    pub enabled: Arc<Mutex<bool>>,
    pub group_id: SessionGroupId,
    /// Counts how many times the set of per-interface gateways has changed,
    /// i.e. an interface was added or removed. Mirrors upstream's
    /// `GatewayFactory::gatewaysChanged()` notification
    /// (`PeerGateways::enable` and the periodic interface scan).
    ///
    /// Crate-internal: upstream's notification is consumed inside the library
    /// (`SessionController::gatewaysChangedCallback`), never by an embedder, so
    /// exposing this would add public API with no upstream counterpart.
    pub(crate) gateways_changed: Arc<AtomicUsize>,
    measurement_responder: Option<crate::link::pingresponder::PingResponder>,
    pub(crate) measurement_routes: MeasurementRoutes,
}

impl Messenger {
    pub(crate) fn set_measurement_responder(
        &mut self,
        responder: crate::link::pingresponder::PingResponder,
    ) {
        self.measurement_responder = Some(responder);
        let mut sockets = self
            .interface_sockets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for group in sockets.values_mut() {
            group.measurement_responder = true;
            for sibling in &mut group.siblings {
                sibling.measurement_responder = true;
            }
        }
    }

    pub(crate) fn discard_queued_datagrams(&self) {
        self.measurement_routes
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        if let Some(socket) = &self.interface {
            drain_socket(socket);
        }
        for entry in interface_socket_entries(&self.interface_sockets) {
            drain_socket(&entry.socket);
        }
    }

    #[cfg(test)]
    pub(crate) async fn receive_peer_state_fixture(&self, local: Ipv4Addr, bytes: &[u8]) {
        // Lifecycle tests inject at the accepted-datagram boundary; real socket
        // metadata and wire ingress are exercised separately in this module.
        let entry = interface_socket_entries(&self.interface_sockets)
            .into_iter()
            .find(|entry| entry.identity.addr == local)
            .expect("registered fixture ingress");
        assert!(entry.is_current().unwrap());
        let (header, length) = parse_message_header(bytes).unwrap();
        assert_eq!(header.message_type, RESPONSE);
        receive_peer_state_with_route(
            self.tx_event.clone(),
            header,
            &bytes[length..],
            Some((
                &self.measurement_routes,
                MeasurementRoute {
                    #[cfg(test)]
                    retire_before_samples: Default::default(),
                    entry,
                    sockets: self.interface_sockets.clone(),
                },
            )),
        )
        .await;
    }

    #[cfg(test)]
    pub(crate) fn socket_probes(&self) -> Vec<std::sync::Weak<UdpSocket>> {
        let mut sockets = vec![Arc::downgrade(self.interface.as_ref().unwrap())];
        sockets.extend(
            interface_socket_entries(&self.interface_sockets)
                .iter()
                .map(|entry| Arc::downgrade(&entry.socket)),
        );
        sockets
    }

    pub fn new(
        peer_state: Arc<Mutex<PeerState>>,
        tx_event: Sender<OnEvent>,
        epoch: Instant,
        notifier: Arc<Notify>,
        enabled: Arc<Mutex<bool>>,
    ) -> Result<Self, std::io::Error> {
        Self::new_with_interfaces(peer_state, tx_event, epoch, notifier, enabled, None)
    }

    fn new_with_interfaces(
        peer_state: Arc<Mutex<PeerState>>,
        tx_event: Sender<OnEvent>,
        epoch: Instant,
        notifier: Arc<Notify>,
        enabled: Arc<Mutex<bool>>,
        interfaces: Option<std::io::Result<Vec<Ipv4Interface>>>,
    ) -> Result<Self, std::io::Error> {
        // Subscribe before enumerating; otherwise a remove/add between the
        // snapshot and subscription would never invalidate its registrations.
        let topology = Arc::new(Topology::new()?);
        let generation = topology.check()?;
        let interfaces = interfaces.unwrap_or_else(scan_discovery_interfaces);
        // Bind the multicast listener on LINK_PORT. With SO_REUSEADDR/SO_REUSEPORT this
        // should coexist with other Ableton Link instances on the same host, but the
        // bind can still fail (e.g. another process holding the port without the
        // reuse flags, or the OS otherwise rejecting the bind). Propagate the error
        // instead of panicking so callers can decide how to handle it.
        let multicast_receiver =
            Arc::new(PacketSocket::new(MULTICAST_IP_ANY, None).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "failed to bind Ableton Link multicast socket on {}: {}",
                        SocketAddr::from(MULTICAST_IP_ANY),
                        e
                    ),
                )
            })?);
        let socket = multicast_receiver.socket.clone();
        socket.set_multicast_loop_v4(true)?;

        let interface_sockets: InterfaceSockets = Arc::new(Mutex::new(HashMap::new()));
        let gateways_changed = Arc::new(AtomicUsize::new(0));

        let initial_interfaces = match interfaces {
            Ok(interfaces) => interfaces,
            Err(error) => {
                warn!(
                    "initial discovery interface scan failed; periodic scan will retry: {}",
                    error
                );
                Vec::new()
            }
        };
        let local_identity = LocalDelivery::identity();
        for interface in initial_interfaces {
            match add_interface_with_identity(
                &socket,
                &interface_sockets,
                interface.clone(),
                Some(topology.clone()),
                generation,
                false,
                &local_identity,
            ) {
                Ok(_) => info!(
                    "joined Ableton Link multicast group on interface {}",
                    interface.addr
                ),
                Err(e) => warn!("failed to set up interface {:?}: {}", interface, e),
            }
        }

        if lock_map(&interface_sockets, |sockets| sockets.is_empty()).unwrap_or(true) {
            warn!("no identified discovery interface available; waiting for an interface scan");
        }

        // Mirrors upstream's `GatewayFactory::gatewaysChanged()` notification, which
        // fires once from `PeerGateways::enable` (unconditionally) and once more from
        // the initial interface scan if it found any gateways. Since construction and
        // enabling are not separate steps here, count the initial population as a
        // single change.
        gateways_changed.fetch_add(1, Ordering::Relaxed);

        Ok(Messenger {
            multicast_receiver,
            topology,
            measurement_responder: None,
            measurement_routes: MeasurementRoutes::default(),
            interface: Some(socket),
            interface_sockets,
            peer_state,
            ttl: 2, // Reduced from 5 to 2 seconds for faster peer timeout detection
            ttl_ratio: 20,
            last_broadcast_time: Arc::new(Mutex::new(epoch)),
            tx_event,
            notifier,
            enabled,
            group_id: 0,
            gateways_changed,
            gate: Arc::new(crate::link::controller::DispatchGate::new_open()),
        })
    }

    pub async fn listen(&self) {
        select! {
            biased;
            _ = self.notifier.notified() => {}
            _ = self.listen_owned() => {}
        }
    }

    fn receive_context(&self) -> ReceiveContext {
        ReceiveContext {
            measurement_responder: self.measurement_responder.clone(),
            measurement_routes: self.measurement_routes.clone(),
            topology: Some(self.topology.clone()),
            interface_sockets: self.interface_sockets.clone(),
            peer_state: self.peer_state.clone(),
            ttl: self.ttl,
            tx_event: self.tx_event.clone(),
            last_broadcast_time: self.last_broadcast_time.clone(),
            enabled: self.enabled.clone(),
            group_id: self.group_id,
            gateways_changed: self.gateways_changed.clone(),
            gate: self.gate.clone(),
            #[cfg(test)]
            fail_receive_once: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            pending_delivery: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            drain_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn listen_owned(&self) -> impl std::future::Future<Output = ()> + '_ {
        let multicast_socket = self.multicast_receiver.clone();
        let interface_sockets = self.interface_sockets.clone();
        let context = self.receive_context();

        // The shared multicast socket receives the multicast traffic of every
        // interface it joined.
        let mut children = tokio::task::JoinSet::new();
        children.spawn(receive_loop(
            multicast_socket.clone(),
            None,
            context.clone(),
        ));

        // Each per-interface socket receives the unicast responses triggered by the
        // messages sent through it.
        for entry in interface_socket_entries(&interface_sockets) {
            children.spawn(receive_loop(
                entry.receiver.clone(),
                Some(entry.cancel.clone()),
                context.clone(),
            ));
        }

        children.spawn(interface_scan(multicast_socket.socket.clone(), context));

        let broadcast = broadcast_state_loop(
            self.ttl,
            self.ttl_ratio,
            self.last_broadcast_time.clone(),
            interface_sockets,
            self.peer_state.clone(),
            SocketAddrV4::new(MULTICAST_ADDR, LINK_PORT),
            self.notifier.clone(),
            self.enabled.clone(),
            self.group_id,
            false,
        );
        async move {
            tokio::pin!(broadcast);
            loop {
                select! {
                    _ = &mut broadcast => break,
                    result = children.join_next(), if !children.is_empty() => {
                        if let Some(Err(error)) = result {
                            warn!("discovery worker failed: {}", error);
                        }
                    }
                }
            }
            children.shutdown().await;
        }
    }
}

#[derive(Clone)]
struct ReceiveContext {
    measurement_routes: MeasurementRoutes,
    measurement_responder: Option<crate::link::pingresponder::PingResponder>,
    topology: Option<Arc<Topology>>,
    #[cfg(test)]
    drain_count: Arc<AtomicUsize>,
    #[cfg(test)]
    fail_receive_once: Arc<AtomicBool>,
    #[cfg(test)]
    pending_delivery: Arc<AtomicUsize>,
    gate: Arc<crate::link::controller::DispatchGate>,
    interface_sockets: InterfaceSockets,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    tx_event: Sender<OnEvent>,
    last_broadcast_time: Arc<Mutex<Instant>>,
    enabled: Arc<Mutex<bool>>,
    group_id: SessionGroupId,
    gateways_changed: Arc<AtomicUsize>,
}

fn lock_map<T>(
    sockets: &InterfaceSockets,
    f: impl FnOnce(&HashMap<Ipv4Addr, InterfaceSocket>) -> T,
) -> Option<T> {
    match sockets.lock() {
        Ok(guard) => Some(f(&guard)),
        Err(_) => None,
    }
}

fn interface_socket_entries(sockets: &InterfaceSockets) -> Vec<InterfaceSocket> {
    lock_map(sockets, |sockets| {
        sockets
            .values()
            .flat_map(InterfaceSocket::registrations)
            .map(InterfaceSocket::lease)
            .collect()
    })
    .unwrap_or_default()
}

/// Join the multicast group on `addr` and create the ephemeral socket used to send
/// discovery messages through that interface.
fn add_interface_with_identity(
    multicast_socket: &Arc<UdpSocket>,
    interface_sockets: &InterfaceSockets,
    identity: Ipv4Interface,
    topology: Option<Arc<Topology>>,
    generation: u64,
    measurement_responder: bool,
    local_identity: &LocalIdentity,
) -> Result<InterfaceSocket, std::io::Error> {
    let addr = identity.addr;
    let receiver = Arc::new(PacketSocket::new(
        SocketAddrV4::new(addr, 0),
        Some(identity.index),
    )?);
    let entry = InterfaceSocket {
        socket: receiver.socket.clone(),
        receiver,
        identity,
        cancel: Cancel::default(),
        topology,
        generation,
        measurement_responder,
        measurement_dispatch: Arc::new(Mutex::new(HashMap::new())),
        local_delivery: Arc::new(LocalDelivery(Mutex::new(local_identity.clone()))),
        siblings: Vec::new(),
    };

    match interface_sockets.lock() {
        Ok(mut sockets) => {
            if sockets
                .values()
                .flat_map(InterfaceSocket::registrations)
                .any(|other| other.identity == entry.identity)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "interface is already registered",
                ));
            }
            // Serialize queue draining, membership publication, receive/route
            // capture and sends. Old queued metadata must not acquire a newly
            // registered socket generation after a scan.
            if !entry.is_current()? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "interface topology changed during registration",
                ));
            }
            if !sockets
                .values()
                .flat_map(InterfaceSocket::registrations)
                .any(|other| other.identity.index == entry.identity.index)
            {
                set_membership(multicast_socket, entry.identity.index, true)?;
            }
            drain_socket(multicast_socket);
            match sockets.get_mut(&addr) {
                Some(group) => group.siblings.push(entry.lease()),
                None => {
                    sockets.insert(addr, entry.lease());
                }
            }
        }
        Err(_) => {
            return Err(std::io::Error::other("interface socket map is poisoned"));
        }
    }

    Ok(entry)
}

fn remove_interface(
    multicast_socket: &Arc<UdpSocket>,
    interface_sockets: &InterfaceSockets,
    identity: &Ipv4Interface,
) {
    match interface_sockets.lock() {
        Ok(mut sockets) => {
            let entry = if let Some(mut group) = sockets.remove(&identity.addr) {
                if group.identity == *identity {
                    if let Some(mut replacement) = group.siblings.pop() {
                        replacement.siblings = std::mem::take(&mut group.siblings);
                        sockets.insert(identity.addr, replacement);
                    }
                    Some(group)
                } else {
                    let removed = group
                        .siblings
                        .iter()
                        .position(|entry| entry.identity == *identity)
                        .map(|index| group.siblings.remove(index));
                    sockets.insert(identity.addr, group);
                    removed
                }
            } else {
                None
            };
            if let Some(entry) = entry {
                entry.cancel.cancel();
                if !sockets
                    .values()
                    .flat_map(InterfaceSocket::registrations)
                    .any(|other| other.identity.index == identity.index)
                {
                    if let Err(error) = set_membership(multicast_socket, identity.index, false) {
                        warn!(
                            "failed to leave discovery membership on {}: {}",
                            identity.addr, error
                        );
                    }
                }
                drain_socket(multicast_socket);
                info!(
                    "left Ableton Link multicast group on interface {}",
                    identity.addr
                );
            }
        }
        Err(_) => warn!("cannot remove discovery interface: socket map is poisoned"),
    }
}

/// Keep the per-interface sockets in sync with the interfaces of the host.
/// Interfaces whose setup failed, with the error. The scan retries a failed
/// interface every [`INTERFACE_SCAN_PERIOD`]; this keeps a persistent failure
/// to one warning (then debug) until the error changes or the interface is
/// set up or goes away.
#[derive(Default)]
struct SetupFailures(HashMap<Ipv4Interface, String>);

impl SetupFailures {
    fn failed(&mut self, interface: &Ipv4Interface, error: &std::io::Error) {
        let message = error.to_string();
        if self.0.get(interface) == Some(&message) {
            debug!("still cannot set up interface {:?}: {}", interface, error);
        } else {
            warn!("failed to set up interface {:?}: {}", interface, error);
            self.0.insert(interface.clone(), message);
        }
    }

    fn forget_except(&mut self, current: &[Ipv4Interface]) {
        self.0.retain(|interface, _| current.contains(interface));
    }
}

async fn scan_interface_snapshot(
    scan: impl FnOnce() -> std::io::Result<(Vec<Ipv4Interface>, LocalIdentity)> + Send + 'static,
) -> Result<std::io::Result<(Vec<Ipv4Interface>, LocalIdentity)>, tokio::task::JoinError> {
    tokio::task::spawn_blocking(scan).await
}

async fn interface_scan(multicast_socket: Arc<UdpSocket>, context: ReceiveContext) {
    let mut children = tokio::task::JoinSet::new();
    let mut failures = SetupFailures::default();
    let mut interval = tokio::time::interval(INTERFACE_SCAN_PERIOD);
    let mut observed = interface_socket_entries(&context.interface_sockets)
        .first()
        .map_or(0, |entry| entry.generation);

    loop {
        select! {
            _ = interval.tick() => {}
            result = async {
                match &context.topology {
                    Some(topology) => topology.changed(observed).await,
                    None => std::future::pending().await,
                }
            } => {
                if let Err(error) = result {
                    warn!("discovery topology monitor failed: {}", error);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            result = children.join_next(), if !children.is_empty() => {
                if let Some(Err(error)) = result {
                    warn!("interface receive worker failed: {}", error);
                }
                continue;
            }
        }

        context.measurement_routes.prune();
        let generation = match &context.topology {
            Some(topology) => match topology.check() {
                Ok(generation) => generation,
                Err(error) => {
                    warn!("discovery topology stream unavailable: {}", error);
                    continue;
                }
            },
            None => 0,
        };
        let (current, local_identity) = match scan_interface_snapshot(|| {
            Ok((scan_discovery_interfaces()?, LocalDelivery::identity()))
        })
        .await
        {
            Ok(Ok(current)) => current,
            Ok(Err(error)) => {
                warn!("discovery interface scan failed: {}", error);
                observed = generation;
                continue;
            }
            Err(error) => {
                warn!("discovery interface scan task failed: {}", error);
                observed = generation;
                continue;
            }
        };

        // A snapshot straddling an event is not evidence for the new generation.
        if let Some(topology) = &context.topology {
            match topology.check() {
                Ok(after) if after == generation => {}
                Ok(_) => continue,
                Err(error) => {
                    warn!(
                        "topology changed or failed during interface scan: {}",
                        error
                    );
                    continue;
                }
            }
        }
        reconcile_interfaces_at(
            &multicast_socket,
            &context,
            &mut children,
            &current,
            &local_identity,
            generation,
            &mut failures,
        );
        observed = generation;
    }
}

#[cfg(test)]
fn reconcile_interfaces(
    multicast_socket: &Arc<UdpSocket>,
    context: &ReceiveContext,
    children: &mut tokio::task::JoinSet<()>,
    current: &[Ipv4Interface],
) {
    let generation = match &context.topology {
        Some(topology) => match topology.check() {
            Ok(generation) => generation,
            Err(error) => {
                warn!("cannot reconcile discovery topology: {}", error);
                return;
            }
        },
        None => 0,
    };
    reconcile_interfaces_at(
        multicast_socket,
        context,
        children,
        current,
        &LocalDelivery::identity(),
        generation,
        &mut SetupFailures::default(),
    );
}

fn reconcile_interfaces_at(
    multicast_socket: &Arc<UdpSocket>,
    context: &ReceiveContext,
    children: &mut tokio::task::JoinSet<()>,
    current: &[Ipv4Interface],
    local_identity: &LocalIdentity,
    generation: u64,
    failures: &mut SetupFailures,
) {
    failures.forget_except(current);
    for entry in interface_socket_entries(&context.interface_sockets) {
        entry.local_delivery.refresh(local_identity);
    }

    let entries = interface_socket_entries(&context.interface_sockets);
    let known: Vec<_> = entries.iter().map(|entry| entry.identity.clone()).collect();

    let stale_addrs: Vec<_> = known
        .iter()
        .zip(&entries)
        .filter(|(addr, entry)| !current.contains(addr) || entry.generation != generation)
        .map(|(addr, _)| addr.clone())
        .collect();
    let new_addrs: Vec<_> = current
        .iter()
        .filter(|addr| !known.contains(addr) || stale_addrs.contains(addr))
        .cloned()
        .collect();

    for addr in &stale_addrs {
        remove_interface(multicast_socket, &context.interface_sockets, addr);
    }

    let mut changed = !stale_addrs.is_empty();
    for addr in &new_addrs {
        match add_interface_with_identity(
            multicast_socket,
            &context.interface_sockets,
            addr.clone(),
            context.topology.clone(),
            generation,
            context.measurement_responder.is_some(),
            local_identity,
        ) {
            Ok(entry) => {
                changed = true;
                failures.0.remove(addr);
                info!(
                    "joined Ableton Link multicast group on interface {}",
                    addr.addr
                );
                children.spawn(receive_loop(
                    entry.receiver.clone(),
                    Some(entry.cancel.clone()),
                    context.clone(),
                ));
            }
            Err(e) => failures.failed(addr, &e),
        }
    }
    // Mirrors upstream's once-per-pass notification, including removal-only
    // scans, but not failed additions that left the registration set unchanged.
    if changed {
        context.gateways_changed.fetch_add(1, Ordering::Relaxed);
    }
}

fn socket_for_ingress(
    sockets: &HashMap<Ipv4Addr, InterfaceSocket>,
    index: u64,
    destination: IpAddr,
    source: IpAddr,
) -> Option<InterfaceSocket> {
    sockets
        .values()
        .flat_map(InterfaceSocket::registrations)
        .filter(|entry| u64::from(entry.identity.index) == index && !entry.cancel.is_cancelled())
        // With multiple addresses on one adapter, prefer the exact unicast
        // destination. Within the verified ingress adapter only, prefer the
        // same link-local scope as the peer; source IP never selects the adapter.
        .min_by_key(|entry| {
            (
                IpAddr::V4(entry.identity.addr) != destination,
                matches!(source, IpAddr::V4(ip) if ip.is_link_local())
                    != entry.identity.addr.is_link_local(),
                !entry.local_delivery.same_subnet(
                    entry.identity.index,
                    entry.identity.addr,
                    source,
                ),
                entry.identity.addr,
            )
        })
        .map(InterfaceSocket::lease)
}

#[cfg(test)]
async fn receive_datagram(
    receiver: &PacketSocket,
    sockets: &InterfaceSockets,
    buf: &mut [u8],
    owner: Option<&Cancel>,
) -> std::io::Result<(usize, SocketAddr, Option<InterfaceSocket>)> {
    receive_datagram_observing(receiver, sockets, buf, owner, |_, _| {}).await
}

async fn receive_datagram_observing(
    receiver: &PacketSocket,
    sockets: &InterfaceSockets,
    buf: &mut [u8],
    owner: Option<&Cancel>,
    mut observe: impl FnMut(&[u8], Option<&InterfaceSocket>),
) -> std::io::Result<(usize, SocketAddr, Option<InterfaceSocket>)> {
    loop {
        receiver.readable().await?;
        let result = {
            let sockets = sockets
                .lock()
                .map_err(|_| std::io::Error::other("interface socket map is poisoned"))?;
            if owner.is_some_and(Cancel::is_cancelled) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "receiving socket generation was removed",
                ));
            }
            receiver.try_recv(buf).and_then(|(size, info)| {
                let route = if owner.is_some()
                    && buf[..size].starts_with(&crate::link::pingresponder::PROTOCOL_HEADER)
                {
                    // A bound measurement port has one registered owner. Local
                    // delivery may report loopback rather than the bound adapter;
                    // allow only a verified loopback index AND assigned source.
                    // Discovery and foreign-adapter traffic remain metadata-scoped.
                    sockets
                        .values()
                        .flat_map(InterfaceSocket::registrations)
                        .find(|entry| {
                            std::ptr::eq(entry.receiver.as_ref(), receiver)
                                && info.addr_dst == IpAddr::V4(entry.identity.addr)
                                && (info.if_index == u64::from(entry.identity.index)
                                    || entry
                                        .local_delivery
                                        .accepts(info.if_index, info.addr_src.ip()))
                        })
                        .map(InterfaceSocket::lease)
                } else {
                    socket_for_ingress(&sockets, info.if_index, info.addr_dst, info.addr_src.ip())
                };
                let route = match route {
                    Some(entry) if entry.is_current()? => Some(entry),
                    _ => None,
                };
                observe(&buf[..size], route.as_ref());
                Ok((size, info.addr_src, route))
            })
        };
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            result => return result,
        }
    }
}

fn drain_socket(socket: &UdpSocket) {
    let mut buf = [0; MAX_MESSAGE_SIZE];
    loop {
        match socket.try_recv_from(&mut buf) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => {
                warn!("discovery socket drain failed: {}", error);
                break;
            }
        }
    }
}

const DISCOVERY_HANDOFF_CAPACITY: usize = 64;

struct DiscoveryEvent {
    event: OnEvent,
    ingress: MeasurementRoute,
    received: Instant,
    revision: Option<Arc<EventRevision>>,
}

async fn event_superseded(revision: Option<&Arc<EventRevision>>) {
    match revision {
        Some(revision) => revision.cancel.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn event_expired(event: &OnEvent) {
    match event {
        OnEvent::PeerState(state) if state.ttl > 0 && state.expires_at.is_some() => {
            tokio::time::sleep_until(state.expires_at.unwrap()).await
        }
        _ => std::future::pending().await,
    }
}

fn receive_loop(
    receive_socket: Arc<PacketSocket>,
    cancel: Option<Cancel>,
    context: ReceiveContext,
) -> impl std::future::Future<Output = ()> {
    let mut open = context.gate.subscribe();
    let mut delivery_open = context.gate.subscribe();
    async move {
        let (handoff, mut pending) =
            tokio::sync::mpsc::channel::<DiscoveryEvent>(DISCOVERY_HANDOFF_CAPACITY);
        let delivery_gate = context.gate.clone();
        let events = context.tx_event.clone();
        let routes = context.measurement_routes.clone();
        #[cfg(test)]
        let pending_delivery = context.pending_delivery.clone();
        let mut delivery_task = tokio::task::JoinSet::new();
        delivery_task.spawn(async move {
            while let Some((_permit, _, event)) = crate::link::controller::gated_recv(
                &delivery_gate,
                &mut delivery_open,
                &mut pending,
            )
            .await
            {
                #[cfg(test)]
                pending_delivery.fetch_add(1, Ordering::SeqCst);
                let slot = select! {
                    biased;
                    _ = delivery_open.closed() => continue,
                    _ = event.ingress.entry.invalidated() => continue,
                    _ = event_expired(&event.event) => continue,
                    _ = event_superseded(event.revision.as_ref()) => continue,
                    result = events.reserve() => match result { Ok(slot) => slot, Err(_) => break },
                };
                let OnEvent::PeerState(state) = event.event else {
                    continue;
                };
                let observation = state.observation.clone().expect("managed observation");
                observation.while_current(|| {
                    let expires = state.expires_at.expect("managed announcement deadline");
                    if state.ttl > 0 && expires <= Instant::now() {
                        return;
                    }
                    if state.ttl == 0 {
                        routes.remove_peer_before(state.node_state.ident(), event.received);
                    } else {
                        routes.record_received(
                            state.node_state.ident(),
                            state.measurement_endpoint,
                            event.received,
                            expires,
                            event.ingress.clone(),
                            false,
                        );
                    }
                    slot.send(OnEvent::PeerState(state));
                });
            }
        });
        loop {
            let mut buf = [0; MAX_MESSAGE_SIZE];
            let cancelled = async {
                match &cancel {
                    Some(cancel) => cancel.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(cancelled);
            select! {
            biased;
            _ = &mut cancelled => break,
            _ = open.wait_open(&context.gate, || {
                drain_socket(&receive_socket);
                #[cfg(test)]
                context.drain_count.fetch_add(1, Ordering::SeqCst);
            }, || receive_socket.readable()) => {}
            }
            let _permit = context.gate.permit().await;
            if !context.gate.is_open() {
                continue;
            }

            let received = select! {
                biased;
                _ = &mut cancelled => break,
                _ = open.closed() => continue,
                result = receive_datagram_observing(&receive_socket, &context.interface_sockets, &mut buf, cancel.as_ref(), |bytes, ingress| {
                    #[cfg(test)]
                    if context.fail_receive_once.load(Ordering::Relaxed) { return; }
                    observe_discovery(&context, &handoff, bytes, ingress);
                }) => result,
            };
            #[cfg(test)]
            let received = if context.fail_receive_once.swap(false, Ordering::Relaxed) {
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
            } else {
                received
            };

            let (amt, src, ingress) = match received {
                Ok(received) => received,
                Err(e) => {
                    warn!("discovery socket receive failed: {}", e);
                    // UDP errors can be transient. Keep the registered interface's
                    // receiver alive, but back off to avoid spinning on a bad socket.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            if ingress.is_none() {
                debug!(
                    "discarding discovery datagram without a registered ingress from {}",
                    src
                );
                continue;
            }

            if buf[..amt].starts_with(&crate::link::pingresponder::PROTOCOL_HEADER) {
                if let (Some(responder), Some(entry), SocketAddr::V4(src)) =
                    (&context.measurement_responder, &ingress, src)
                {
                    // Only the advertised, bound unicast endpoint serves PINGs.
                    // Never answer through the wildcard multicast listener or
                    // substitute another interface's socket after receipt.
                    if entry.measurement_responder && Arc::ptr_eq(&entry.receiver, &receive_socket)
                    {
                        if buf.get(crate::link::pingresponder::PROTOCOL_HEADER_SIZE)
                            == Some(&crate::link::pingresponder::PONG)
                        {
                            crate::link::measurement::dispatch_pong(
                                &entry.measurement_dispatch,
                                &buf[..amt],
                                src.into(),
                            );
                            continue;
                        }
                        if let Some(message) = responder.pong_message(&buf[..amt], src.into()) {
                            select! {
                                biased;
                                _ = open.closed() => {}
                                _ = &mut cancelled => break,
                                result = send_registered(entry, &context.interface_sockets, &message, src) => {
                                    if let Err(error) = result { debug!("failed to send registered PONG: {}", error); }
                                }
                            }
                        }
                    }
                }
                continue;
            }

            let (header, _header_len) = match parse_message_header(&buf[..amt]) {
                Ok(header) => header,
                Err(e) => {
                    debug!("ignoring malformed message from {}: {}", src, e);
                    continue;
                }
            };

            // TODO figure out how to encode group ID
            let should_ignore = match context.peer_state.try_lock() {
                Ok(guard) => header.ident == guard.ident() && header.group_id == context.group_id,
                Err(_) => false, // If we can't get the lock, don't ignore
            };

            if should_ignore {
                debug!("ignoring messages from self (peer {})", header.ident);
                continue;
            } else {
                debug!(
                    "received message type {} from peer {} at {}",
                    MESSAGE_TYPES
                        .get(usize::from(header.message_type))
                        .copied()
                        .unwrap_or("UNKNOWN"),
                    header.ident,
                    src
                );
            }

            // Check if Link is enabled before processing ALIVE and RESPONSE messages
            // BYEBYE messages should still be processed even when disabled to properly clean up peers
            let is_enabled = if let Ok(enabled_guard) = context.enabled.try_lock() {
                *enabled_guard
            } else {
                false
            };

            let handle = async {
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
                                return;
                            }

                            if let Some(entry) = &ingress {
                                send_ingress_response(entry, &context, src).await;
                            } else {
                                warn!("no registered ingress interface for response to {}", src);
                            }
                        }
                        RESPONSE => {}
                        BYEBYE => {}
                        _ => {
                            tracing::warn!(
                                "unknown message type {} from peer {}",
                                header.message_type,
                                header.ident
                            );
                        }
                    }
                }
            };
            select! {
                biased;
                _ = open.closed() => {}
                _ = &mut cancelled => break,
                _ = async {
                    match &ingress {
                        Some(entry) => entry.invalidated().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {}
                _ = handle => {}
            }
        }
    }
}

pub async fn broadcast_state(
    ttl: u8,
    ttl_ratio: u8,
    last_broadcast_time: Arc<Mutex<Instant>>,
    interface_sockets: InterfaceSockets,
    peer_state: Arc<Mutex<PeerState>>,
    to: SocketAddrV4,
    n: Arc<Notify>,
    enabled: Arc<Mutex<bool>>,
    group_id: SessionGroupId,
) {
    broadcast_state_loop(
        ttl,
        ttl_ratio,
        last_broadcast_time,
        interface_sockets,
        peer_state,
        to,
        n,
        enabled,
        group_id,
        true,
    )
    .await;
}

async fn broadcast_state_loop(
    ttl: u8,
    ttl_ratio: u8,
    last_broadcast_time: Arc<Mutex<Instant>>,
    interface_sockets: InterfaceSockets,
    peer_state: Arc<Mutex<PeerState>>,
    to: SocketAddrV4,
    n: Arc<Notify>,
    enabled: Arc<Mutex<bool>>,
    group_id: SessionGroupId,
    stop_on_notify: bool,
) {
    let lbt = last_broadcast_time.clone();

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
                        // Announce ourselves through every interface, so peers on any
                        // of them can discover us.
                        for entry in interface_socket_entries(&interface_sockets) {
                            send_peer_state_via(entry.socket.clone(), peer_state.clone(), ttl, ALIVE, to, lbt.clone(), group_id, Some((&entry, &interface_sockets))).await;
                        }
                    }
                }
            }
            _ = n.notified() => {
                if stop_on_notify {
                    break;
                }
                // This shared signal only wakes the owned broadcaster. Terminal
                // shutdown cancels its parent, never infers intent from enabled.
                sleep_time = Duration::ZERO;
            }
        }
    }
}

async fn send_ingress_response(
    entry: &InterfaceSocket,
    context: &ReceiveContext,
    to: SocketAddrV4,
) {
    send_peer_state_via(
        entry.socket.clone(),
        context.peer_state.clone(),
        context.ttl,
        RESPONSE,
        to,
        context.last_broadcast_time.clone(),
        context.group_id,
        Some((entry, &context.interface_sockets)),
    )
    .await;
}

pub async fn send_response(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    to: SocketAddrV4,
    last_broadcast_time: Arc<Mutex<Instant>>,
    group_id: SessionGroupId,
) {
    send_peer_state(
        socket,
        peer_state,
        ttl,
        RESPONSE,
        to,
        last_broadcast_time,
        group_id,
    )
    .await
}

pub async fn send_message(
    socket: Arc<UdpSocket>,
    from: NodeId,
    ttl: u8,
    message_type: MessageType,
    payload: &Payload,
    to: SocketAddrV4,
    group_id: SessionGroupId,
) -> std::io::Result<()> {
    send_message_reporting(socket, from, ttl, message_type, payload, to, group_id, None)
        .await
        .map(|_| ())
}

/// Sends a message, reporting whether a datagram actually went out.
///
/// Returns `Ok(true)` when the message was transmitted and `Ok(false)` when it
/// could not be encoded and was therefore dropped. Callers that rate-limit on
/// "we just broadcast" must not treat a dropped message as a send: upstream
/// advances its broadcast clock only after `sendUdpMessage` succeeds.
async fn send_message_reporting(
    socket: Arc<UdpSocket>,
    from: NodeId,
    ttl: u8,
    message_type: MessageType,
    payload: &Payload,
    to: SocketAddrV4,
    group_id: SessionGroupId,
    registration: Option<(&InterfaceSocket, &InterfaceSockets)>,
) -> std::io::Result<bool> {
    socket.set_broadcast(true)?;
    socket.set_multicast_ttl_v4(2)?;
    socket.set_multicast_loop_v4(true)?;

    // Matches upstream's `sendUdpMessage`: encoding is inside the fallible
    // path so an oversized/unencodable payload is logged and dropped instead
    // of taking down the caller.
    let message = match encode_message(from, ttl, message_type, payload, group_id) {
        Ok(message) => message,
        Err(err) => {
            debug!("Failed to encode message: {}", err);
            return Ok(false);
        }
    };

    if let Some((entry, sockets)) = registration {
        send_registered(entry, sockets, &message, to).await?;
    } else {
        socket.send_to(&message, to).await?;
    }
    Ok(true)
}

async fn send_registered(
    entry: &InterfaceSocket,
    sockets: &InterfaceSockets,
    message: &[u8],
    to: SocketAddrV4,
) -> std::io::Result<()> {
    loop {
        select! {
            biased;
            _ = entry.invalidated() => {
                return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "ingress lease was invalidated"));
            }
            result = entry.socket.writable() => result?,
        }
        let result = {
            let sockets = sockets
                .lock()
                .map_err(|_| std::io::Error::other("interface socket map is poisoned"))?;
            let current = sockets.get(&entry.identity.addr).is_some_and(|current| {
                current
                    .registrations()
                    .any(|current| Arc::ptr_eq(&current.socket, &entry.socket))
            });
            if !current || !entry.is_current()? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "ingress socket generation is no longer registered",
                ));
            }
            // No await between checking the registration and the actual syscall.
            entry.socket.try_send_to(message, to.into())
        };
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            result => return result.map(|_| ()),
        }
    }
}

pub async fn send_peer_state(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    message_type: MessageType,
    to: SocketAddrV4,
    last_broadcast_time: Arc<Mutex<Instant>>,
    group_id: SessionGroupId,
) {
    send_peer_state_via(
        socket,
        peer_state,
        ttl,
        message_type,
        to,
        last_broadcast_time,
        group_id,
        None,
    )
    .await;
}

async fn send_peer_state_via(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    message_type: MessageType,
    to: SocketAddrV4,
    last_broadcast_time: Arc<Mutex<Instant>>,
    group_id: SessionGroupId,
    registration: Option<(&InterfaceSocket, &InterfaceSockets)>,
) {
    let (ident, mut peer_state_clone) = match peer_state.try_lock() {
        Ok(guard) => (guard.ident(), guard.clone()),
        Err(_) => {
            // If we can't get the lock, skip this broadcast
            return;
        }
    };
    if registration.is_some_and(|(entry, _)| entry.measurement_responder)
        && peer_state_clone.measurement_endpoint.is_some()
    {
        peer_state_clone.measurement_endpoint = match socket.local_addr() {
            Ok(SocketAddr::V4(endpoint)) if !endpoint.ip().is_unspecified() => Some(endpoint),
            _ => None,
        };
    } else {
        advertise_endpoint_for(
            &mut peer_state_clone,
            registration.map(|(entry, _)| entry.identity.addr),
            &socket,
        );
    }

    match send_message_reporting(
        socket,
        ident,
        ttl,
        message_type,
        &peer_state_clone.into(),
        to,
        group_id,
        registration,
    )
    .await
    {
        Ok(true) => {}
        // The message could not be encoded and no datagram went out. Leave the
        // broadcast clock alone: advancing it here would make the rate limiter
        // suppress the next real broadcast on the strength of a send that
        // never happened.
        Ok(false) => return,
        Err(err) => {
            debug!("Failed to send peer state message: {}", err);
            return;
        }
    }

    if let Ok(mut last_time) = last_broadcast_time.try_lock() {
        *last_time = Instant::now();
    }
}

/// Compatibility fallback for standalone send helpers without a managed
/// per-interface measurement responder. If their responder uses a wildcard, its
/// address names no interface; what a peer needs is this host's address on the
/// network the message goes out on, with the responder's port. Upstream gets the
/// same effect by running one gateway, and so one measurement endpoint, per
/// interface.
///
/// `interface` is the address of the interface socket the message is sent
/// through. Without one, the sending socket's own address is used when it names
/// an interface. An endpoint already bound to a concrete address is left alone.
fn advertise_endpoint_for(state: &mut PeerState, interface: Option<Ipv4Addr>, socket: &UdpSocket) {
    let Some(endpoint) = state.measurement_endpoint else {
        return;
    };
    if !endpoint.ip().is_unspecified() {
        return;
    }
    let local = interface.or_else(|| match socket.local_addr() {
        Ok(SocketAddr::V4(addr)) if !addr.ip().is_unspecified() => Some(*addr.ip()),
        _ => None,
    });
    state.measurement_endpoint = local.map(|ip| SocketAddrV4::new(ip, endpoint.port()));
}

pub async fn receive_peer_state(tx: Sender<OnEvent>, header: MessageHeader, buf: &[u8]) {
    receive_peer_state_with_route(tx, header, buf, None).await;
}

async fn receive_peer_state_with_route(
    tx: Sender<OnEvent>,
    header: MessageHeader,
    buf: &[u8],
    route: Option<(&MeasurementRoutes, MeasurementRoute)>,
) {
    if let Some(event) = peer_state_event(header, buf, route) {
        let _ = tx.send(event).await;
    }
}

fn observe_discovery(
    context: &ReceiveContext,
    handoff: &Sender<DiscoveryEvent>,
    bytes: &[u8],
    ingress: Option<&InterfaceSocket>,
) {
    let Some(entry) = ingress else {
        return;
    };
    if bytes.starts_with(&crate::link::pingresponder::PROTOCOL_HEADER) {
        return;
    }
    let received = Instant::now();
    let Ok((mut header, len)) = parse_message_header(bytes) else {
        return;
    };
    if !matches!(header.message_type, ALIVE | RESPONSE | BYEBYE) {
        return;
    }
    if context
        .peer_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ident()
        == header.ident
        && header.group_id == context.group_id
    {
        return;
    }
    if header.message_type != BYEBYE && !*context.enabled.lock().unwrap_or_else(|e| e.into_inner())
    {
        return;
    }
    let payload = if header.message_type == BYEBYE {
        header.ttl = 0;
        &[][..]
    } else {
        &bytes[len..]
    };
    queue_peer_state_current(
        handoff,
        received,
        header,
        payload,
        &context.measurement_routes,
        MeasurementRoute {
            #[cfg(test)]
            retire_before_samples: Default::default(),
            entry: entry.lease(),
            sockets: context.interface_sockets.clone(),
        },
    );
}

#[cfg(test)]
fn queue_peer_state(
    tx: &Sender<DiscoveryEvent>,
    received: Instant,
    header: MessageHeader,
    buf: &[u8],
    route: Option<(&MeasurementRoutes, MeasurementRoute)>,
) {
    let Some((routes, ingress)) = route else {
        return;
    };
    ingress.while_current(|| {
        queue_peer_state_current(tx, received, header, buf, routes, ingress.clone())
    });
}

// Caller holds the ingress map from recv through observation/admission. A reader
// cannot retain an older timestamp outside that serialized region.
fn queue_peer_state_current(
    tx: &Sender<DiscoveryEvent>,
    received: Instant,
    header: MessageHeader,
    buf: &[u8],
    routes: &MeasurementRoutes,
    ingress: MeasurementRoute,
) {
    let expires = received + Duration::from_secs(u64::from(header.ttl));
    let Some(OnEvent::PeerState(mut state)) = peer_state_event(header, buf, None) else {
        return;
    };
    let slot = tx.try_reserve().ok();
    let revision = routes.observe(
        state.node_state.ident(),
        state.measurement_endpoint,
        received,
        slot.is_some() && (state.ttl == 0 || expires > Instant::now()),
    );
    if state.ttl == 0 {
        routes.remove_peer_before(state.node_state.ident(), received);
    } else {
        routes.record_received(
            state.node_state.ident(),
            state.measurement_endpoint,
            received,
            expires,
            ingress.clone(),
            true,
        );
    }
    if let (Some(slot), Some(revision)) = (slot, revision) {
        state.expires_at = Some(expires);
        state.observation = Some(Arc::new(PeerObservation {
            revision: revision.clone(),
            ingress: ingress.clone(),
            routes: routes.clone(),
        }));
        slot.send(DiscoveryEvent {
            event: OnEvent::PeerState(state),
            ingress,
            received,
            revision: Some(revision),
        });
    }
}

fn peer_state_event(
    header: MessageHeader,
    buf: &[u8],
    route: Option<(&MeasurementRoutes, MeasurementRoute)>,
) -> Option<OnEvent> {
    // Received from the network: a malformed payload is dropped.
    let payload = match parse_payload(buf) {
        Ok(payload) => payload,
        Err(e) => {
            debug!("dropping peer state with a malformed payload: {}", e);
            return None;
        }
    };
    let measurement_endpoint = payload.entries.iter().find_map(|e| {
        if let PayloadEntry::MeasurementEndpointV4(me) = e {
            me.endpoint
        } else {
            None
        }
    });

    let audio_endpoint = payload.entries.iter().find_map(|e| {
        if let PayloadEntry::AudioEndpointV4(ae) = e {
            ae.endpoint
        } else {
            None
        }
    });

    let node_state: NodeState = NodeState::from_payload(header.ident, &payload);
    if let Some((routes, route)) = route {
        routes.record(header.ident, measurement_endpoint, header.ttl, route);
    }

    Some(OnEvent::PeerState(PeerStateMessageType {
        node_state,
        ttl: header.ttl,
        expires_at: Some(Instant::now() + Duration::from_secs(u64::from(header.ttl))),
        observation: None,
        measurement_endpoint,
        audio_endpoint,
    }))
}

pub async fn receive_bye_bye(tx: Sender<OnEvent>, node_id: NodeId) {
    info!("Received BYEBYE message from peer {}", node_id);
    if let Err(e) = tx.send(OnEvent::Byebye(node_id)).await {
        debug!("Failed to send BYEBYE event: {:?}", e);
    } else {
        info!("Successfully forwarded BYEBYE event for peer {}", node_id);
    }
}

pub fn send_byebye(node_state: NodeId) {
    info!("sending bye bye");

    let socket = match new_std_udp_reuseport(MULTICAST_IP_ANY.into()) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to create socket for BYEBYE: {}", e);
            return;
        }
    };
    if let Err(error) = socket
        .set_broadcast(true)
        .and_then(|()| socket.set_multicast_ttl_v4(2))
    {
        warn!("Failed to configure BYEBYE socket: {}", error);
        return;
    }

    let message = match encode_message(node_state, 0, BYEBYE, &Payload::default(), 0) {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to encode BYEBYE message: {}", e);
            return;
        }
    };

    if let Err(e) = socket.send_to(&message, (MULTICAST_ADDR, LINK_PORT)) {
        warn!("Failed to send BYEBYE: {}", e);
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn origin_finality_discovery_withdrawal_overlaps_observer_commit_without_inversion() {
        use super::super::{
            gateway::on_peer_state,
            peers::{ControllerPeer, GatewayObserver, PeerEvent, PeerStateChange},
        };
        use crate::link::controller::SessionPeerCounter;
        tokio::time::timeout(Duration::from_secs(5), async {
            let (outbound, _events) = tokio::sync::mpsc::channel(1);
            let context = receive_context(outbound);
            let route = MeasurementRoute {
                retire_before_samples: Default::default(),
                entry: context.interface_sockets.lock().unwrap()[&Ipv4Addr::LOCALHOST].lease(),
                sockets: context.interface_sockets.clone(),
            };
            let peer = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([87; 8]),
                    timeline: crate::link::timeline::Timeline {
                        tempo: crate::link::tempo::Tempo::new(120.0),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            let peers = Arc::new(Mutex::new(vec![ControllerPeer { peer_state: peer.clone() }]));
            let timeouts = Arc::new(Mutex::new(vec![(Instant::now() + Duration::from_secs(30), peer.ident())]));
            let held_peers = peers.clone();
            let (ready, ready_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _guard = held_peers.lock().unwrap();
                ready.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let (events, rx_events) = tokio::sync::mpsc::channel(1);
            let (changes, mut changed) = tokio::sync::mpsc::channel(1);
            let observer = GatewayObserver::new(
                rx_events, context.peer_state.clone(),
                Arc::new(Mutex::new(SessionPeerCounter::default())),
                changes, peers.clone(), Arc::new(Notify::new()), None,
            ).await;
            let (handoff, mut pending) = tokio::sync::mpsc::channel(1);
            queued_state(&handoff, &context.measurement_routes, &route, &peer, 30);
            let OnEvent::PeerState(mut old) = pending.recv().await.unwrap().event else { panic!("state") };
            let deadline = Instant::now() + Duration::from_millis(500);
            old.expires_at = Some(deadline);
            let old_observation = old.observation.clone().unwrap();
            events.send(PeerEvent::Observed(old)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while observer.state_ready.load(Ordering::Acquire) == 0 {
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            let listener = Arc::new(PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap());
            let remote = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            remote.send_to(&encode_message(peer.ident(), 0, RESPONSE, &Payload::from(peer.clone()), 0).unwrap(), listener.local_addr().unwrap()).await.unwrap();
            let receive_context = context.clone();
            let receive_handoff = handoff.clone();
            let receive_listener = listener.clone();
            let mut reader = tokio::spawn(async move {
                let mut bytes = [0; MAX_MESSAGE_SIZE];
                receive_datagram_observing(&receive_listener, &receive_context.interface_sockets, &mut bytes, None, |bytes, ingress| {
                    observe_discovery(&receive_context, &receive_handoff, bytes, ingress);
                }).await.unwrap();
            });
            let completed_before_release = match tokio::time::timeout(Duration::from_millis(200), &mut reader).await {
                Ok(result) => { result.unwrap(); true }
                Err(_) => false,
            };
            // Expiry lets the counterfactual retained identity guard unwind before
            // entering ingress, so the failing regression does not deadlock teardown.
            tokio::time::sleep_until(deadline + Duration::from_millis(10)).await;
            release.send(()).unwrap();
            holder.join().unwrap();
            if !completed_before_release { reader.await.unwrap(); }
            assert!(completed_before_release, "production discovery receive must not wait on the observer's retained identity guard");
            assert!(old_observation.revision.cancel.is_cancelled());
            let OnEvent::PeerState(withdrawn) = pending.recv().await.unwrap().event else { panic!("withdrawal") };
            assert_eq!(withdrawn.ttl, 0, "contended withdrawals must be admitted, not silently dropped");
            on_peer_state(withdrawn, timeouts.clone(), events.clone(), Instant::now(), Arc::new(Notify::new()), NodeId::default()).await;
            assert!(matches!(changed.recv().await.unwrap().as_slice(), [PeerStateChange::PeerLeft]));
            assert!(peers.lock().unwrap().is_empty());
            assert!(timeouts.lock().unwrap().is_empty());
            assert!(changed.try_recv().is_err());

            remote.send_to(&encode_message(peer.ident(), 30, RESPONSE, &Payload::from(peer.clone()), 0).unwrap(), listener.local_addr().unwrap()).await.unwrap();
            let mut bytes = [0; MAX_MESSAGE_SIZE];
            receive_datagram_observing(&listener, &context.interface_sockets, &mut bytes, None, |bytes, ingress| {
                observe_discovery(&context, &handoff, bytes, ingress);
            }).await.unwrap();
            let OnEvent::PeerState(fresh) = pending.recv().await.unwrap().event else { panic!("fresh state") };
            assert_eq!(fresh.expires_at, Some(fresh.observation.as_ref().unwrap().revision.received + Duration::from_secs(30)));
            on_peer_state(fresh, timeouts.clone(), events, Instant::now(), Arc::new(Notify::new()), NodeId::default()).await;
            assert!(changed.recv().await.unwrap().iter().any(|change| matches!(change, PeerStateChange::SessionMembership)));
            assert_eq!(peers.lock().unwrap().as_slice(), &[ControllerPeer { peer_state: peer }]);
            assert_eq!(timeouts.lock().unwrap().len(), 1);
        }).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn origin_finality_state_contention_does_not_block_registered_ping_pong() {
        use super::super::peers::{GatewayObserver, PeerEvent};
        use crate::link::{controller::SessionPeerCounter, pingresponder};
        for hold_self in [false, true] {
            let (route, _retire) = MeasurementRoute::test_fixture();
            let routes = MeasurementRoutes::default();
            let (handoff, mut pending) = tokio::sync::mpsc::channel(1);
            let peer = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([86; 8]),
                    ..Default::default()
                },
                ..Default::default()
            };
            let peers = Arc::new(Mutex::new(Vec::new()));
            let self_state = Arc::new(Mutex::new(PeerState::default()));
            let held_peers = peers.clone();
            let held_self = self_state.clone();
            let (ready, ready_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let peers = (!hold_self).then(|| held_peers.lock().unwrap());
                let state = hold_self.then(|| held_self.lock().unwrap());
                ready.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                drop((peers, state));
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let (events, rx_events) = tokio::sync::mpsc::channel(1);
            let (changes, mut changed) = tokio::sync::mpsc::channel(1);
            let capacity = changes.clone();
            let _observer = GatewayObserver::new(
                rx_events,
                self_state,
                Arc::new(Mutex::new(SessionPeerCounter::default())),
                changes,
                peers.clone(),
                Arc::new(Notify::new()),
                None,
            )
            .await;
            queued_state(&handoff, &routes, &route, &peer, 30);
            let OnEvent::PeerState(state) = pending.recv().await.unwrap().event else {
                panic!("state")
            };
            events.send(PeerEvent::Observed(state)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while capacity.capacity() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let remote = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let SocketAddr::V4(endpoint) = remote.local_addr().unwrap() else {
                panic!("IPv4")
            };
            let socket = route.socket();
            let mut network = tokio::spawn(async move {
                let ping = pingresponder::encode_message(pingresponder::PING, &Payload::default())
                    .unwrap();
                route.send(&ping, endpoint).await.unwrap();
                let mut buf = [0; MAX_MESSAGE_SIZE];
                let (n, src) = remote.recv_from(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], ping.as_slice());
                let pong = pingresponder::encode_message(pingresponder::PONG, &Payload::default())
                    .unwrap();
                remote.send_to(&pong, src).await.unwrap();
                let (n, source) = socket.recv_from(&mut buf).await.unwrap();
                assert_eq!(source, SocketAddr::V4(endpoint));
                assert_eq!(&buf[..n], pong.as_slice());
            });
            let result = tokio::time::timeout(Duration::from_millis(200), &mut network).await;
            release.send(()).unwrap();
            holder.join().unwrap();
            if result.is_err() {
                let _ = tokio::time::timeout(Duration::from_secs(1), network).await;
            }
            assert!(
                result.is_ok(),
                "state contention must not retain ingress locks needed for PING/PONG"
            );
            result.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(1), changed.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(peers.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn gateway_capacity_wait_is_cancelled_by_newer_ttl_zero() {
        use super::super::{gateway::on_peer_state, peers::PeerEvent};
        tokio::time::timeout(Duration::from_secs(3), async {
            let (route, _cancel) = MeasurementRoute::test_fixture();
            let routes = MeasurementRoutes::default();
            let (handoff, mut pending) = tokio::sync::mpsc::channel(1);
            let peer = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([73; 8]),
                    ..NodeState::default()
                },
                ..PeerState::default()
            };
            queued_state(&handoff, &routes, &route, &peer, 30);
            let OnEvent::PeerState(old) = pending.recv().await.unwrap().event else {
                panic!("state")
            };
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.send(PeerEvent::PeerLeft(NodeId::default()))
                .await
                .unwrap();
            let original = vec![(Instant::now() + Duration::from_secs(3), peer.ident())];
            let timeouts = Arc::new(Mutex::new(original.clone()));
            let mut work = Box::pin(on_peer_state(
                old,
                timeouts.clone(),
                tx.clone(),
                Instant::now(),
                Arc::new(Notify::new()),
                NodeId::default(),
            ));
            assert!(std::future::Future::poll(
                work.as_mut(),
                &mut std::task::Context::from_waker(std::task::Waker::noop())
            )
            .is_pending());
            queued_state(&handoff, &routes, &route, &peer, 0);
            let OnEvent::PeerState(withdrawn) = pending.recv().await.unwrap().event else {
                panic!("withdrawal")
            };
            work.await;
            assert_eq!(
                *timeouts.lock().unwrap(),
                original,
                "superseded wait must not renew old timeout"
            );
            rx.recv().await.unwrap();
            on_peer_state(
                withdrawn,
                timeouts.clone(),
                tx,
                Instant::now(),
                Arc::new(Notify::new()),
                NodeId::default(),
            )
            .await;
            assert!(matches!(rx.recv().await, Some(PeerEvent::PeerLeft(id)) if id == peer.ident()));
            assert!(timeouts.lock().unwrap().is_empty());
            assert!(rx.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn live_ttl_one_keeps_wire_value_and_original_deadline_after_handoff() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let listener = Arc::new(
                PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap(),
            );
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            let context = receive_context(tx);
            let work = tokio::spawn(receive_loop(listener.clone(), None, context));
            peer.send_to(
                &encode_message(
                    NodeId::from_array([74; 8]),
                    1,
                    RESPONSE,
                    &Payload::default(),
                    0,
                )
                .unwrap(),
                listener.local_addr().unwrap(),
            )
            .await
            .unwrap();
            let Some(OnEvent::PeerState(state)) = rx.recv().await else {
                panic!("state")
            };
            assert_eq!(state.ttl, 1);
            assert_eq!(
                state.expires_at,
                Some(
                    state.observation.as_ref().unwrap().revision.received + Duration::from_secs(1)
                )
            );
            assert!(state.expires_at.unwrap() > Instant::now());
            work.abort();
            let _ = work.await;
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn identity_enumeration_is_offloaded_and_one_snapshot_is_reused() {
        let entered = Arc::new(AtomicBool::new(false));
        let probe = entered.clone();
        let (release, released) = std::sync::mpsc::channel();
        let task = tokio::spawn(scan_interface_snapshot(move || {
            probe.store(true, Ordering::SeqCst);
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok((
                Vec::new(),
                LocalIdentity {
                    addresses: HashSet::from([IpAddr::V4(Ipv4Addr::LOCALHOST)]),
                    ..LocalIdentity::default()
                },
            ))
        }));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // The current-thread executor remains available while enumeration waits.
        tokio::time::sleep(Duration::from_millis(10)).await;
        release.send(()).unwrap();
        let (_, identity) = task.await.unwrap().unwrap().unwrap();
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let context = receive_context(tx);
        let entries = interface_socket_entries(&context.interface_sockets);
        let current: Vec<_> = entries.iter().map(|entry| entry.identity.clone()).collect();
        let socket = entries[0].socket.clone();
        let mut children = tokio::task::JoinSet::new();
        reconcile_interfaces_at(
            &socket,
            &context,
            &mut children,
            &current,
            &identity,
            0,
            &mut SetupFailures::default(),
        );
        for entry in interface_socket_entries(&context.interface_sockets) {
            assert_eq!(
                entry.local_delivery.0.lock().unwrap().addresses,
                identity.addresses
            );
            assert!(
                entry
                    .local_delivery
                    .0
                    .lock()
                    .unwrap()
                    .loopback_indices
                    .is_empty(),
                "reconciliation must use the supplied snapshot, not enumerate again"
            );
        }
        children.shutdown().await;
    }

    fn queued_state(
        tx: &Sender<DiscoveryEvent>,
        routes: &MeasurementRoutes,
        route: &MeasurementRoute,
        peer: &PeerState,
        ttl: u8,
    ) {
        let bytes =
            encode_message(peer.ident(), ttl, RESPONSE, &Payload::from(peer.clone()), 0).unwrap();
        let (header, len) = parse_message_header(&bytes).unwrap();
        queue_peer_state(
            tx,
            Instant::now(),
            header,
            &bytes[len..],
            Some((routes, route.clone())),
        );
    }

    #[tokio::test]
    async fn node_revisions_survive_different_endpoints_and_gateway_backpressure() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (route, _cancel) = MeasurementRoute::test_fixture();
            let routes = MeasurementRoutes::default();
            let (first_tx, mut first_rx) = tokio::sync::mpsc::channel(1);
            let (second_tx, mut second_rx) = tokio::sync::mpsc::channel(1);
            let old = PeerState { node_state: NodeState { node_id: NodeId::from_array([77; 8]), ..NodeState::default() }, measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 10001)), ..PeerState::default() };
            let fresh = PeerState { measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 10002)), ..old.clone() };
            queued_state(&first_tx, &routes, &route, &old, 1);
            let OnEvent::PeerState(old) = first_rx.recv().await.unwrap().event else { panic!("state") };
            assert_eq!(old.ttl, 1);
            let deadline = old.expires_at;
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.send(super::super::peers::PeerEvent::PeerLeft(NodeId::default())).await.unwrap();
            let timeouts = Arc::new(Mutex::new(Vec::new()));
            let pending = tokio::spawn(super::super::gateway::on_peer_state(old.clone(), timeouts.clone(), tx.clone(), Instant::now(), Arc::new(Notify::new()), NodeId::default()));
            tokio::task::yield_now().await;
            queued_state(&second_tx, &routes, &route, &fresh, 1);
            let OnEvent::PeerState(new) = second_rx.recv().await.unwrap().event else { panic!("state") };
            pending.await.unwrap();
            assert!(timeouts.lock().unwrap().is_empty());
            assert!(old.observation.as_ref().unwrap().while_current(|| ()).is_none());
            assert_eq!(old.expires_at, deadline);
            assert_eq!(new.ttl, 1, "wire TTL must not be floored");
            rx.recv().await.unwrap();
            super::super::gateway::on_peer_state(new, timeouts.clone(), tx, Instant::now(), Arc::new(Notify::new()), NodeId::default()).await;
            assert!(matches!(rx.recv().await, Some(super::super::peers::PeerEvent::Observed(state)) if state.measurement_endpoint == fresh.measurement_endpoint && state.ttl == 1));
        }).await.unwrap();
    }

    #[tokio::test]
    async fn three_gaps_committed_withdrawal_survives_dropped_successor_and_reannounces() {
        use super::super::{
            gateway::on_peer_state,
            peers::{ControllerPeer, GatewayObserver, PeerStateChange},
        };
        use crate::link::controller::SessionPeerCounter;
        tokio::time::timeout(Duration::from_secs(3), async {
            let (route, _cancel) = MeasurementRoute::test_fixture();
            let routes = MeasurementRoutes::default();
            let (handoff, mut pending) = tokio::sync::mpsc::channel(1);
            let peer = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([84; 8]),
                    ..NodeState::default()
                },
                ..PeerState::default()
            };
            let timeouts = Arc::new(Mutex::new(vec![(
                Instant::now() + Duration::from_secs(30),
                peer.ident(),
            )]));
            let peers = Arc::new(Mutex::new(vec![ControllerPeer {
                peer_state: peer.clone(),
            }]));
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let (changes, mut changed) = tokio::sync::mpsc::channel(1);
            changes.send(Vec::new()).await.unwrap();
            let _observer = GatewayObserver::new(
                rx,
                Arc::new(Mutex::new(PeerState::default())),
                Arc::new(Mutex::new(SessionPeerCounter::default())),
                changes,
                peers.clone(),
                Arc::new(Notify::new()),
                None,
            )
            .await;
            queued_state(&handoff, &routes, &route, &peer, 0);
            let OnEvent::PeerState(withdrawn) = pending.recv().await.unwrap().event else {
                panic!("withdrawal")
            };
            let revision = withdrawn.observation.clone().unwrap();
            on_peer_state(
                withdrawn,
                timeouts.clone(),
                tx.clone(),
                Instant::now(),
                Arc::new(Notify::new()),
                NodeId::default(),
            )
            .await;
            while tx.capacity() == 0 {
                tokio::task::yield_now().await;
            }
            assert!(timeouts.lock().unwrap().is_empty());
            assert_eq!(peers.lock().unwrap().len(), 1);
            // Occupy the entire handoff with another node, then observe a newer
            // positive announcement for the withdrawn node which cannot be admitted.
            let filler = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([85; 8]),
                    ..NodeState::default()
                },
                ..PeerState::default()
            };
            queued_state(&handoff, &routes, &route, &filler, 30);
            assert_eq!(handoff.capacity(), 0);
            queued_state(&handoff, &routes, &route, &peer, 30);
            assert!(revision.revision.cancel.is_cancelled());
            changed.recv().await.unwrap();
            let delta = changed.recv().await.unwrap();
            assert!(matches!(delta.as_slice(), [PeerStateChange::PeerLeft]));
            assert!(
                peers.lock().unwrap().is_empty(),
                "committed departure must not orphan a peer without a timeout"
            );
            assert!(timeouts.lock().unwrap().is_empty());
            pending.recv().await.unwrap();
            assert!(
                pending.try_recv().is_err(),
                "newer positive traffic was dropped, not a cleanup successor"
            );
            queued_state(&handoff, &routes, &route, &peer, 30);
            let OnEvent::PeerState(fresh) = pending.recv().await.unwrap().event else {
                panic!("fresh announcement")
            };
            on_peer_state(
                fresh,
                timeouts.clone(),
                tx,
                Instant::now(),
                Arc::new(Notify::new()),
                NodeId::default(),
            )
            .await;
            let delta = changed.recv().await.unwrap();
            assert!(delta
                .iter()
                .any(|change| matches!(change, PeerStateChange::SessionMembership)));
            assert_eq!(peers.lock().unwrap().len(), 1);
            assert_eq!(timeouts.lock().unwrap().len(), 1);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn managed_withdrawal_cancels_gateway_and_observer_waits() {
        use super::super::{
            gateway::on_peer_state,
            peers::{ControllerPeer, GatewayObserver, PeerEvent},
        };
        use crate::link::controller::SessionPeerCounter;
        tokio::time::timeout(Duration::from_secs(3), async {
            let (route, _cancel) = MeasurementRoute::test_fixture();
            let routes = MeasurementRoutes::default();
            let (handoff, mut queued) = tokio::sync::mpsc::channel(2);
            let peer = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([76; 8]),
                    ..NodeState::default()
                },
                ..PeerState::default()
            };
            let timeouts = Arc::new(Mutex::new(vec![(
                Instant::now() + Duration::from_secs(30),
                peer.ident(),
            )]));
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let (changes, mut changed) = tokio::sync::mpsc::channel(1);
            changes.send(Vec::new()).await.unwrap();
            let peers = Arc::new(Mutex::new(vec![ControllerPeer {
                peer_state: peer.clone(),
            }]));
            let _observer = GatewayObserver::new(
                rx,
                Arc::new(Mutex::new(PeerState::default())),
                Arc::new(Mutex::new(SessionPeerCounter::default())),
                changes,
                peers.clone(),
                Arc::new(Notify::new()),
                None,
            )
            .await;
            queued_state(&handoff, &routes, &route, &peer, 30);
            let OnEvent::PeerState(old) = queued.recv().await.unwrap().event else {
                panic!("state")
            };
            tx.send(PeerEvent::Observed(old)).await.unwrap();
            while tx.capacity() == 0 {
                tokio::task::yield_now().await;
            }
            queued_state(&handoff, &routes, &route, &peer, 0);
            let OnEvent::PeerState(withdrawn) = queued.recv().await.unwrap().event else {
                panic!("withdrawal")
            };
            on_peer_state(
                withdrawn,
                timeouts.clone(),
                tx,
                Instant::now(),
                Arc::new(Notify::new()),
                NodeId::default(),
            )
            .await;
            assert!(timeouts.lock().unwrap().is_empty());
            changed.recv().await.unwrap();
            let delta = changed.recv().await.unwrap();
            assert!(matches!(
                delta.as_slice(),
                [super::super::peers::PeerStateChange::PeerLeft]
            ));
            assert!(peers.lock().unwrap().is_empty());
            assert!(
                changed.try_recv().is_err(),
                "superseded positive state must not publish observer changes"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn receive_observation_and_admission_are_serialized_across_readers() {
        tokio::time::timeout(Duration::from_secs(4), async {
            let (tx, _) = tokio::sync::mpsc::channel(1);
            let context = receive_context(tx);
            let listener_a = Arc::new(
                PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap(),
            );
            let listener_b = Arc::new(
                PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap(),
            );
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let node = NodeId::from_array([75; 8]);
            let state = PeerState {
                node_state: NodeState {
                    node_id: node,
                    ..NodeState::default()
                },
                ..PeerState::default()
            };
            let (handoff, mut queued) = tokio::sync::mpsc::channel(2);
            peer.send_to(
                &encode_message(node, 30, RESPONSE, &Payload::from(state.clone()), 0).unwrap(),
                listener_a.local_addr().unwrap(),
            )
            .await
            .unwrap();
            let (entered, entered_rx) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let a_context = context.clone();
            let a_handoff = handoff.clone();
            let a = tokio::spawn(async move {
                let mut entered = Some(entered);
                let a_sockets = a_context.interface_sockets.clone();
                let mut buffer = [0; MAX_MESSAGE_SIZE];
                receive_datagram_observing(
                    &listener_a,
                    &a_sockets,
                    &mut buffer,
                    None,
                    move |bytes, ingress| {
                        entered.take().unwrap().send(()).unwrap();
                        released.recv_timeout(Duration::from_secs(2)).unwrap();
                        observe_discovery(&a_context, &a_handoff, bytes, ingress);
                    },
                )
                .await
                .unwrap();
            });
            entered_rx.await.unwrap();
            peer.send_to(
                &encode_message(node, 0, RESPONSE, &Payload::from(state), 0).unwrap(),
                listener_b.local_addr().unwrap(),
            )
            .await
            .unwrap();
            let b_context = context.clone();
            let b_handoff = handoff;
            let started = Arc::new(AtomicBool::new(false));
            let observed = Arc::new(AtomicBool::new(false));
            let b_started = started.clone();
            let b_observed = observed.clone();
            let b = tokio::spawn(async move {
                b_started.store(true, Ordering::SeqCst);
                let mut buffer = [0; MAX_MESSAGE_SIZE];
                receive_datagram_observing(
                    &listener_b,
                    &b_context.interface_sockets,
                    &mut buffer,
                    None,
                    |bytes, ingress| {
                        b_observed.store(true, Ordering::SeqCst);
                        observe_discovery(&b_context, &b_handoff, bytes, ingress)
                    },
                )
                .await
                .unwrap();
            });
            let arbiter = std::thread::spawn(move || {
                let until = std::time::Instant::now() + Duration::from_secs(1);
                while !started.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                    std::thread::yield_now();
                }
                assert!(started.load(Ordering::SeqCst));
                std::thread::sleep(Duration::from_millis(30));
                let premature = observed.load(Ordering::SeqCst);
                release.send(()).unwrap();
                assert!(
                    !premature,
                    "newer observation must wait for earlier read/admission"
                );
            });
            a.await.unwrap();
            b.await.unwrap();
            arbiter.join().unwrap();
            let old = queued.recv().await.unwrap();
            let newer = queued.recv().await.unwrap();
            assert!(old.revision.unwrap().cancel.is_cancelled());
            assert!(matches!(newer.event, OnEvent::PeerState(state) if state.ttl == 0));
        })
        .await
        .unwrap();
    }

    use std::net::Ipv6Addr;

    use super::*;

    #[test]
    fn setup_failures_warn_once_per_error_and_forget_gone_interfaces() {
        let bt = Ipv4Interface {
            addr: Ipv4Addr::new(169, 254, 67, 110),
            index: 8,
            name: "Bluetooth Network Connection".into(),
        };
        let mut failures = SetupFailures::default();
        let not_valid = std::io::Error::from(std::io::ErrorKind::AddrNotAvailable);
        failures.failed(&bt, &not_valid);
        failures.failed(&bt, &not_valid);
        assert_eq!(failures.0.len(), 1, "a repeated failure is remembered once");
        failures.failed(
            &bt,
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(
            failures.0[&bt].contains("ermission"),
            "a new error replaces the old one"
        );
        failures.forget_except(&[]);
        assert!(
            failures.0.is_empty(),
            "an interface that's gone is forgotten"
        );
    }
    use crate::discovery::socket_test_support::{
        assert_ephemeral_socket_is_unshared, shared_loopback_socket,
    };

    #[tokio::test]
    async fn ephemeral_udp_sockets_do_not_enable_port_sharing() {
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            let socket = new_udp_reuseport(SocketAddr::new(ip, 0)).unwrap();
            assert_ephemeral_socket_is_unshared(&socket);
        }
    }

    #[tokio::test]
    async fn explicit_udp_ports_can_still_be_shared() {
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            let reservation = shared_loopback_socket(SocketAddr::new(ip, 0));
            let endpoint = reservation.local_addr().unwrap().as_socket().unwrap();
            assert_ne!(endpoint.port(), 0);
            let socket = new_udp_reuseport(endpoint).unwrap();
            assert_eq!(socket.local_addr().unwrap(), endpoint);
            let options = socket2::SockRef::from(&socket);
            assert!(options.reuse_address().unwrap());
            #[cfg(unix)]
            assert!(options.reuse_port().unwrap());
        }
    }

    #[tokio::test]
    async fn public_messenger_listen_preserves_notifier_cancellation() {
        use std::future::Future;
        let notifier = Arc::new(Notify::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let messenger = Messenger::new(
            Arc::new(Mutex::new(PeerState::default())),
            tx,
            Instant::now(),
            notifier.clone(),
            Arc::new(Mutex::new(false)),
        )
        .unwrap();
        let mut listen = Box::pin(messenger.listen());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(listen.as_mut().poll(&mut context).is_pending());
        notifier.notify_waiters();
        assert!(listen.as_mut().poll(&mut context).is_ready());
    }

    fn with_endpoint(endpoint: Option<SocketAddrV4>) -> PeerState {
        PeerState {
            measurement_endpoint: endpoint,
            ..PeerState::default()
        }
    }

    #[tokio::test]
    async fn advertised_endpoint_follows_the_sending_interface() {
        let wildcard = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let wired = Ipv4Addr::new(169, 254, 0, 2);

        // The wildcard responder is advertised at the interface's address.
        let mut state = with_endpoint(Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 4321)));
        advertise_endpoint_for(&mut state, Some(wired), &wildcard);
        assert_eq!(
            state.measurement_endpoint,
            Some(SocketAddrV4::new(wired, 4321))
        );

        // An endpoint bound to a concrete address is left as it is.
        let concrete = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 5), 4321);
        let mut state = with_endpoint(Some(concrete));
        advertise_endpoint_for(&mut state, Some(wired), &wildcard);
        assert_eq!(state.measurement_endpoint, Some(concrete));

        // No interface: fall back to the sending socket's own address...
        let loopback = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut state = with_endpoint(Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 4321)));
        advertise_endpoint_for(&mut state, None, &loopback);
        assert_eq!(
            state.measurement_endpoint,
            Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4321))
        );

        // ...and advertise nothing rather than an unreachable 0.0.0.0.
        let mut state = with_endpoint(Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 4321)));
        advertise_endpoint_for(&mut state, None, &wildcard);
        assert_eq!(state.measurement_endpoint, None);

        // No endpoint stays no endpoint.
        let mut state = with_endpoint(None);
        advertise_endpoint_for(&mut state, Some(wired), &wildcard);
        assert_eq!(state.measurement_endpoint, None);
    }

    /// End to end over the loopback adapter: a peer state sent through an
    /// interface socket carries that interface's address as its measurement
    /// endpoint, not the wildcard the responder is bound to.
    #[tokio::test]
    async fn peer_state_sent_through_an_interface_advertises_its_address() {
        let sockets: InterfaceSockets = Arc::new(Mutex::new(HashMap::from([(
            Ipv4Addr::LOCALHOST,
            interface_socket(),
        )])));
        // Sends are only made through a registered socket.
        let entry = interface_socket_entries(&sockets)
            .into_iter()
            .next()
            .unwrap();
        let peer_state = Arc::new(Mutex::new(with_endpoint(Some(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            4321,
        )))));
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(to) = receiver.local_addr().unwrap() else {
            unreachable!()
        };

        send_peer_state_via(
            entry.socket.clone(),
            peer_state.clone(),
            1,
            ALIVE,
            to,
            Arc::new(Mutex::new(Instant::now())),
            0,
            Some((&entry, &sockets)),
        )
        .await;

        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let (size, _) = tokio::time::timeout(Duration::from_secs(5), receiver.recv_from(&mut buf))
            .await
            .expect("the peer state arrives")
            .unwrap();
        let (header, header_len) = parse_message_header(&buf[..size]).unwrap();
        assert_eq!(header.message_type, ALIVE);
        let payload = parse_payload(&buf[header_len..size]).unwrap();
        let advertised = payload.entries.iter().find_map(|e| match e {
            PayloadEntry::MeasurementEndpointV4(me) => me.endpoint,
            _ => None,
        });
        assert_eq!(
            advertised,
            Some(SocketAddrV4::new(entry.identity.addr, 4321))
        );
        // The shared state itself keeps the wildcard: each send picks its own.
        assert_eq!(
            peer_state.lock().unwrap().measurement_endpoint,
            Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 4321))
        );
    }

    #[tokio::test]
    async fn managed_measurement_reply_matches_advertised_endpoint_despite_preferred_route() {
        use crate::link::{
            clock::Clock, ghostxform::GhostXForm, pingresponder, sessions::SessionId,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            // Winsock rejects loopback-to-non-loopback probes. Its loopback
            // subnet supports a second address without configuring an alias.
            #[cfg(target_os = "windows")]
            let addr = Ipv4Addr::new(127, 0, 0, 2);
            #[cfg(not(target_os = "windows"))]
            let addr = crate::platform::network::scan_network_interfaces_blocking()
                .into_iter().find_map(|ip| match ip { IpAddr::V4(ip) => Some(ip), _ => None })
                .expect("a non-loopback local IPv4 address");
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let SocketAddr::V4(destination) = peer.local_addr().unwrap() else { panic!("expected IPv4") };
            let wildcard = Arc::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap());
            let mut buffer = [0; MAX_MESSAGE_SIZE];
            wildcard.send_to(b"route probe", destination).await.unwrap();
            let (_, preferred) = peer.recv_from(&mut buffer).await.unwrap();
            assert_ne!(preferred.ip(), IpAddr::V4(addr), "fixture must expose different wildcard source selection");

            let receiver = Arc::new(PacketSocket::new(SocketAddrV4::new(addr, 0), None).unwrap());
            let SocketAddr::V4(endpoint) = receiver.local_addr().unwrap() else { panic!("expected IPv4") };
            // Observe the actual local delivery interface, without changing any
            // host routes or requiring a second configured loopback alias.
            peer.send_to(b"ingress probe", endpoint).await.unwrap();
            let info = loop {
                receiver.readable().await.unwrap();
                match receiver.try_recv(&mut buffer) {
                    Ok((_, info)) => break info,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(error) => panic!("ingress probe: {}", error),
                }
            };
            let entry = InterfaceSocket {
                socket: receiver.socket.clone(), receiver: receiver.clone(),
                identity: Ipv4Interface { addr, index: u32::try_from(info.if_index).unwrap(), name: "measurement-source-fixture".into() },
                cancel: Cancel::default(), topology: None, generation: 0,
                measurement_responder: true, measurement_dispatch: Arc::new(Mutex::new(HashMap::new())),
            local_delivery: Arc::new(LocalDelivery::snapshot()), siblings: Vec::new(),
            };
            let (tx_event, _rx_event) = tokio::sync::mpsc::channel(1);
            let mut context = receive_context(tx_event);
            context.interface_sockets = Arc::new(Mutex::new(HashMap::from([(addr, entry.lease())])));
            context.measurement_responder = Some(pingresponder::PingResponder::new(
                wildcard.clone(), SessionId::default(), GhostXForm::default(), Clock::new(),
            ));
            context.peer_state.lock().unwrap().measurement_endpoint = Some(match wildcard.local_addr().unwrap() {
                SocketAddr::V4(endpoint) => endpoint, _ => panic!("expected IPv4"),
            });
            send_peer_state_via(
                entry.socket.clone(), context.peer_state.clone(), 1, ALIVE, destination,
                context.last_broadcast_time.clone(), 0, Some((&entry, &context.interface_sockets)),
            ).await;
            let (size, source) = peer.recv_from(&mut buffer).await.unwrap();
            let (_, header_len) = parse_message_header(&buffer[..size]).unwrap();
            let payload = parse_payload(&buffer[header_len..size]).unwrap();
            assert!(payload.entries.iter().any(|entry| matches!(entry,
                PayloadEntry::MeasurementEndpointV4(measurement) if measurement.endpoint == Some(endpoint))));
            assert_eq!(source, SocketAddr::V4(endpoint));

            let mut children = tokio::task::JoinSet::new();
            children.spawn(receive_loop(receiver, Some(entry.cancel.clone()), context));
            let ping = pingresponder::encode_message(pingresponder::PING, &Payload::default()).unwrap();
            peer.send_to(&ping, endpoint).await.unwrap();
            let (size, source) = peer.recv_from(&mut buffer).await.unwrap();
            assert_eq!(source, SocketAddr::V4(endpoint), "PONG must pass exact advertised endpoint matching");
            assert_eq!(pingresponder::parse_message_header(&buffer[..size]).unwrap().0.message_type, pingresponder::PONG);
            children.shutdown().await;
        }).await.expect("interface-bound measurement round trip must complete");
    }

    fn receive_context(tx_event: Sender<OnEvent>) -> ReceiveContext {
        ReceiveContext {
            measurement_routes: MeasurementRoutes::default(),
            measurement_responder: None,
            topology: None,
            drain_count: Arc::new(AtomicUsize::new(0)),
            fail_receive_once: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            pending_delivery: Arc::new(AtomicUsize::new(0)),
            gate: Arc::new(crate::link::controller::DispatchGate::new_open()),
            interface_sockets: Arc::new(Mutex::new(HashMap::from([(
                Ipv4Addr::LOCALHOST,
                interface_socket(),
            )]))),
            peer_state: Arc::new(Mutex::new(PeerState {
                measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1)),
                ..PeerState::default()
            })),
            ttl: 1,
            tx_event,
            last_broadcast_time: Arc::new(Mutex::new(Instant::now())),
            enabled: Arc::new(Mutex::new(true)),
            group_id: 0,
            gateways_changed: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[tokio::test]
    async fn local_measurement_delivery_uses_bound_owner_without_relaxing_discovery_ingress() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut entry = interface_socket();
            entry.identity.index += 1000;
            let receiver = entry.receiver.clone();
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let sockets = Arc::new(Mutex::new(HashMap::from([(
                entry.identity.addr,
                entry.lease(),
            )])));
            let endpoint = receiver.local_addr().unwrap();
            let ping = crate::link::pingresponder::encode_message(
                crate::link::pingresponder::PING,
                &Payload::default(),
            )
            .unwrap();
            let mut bytes = [0; MAX_MESSAGE_SIZE];
            socket.send_to(&ping, endpoint).await.unwrap();
            let (_, _, owner) =
                receive_datagram(&receiver, &sockets, &mut bytes, Some(&entry.cancel))
                    .await
                    .unwrap();
            assert!(Arc::ptr_eq(&owner.unwrap().socket, &entry.socket));
            let discovery = encode_message(
                NodeId::from_array([90; 8]),
                1,
                RESPONSE,
                &Payload::default(),
                0,
            )
            .unwrap();
            socket.send_to(&discovery, endpoint).await.unwrap();
            assert!(
                receive_datagram(&receiver, &sockets, &mut bytes, Some(&entry.cancel))
                    .await
                    .unwrap()
                    .2
                    .is_none()
            );
            sockets
                .lock()
                .unwrap()
                .get_mut(&entry.identity.addr)
                .unwrap()
                .local_delivery = Arc::new(LocalDelivery::default());
            socket.send_to(&ping, endpoint).await.unwrap();
            assert!(
                receive_datagram(&receiver, &sockets, &mut bytes, Some(&entry.cancel))
                    .await
                    .unwrap()
                    .2
                    .is_none(),
                "unknown local-delivery identity must fail closed"
            );
        })
        .await
        .expect("local measurement ownership fixture must complete");
    }

    #[tokio::test]
    async fn measurement_routes_isolate_overlapping_adapter_endpoints_and_reject_stale_hints() {
        let first = interface_socket();
        let mut second = interface_socket();
        second.identity.index += 1;
        second.identity.name = "synthetic-second-adapter".into();
        let mut group = first.lease();
        group.siblings.push(second.lease());
        let sockets = Arc::new(Mutex::new(HashMap::from([(first.identity.addr, group)])));
        let routes = MeasurementRoutes::default();
        let endpoint = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 20808);
        let peer = |byte| PeerState {
            node_state: NodeState {
                node_id: NodeId::from_array([byte; 8]),
                ..NodeState::default()
            },
            measurement_endpoint: Some(endpoint),
            ..PeerState::default()
        };
        let a = peer(41);
        let b = peer(42);
        routes.record(
            a.ident(),
            Some(endpoint),
            30,
            MeasurementRoute {
                #[cfg(test)]
                retire_before_samples: Default::default(),
                entry: first.lease(),
                sockets: sockets.clone(),
            },
        );
        routes.record(
            b.ident(),
            Some(endpoint),
            30,
            MeasurementRoute {
                #[cfg(test)]
                retire_before_samples: Default::default(),
                entry: second.lease(),
                sockets: sockets.clone(),
            },
        );
        let a_route = routes.resolve(&a).unwrap();
        let b_route = routes.resolve(&b).unwrap();
        assert!(Arc::ptr_eq(&a_route.socket(), &first.socket));
        assert!(Arc::ptr_eq(&b_route.socket(), &second.socket));
        assert!(!Arc::ptr_eq(&a_route.dispatch(), &b_route.dispatch()));
        let (a_tx, mut a_rx) = tokio::sync::mpsc::channel(1);
        let (b_tx, mut b_rx) = tokio::sync::mpsc::channel(1);
        a_route
            .dispatch()
            .lock()
            .unwrap()
            .insert(endpoint.into(), a_tx);
        b_route
            .dispatch()
            .lock()
            .unwrap()
            .insert(endpoint.into(), b_tx);
        let pong = crate::link::pingresponder::encode_message(
            crate::link::pingresponder::PONG,
            &Payload::default(),
        )
        .unwrap();
        crate::link::measurement::dispatch_pong(&a_route.dispatch(), &pong, endpoint.into());
        assert_eq!(a_rx.try_recv().unwrap().0, pong);
        assert!(
            b_rx.try_recv().is_err(),
            "same remote endpoint on another adapter must not receive this pong"
        );
        first.cancel.cancel();
        assert!(routes.resolve(&a).is_none());
        assert!(a_route.send(b"stale", endpoint).await.is_err());
        routes.record(b.ident(), Some(endpoint), 0, b_route);
        routes.prune();
        assert!(routes.resolve(&b).is_none());
        assert!(routes
            .0
            .lock()
            .unwrap()
            .get(&(b.ident(), endpoint))
            .is_none());
    }

    #[tokio::test]
    async fn invalidated_measurement_route_does_not_block_completed_sample_channel() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let entry = interface_socket();
            let route = MeasurementRoute {
                #[cfg(test)]
                retire_before_samples: Default::default(),
                entry: entry.lease(),
                sockets: Arc::new(Mutex::new(HashMap::from([(
                    entry.identity.addr,
                    entry.lease(),
                )]))),
            };
            entry.cancel.cancel();
            let endpoint = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808);
            assert!(route.send(b"invalidated", endpoint).await.is_err());
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            let completed = vec![(1.0, 2.0)];
            tx.send(completed.clone()).await.unwrap();
            crate::link::measurement::reset_timer(
                Arc::new(Mutex::new(0)),
                crate::link::clock::Clock::new(),
                Some(route.socket()),
                endpoint,
                Arc::new(Mutex::new(Vec::new())),
                tx.clone(),
                Arc::new(tokio::sync::Notify::new()),
                Some(route),
            )
            .await;
            assert_eq!(rx.try_recv().unwrap(), completed);
            assert!(matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        })
        .await
        .expect("failed routed ping must finish before the result receiver is drained");
    }

    #[tokio::test]
    async fn absent_measurement_endpoint_roundtrips_through_peer_state_wire_messages() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let original = PeerState::default();
            for explicit_absent_entry in [false, true] {
                let mut payload = Payload::from(original.clone());
                if explicit_absent_entry {
                    payload.entries.push(PayloadEntry::MeasurementEndpointV4(
                        crate::link::measurement::MeasurementEndpointV4 { endpoint: None },
                    ));
                }
                let bytes = encode_message(original.ident(), 1, RESPONSE, &payload, 0).unwrap();
                let (header, header_len) = parse_message_header(&bytes).unwrap();
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                receive_peer_state(tx, header, &bytes[header_len..]).await;
                let Some(OnEvent::PeerState(state)) = rx.recv().await else {
                    panic!("expected peer state")
                };
                assert_eq!(state.node_state.node_id, original.node_state.node_id);
                assert_eq!(state.node_state.session_id, original.node_state.session_id);
                assert_eq!(state.measurement_endpoint, None);
                assert_eq!(state.audio_endpoint, None);
            }
        })
        .await
        .expect("absent endpoint wire round trip must complete");
    }

    #[tokio::test]
    async fn messenger_enables_measurement_on_initial_and_reconciled_registrations() {
        let _trace = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_test_writer()
                .finish(),
        );

        use crate::link::{
            clock::Clock, ghostxform::GhostXForm, pingresponder, sessions::SessionId,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let identity = interface_socket().identity;
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let SocketAddr::V4(destination) = peer.local_addr().unwrap() else { panic!("expected IPv4") };
            let wildcard = Arc::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap());
            let SocketAddr::V4(wildcard_endpoint) = wildcard.local_addr().unwrap() else { panic!("expected IPv4") };
            let state = Arc::new(Mutex::new(PeerState { measurement_endpoint: Some(wildcard_endpoint), ..PeerState::default() }));
            let (tx, mut events) = tokio::sync::mpsc::channel(1);
            let mut messenger = Messenger::new_with_interfaces(
                state, tx, Instant::now(), Arc::new(Notify::new()), Arc::new(Mutex::new(true)),
                Some(Ok(vec![identity.clone()])),
            ).unwrap();
            assert_eq!(interface_socket_entries(&messenger.interface_sockets).len(), 1);
            messenger.set_measurement_responder(pingresponder::PingResponder::new(
                wildcard, SessionId::default(), GhostXForm::default(), Clock::new(),
            ));
            let context = messenger.receive_context();
            let initial = interface_socket_entries(&messenger.interface_sockets).pop().unwrap();
            let mut children = tokio::task::JoinSet::new();
            children.spawn(receive_loop(initial.receiver.clone(), Some(initial.cancel.clone()), context.clone()));
            for newly_reconciled in [false, true] {
                if newly_reconciled {
                    reconcile_interfaces(&messenger.multicast_receiver.socket, &context, &mut children, &[]);
                    assert!(interface_socket_entries(&messenger.interface_sockets).is_empty());
                    reconcile_interfaces(&messenger.multicast_receiver.socket, &context, &mut children, std::slice::from_ref(&identity));
                }
                let entries = interface_socket_entries(&messenger.interface_sockets);
                assert_eq!(entries.len(), 1);
                let entry = &entries[0];
                if newly_reconciled { assert!(!Arc::ptr_eq(&initial.socket, &entry.socket)); }
                let endpoint = entry.socket.local_addr().unwrap();
                let remote = PeerState {
                    node_state: NodeState { node_id: NodeId::from_array([91; 8]), ..NodeState::default() },
                    measurement_endpoint: Some(destination), ..PeerState::default()
                };
                let announcement = encode_message(remote.ident(), 30, RESPONSE, &Payload::from(remote.clone()), 0).unwrap();
                for invalid in [vec![0], vec![0; 20], encode_message(remote.ident(), 30, u8::MAX, &Payload::default(), 0).unwrap()] {
                    peer.send_to(&invalid, endpoint).await.unwrap();
                }
                peer.send_to(&announcement, endpoint).await.unwrap();
                assert!(matches!(events.recv().await, Some(OnEvent::PeerState(_))));
                let route = messenger.measurement_routes.resolve(&remote).expect("measurement must retain discovery ingress");
                assert!(Arc::ptr_eq(&route.socket(), &entry.socket));
                let (tx_pong, mut rx_pong) = tokio::sync::mpsc::channel(1);
                route.dispatch().lock().unwrap().insert(destination.into(), tx_pong);
                let ping = pingresponder::encode_message(pingresponder::PING, &Payload::default()).unwrap();
                route.send(&ping, destination).await.unwrap();
                let mut routed = [0; MAX_MESSAGE_SIZE];
                let (_, ping_source) = peer.recv_from(&mut routed).await.unwrap();
                assert_eq!(ping_source, endpoint, "outbound PING must use learned ingress, not wildcard routing");
                let pong = pingresponder::encode_message(pingresponder::PONG, &Payload::default()).unwrap();
                peer.send_to(&pong, endpoint).await.unwrap();
                assert_eq!(rx_pong.recv().await.unwrap(), (pong, destination.into()));

                send_peer_state_via(
                    entry.socket.clone(), messenger.peer_state.clone(), 1, ALIVE, destination,
                    messenger.last_broadcast_time.clone(), 0, Some((entry, &messenger.interface_sockets)),
                ).await;
                let mut buffer = [0; MAX_MESSAGE_SIZE];
                let (size, source) = peer.recv_from(&mut buffer).await.unwrap();
                assert_eq!(source, endpoint);
                let (_, header_len) = parse_message_header(&buffer[..size]).unwrap();
                let payload = parse_payload(&buffer[header_len..size]).unwrap();
                assert!(payload.entries.iter().any(|entry| matches!(entry,
                    PayloadEntry::MeasurementEndpointV4(measurement) if measurement.endpoint.map(SocketAddr::V4) == Some(endpoint))));
                let ping = pingresponder::encode_message(pingresponder::PING, &Payload::default()).unwrap();
                let mut malformed = ping.clone();
                malformed.extend_from_slice(&crate::link::payload::HOST_TIME_HEADER_KEY.to_be_bytes());
                malformed.extend_from_slice(&crate::link::payload::HOST_TIME_SIZE.to_be_bytes());
                peer.send_to(&malformed, endpoint).await.unwrap();
                peer.send_to(&ping, endpoint).await.unwrap();
                let (size, source) = peer.recv_from(&mut buffer).await.unwrap();
                assert_eq!(source, endpoint);
                assert_eq!(pingresponder::parse_message_header(&buffer[..size]).unwrap().0.message_type, pingresponder::PONG);
            }
            reconcile_interfaces(&messenger.multicast_receiver.socket, &context, &mut children, &[]);
            children.shutdown().await;
        }).await.expect("Messenger setup and reconciliation must retain measurement endpoints");
    }

    #[tokio::test]
    async fn saturated_discovery_handoff_keeps_measurement_roundtrips_and_restart_live() {
        use crate::link::{
            clock::Clock, ghostxform::GhostXForm, pingresponder, sessions::SessionId,
        };
        tokio::time::timeout(Duration::from_secs(8), async {
            let identity = interface_socket().identity;
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let SocketAddr::V4(peer_endpoint) = peer.local_addr().unwrap() else {
                panic!("IPv4")
            };
            let wildcard = Arc::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap());
            let SocketAddr::V4(wildcard_endpoint) = wildcard.local_addr().unwrap() else {
                panic!("IPv4")
            };
            let (tx, mut events) = tokio::sync::mpsc::channel(1);
            let mut messenger = Messenger::new_with_interfaces(
                Arc::new(Mutex::new(PeerState {
                    measurement_endpoint: Some(wildcard_endpoint),
                    ..PeerState::default()
                })),
                tx.clone(),
                Instant::now(),
                Arc::new(Notify::new()),
                Arc::new(Mutex::new(true)),
                Some(Ok(vec![identity])),
            )
            .unwrap();
            messenger.set_measurement_responder(pingresponder::PingResponder::new(
                wildcard,
                SessionId::default(),
                GhostXForm::default(),
                Clock::new(),
            ));
            let entry = interface_socket_entries(&messenger.interface_sockets)
                .pop()
                .unwrap();
            let endpoint = entry.socket.local_addr().unwrap();
            let mut children = tokio::task::JoinSet::new();
            children.spawn(receive_loop(
                entry.receiver.clone(),
                Some(entry.cancel.clone()),
                messenger.receive_context(),
            ));
            let remote = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([92; 8]),
                    ..NodeState::default()
                },
                measurement_endpoint: Some(peer_endpoint),
                ..PeerState::default()
            };
            let announcement = encode_message(
                remote.ident(),
                30,
                RESPONSE,
                &Payload::from(remote.clone()),
                0,
            )
            .unwrap();
            peer.send_to(&announcement, endpoint).await.unwrap();
            assert!(matches!(events.recv().await, Some(OnEvent::PeerState(_))));
            tx.send(OnEvent::Byebye(NodeId::from_array([88; 8])))
                .await
                .unwrap();
            for _ in 0..DISCOVERY_HANDOFF_CAPACITY * 2 {
                peer.send_to(&announcement, endpoint).await.unwrap();
            }
            let ping =
                pingresponder::encode_message(pingresponder::PING, &Payload::default()).unwrap();
            peer.send_to(&ping, endpoint).await.unwrap();
            let mut buf = [0; MAX_MESSAGE_SIZE];
            let (size, source) = peer.recv_from(&mut buf).await.unwrap();
            assert_eq!(source, endpoint);
            assert_eq!(
                pingresponder::parse_message_header(&buf[..size])
                    .unwrap()
                    .0
                    .message_type,
                pingresponder::PONG
            );
            let route = messenger.measurement_routes.resolve(&remote).unwrap();
            let (tx_pong, mut rx_pong) = tokio::sync::mpsc::channel(1);
            route
                .dispatch()
                .lock()
                .unwrap()
                .insert(peer_endpoint.into(), tx_pong);
            route.send(&ping, peer_endpoint).await.unwrap();
            let (_, source) = peer.recv_from(&mut buf).await.unwrap();
            assert_eq!(source, endpoint);
            let pong =
                pingresponder::encode_message(pingresponder::PONG, &Payload::default()).unwrap();
            peer.send_to(&pong, endpoint).await.unwrap();
            assert_eq!(rx_pong.recv().await.unwrap(), (pong, peer_endpoint.into()));
            assert_eq!(
                events.len(),
                1,
                "measurement completed without draining discovery events"
            );
            messenger.gate.stop().await;
            while events.try_recv().is_ok() {}
            messenger.gate.start().await;
            assert!(
                events.try_recv().is_err(),
                "old handoff events must not cross restart"
            );
            peer.send_to(&announcement, endpoint).await.unwrap();
            assert!(matches!(events.recv().await, Some(OnEvent::PeerState(_))));
            children.shutdown().await;
        })
        .await
        .expect("full discovery queues must not block measurement traffic or lifecycle stop");
    }

    #[tokio::test]
    async fn wildcard_handoff_discards_invalidated_leases_before_capacity_returns() {
        tokio::time::timeout(Duration::from_secs(8), async {
            let listener = Arc::new(
                PacketSocket::new(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), None).unwrap(),
            );
            let destination = SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                listener.socket.local_addr().unwrap().port(),
            );
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let (tx, mut events) = tokio::sync::mpsc::channel(1);
            tx.send(OnEvent::Byebye(NodeId::from_array([99; 8])))
                .await
                .unwrap();
            let context = receive_context(tx);
            let old = interface_socket_entries(&context.interface_sockets)
                .pop()
                .unwrap();
            let worker = tokio::spawn(receive_loop(listener.clone(), None, context.clone()));
            let state = |byte| PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([byte; 8]),
                    ..NodeState::default()
                },
                measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20809)),
                ..PeerState::default()
            };
            let stale = state(86);
            let announcement =
                encode_message(stale.ident(), 30, ALIVE, &Payload::from(stale.clone()), 0).unwrap();
            for _ in 0..3 {
                peer.send_to(&announcement, destination).await.unwrap();
            }
            let mut buffer = [0; MAX_MESSAGE_SIZE];
            for _ in 0..3 {
                peer.recv_from(&mut buffer).await.unwrap();
            }
            while context.pending_delivery.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            remove_interface(&listener.socket, &context.interface_sockets, &old.identity);
            assert!(
                !worker.is_finished(),
                "wildcard reader must survive registration removal"
            );
            let fresh = interface_socket();
            context
                .interface_sockets
                .lock()
                .unwrap()
                .insert(fresh.identity.addr, fresh.lease());
            assert!(matches!(events.recv().await, Some(OnEvent::Byebye(_))));
            let valid = state(87);
            let announcement = encode_message(
                valid.ident(),
                30,
                RESPONSE,
                &Payload::from(valid.clone()),
                0,
            )
            .unwrap();
            peer.send_to(&announcement, destination).await.unwrap();
            let Some(OnEvent::PeerState(delivered)) = events.recv().await else {
                panic!("expected fresh peer")
            };
            assert_eq!(delivered.node_state.ident(), valid.ident());
            assert!(context.measurement_routes.resolve(&stale).is_none());
            assert!(Arc::ptr_eq(
                &context.measurement_routes.resolve(&valid).unwrap().socket(),
                &fresh.socket
            ));
            assert!(events.try_recv().is_err());
            worker.abort();
            let _ = worker.await;
        })
        .await
        .expect("stale wildcard handoff must be cancelled while event capacity is unavailable");
    }

    async fn queued_route_does_not_revive_after_newer_expiry(ttl: u8) {
        tokio::time::timeout(Duration::from_secs(8), async {
            let entry = interface_socket();
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let listener = Arc::new(
                PacketSocket::new(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), None).unwrap(),
            );
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.send(OnEvent::Byebye(NodeId::default())).await.unwrap();
            let context = receive_context(tx);
            context
                .interface_sockets
                .lock()
                .unwrap()
                .insert(entry.identity.addr, entry.lease());
            let routes = context.measurement_routes.clone();
            let pending = context.pending_delivery.clone();
            let remote = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([93; 8]),
                    ..NodeState::default()
                },
                measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808)),
                ..PeerState::default()
            };
            routes.record(
                remote.ident(),
                remote.measurement_endpoint,
                30,
                MeasurementRoute {
                    #[cfg(test)]
                    retire_before_samples: Default::default(),
                    entry: entry.lease(),
                    sockets: context.interface_sockets.clone(),
                },
            );
            let worker = tokio::spawn(receive_loop(listener.clone(), None, context));
            let destination =
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port());
            peer.send_to(
                &encode_message(
                    remote.ident(),
                    30,
                    RESPONSE,
                    &Payload::from(remote.clone()),
                    0,
                )
                .unwrap(),
                destination,
            )
            .await
            .unwrap();
            while pending.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            peer.send_to(
                &encode_message(
                    remote.ident(),
                    ttl,
                    RESPONSE,
                    &Payload::from(remote.clone()),
                    0,
                )
                .unwrap(),
                destination,
            )
            .await
            .unwrap();
            loop {
                let shorter = routes
                    .0
                    .lock()
                    .unwrap()
                    .get(&(remote.ident(), remote.measurement_endpoint.unwrap()))
                    .is_none_or(|hint| hint.expires < Instant::now() + Duration::from_secs(2));
                if shorter {
                    break;
                }
                tokio::task::yield_now().await;
            }
            if ttl > 0 {
                tokio::time::sleep(Duration::from_millis(1100)).await;
            }
            assert!(routes.resolve(&remote).is_none());
            rx.recv().await.unwrap();
            let fresh = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([94; 8]),
                    ..NodeState::default()
                },
                ..remote.clone()
            };
            peer.send_to(
                &encode_message(fresh.ident(), 2, RESPONSE, &Payload::from(fresh.clone()), 0)
                    .unwrap(),
                destination,
            )
            .await
            .unwrap();
            let next = rx.recv().await;
            let next = if ttl == 0 {
                let Some(OnEvent::PeerState(withdrawn)) = next else {
                    panic!("explicit withdrawal");
                };
                assert_eq!(withdrawn.ttl, 0);
                assert_eq!(withdrawn.node_state.ident(), remote.ident());
                rx.recv().await
            } else {
                next
            };
            let Some(OnEvent::PeerState(state)) = next else {
                panic!("fresh announcement")
            };
            assert_eq!(
                state.node_state.ident(),
                fresh.ident(),
                "older queued announcement must not survive newer withdrawal or expiry"
            );
            assert!(
                routes.resolve(&remote).is_none(),
                "removed hint must not be resurrected"
            );
            worker.abort();
            let _ = worker.await;
        })
        .await
        .expect("superseded queued announcements must remain cancelled after hint removal");
    }

    #[tokio::test]
    async fn queued_positive_ttl_stays_withdrawn_after_newer_ttl_zero() {
        queued_route_does_not_revive_after_newer_expiry(0).await;
    }

    #[tokio::test]
    async fn queued_positive_ttl_stays_absent_after_newer_short_expiry() {
        queued_route_does_not_revive_after_newer_expiry(1).await;
    }

    #[tokio::test]
    async fn pending_revision_index_is_bounded_by_owned_handoff_work() {
        let (route, _cancel) = MeasurementRoute::test_fixture();
        let routes = MeasurementRoutes::default();
        let (tx, rx) = tokio::sync::mpsc::channel(DISCOVERY_HANDOFF_CAPACITY);
        for n in 0..DISCOVERY_HANDOFF_CAPACITY * 8 {
            let peer = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array((n as u64).to_be_bytes()),
                    ..NodeState::default()
                },
                ..PeerState::default()
            };
            let bytes =
                encode_message(peer.ident(), 30, RESPONSE, &Payload::from(peer), 0).unwrap();
            let (header, len) = parse_message_header(&bytes).unwrap();
            queue_peer_state(
                &tx,
                Instant::now(),
                header,
                &bytes[len..],
                Some((&routes, route.clone())),
            );
        }
        assert_eq!(tx.capacity(), 0);
        assert!(routes.1.lock().unwrap().len() <= DISCOVERY_HANDOFF_CAPACITY);
        assert!(
            routes.0.lock().unwrap().is_empty(),
            "unpublished traffic must not create active hints"
        );
        drop(rx);
        routes.observe(NodeId::default(), None, Instant::now(), false);
        assert!(
            routes.1.lock().unwrap().is_empty(),
            "weak revision keys must be reclaimed after work is dropped"
        );
    }

    #[tokio::test]
    async fn cache_sweeps_check_a_shared_topology_only_once() {
        let topology = Arc::new(Topology::new().unwrap());
        let mut entry = interface_socket();
        entry.generation = topology.check().unwrap();
        entry.topology = Some(topology.clone());
        let route = MeasurementRoute {
            #[cfg(test)]
            retire_before_samples: Default::default(),
            entry: entry.lease(),
            sockets: Arc::new(Mutex::new(HashMap::from([(
                entry.identity.addr,
                entry.lease(),
            )]))),
        };
        let routes = MeasurementRoutes::default();
        let peer = PeerState {
            node_state: NodeState {
                node_id: NodeId::from_array([99; 8]),
                ..NodeState::default()
            },
            measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20)),
            ..PeerState::default()
        };
        for n in 0..MAX_MEASUREMENT_ROUTES - 1 {
            routes.record(
                NodeId::from_array((n as u64).to_be_bytes()),
                peer.measurement_endpoint,
                255,
                route.clone(),
            );
        }
        let before = topology.check_count();
        routes.record(peer.ident(), peer.measurement_endpoint, 255, route);
        assert_eq!(topology.check_count() - before, 1);
        let before = topology.check_count();
        assert!(routes.resolve(&peer).is_some());
        assert_eq!(topology.check_count() - before, 1);
        let before = topology.check_count();
        routes.prune();
        assert_eq!(topology.check_count() - before, 1);
        topology.invalidate();
        routes.prune();
        assert!(routes.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unchanged_registration_refreshes_a_failed_local_identity_snapshot() {
        let mut entry = interface_socket();
        entry.local_delivery = Arc::new(LocalDelivery::default());
        let identity = LocalDelivery::identity();
        let index = *identity
            .loopback_indices
            .iter()
            .next()
            .expect("loopback identity");
        let retained = entry.lease();
        assert!(!retained
            .local_delivery
            .accepts(u64::from(index), IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let context = receive_context(tx);
        context
            .interface_sockets
            .lock()
            .unwrap()
            .insert(entry.identity.addr, entry.lease());
        let listener = Arc::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap());
        let mut children = tokio::task::JoinSet::new();
        reconcile_interfaces(
            &listener,
            &context,
            &mut children,
            std::slice::from_ref(&entry.identity),
        );
        assert!(retained
            .local_delivery
            .accepts(u64::from(index), IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!retained
            .local_delivery
            .accepts(u64::from(index), IpAddr::V4(Ipv4Addr::new(203, 0, 113, 19))));
        assert!(Arc::ptr_eq(
            &context
                .interface_sockets
                .lock()
                .unwrap()
                .get(&entry.identity.addr)
                .unwrap()
                .socket,
            &entry.socket
        ));
        children.shutdown().await;
    }

    #[tokio::test]
    async fn same_ingress_adapter_selects_link_local_scope_without_crossing_adapters() {
        let mut routed = interface_socket();
        routed.identity.addr = Ipv4Addr::new(10, 0, 0, 10);
        routed.identity.index = 77;
        let mut local = interface_socket();
        local.identity.addr = Ipv4Addr::new(169, 254, 0, 2);
        local.identity.index = 77;
        let mut foreign = interface_socket();
        foreign.identity.addr = Ipv4Addr::new(169, 254, 0, 1);
        foreign.identity.index = 88;
        let map = HashMap::from([
            (routed.identity.addr, routed.clone()),
            (local.identity.addr, local.clone()),
            (foreign.identity.addr, foreign),
        ]);
        let source = IpAddr::V4(Ipv4Addr::new(169, 254, 4, 8));
        let selected = socket_for_ingress(&map, 77, IpAddr::V4(MULTICAST_ADDR), source).unwrap();
        assert!(Arc::ptr_eq(&selected.socket, &local.socket));
        let unicast =
            socket_for_ingress(&map, 77, IpAddr::V4(routed.identity.addr), source).unwrap();
        assert!(Arc::ptr_eq(&unicast.socket, &routed.socket));
        assert!(socket_for_ingress(&map, 99, IpAddr::V4(MULTICAST_ADDR), source).is_none());
        let mut subnet = interface_socket();
        subnet.identity.addr = Ipv4Addr::new(192, 168, 4, 10);
        subnet.identity.index = 77;
        let identity = LocalIdentity {
            subnets: HashMap::from([
                ((77, routed.identity.addr), Ipv4Addr::new(255, 255, 255, 0)),
                ((77, subnet.identity.addr), Ipv4Addr::new(255, 255, 255, 0)),
            ]),
            ..LocalIdentity::default()
        };
        routed.local_delivery.refresh(&identity);
        subnet.local_delivery.refresh(&identity);
        let map = HashMap::from([
            (routed.identity.addr, routed.clone()),
            (subnet.identity.addr, subnet.clone()),
        ]);
        let selected = socket_for_ingress(
            &map,
            77,
            IpAddr::V4(MULTICAST_ADDR),
            IpAddr::V4(Ipv4Addr::new(192, 168, 4, 2)),
        )
        .unwrap();
        assert!(
            Arc::ptr_eq(&selected.socket, &subnet.socket),
            "a verified subnet match outranks numerical alias order"
        );
    }

    #[tokio::test]
    async fn full_handoff_refreshes_only_published_routes_past_the_original_ttl() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (route, _cancel) = MeasurementRoute::test_fixture(); let routes = MeasurementRoutes::default();
            let peer = PeerState { measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808)), ..PeerState::default() };
            let other = PeerState { node_state: NodeState { node_id: NodeId::from_array([99;8]), ..NodeState::default() }, ..peer.clone() };
            let first = Instant::now(); routes.record(peer.ident(),peer.measurement_endpoint,1,route.clone());
            let (tx,_rx) = tokio::sync::mpsc::channel(1);
            tx.try_send(DiscoveryEvent { event: OnEvent::Byebye(NodeId::default()), ingress: route.clone(), received: first, revision: None }).ok().unwrap();
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                for state in [&peer,&other] {
                    let packet=encode_message(state.ident(),1,RESPONSE,&Payload::from(state.clone()),0).unwrap();
                    let (header,len)=parse_message_header(&packet).unwrap();
                    queue_peer_state(&tx,Instant::now(),header,&packet[len..],Some((&routes,route.clone())));
                }
            }
            assert!(Instant::now()>first+Duration::from_secs(1));
            assert!(routes.resolve(&peer).is_some(), "an admitted peer's existing route must survive fresh traffic while discovery is blocked");
            assert!(routes.resolve(&other).is_none(), "unpublished identity must not add a hint");
            routes.record_received(peer.ident(),peer.measurement_endpoint,first,first+Duration::from_secs(1),route,false);
            assert!(routes.resolve(&peer).is_some(), "older queued publication must not regress a refreshed hint");
            let refreshed=routes.resolve(&peer).unwrap();
            routes.record_received(peer.ident(),None,first,first+Duration::from_secs(1),refreshed,false);
            assert!(routes.resolve(&peer).is_some(), "older endpoint withdrawal must not remove newer hint");
        }).await.unwrap();
    }

    #[tokio::test]
    async fn wildcard_handoff_drops_expired_announcements_instead_of_renewing_ttl() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let entry = interface_socket();
            let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let listener = Arc::new(
                PacketSocket::new(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), None).unwrap(),
            );
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.send(OnEvent::Byebye(NodeId::default())).await.unwrap();
            let context = receive_context(tx);
            context
                .interface_sockets
                .lock()
                .unwrap()
                .insert(entry.identity.addr, entry.lease());
            let pending = context.pending_delivery.clone();
            let routes = context.measurement_routes.clone();
            let worker = tokio::spawn(receive_loop(listener.clone(), None, context));
            let expired = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([90; 8]),
                    ..NodeState::default()
                },
                measurement_endpoint: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20808)),
                ..PeerState::default()
            };
            let fresh = PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([91; 8]),
                    ..NodeState::default()
                },
                ..expired.clone()
            };
            let address =
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port());
            peer.send_to(
                &encode_message(
                    expired.ident(),
                    1,
                    RESPONSE,
                    &Payload::from(expired.clone()),
                    0,
                )
                .unwrap(),
                address,
            )
            .await
            .unwrap();
            while pending.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(1100)).await;
            rx.recv().await.unwrap();
            peer.send_to(
                &encode_message(fresh.ident(), 2, RESPONSE, &Payload::from(fresh.clone()), 0)
                    .unwrap(),
                address,
            )
            .await
            .unwrap();
            let Some(OnEvent::PeerState(state)) = rx.recv().await else {
                panic!("fresh announcement")
            };
            assert_eq!(state.node_state.ident(), fresh.ident());
            assert!(state.expires_at.unwrap() > Instant::now());
            assert!(routes.resolve(&expired).is_none());
            assert!(routes.resolve(&fresh).is_some());
            worker.abort();
            let _ = worker.await;
        })
        .await
        .expect("expired queued state must be dropped without needing downstream capacity");
    }

    #[tokio::test]
    async fn route_cache_bounds_nodes_endpoints_and_prunes_retired_leases() {
        let entry = interface_socket();
        let sockets = Arc::new(Mutex::new(HashMap::from([(
            entry.identity.addr,
            entry.lease(),
        )])));
        let route = MeasurementRoute {
            #[cfg(test)]
            retire_before_samples: Default::default(),
            entry: entry.lease(),
            sockets,
        };
        let routes = MeasurementRoutes::default();
        let peer = NodeId::from_array([77; 8]);
        let endpoint = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20809);
        routes.record(peer, Some(endpoint), 255, route.clone());
        routes.record(peer, Some(endpoint), 0, route.clone());
        assert!(
            routes.0.lock().unwrap().is_empty(),
            "zero TTL must retire an existing hint, not leave its old expiration active"
        );
        for port in 1..=MAX_ROUTES_PER_PEER * 3 {
            routes.record(
                peer,
                Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port as u16)),
                255,
                route.clone(),
            );
        }
        assert_eq!(routes.0.lock().unwrap().len(), MAX_ROUTES_PER_PEER);
        for node in 0..MAX_MEASUREMENT_ROUTES * 2 {
            routes.record(
                NodeId::from_array((node as u64).to_be_bytes()),
                Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20809)),
                255,
                route.clone(),
            );
        }
        assert_eq!(routes.0.lock().unwrap().len(), MAX_MEASUREMENT_ROUTES);
        entry.cancel.cancel();
        routes.prune();
        assert!(routes.0.lock().unwrap().is_empty());
        routes.record(
            peer,
            Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 20809)),
            255,
            route,
        );
        assert!(
            routes.0.lock().unwrap().is_empty(),
            "retired lease must not be cached again"
        );
    }

    #[tokio::test]
    async fn initial_scan_failure_keeps_discovery_available_for_retry() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut context = receive_context(tx.clone());
        let messenger = Messenger::new_with_interfaces(
            context.peer_state.clone(),
            tx,
            Instant::now(),
            Arc::new(Notify::new()),
            context.enabled.clone(),
            Some(Err(std::io::Error::other("injected initial scan failure"))),
        )
        .unwrap();
        assert!(messenger.interface_sockets.lock().unwrap().is_empty());
        context.interface_sockets = messenger.interface_sockets.clone();
        let mut children = tokio::task::JoinSet::new();
        reconcile_interfaces(
            &messenger.multicast_receiver.socket,
            &context,
            &mut children,
            &[interface_socket().identity],
        );
        assert_eq!(messenger.interface_sockets.lock().unwrap().len(), 1);
        reconcile_interfaces(
            &messenger.multicast_receiver.socket,
            &context,
            &mut children,
            &[],
        );
        children.shutdown().await;
    }

    #[tokio::test]
    async fn receiver_rearms_and_accepts_the_first_fresh_packet_after_restart() {
        let socket =
            Arc::new(PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap());
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let context = receive_context(tx);
        let gate = context.gate.clone();
        let drain_count = context.drain_count.clone();
        let cancel = Cancel::default();
        let worker = tokio::spawn(receive_loop(socket.clone(), Some(cancel.clone()), context));
        for cycle in 0..3 {
            let node = NodeId::from_array([42 + cycle; 8]);
            let packet = encode_message(node, 0, BYEBYE, &Payload::default(), 0).unwrap();
            sender
                .send_to(&packet, socket.local_addr().unwrap())
                .await
                .unwrap();
            let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap();
            assert!(
                matches!(event, Some(OnEvent::PeerState(state)) if state.node_state.ident() == node && state.ttl == 0)
            );
            gate.stop().await;
            let drained = drain_count.load(Ordering::SeqCst);
            sender
                .send_to(&packet, socket.local_addr().unwrap())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while drain_count.load(Ordering::SeqCst) == drained {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            gate.start().await;
            assert!(rx.try_recv().is_err());
        }
        gate.stop().await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
    }

    /// A datagram with an out-of-range message type reaches the receive loop
    /// with debug logging on (where the type is looked up by name) and must
    /// not take the loop down: the BYEBYE sent after it still arrives.
    #[tokio::test]
    async fn receive_loop_survives_unknown_message_types() {
        let _log = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_test_writer()
                .finish(),
        );
        let socket =
            Arc::new(PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap());
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let context = receive_context(tx);
        let cancel = Cancel::default();
        let worker = tokio::spawn(receive_loop(socket.clone(), Some(cancel.clone()), context));
        let node = NodeId::from_array([9; 8]);
        let mut bad = encode_message(node, 0, BYEBYE, &Payload::default(), 0).unwrap();
        bad[crate::discovery::messages::PROTOCOL_HEADER_SIZE] = 255;
        sender
            .send_to(&bad, socket.local_addr().unwrap())
            .await
            .unwrap();
        let good = encode_message(node, 0, BYEBYE, &Payload::default(), 0).unwrap();
        sender
            .send_to(&good, socket.local_addr().unwrap())
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the loop is still running after the bad datagram");
        assert!(
            matches!(event, Some(OnEvent::PeerState(state)) if state.node_state.ident() == node && state.ttl == 0)
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn receiver_keeps_discarding_after_acknowledging_preparation() {
        use std::future::Future;
        let socket =
            Arc::new(PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap());
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let context = receive_context(tx);
        let gate = context.gate.clone();
        gate.close();
        let drain_count = context.drain_count.clone();
        let worker = tokio::spawn(receive_loop(socket.clone(), None, context));
        let blocker = gate.subscribe();
        let mut start = Box::pin(gate.start());
        let mut poll_context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(start.as_mut().poll(&mut poll_context).is_pending());
        tokio::time::timeout(Duration::from_secs(5), async {
            while drain_count.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let acknowledged = drain_count.load(Ordering::SeqCst);
        let old = NodeId::from_array([41; 8]);
        let packet = encode_message(old, 0, BYEBYE, &Payload::default(), 0).unwrap();
        sender
            .send_to(&packet, socket.local_addr().unwrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while drain_count.load(Ordering::SeqCst) == acknowledged {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("receiver must drain arrivals while another subscriber delays opening");
        drop(blocker);
        start.await;
        let fresh = NodeId::from_array([42; 8]);
        let packet = encode_message(fresh, 0, BYEBYE, &Payload::default(), 0).unwrap();
        sender
            .send_to(&packet, socket.local_addr().unwrap())
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert!(
            matches!(event, Some(OnEvent::PeerState(state)) if state.node_state.ident() == fresh && state.ttl == 0)
        );
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn blocked_byebye_send_is_cancelled_inside_its_epoch() {
        use std::future::Future;
        let gate = crate::link::controller::DispatchGate::new_open();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let old = NodeId::from_array([1; 8]);
        tx.send(OnEvent::Byebye(old)).await.unwrap();
        let mut work = Box::pin(gate.run_in_epoch(
            gate.epoch(),
            receive_bye_bye(tx.clone(), NodeId::from_array([2; 8])),
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(work.as_mut().poll(&mut context).is_pending());
        gate.close();
        assert!(matches!(
            work.as_mut().poll(&mut context),
            std::task::Poll::Ready(None)
        ));
        drop(work);
        assert!(matches!(rx.recv().await, Some(OnEvent::Byebye(id)) if id == old));
        gate.start().await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn transient_receive_error_does_not_remove_the_worker() {
        let socket =
            Arc::new(PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap());
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (tx_event, mut rx_event) = tokio::sync::mpsc::channel(1);
        let context = receive_context(tx_event);
        context.fail_receive_once.store(true, Ordering::Relaxed);
        let node = NodeId::from_array([42; 8]);
        let packet = encode_message(node, 0, BYEBYE, &Payload::default(), 0).unwrap();
        let worker = tokio::spawn(receive_loop(socket.clone(), None, context));
        for _ in 0..2 {
            sender
                .send_to(&packet, socket.local_addr().unwrap())
                .await
                .unwrap();
        }
        let event = tokio::time::timeout(Duration::from_secs(5), rx_event.recv())
            .await
            .unwrap();
        assert!(
            matches!(event, Some(OnEvent::PeerState(state)) if state.node_state.ident() == node && state.ttl == 0)
        );
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
    }

    pub(super) fn interface_socket() -> InterfaceSocket {
        use network_interface::NetworkInterfaceConfig;
        let interface = network_interface::NetworkInterface::show()
            .unwrap()
            .into_iter()
            .find(|interface| {
                interface
                    .addr
                    .iter()
                    .any(|addr| addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
            })
            .expect("loopback adapter");
        let receiver = Arc::new(
            PacketSocket::new(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
                Some(interface.index),
            )
            .unwrap(),
        );
        InterfaceSocket {
            socket: receiver.socket.clone(),
            receiver,
            identity: Ipv4Interface {
                addr: Ipv4Addr::LOCALHOST,
                index: interface.index,
                name: interface.name,
            },
            cancel: Cancel::default(),
            topology: None,
            generation: 0,
            measurement_responder: false,
            measurement_dispatch: Arc::new(Mutex::new(HashMap::new())),
            local_delivery: Arc::new(LocalDelivery::snapshot()),
            siblings: Vec::new(),
        }
    }

    #[tokio::test]
    async fn restart_broadcasts_after_disable_notifications() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let SocketAddr::V4(destination) = receiver.local_addr().unwrap() else {
            panic!("expected IPv4");
        };
        let interfaces = Arc::new(Mutex::new(HashMap::from([(
            Ipv4Addr::LOCALHOST,
            interface_socket(),
        )])));
        let enabled = Arc::new(Mutex::new(true));
        let notifier = Arc::new(Notify::new());
        let mut broadcast = Box::pin(broadcast_state_loop(
            1,
            1,
            Arc::new(Mutex::new(Instant::now() - Duration::from_secs(2))),
            interfaces,
            Arc::new(Mutex::new(PeerState {
                measurement_endpoint: Some(destination),
                ..PeerState::default()
            })),
            destination,
            notifier.clone(),
            enabled.clone(),
            0,
            false,
        ));
        let mut buf = [0; MAX_MESSAGE_SIZE];
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::select! {
                    _ = &mut broadcast => panic!("disable must not terminate broadcasting"),
                    packet = receiver.recv_from(&mut buf) => {
                        let (size, _) = packet.unwrap();
                        let (header, _) = parse_message_header(&buf[..size]).unwrap();
                        assert_eq!(header.message_type, ALIVE);
                    }
                }
            })
            .await
            .unwrap();

            *enabled.lock().unwrap() = false;
            notifier.notify_waiters();
            // Poll the actual notification path while disabled, without relying
            // on a sleep to guess when a separately spawned task has processed it.
            use std::future::Future;
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(broadcast.as_mut().poll(&mut context).is_pending());
            assert!(receiver.try_recv_from(&mut buf).is_err());
            *enabled.lock().unwrap() = true;

            // Reproduce a disable notification consumed only after re-enable.
            *enabled.lock().unwrap() = false;
            notifier.notify_waiters();
            *enabled.lock().unwrap() = true;
            assert!(broadcast.as_mut().poll(&mut context).is_pending());
        }
    }

    #[tokio::test]
    async fn standalone_broadcast_still_stops_on_notification() {
        use std::future::Future;

        let notifier = Arc::new(Notify::new());
        let mut broadcast = Box::pin(broadcast_state(
            1,
            1,
            Arc::new(Mutex::new(Instant::now())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(PeerState::default())),
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1),
            notifier.clone(),
            Arc::new(Mutex::new(false)),
            0,
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(broadcast.as_mut().poll(&mut context).is_pending());
        notifier.notify_waiters();
        assert!(broadcast.as_mut().poll(&mut context).is_ready());
    }

    #[tokio::test]
    async fn enabled_controller_broadcast_does_not_treat_wakeup_as_terminal() {
        use std::future::Future;

        let notifier = Arc::new(Notify::new());
        let mut broadcast = Box::pin(broadcast_state_loop(
            1,
            1,
            Arc::new(Mutex::new(Instant::now())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(PeerState::default())),
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1),
            notifier.clone(),
            Arc::new(Mutex::new(true)),
            0,
            false,
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(broadcast.as_mut().poll(&mut context).is_pending());
        notifier.notify_waiters();
        assert!(broadcast.as_mut().poll(&mut context).is_pending());
    }

    /// `IP_MULTICAST_ALL=0` is the whole behavioral change in this port, and it is a
    /// socket option with no observable effect on a single-group test host, so nothing
    /// else in the suite would notice it being removed or mis-gated. Read it back off a
    /// socket the helper produced.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ipv4_sockets_are_not_given_groups_they_never_joined() {
        let socket = new_udp_reuseport(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into()).unwrap();

        assert!(
            !socket2::SockRef::from(&socket).multicast_all_v4().unwrap(),
            "new_udp_reuseport must clear IP_MULTICAST_ALL on IPv4 sockets, or the \
             discovery listener keeps receiving multicast for groups it never joined"
        );
    }

    /// `IP_MULTICAST_ALL` is an IPv4-only option, so setting it on an IPv6 socket fails
    /// outright. This helper serves both families, and an earlier revision of this port
    /// gated the call on Linux alone: every IPv6 caller then failed before bind. Keep a
    /// test on the IPv6 path so that mis-gating cannot come back silently.
    #[tokio::test]
    async fn ipv6_sockets_are_still_constructible() {
        new_udp_reuseport(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0))
            .expect("new_udp_reuseport must support IPv6 callers");
    }

    #[tokio::test]
    async fn overlapping_prefix_uses_ingress_not_sender_proximity() {
        let mut wifi = interface_socket();
        wifi.identity.addr = Ipv4Addr::new(10, 42, 0, 1);
        wifi.identity.index = 11;
        let mut ethernet = interface_socket();
        ethernet.identity.addr = Ipv4Addr::new(10, 42, 0, 129);
        ethernet.identity.index = 12;
        let sender = Ipv4Addr::new(10, 42, 0, 130);
        let sockets = HashMap::from([
            (wifi.identity.addr, wifi.clone()),
            (ethernet.identity.addr, ethernet.clone()),
        ]);
        // Negative control: the removed algorithm picks the other adapter.
        let old = sockets
            .values()
            .max_by_key(|entry| {
                (u32::from(entry.identity.addr) ^ u32::from(sender)).leading_zeros()
            })
            .unwrap();
        assert!(Arc::ptr_eq(&old.socket, &ethernet.socket));
        let selected = socket_for_ingress(
            &sockets,
            11,
            IpAddr::V4(MULTICAST_ADDR),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&selected.socket, &wifi.socket));
        let selected = socket_for_ingress(
            &sockets,
            12,
            IpAddr::V4(MULTICAST_ADDR),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&selected.socket, &ethernet.socket));
        assert!(socket_for_ingress(
            &sockets,
            0,
            IpAddr::V4(sender),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        )
        .is_none());
        assert!(socket_for_ingress(
            &sockets,
            13,
            IpAddr::V4(sender),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        )
        .is_none());
    }

    #[tokio::test]
    async fn removed_or_replaced_ingress_never_uses_another_socket() {
        let old = interface_socket();
        let sockets: InterfaceSockets = Arc::new(Mutex::new(HashMap::from([(
            old.identity.addr,
            old.clone(),
        )])));
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let SocketAddr::V4(to) = receiver.local_addr().unwrap() else {
            panic!("IPv4")
        };
        send_registered(&old, &sockets, b"before", to)
            .await
            .unwrap();
        let mut buffer = [0; 64];
        receiver.recv_from(&mut buffer).await.unwrap();

        sockets.lock().unwrap().clear();
        assert_eq!(
            send_registered(&old, &sockets, b"removed", to)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotConnected
        );
        let replacement = interface_socket();
        sockets
            .lock()
            .unwrap()
            .insert(replacement.identity.addr, replacement.clone());
        assert_eq!(
            send_registered(&old, &sockets, b"stale", to)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotConnected
        );
        assert!(receiver.try_recv_from(&mut buffer).is_err());
        send_registered(&replacement, &sockets, b"fresh", to)
            .await
            .unwrap();
        let (size, source) = receiver.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..size], b"fresh");
        assert_eq!(source, replacement.socket.local_addr().unwrap());
    }

    #[tokio::test]
    async fn packet_metadata_and_response_source_are_real_os_values() {
        let route = interface_socket();
        let listener =
            Arc::new(PacketSocket::new(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), None).unwrap());
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (tx, mut events) = tokio::sync::mpsc::channel(2);
        let context = receive_context(tx);
        *context.interface_sockets.lock().unwrap() =
            HashMap::from([(route.identity.addr, route.clone())]);
        let worker = tokio::spawn(receive_loop(listener.clone(), None, context));
        let packet = encode_message(
            NodeId::from_array([42; 8]),
            1,
            ALIVE,
            &Payload::default(),
            0,
        )
        .unwrap();
        sender
            .send_to(
                &packet,
                (Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port()),
            )
            .await
            .unwrap();
        let mut buffer = [0; MAX_MESSAGE_SIZE];
        let (size, source) =
            tokio::time::timeout(Duration::from_secs(5), sender.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(source, route.socket.local_addr().unwrap());
        assert_eq!(
            parse_message_header(&buffer[..size])
                .unwrap()
                .0
                .message_type,
            RESPONSE
        );
        assert!(matches!(events.recv().await, Some(OnEvent::PeerState(_))));
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn scan_replaces_identity_drains_old_packets_and_releases_parked_receivers() {
        let listener =
            PacketSocket::new(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), None).unwrap();
        let (tx, _events) = tokio::sync::mpsc::channel(1);
        let context = receive_context(tx);
        let identity = interface_socket().identity;
        context.interface_sockets.lock().unwrap().clear();
        context.gate.close();
        let mut children = tokio::task::JoinSet::new();
        reconcile_interfaces(
            &listener.socket,
            &context,
            &mut children,
            std::slice::from_ref(&identity),
        );
        let old = context.interface_sockets.lock().unwrap()[&identity.addr].clone();
        assert_eq!(context.gateways_changed.load(Ordering::Relaxed), 1);
        let old_receiver = Arc::downgrade(&old.receiver);
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        sender
            .send_to(
                b"old queued packet",
                (Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port()),
            )
            .await
            .unwrap();
        listener.readable().await.unwrap();
        let mut replacement = identity.clone();
        replacement.name.push_str("-replacement");
        reconcile_interfaces(&listener.socket, &context, &mut children, &[replacement]);
        assert!(old.cancel.is_cancelled());
        assert_eq!(context.gateways_changed.load(Ordering::Relaxed), 2);
        assert!(!Arc::ptr_eq(
            &old.socket,
            &context.interface_sockets.lock().unwrap()[&identity.addr].socket
        ));
        let mut buf = [0; 64];
        assert_eq!(
            listener.try_recv(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(old);
        tokio::time::timeout(Duration::from_secs(5), children.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(old_receiver.upgrade().is_none());

        let remaining =
            Arc::downgrade(&context.interface_sockets.lock().unwrap()[&identity.addr].receiver);
        reconcile_interfaces(&listener.socket, &context, &mut children, &[]);
        assert_eq!(context.gateways_changed.load(Ordering::Relaxed), 3);
        assert!(
            context.interface_sockets.lock().unwrap().is_empty(),
            "an empty successful scan must remove vanished interfaces"
        );
        tokio::time::timeout(Duration::from_secs(5), children.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(remaining.upgrade().is_none());
        context.gate.start().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "helper process for the isolated Linux network-namespace fixture"]
    async fn namespace_peer() {
        assert_eq!(std::env::var("LINK_154_NETNS").as_deref(), Ok("1"));
        let local: Ipv4Addr = std::env::var("LINK_154_PEER_IP").unwrap().parse().unwrap();
        let expected: Ipv4Addr = std::env::var("LINK_154_EXPECTED_IP")
            .unwrap()
            .parse()
            .unwrap();
        if std::env::var("LINK_154_BROADCAST").as_deref() == Ok("1") {
            let socket = new_udp_reuseport(MULTICAST_IP_ANY.into()).unwrap();
            socket.join_multicast_v4(MULTICAST_ADDR, local).unwrap();
            println!("LINK154_READY");
            let mut bytes = [0; MAX_MESSAGE_SIZE];
            let (size, from) =
                tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                from.to_string(),
                std::env::var("LINK_154_EXPECTED_ENDPOINT").unwrap()
            );
            assert_eq!(
                parse_message_header(&bytes[..size]).unwrap().0.message_type,
                ALIVE
            );
            return;
        }
        let socket = new_udp_reuseport(SocketAddrV4::new(local, 0).into()).unwrap();
        let packet = encode_message(
            NodeId::from_array([42; 8]),
            1,
            ALIVE,
            &Payload::default(),
            0,
        )
        .unwrap();
        socket
            .send_to(&packet, (MULTICAST_ADDR, LINK_PORT))
            .await
            .unwrap();
        let mut buf = [0; MAX_MESSAGE_SIZE];
        let (size, src) = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            src.ip(),
            IpAddr::V4(expected),
            "response must leave the actual ingress adapter"
        );
        assert_eq!(
            parse_message_header(&buf[..size]).unwrap().0.message_type,
            RESPONSE
        );
    }

    #[cfg(target_os = "linux")]
    async fn run_namespace_peer(namespace: &str, local: &str, expected: &str) {
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new("ip")
                .args(["netns", "exec", namespace])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "discovery::messenger::tests::namespace_peer",
                    "--nocapture",
                ])
                .env("LINK_154_PEER_IP", local)
                .env("LINK_154_EXPECTED_IP", expected)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires .github/scripts/test-discovery-ingress.sh in an isolated network namespace"]
    async fn multihomed_namespace_ingress_and_churn() {
        assert_eq!(std::env::var("LINK_154_NETNS").as_deref(), Ok("1"));
        let listener = Arc::new(PacketSocket::new(MULTICAST_IP_ANY, None).unwrap());
        let (tx, _events) = tokio::sync::mpsc::channel(32);
        let mut context = receive_context(tx);
        context.topology = Some(Arc::new(Topology::new().unwrap()));
        context.interface_sockets.lock().unwrap().clear();
        let mut children = tokio::task::JoinSet::new();
        reconcile_interfaces(
            &listener.socket,
            &context,
            &mut children,
            &scan_discovery_interfaces().unwrap(),
        );
        assert_eq!(context.interface_sockets.lock().unwrap().len(), 2);
        children.spawn(receive_loop(listener.clone(), None, context.clone()));
        for _ in 0..3 {
            // Each sender is closer to the *other* adapter's address.
            run_namespace_peer("link154-a", "10.42.0.130", "10.42.0.1").await;
            run_namespace_peer("link154-b", "10.42.0.2", "10.42.0.129").await;
            context.gate.stop().await;
            context.gate.start().await;
        }
        let old = context.interface_sockets.lock().unwrap()[&Ipv4Addr::new(10, 42, 0, 1)].clone();
        assert!(tokio::process::Command::new("ip")
            .args(["link", "del", "veth-a"])
            .status()
            .await
            .unwrap()
            .success());
        // Before the scanner notices removal, the OS must reject the pinned
        // egress rather than route this through the still-live overlapping B.
        assert!(send_registered(
            &old,
            &context.interface_sockets,
            b"must not reroute",
            SocketAddrV4::new(Ipv4Addr::new(10, 42, 0, 2), 20809)
        )
        .await
        .is_err());
        reconcile_interfaces(
            &listener.socket,
            &context,
            &mut children,
            &scan_discovery_interfaces().unwrap(),
        );
        assert!(old.cancel.is_cancelled());
        assert!(tokio::process::Command::new("bash")
            .args([".github/scripts/test-discovery-ingress.sh", "--replace-a"])
            .status()
            .await
            .unwrap()
            .success());
        reconcile_interfaces(
            &listener.socket,
            &context,
            &mut children,
            &scan_discovery_interfaces().unwrap(),
        );
        assert!(send_registered(
            &old,
            &context.interface_sockets,
            b"stale generation",
            SocketAddrV4::new(Ipv4Addr::new(10, 42, 0, 130), 20809)
        )
        .await
        .is_err());
        run_namespace_peer("link154-a", "10.42.0.130", "10.42.0.1").await;
        // Reuse the complete identity without allowing a reconciliation between
        // delete and add. A snapshot-only generation detector cannot pass this.
        let old = interface_socket_entries(&context.interface_sockets)
            .into_iter()
            .find(|entry| entry.identity.addr == Ipv4Addr::new(10, 42, 0, 1))
            .unwrap();
        let before = scan_discovery_interfaces().unwrap();
        assert!(tokio::process::Command::new("bash")
            .args([".github/scripts/test-discovery-ingress.sh", "--reuse-a"])
            .env("LINK_154_INDEX", old.identity.index.to_string())
            .status()
            .await
            .unwrap()
            .success());
        assert_eq!(scan_discovery_interfaces().unwrap(), before);
        assert!(send_registered(
            &old,
            &context.interface_sockets,
            b"same identity, stale lease",
            SocketAddrV4::new(Ipv4Addr::new(10, 42, 0, 130), 20809)
        )
        .await
        .is_err());
        reconcile_interfaces(&listener.socket, &context, &mut children, &before);
        run_namespace_peer("link154-a", "10.42.0.130", "10.42.0.1").await;

        assert!(tokio::process::Command::new("ip")
            .args(["addr", "add", "10.42.0.1/24", "dev", "veth-b"])
            .status()
            .await
            .unwrap()
            .success());
        let duplicates: Vec<_> = scan_discovery_interfaces()
            .unwrap()
            .into_iter()
            .filter(|interface| interface.addr == Ipv4Addr::new(10, 42, 0, 1))
            .collect();
        assert_eq!(
            duplicates.len(),
            2,
            "enumeration must retain duplicate addresses"
        );
        reconcile_interfaces(&listener.socket, &context, &mut children, &duplicates);
        assert_eq!(context.interface_sockets.lock().unwrap().len(), 1);
        assert_eq!(
            interface_socket_entries(&context.interface_sockets).len(),
            2
        );
        run_namespace_peer("link154-a", "10.42.0.130", "10.42.0.1").await;
        run_namespace_peer("link154-b", "10.42.0.2", "10.42.0.1").await;
        for (namespace, local, name) in [
            ("link154-a", "10.42.0.130", "veth-a"),
            ("link154-b", "10.42.0.2", "veth-b"),
        ] {
            let entry = interface_socket_entries(&context.interface_sockets)
                .into_iter()
                .find(|entry| entry.identity.name == name)
                .unwrap();
            let mut command = tokio::process::Command::new("ip");
            command
                .args(["netns", "exec", namespace])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "discovery::messenger::tests::namespace_peer",
                    "--nocapture",
                ])
                .env("LINK_154_PEER_IP", local)
                .env("LINK_154_EXPECTED_IP", "10.42.0.1")
                .env("LINK_154_BROADCAST", "1")
                .env(
                    "LINK_154_EXPECTED_ENDPOINT",
                    entry.socket.local_addr().unwrap().to_string(),
                );
            fixture_broadcast(command, &entry, &context).await;
        }
        context.gate.stop().await;
        reconcile_interfaces(&listener.socket, &context, &mut children, &[]);
        children.shutdown().await;
    }

    #[tokio::test]
    async fn identical_snapshot_invalidates_queued_leases_and_cleans_up() {
        let listener = Arc::new(PacketSocket::new(MULTICAST_IP_ANY, None).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut context = receive_context(tx);
        context.interface_sockets.lock().unwrap().clear();
        let topology = Arc::new(Topology::new().unwrap());
        context.topology = Some(topology.clone());
        let identity = interface_socket().identity;
        let mut children = tokio::task::JoinSet::new();
        reconcile_interfaces(
            &listener.socket,
            &context,
            &mut children,
            std::slice::from_ref(&identity),
        );
        let old = interface_socket_entries(&context.interface_sockets)
            .pop()
            .unwrap();
        let weak = Arc::downgrade(&old.receiver);
        context.gate.stop().await;
        topology.invalidate();
        assert!(send_registered(
            &old,
            &context.interface_sockets,
            b"stale",
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345)
        )
        .await
        .is_err());
        reconcile_interfaces(&listener.socket, &context, &mut children, &[identity]);
        assert!(old.cancel.is_cancelled());
        let fresh = interface_socket_entries(&context.interface_sockets)
            .pop()
            .unwrap();
        assert!(fresh.is_current().unwrap());
        assert!(!Arc::ptr_eq(&old.socket, &fresh.socket));
        drop(old);
        tokio::time::timeout(Duration::from_secs(5), children.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(weak.upgrade().is_none());
        context.gate.start().await;
        reconcile_interfaces(&listener.socket, &context, &mut children, &[]);
        children.shutdown().await;
    }

    #[tokio::test]
    async fn retired_receiver_cannot_relabel_queued_data_with_a_new_lease() {
        let old = interface_socket();
        let fresh = interface_socket();
        let map = Arc::new(Mutex::new(HashMap::from([(fresh.identity.addr, fresh)])));
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        sender
            .send_to(b"old queue", old.socket.local_addr().unwrap())
            .await
            .unwrap();
        old.cancel.cancel();
        let mut buffer = [0; MAX_MESSAGE_SIZE];
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            receive_datagram(&old.receiver, &map, &mut buffer, Some(&old.cancel)),
        )
        .await
        .unwrap()
        .err()
        .expect("retired receiver must reject queued data");
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn duplicate_address_groups_preserve_ingress_and_independent_leases() {
        let mut a = interface_socket();
        let mut b = interface_socket();
        a.identity.index = 101;
        b.identity.index = 102;
        let b_socket = b.socket.clone();
        a.siblings.push(b);
        let map = HashMap::from([(Ipv4Addr::LOCALHOST, a)]);
        let first = socket_for_ingress(
            &map,
            101,
            IpAddr::V4(MULTICAST_ADDR),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        let second = socket_for_ingress(
            &map,
            102,
            IpAddr::V4(MULTICAST_ADDR),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        assert!(first.siblings.is_empty() && second.siblings.is_empty());
        assert!(Arc::ptr_eq(&second.socket, &b_socket));
        let map = Arc::new(Mutex::new(map));
        assert_eq!(interface_socket_entries(&map).len(), 2);
        let listener = Arc::new(PacketSocket::new(MULTICAST_IP_ANY, None).unwrap());
        remove_interface(&listener.socket, &map, &first.identity);
        assert!(first.cancel.is_cancelled());
        assert!(!second.cancel.is_cancelled());
        assert!(Arc::ptr_eq(
            &map.lock().unwrap()[&Ipv4Addr::LOCALHOST].socket,
            &b_socket
        ));
        // A managed send must also search the full group, not only its root.
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let SocketAddr::V4(to) = receiver.local_addr().unwrap() else {
            unreachable!()
        };
        send_registered(&second, &map, b"survivor", to)
            .await
            .unwrap();
        let mut bytes = [0; 16];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), receiver.recv(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            8
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[tokio::test]
    #[ignore = "requires disposable CI adapters configured by the platform fixture"]
    async fn multihomed_adapter_ingress_and_churn() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init()
            .unwrap();
        assert_eq!(
            std::env::var("LINK_154_ADAPTER_FIXTURE").as_deref(),
            Ok("1")
        );
        let listener = Arc::new(PacketSocket::new(MULTICAST_IP_ANY, None).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        let mut context = receive_context(tx);
        context.interface_sockets.lock().unwrap().clear();
        let topology = Arc::new(Topology::new().unwrap());
        context.topology = Some(topology.clone());
        let all = scan_discovery_interfaces().unwrap();
        let host_a = all
            .iter()
            .find(|entry| entry.addr == Ipv4Addr::new(10, 42, 0, 1))
            .unwrap()
            .clone();
        let host_b = all
            .iter()
            .find(|entry| entry.addr == Ipv4Addr::new(10, 42, 0, 129))
            .unwrap()
            .clone();
        assert_ne!(host_a.index, host_b.index);
        let mut children = tokio::task::JoinSet::new();
        reconcile_interfaces(
            &listener.socket,
            &context,
            &mut children,
            &[host_a.clone(), host_b.clone()],
        );
        assert_eq!(
            interface_socket_entries(&context.interface_sockets).len(),
            2
        );
        eprintln!(
            "fixture adapters: {:?}, {:?}; generation {}",
            host_a,
            host_b,
            topology.check().unwrap()
        );
        children.spawn(receive_loop(listener.clone(), None, context.clone()));
        for _ in 0..3 {
            adapter_peer(Ipv4Addr::new(10, 42, 0, 130), &host_a, &context).await;
            adapter_peer(Ipv4Addr::new(10, 42, 0, 2), &host_b, &context).await;
            context.gate.stop().await;
            context.gate.start().await;
        }
        let old = interface_socket_entries(&context.interface_sockets)
            .into_iter()
            .find(|entry| entry.identity == host_a)
            .unwrap();
        #[cfg(target_os = "macos")]
        let status = tokio::process::Command::new("bash")
            .args([".github/scripts/test-discovery-ingress-macos.sh", "--churn"])
            .status()
            .await
            .unwrap();
        #[cfg(windows)]
        let status = tokio::process::Command::new("pwsh")
            .args([
                "-File",
                ".github/scripts/test-discovery-ingress-windows.ps1",
                "-Churn",
            ])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        tokio::time::timeout(Duration::from_secs(10), topology.changed(old.generation))
            .await
            .unwrap()
            .unwrap();
        assert!(
            scan_discovery_interfaces().unwrap().contains(&host_a),
            "the final snapshot still has the original identity"
        );
        assert!(send_registered(
            &old,
            &context.interface_sockets,
            b"stale",
            SocketAddrV4::new(Ipv4Addr::new(10, 42, 0, 130), 20809)
        )
        .await
        .is_err());
        reconcile_interfaces(
            &listener.socket,
            &context,
            &mut children,
            &[host_a.clone(), host_b.clone()],
        );
        adapter_peer(Ipv4Addr::new(10, 42, 0, 130), &host_a, &context).await;
        let duplicates: Vec<_> = scan_discovery_interfaces()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.addr == Ipv4Addr::new(10, 42, 0, 9))
            .collect();
        #[cfg(windows)]
        if std::env::var("LINK_154_DUPLICATE_REJECTED").as_deref() == Ok("5010") {
            assert_eq!(duplicates.len(), 1);
            eprintln!("Windows runner rejected duplicate-address setup with ERROR_OBJECT_ALREADY_EXISTS; overlap/churn passed, duplicate network behavior remains unvalidated");
            context.gate.stop().await;
            reconcile_interfaces(&listener.socket, &context, &mut children, &[]);
            children.shutdown().await;
            return;
        }
        assert_eq!(duplicates.len(), 2);
        reconcile_interfaces(&listener.socket, &context, &mut children, &duplicates);
        assert_eq!(context.interface_sockets.lock().unwrap().len(), 1);
        assert_eq!(
            interface_socket_entries(&context.interface_sockets).len(),
            2
        );
        for (local, index) in [
            (Ipv4Addr::new(10, 42, 0, 130), host_a.index),
            (Ipv4Addr::new(10, 42, 0, 2), host_b.index),
        ] {
            adapter_peer(
                local,
                duplicates
                    .iter()
                    .find(|entry| entry.index == index)
                    .unwrap(),
                &context,
            )
            .await;
        }
        #[cfg(target_os = "macos")]
        for entry in interface_socket_entries(&context.interface_sockets) {
            let mut command = feth_peer_command(Ipv4Addr::UNSPECIFIED, &entry.identity, &context);
            command.arg("--broadcast");
            let output = fixture_broadcast(command, &entry, &context).await;
            assert_eq!(
                parse_message_header(&output.stdout).unwrap().0.message_type,
                ALIVE
            );
        }
        context.gate.stop().await;
        reconcile_interfaces(&listener.socket, &context, &mut children, &[]);
        children.shutdown().await;
    }

    #[cfg(windows)]
    async fn adapter_peer(local: Ipv4Addr, host: &Ipv4Interface, context: &ReceiveContext) {
        eprintln!("peer {} expects response from {:?}", local, host);
        let peer = scan_discovery_interfaces()
            .unwrap()
            .into_iter()
            .find(|entry| entry.addr == local)
            .unwrap();
        let socket = PacketSocket::new(SocketAddrV4::new(local, 0), Some(peer.index)).unwrap();
        let expected = interface_socket_entries(&context.interface_sockets)
            .into_iter()
            .find(|entry| entry.identity == *host)
            .unwrap()
            .socket
            .local_addr()
            .unwrap();
        let packet = encode_message(
            NodeId::from_array([42; 8]),
            1,
            ALIVE,
            &Payload::default(),
            0,
        )
        .unwrap();
        socket
            .send_to(&packet, (MULTICAST_ADDR, LINK_PORT))
            .await
            .unwrap();
        let mut bytes = [0; MAX_MESSAGE_SIZE];
        let (size, info) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                socket.readable().await.unwrap();
                match socket.try_recv(&mut bytes) {
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    result => break result.unwrap(),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            info.addr_src, expected,
            "response must use the ingress registration's unique endpoint"
        );
        assert_eq!(
            info.if_index,
            u64::from(peer.index),
            "reply must arrive on the configured peer adapter"
        );
        eprintln!(
            "response endpoint {:?}, local delivery index {}",
            info.addr_src, info.if_index
        );
        assert_eq!(
            parse_message_header(&bytes[..size]).unwrap().0.message_type,
            RESPONSE
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn fixture_broadcast(
        mut command: tokio::process::Command,
        entry: &InterfaceSocket,
        context: &ReceiveContext,
    ) -> std::process::Output {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut child = command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut reader = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                assert_ne!(
                    reader.read_line(&mut line).await.unwrap(),
                    0,
                    "broadcast peer exited before readiness"
                );
                if line.trim() == "LINK154_READY" {
                    break;
                }
                line.clear();
            }
            send_peer_state_via(
                entry.socket.clone(),
                context.peer_state.clone(),
                1,
                ALIVE,
                SocketAddrV4::new(MULTICAST_ADDR, LINK_PORT),
                context.last_broadcast_time.clone(),
                0,
                Some((entry, &context.interface_sockets)),
            )
            .await;
            let mut bytes = Vec::new();
            let (read, output) =
                tokio::join!(reader.read_to_end(&mut bytes), child.wait_with_output());
            read.unwrap();
            let mut output = output.unwrap();
            output.stdout = bytes;
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            output
        })
        .await
        .unwrap()
    }

    #[cfg(target_os = "macos")]
    fn feth_peer_command(
        local: Ipv4Addr,
        host: &Ipv4Interface,
        context: &ReceiveContext,
    ) -> tokio::process::Command {
        let peer_port = match host.name.as_str() {
            "feth1540" => "feth1541",
            "feth1542" => "feth1543",
            other => panic!("unexpected fixture adapter {other}"),
        };
        let expected = interface_socket_entries(&context.interface_sockets)
            .into_iter()
            .find(|entry| entry.identity == *host)
            .unwrap()
            .socket
            .local_addr()
            .unwrap();
        let packet = encode_message(
            NodeId::from_array([42; 8]),
            1,
            ALIVE,
            &Payload::default(),
            0,
        )
        .unwrap();
        let payload: String = packet.iter().map(|byte| format!("{byte:02x}")).collect();
        let mut command = tokio::process::Command::new(std::env::var("LINK_154_PYTHON").unwrap());
        command
            .arg(".github/scripts/discovery-feth-peer.py")
            .args([
                "--interface",
                peer_port,
                "--local",
                &local.to_string(),
                "--group",
                &MULTICAST_ADDR.to_string(),
                "--expected",
                &expected.to_string(),
                "--payload",
                &payload,
            ])
            .kill_on_drop(true);
        command
    }

    #[cfg(target_os = "macos")]
    async fn adapter_peer(local: Ipv4Addr, host: &Ipv4Interface, context: &ReceiveContext) {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            feth_peer_command(local, host, context).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            parse_message_header(&output.stdout).unwrap().0.message_type,
            RESPONSE
        );
    }

    // Covers the send path itself, not just `encode_message`. Upstream's
    // `sendUdpMessage` moved the encode call inside its `try` block so an
    // unencodable payload is logged and dropped rather than propagating out
    // of the send. If this branch ever regresses to `unwrap()`, the oversized
    // case below panics and this test fails.
    #[tokio::test]
    async fn send_message_drops_an_oversized_payload_instead_of_panicking() {
        use crate::discovery::messages::{ALIVE, MAX_MESSAGE_SIZE};
        use crate::link::beats::Beats;
        use crate::link::payload::PayloadEntry;
        use crate::link::tempo::Tempo;
        use crate::link::timeline::{Timeline, TIMELINE_SIZE};

        let socket = interface_socket().socket;
        // A real receiver, so the "well-formed payload still sends" half of
        // this test cannot be satisfied by an unconditional early return.
        let receiver = new_udp_reuseport(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let to = match receiver.local_addr().unwrap() {
            SocketAddr::V4(addr) => addr,
            other => panic!("expected an IPv4 receiver address, got {}", other),
        };
        let from = NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]);

        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: chrono::Duration::zero(),
        };
        let mut oversized = Payload::default();
        for _ in 0..(MAX_MESSAGE_SIZE / TIMELINE_SIZE as usize + 1) {
            oversized.entries.push(PayloadEntry::Timeline(timeline));
        }

        let result = send_message(socket.clone(), from, 5, ALIVE, &oversized, to, 0).await;
        assert!(
            result.is_ok(),
            "an oversized payload must be dropped, not surfaced as an error"
        );
        // The dropped/sent distinction is what keeps `send_peer_state` from
        // advancing `last_broadcast_time` for a message that never went out.
        assert!(
            !send_message_reporting(socket.clone(), from, 5, ALIVE, &oversized, to, 0, None)
                .await
                .unwrap(),
            "an oversized payload must report as dropped, not sent"
        );

        // Guard against the inverse regression: an unconditional early return
        // would also satisfy the assertion above, so prove a well-formed
        // payload actually reaches the wire.
        let small = Payload::default();
        assert!(
            send_message_reporting(socket, from, 5, ALIVE, &small, to, 0, None)
                .await
                .unwrap(),
            "a well-formed payload must report as sent"
        );

        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("a well-formed message should have been sent")
        .expect("receiving the well-formed message should succeed")
        .0;
        assert!(received > 0);
        // Exactly one datagram: the oversized message must not have been sent.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                receiver.recv_from(&mut buf),
            )
            .await
            .is_err(),
            "the oversized message must be dropped, not transmitted"
        );
    }

    // Mirrors upstream's `tst_PeerGateways.cpp` `CallGatewaysChangedOnEnable` /
    // `EmptyIfNoInterfaces` sections, which assert `changedCount == 1` right after
    // `PeerGateways::enable(true)` populates its initial gateway set.
    #[tokio::test]
    async fn new_counts_initial_gateway_population_as_one_change() {
        let peer_state = Arc::new(Mutex::new(PeerState {
            node_state: NodeState::default(),
            measurement_endpoint: None,
            audio_endpoint: None,
        }));
        let (tx_event, _rx_event) = tokio::sync::mpsc::channel(16);

        let messenger = Messenger::new(
            peer_state,
            tx_event,
            Instant::now(),
            Arc::new(Notify::new()),
            Arc::new(Mutex::new(true)),
        )
        .unwrap();

        assert_eq!(messenger.gateways_changed.load(Ordering::Relaxed), 1);
    }
}
