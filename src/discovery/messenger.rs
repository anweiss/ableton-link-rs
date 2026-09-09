use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
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
    ingress::PacketSocket,
    messages::{
        encode_message, parse_message_header, parse_payload, MessageHeader, MessageType,
        SessionGroupId, ALIVE, BYEBYE, MAX_MESSAGE_SIZE, RESPONSE,
    },
    peers::PeerState,
    LINK_PORT, MULTICAST_ADDR, MULTICAST_IP_ANY,
};
use crate::platform::network::{scan_discovery_interfaces, Ipv4Interface};

// Safe UDP socket creation using socket2 and safe options
pub fn new_udp_reuseport(addr: SocketAddr) -> Result<UdpSocket, std::io::Error> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };

    let udp_sock = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;

    udp_sock.set_reuse_address(true)?;

    // Set SO_REUSEPORT on Unix systems so multiple sockets (discovery listener,
    // send_byebye, etc.) can bind to the same multicast port concurrently.
    #[cfg(unix)]
    udp_sock.set_reuse_port(true)?;

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

    // Convert to std::net::UdpSocket and then to tokio::net::UdpSocket
    let std_socket: std::net::UdpSocket = udp_sock.into();
    std_socket.try_into()
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

/// A send/receive socket bound to a single network interface.
#[derive(Clone)]
pub struct InterfaceSocket {
    socket: Arc<UdpSocket>,
    receiver: Arc<PacketSocket>,
    identity: Ipv4Interface,
    cancel: Cancel,
}

/// The set of per-interface sockets, keyed by the interface address they are bound to.
pub type InterfaceSockets = Arc<Mutex<HashMap<Ipv4Addr, InterfaceSocket>>>;

pub struct Messenger {
    multicast_receiver: Arc<PacketSocket>,
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
}

impl Messenger {
    pub(crate) fn discard_queued_datagrams(&self) {
        if let Some(socket) = &self.interface {
            drain_socket(socket);
        }
        for entry in interface_socket_entries(&self.interface_sockets) {
            drain_socket(&entry.socket);
        }
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

        for interface in scan_discovery_interfaces()? {
            match add_interface(&socket, &interface_sockets, interface.clone()) {
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

    pub(crate) fn listen_owned(&self) -> impl std::future::Future<Output = ()> + '_ {
        let multicast_socket = self.multicast_receiver.clone();
        let interface_sockets = self.interface_sockets.clone();
        let peer_state = self.peer_state.clone();
        let ttl = self.ttl;
        let tx_event = self.tx_event.clone();
        let last_broadcast_time = self.last_broadcast_time.clone();
        let enabled = self.enabled.clone();
        let group_id = self.group_id;

        let _n = self.notifier.clone();

        let context = ReceiveContext {
            interface_sockets: interface_sockets.clone(),
            peer_state: peer_state.clone(),
            ttl,
            tx_event: tx_event.clone(),
            last_broadcast_time: last_broadcast_time.clone(),
            enabled: enabled.clone(),
            group_id,
            gateways_changed: self.gateways_changed.clone(),
            gate: self.gate.clone(),
            #[cfg(test)]
            fail_receive_once: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            drain_count: Arc::new(AtomicUsize::new(0)),
        };

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
    #[cfg(test)]
    drain_count: Arc<AtomicUsize>,
    #[cfg(test)]
    fail_receive_once: Arc<AtomicBool>,
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
    lock_map(sockets, |sockets| sockets.values().cloned().collect()).unwrap_or_default()
}

/// Join the multicast group on `addr` and create the ephemeral socket used to send
/// discovery messages through that interface.
fn add_interface(
    multicast_socket: &Arc<UdpSocket>,
    interface_sockets: &InterfaceSockets,
    identity: Ipv4Interface,
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
    };

    match interface_sockets.lock() {
        Ok(mut sockets) => {
            if sockets.contains_key(&addr) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "interface is already registered",
                ));
            }
            // Serialize queue draining, membership publication, receive/route
            // capture and sends. Old queued metadata must not acquire a newly
            // registered socket generation after a scan.
            multicast_socket.join_multicast_v4(MULTICAST_ADDR, addr)?;
            drain_socket(multicast_socket);
            sockets.insert(addr, entry.clone());
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
    addr: Ipv4Addr,
) {
    match interface_sockets.lock() {
        Ok(mut sockets) => {
            if let Some(entry) = sockets.remove(&addr) {
                entry.cancel.cancel();
                if let Err(error) = multicast_socket.leave_multicast_v4(MULTICAST_ADDR, addr) {
                    warn!(
                        "failed to leave discovery membership on {}: {}",
                        addr, error
                    );
                }
                drain_socket(multicast_socket);
                info!("left Ableton Link multicast group on interface {}", addr);
            }
        }
        Err(_) => warn!("cannot remove discovery interface: socket map is poisoned"),
    }
}

/// Keep the per-interface sockets in sync with the interfaces of the host.
async fn interface_scan(multicast_socket: Arc<UdpSocket>, context: ReceiveContext) {
    let mut children = tokio::task::JoinSet::new();
    let mut interval = tokio::time::interval(INTERFACE_SCAN_PERIOD);
    interval.tick().await;

    loop {
        select! {
            _ = interval.tick() => {}
            result = children.join_next(), if !children.is_empty() => {
                if let Some(Err(error)) = result {
                    warn!("interface receive worker failed: {}", error);
                }
                continue;
            }
        }

        let current = match tokio::task::spawn_blocking(scan_discovery_interfaces).await {
            Ok(Ok(current)) => current,
            Ok(Err(error)) => {
                warn!("discovery interface scan failed: {}", error);
                continue;
            }
            Err(error) => {
                warn!("discovery interface scan task failed: {}", error);
                continue;
            }
        };

        reconcile_interfaces(&multicast_socket, &context, &mut children, &current);
    }
}

fn reconcile_interfaces(
    multicast_socket: &Arc<UdpSocket>,
    context: &ReceiveContext,
    children: &mut tokio::task::JoinSet<()>,
    current: &[Ipv4Interface],
) {
    let known = lock_map(&context.interface_sockets, |sockets| {
        sockets
            .values()
            .map(|entry| entry.identity.clone())
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    let stale_addrs: Vec<_> = known
        .iter()
        .filter(|addr| !current.contains(addr))
        .cloned()
        .collect();
    let new_addrs: Vec<_> = current
        .iter()
        .filter(|addr| !known.contains(addr))
        .cloned()
        .collect();

    for addr in &stale_addrs {
        remove_interface(multicast_socket, &context.interface_sockets, addr.addr);
    }

    for addr in &new_addrs {
        match add_interface(multicast_socket, &context.interface_sockets, addr.clone()) {
            Ok(entry) => {
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
            Err(e) => warn!("failed to set up interface {:?}: {}", addr, e),
        }

        // Mirrors upstream's `PeerGateways::Callback::operator()`, which fires
        // `gatewaysChanged()` once per scan pass (not once per interface) when
        // the interface set actually changed.
        if !stale_addrs.is_empty() || !new_addrs.is_empty() {
            context.gateways_changed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn socket_for_ingress(
    sockets: &HashMap<Ipv4Addr, InterfaceSocket>,
    index: u64,
    destination: IpAddr,
) -> Option<InterfaceSocket> {
    sockets
        .values()
        .filter(|entry| u64::from(entry.identity.index) == index && !entry.cancel.is_cancelled())
        // With multiple addresses on one adapter, prefer the exact unicast
        // destination, otherwise a stable local address; never use the source.
        .min_by_key(|entry| {
            (
                IpAddr::V4(entry.identity.addr) != destination,
                entry.identity.addr,
            )
        })
        .cloned()
}

async fn receive_datagram(
    receiver: &PacketSocket,
    sockets: &InterfaceSockets,
    buf: &mut [u8],
) -> std::io::Result<(usize, SocketAddr, Option<InterfaceSocket>)> {
    loop {
        receiver.readable().await?;
        let result = {
            let sockets = sockets
                .lock()
                .map_err(|_| std::io::Error::other("interface socket map is poisoned"))?;
            receiver.try_recv(buf).map(|(size, info)| {
                let route = socket_for_ingress(&sockets, info.if_index, info.addr_dst);
                (size, info.addr_src, route)
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

fn receive_loop(
    receive_socket: Arc<PacketSocket>,
    cancel: Option<Cancel>,
    context: ReceiveContext,
) -> impl std::future::Future<Output = ()> {
    let mut open = context.gate.subscribe();
    async move {
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
                result = receive_datagram(&receive_socket, &context.interface_sockets, &mut buf) => result,
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

            let (header, header_len) = match parse_message_header(&buf[..amt]) {
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
                    MESSAGE_TYPES[header.message_type as usize], header.ident, src
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
                                return;
                            }

                            receive_peer_state(
                                context.tx_event.clone(),
                                header,
                                &buf[header_len..amt],
                            )
                            .await;
                        }
                        RESPONSE => {
                            if !is_enabled {
                                debug!(
                                "ignoring RESPONSE message from peer {} because Link is disabled",
                                header.ident
                            );
                                return;
                            }

                            receive_peer_state(
                                context.tx_event.clone(),
                                header,
                                &buf[header_len..amt],
                            )
                            .await;
                        }
                        BYEBYE => {
                            info!("Received BYEBYE message from peer {}", header.ident);
                            receive_bye_bye(context.tx_event.clone(), header.ident).await;
                        }
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
                        Some(entry) => entry.cancel.cancelled().await,
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
            _ = entry.cancel.cancelled() => {
                return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "ingress interface was removed"));
            }
            result = entry.socket.writable() => result?,
        }
        let result = {
            let sockets = sockets
                .lock()
                .map_err(|_| std::io::Error::other("interface socket map is poisoned"))?;
            let current = sockets.get(&entry.identity.addr).is_some_and(|current| {
                Arc::ptr_eq(&current.socket, &entry.socket) && !entry.cancel.is_cancelled()
            });
            if !current {
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
    let (ident, peer_state_clone) = match peer_state.try_lock() {
        Ok(guard) => (guard.ident(), guard.clone()),
        Err(_) => {
            // If we can't get the lock, skip this broadcast
            return;
        }
    };

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

pub async fn receive_peer_state(tx: Sender<OnEvent>, header: MessageHeader, buf: &[u8]) {
    let payload = parse_payload(buf).unwrap();
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

    debug!("sending peer state to gateway {}", node_state.ident());
    let _ = tx
        .send(OnEvent::PeerState(PeerStateMessageType {
            node_state,
            ttl: header.ttl,
            measurement_endpoint,
            audio_endpoint,
        }))
        .await;

    // info!("peer state sent")
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

    let socket = match new_udp_reuseport(MULTICAST_IP_ANY.into()) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to create socket for BYEBYE: {}", e);
            return;
        }
    };
    let _ = socket.set_broadcast(true);
    let _ = socket.set_multicast_ttl_v4(2);

    let message = match encode_message(node_state, 0, BYEBYE, &Payload::default(), 0) {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to encode BYEBYE message: {}", e);
            return;
        }
    };

    if let Ok(std_socket) = socket.into_std() {
        if let Err(e) = std_socket.send_to(&message, (MULTICAST_ADDR, LINK_PORT)) {
            warn!("Failed to send BYEBYE: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

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

    fn receive_context(tx_event: Sender<OnEvent>) -> ReceiveContext {
        ReceiveContext {
            drain_count: Arc::new(AtomicUsize::new(0)),
            fail_receive_once: Arc::new(AtomicBool::new(false)),
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
            assert!(matches!(event, Some(OnEvent::Byebye(id)) if id == node));
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
        assert!(matches!(event, Some(OnEvent::Byebye(id)) if id == fresh));
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
        assert!(matches!(event, Some(OnEvent::Byebye(id)) if id == node));
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
    }

    fn interface_socket() -> InterfaceSocket {
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
        let receiver =
            Arc::new(PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap());
        InterfaceSocket {
            socket: receiver.socket.clone(),
            receiver,
            identity: Ipv4Interface {
                addr: Ipv4Addr::LOCALHOST,
                index: interface.index,
                name: interface.name,
            },
            cancel: Cancel::default(),
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
        let selected = socket_for_ingress(&sockets, 11, IpAddr::V4(MULTICAST_ADDR)).unwrap();
        assert!(Arc::ptr_eq(&selected.socket, &wifi.socket));
        let selected = socket_for_ingress(&sockets, 12, IpAddr::V4(MULTICAST_ADDR)).unwrap();
        assert!(Arc::ptr_eq(&selected.socket, &ethernet.socket));
        assert!(socket_for_ingress(&sockets, 0, IpAddr::V4(sender)).is_none());
        assert!(socket_for_ingress(&sockets, 13, IpAddr::V4(sender)).is_none());
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
        let context = receive_context(tx);
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
        context.gate.stop().await;
        reconcile_interfaces(&listener.socket, &context, &mut children, &[]);
        children.shutdown().await;
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
