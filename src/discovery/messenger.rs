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
    messages::{
        encode_message, parse_message_header, parse_payload, MessageHeader, MessageType,
        SessionGroupId, ALIVE, BYEBYE, MAX_MESSAGE_SIZE, RESPONSE,
    },
    peers::PeerState,
    LINK_PORT, MULTICAST_ADDR, MULTICAST_IP_ANY,
};

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
    // which interface a datagram came in on. Response-socket selection in
    // `socket_for_target` therefore remains a longest-prefix match on the source
    // address. Closing that gap needs per-interface listeners or arrival metadata
    // and is tracked in #154.
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
    cancel: Cancel,
}

/// The set of per-interface sockets, keyed by the interface address they are bound to.
pub type InterfaceSockets = Arc<Mutex<HashMap<Ipv4Addr, InterfaceSocket>>>;

pub struct Messenger {
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
        let socket = Arc::new(new_udp_reuseport(MULTICAST_IP_ANY.into()).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "failed to bind Ableton Link multicast socket on {}: {}",
                    SocketAddr::from(MULTICAST_IP_ANY),
                    e
                ),
            )
        })?);
        socket.set_multicast_loop_v4(true)?;

        let interface_sockets: InterfaceSockets = Arc::new(Mutex::new(HashMap::new()));
        let gateways_changed = Arc::new(AtomicUsize::new(0));

        for addr in usable_interfaces_v4() {
            match add_interface(&socket, &interface_sockets, addr) {
                Ok(_) => info!("joined Ableton Link multicast group on interface {}", addr),
                Err(e) => warn!("failed to set up interface {}: {}", addr, e),
            }
        }

        if lock_map(&interface_sockets, |sockets| sockets.is_empty()).unwrap_or(true) {
            // No usable interface was found or none of them could be set up (e.g. a
            // container without external networking). Fall back to the default
            // interface so discovery keeps working; the periodic scan picks up
            // interfaces as they appear.
            warn!("no usable multicast interface found, falling back to the default interface");
            add_interface(&socket, &interface_sockets, Ipv4Addr::UNSPECIFIED)?;
        }

        // Mirrors upstream's `GatewayFactory::gatewaysChanged()` notification, which
        // fires once from `PeerGateways::enable` (unconditionally) and once more from
        // the initial interface scan if it found any gateways. Since construction and
        // enabling are not separate steps here, count the initial population as a
        // single change.
        gateways_changed.fetch_add(1, Ordering::Relaxed);

        Ok(Messenger {
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
        let multicast_socket = self.interface.as_ref().unwrap().clone();
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
                entry.socket.clone(),
                Some(entry.cancel.clone()),
                context.clone(),
            ));
        }

        children.spawn(interface_scan(multicast_socket, context));

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

/// All IPv4 interfaces that can be used for Link discovery.
fn usable_interfaces_v4() -> Vec<Ipv4Addr> {
    only_ipv4(crate::platform::network::scan_network_interfaces_blocking())
}

async fn usable_interfaces_v4_async() -> Vec<Ipv4Addr> {
    only_ipv4(crate::platform::network::scan_network_interfaces().await)
}

fn only_ipv4(addrs: Vec<IpAddr>) -> Vec<Ipv4Addr> {
    addrs
        .into_iter()
        .filter_map(|addr| match addr {
            IpAddr::V4(addr) => Some(addr),
            IpAddr::V6(_) => None,
        })
        .collect()
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
    addr: Ipv4Addr,
) -> Result<InterfaceSocket, std::io::Error> {
    multicast_socket.join_multicast_v4(MULTICAST_ADDR, addr)?;

    let send_socket = match new_udp_reuseport(SocketAddrV4::new(addr, 0).into()) {
        Ok(socket) => socket,
        Err(e) => {
            let _ = multicast_socket.leave_multicast_v4(MULTICAST_ADDR, addr);
            return Err(e);
        }
    };
    if let Err(e) = send_socket.set_multicast_loop_v4(true) {
        let _ = multicast_socket.leave_multicast_v4(MULTICAST_ADDR, addr);
        return Err(e);
    }

    let entry = InterfaceSocket {
        socket: Arc::new(send_socket),
        cancel: Cancel::default(),
    };

    match interface_sockets.lock() {
        Ok(mut sockets) => {
            sockets.insert(addr, entry.clone());
        }
        Err(_) => {
            let _ = multicast_socket.leave_multicast_v4(MULTICAST_ADDR, addr);
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
    let entry = match interface_sockets.lock() {
        Ok(mut sockets) => sockets.remove(&addr),
        Err(_) => None,
    };

    if let Some(entry) = entry {
        entry.cancel.cancel();
        let _ = multicast_socket.leave_multicast_v4(MULTICAST_ADDR, addr);
        info!("left Ableton Link multicast group on interface {}", addr);
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

        let current = usable_interfaces_v4_async().await;
        if current.is_empty() {
            // Keep the existing sockets rather than tearing discovery down while
            // the host temporarily has no usable interface.
            continue;
        }

        let known = lock_map(&context.interface_sockets, |sockets| {
            sockets.keys().copied().collect::<Vec<_>>()
        })
        .unwrap_or_default();

        let stale_addrs: Vec<Ipv4Addr> = known
            .iter()
            .filter(|addr| !current.contains(addr))
            .copied()
            .collect();
        let new_addrs: Vec<Ipv4Addr> = current
            .iter()
            .filter(|addr| !known.contains(addr))
            .copied()
            .collect();

        for addr in &stale_addrs {
            remove_interface(&multicast_socket, &context.interface_sockets, *addr);
        }

        for addr in &new_addrs {
            match add_interface(&multicast_socket, &context.interface_sockets, *addr) {
                Ok(entry) => {
                    info!("joined Ableton Link multicast group on interface {}", addr);
                    children.spawn(receive_loop(
                        entry.socket.clone(),
                        Some(entry.cancel.clone()),
                        context.clone(),
                    ));
                }
                Err(e) => warn!("failed to set up interface {}: {}", addr, e),
            }
        }

        // Mirrors upstream's `PeerGateways::Callback::operator()`, which fires
        // `gatewaysChanged()` once per scan pass (not once per interface) when
        // the interface set actually changed.
        if !stale_addrs.is_empty() || !new_addrs.is_empty() {
            context.gateways_changed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Pick the socket that is most likely to reach `to`, i.e. the one bound to the
/// interface address sharing the longest prefix with it.
fn socket_for_target(interface_sockets: &InterfaceSockets, to: Ipv4Addr) -> Option<Arc<UdpSocket>> {
    lock_map(interface_sockets, |sockets| {
        sockets
            .iter()
            .max_by_key(|(addr, _)| common_prefix_len(**addr, to))
            .map(|(_, entry)| entry.socket.clone())
    })
    .flatten()
}

fn common_prefix_len(a: Ipv4Addr, b: Ipv4Addr) -> u32 {
    (u32::from(a) ^ u32::from(b)).leading_zeros()
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
    receive_socket: Arc<UdpSocket>,
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
                result = receive_socket.recv_from(&mut buf) => result,
            };
            #[cfg(test)]
            let received = if context.fail_receive_once.swap(false, Ordering::Relaxed) {
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
            } else {
                received
            };

            let (amt, src) = match received {
                Ok(received) => received,
                Err(e) => {
                    warn!("discovery socket receive failed: {}", e);
                    // UDP errors can be transient. Keep the registered interface's
                    // receiver alive, but back off to avoid spinning on a bad socket.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };

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

                            if let Some(socket) =
                                socket_for_target(&context.interface_sockets, *src.ip())
                            {
                                send_response(
                                    socket,
                                    context.peer_state.clone(),
                                    context.ttl,
                                    src,
                                    context.last_broadcast_time.clone(),
                                    context.group_id,
                                )
                                .await;
                            } else {
                                warn!("no interface socket available to respond to {}", src);
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
                            send_peer_state(entry.socket.clone(), peer_state.clone(), ttl, ALIVE, to, lbt.clone(), group_id).await;
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
    send_message_reporting(socket, from, ttl, message_type, payload, to, group_id)
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
) -> std::io::Result<bool> {
    socket.set_broadcast(true).unwrap();
    socket.set_multicast_ttl_v4(2).unwrap();
    socket.set_multicast_loop_v4(true).unwrap();

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

    socket.send_to(&message, to).await?;
    Ok(true)
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
            interface_sockets: Arc::new(Mutex::new(HashMap::new())),
            peer_state: Arc::new(Mutex::new(PeerState::default())),
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
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
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
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
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
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
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
        InterfaceSocket {
            socket: Arc::new(
                new_udp_reuseport(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into()).unwrap(),
            ),
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

    #[test]
    fn common_prefix_len_prefers_closest_address() {
        assert_eq!(
            common_prefix_len(Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 1)),
            32
        );
        assert!(
            common_prefix_len(Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 2))
                > common_prefix_len(Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(192, 168, 1, 2))
        );
    }

    #[tokio::test]
    async fn socket_for_target_picks_matching_interface() {
        let wifi = Ipv4Addr::new(192, 168, 1, 10);
        let ethernet = Ipv4Addr::new(10, 0, 0, 10);

        let mut sockets = HashMap::new();
        sockets.insert(wifi, interface_socket());
        sockets.insert(ethernet, interface_socket());
        let interface_sockets: InterfaceSockets = Arc::new(Mutex::new(sockets));

        let wifi_socket = interface_sockets.lock().unwrap()[&wifi].socket.clone();
        let ethernet_socket = interface_sockets.lock().unwrap()[&ethernet].socket.clone();

        let selected = socket_for_target(&interface_sockets, Ipv4Addr::new(10, 0, 0, 42)).unwrap();
        assert!(Arc::ptr_eq(&selected, &ethernet_socket));

        let selected =
            socket_for_target(&interface_sockets, Ipv4Addr::new(192, 168, 1, 42)).unwrap();
        assert!(Arc::ptr_eq(&selected, &wifi_socket));
    }

    #[test]
    fn only_ipv4_filters_ipv6_addresses() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        ];

        assert_eq!(only_ipv4(addrs), vec![Ipv4Addr::new(192, 168, 1, 10)]);
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
            !send_message_reporting(socket.clone(), from, 5, ALIVE, &oversized, to, 0)
                .await
                .unwrap(),
            "an oversized payload must report as dropped, not sent"
        );

        // Guard against the inverse regression: an unconditional early return
        // would also satisfy the assertion above, so prove a well-formed
        // payload actually reaches the wire.
        let small = Payload::default();
        assert!(
            send_message_reporting(socket, from, 5, ALIVE, &small, to, 0)
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
