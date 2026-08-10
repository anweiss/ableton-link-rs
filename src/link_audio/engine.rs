//! The LinkAudio runtime.
//!
//! This is a tokio-based port of upstream's `UdpMessenger`, `SinkProcessor`,
//! `SourceProcessor`, `MainProcessor` and `Controller`. Upstream splits these
//! across several injection-heavy templates driven by an ASIO io context;
//! here they are a single engine whose state is guarded by a mutex and driven
//! by four tasks:
//!
//! * a receive task that dispatches incoming messages,
//! * an announce task that broadcasts peer announcements and pings,
//! * a process task that encodes and sends committed sink audio,
//! * a request task that keeps channel requests alive for every source.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

use chrono::Duration;
use tokio::{net::UdpSocket, task::JoinHandle};
use tracing::debug;

use crate::link::{node::NodeId, sessions::SessionId};

use super::{
    buffer::Buffer,
    channels::{AnnouncedChannel, Channel, Channels},
    codec::{AudioBufferSender, Encoder, PcmDecoder},
    messages::{
        audio_buffer_message, encode_message, parse_message_header, AUDIO_BUFFER, CHANNEL_BYES,
        CHANNEL_REQUEST, MAX_MESSAGE_SIZE, MAX_PAYLOAD_SIZE, PEER_ANNOUNCEMENT, PONG,
        STOP_CHANNEL_REQUEST,
    },
    network_metrics::NetworkMetricsFilter,
    payload::{
        parse_payload, truncate_name, AudioBuffer, ChannelAnnouncement, ChannelAnnouncements,
        ChannelBye, ChannelByes, ChannelRequest, ChannelStopRequest, Entry, HostTime, Id,
        PeerAnnouncement, PeerInfo, AUDIO_BUFFER_KEY, CHANNEL_BYES_KEY, HOST_TIME_KEY,
    },
    queue::Reader,
    receivers::Receivers,
    sink::Sink,
    source::Source,
};

/// Time-to-live of announcements, in seconds.
pub const TTL: u8 = 5;
/// Ratio of the ttl at which announcements are re-sent.
pub const TTL_RATIO: u8 = 20;
/// Nominal announcement period: `TTL * 1000 / TTL_RATIO` milliseconds.
pub const NOMINAL_BROADCAST_PERIOD: StdDuration =
    StdDuration::from_millis((TTL as u64) * 1000 / (TTL_RATIO as u64));
/// Period at which sink queues are drained.
pub const PROCESS_PERIOD: StdDuration = StdDuration::from_millis(1);
/// Period at which channel requests are refreshed.
pub const REQUEST_PERIOD: StdDuration = StdDuration::from_secs(TTL as u64);

/// Callback invoked when the set of available channels changes.
pub type ChannelsChangedCallback = Box<dyn Fn() + Send + 'static>;

#[derive(Debug, Clone)]
struct PeerReceiver {
    peer_id: NodeId,
    endpoint: SocketAddrV4,
    metrics: NetworkMetricsFilter,
}

/// Collects encoded audio buffers so the process task can send them.
#[derive(Clone, Default)]
struct Outbox(Arc<Mutex<Vec<AudioBuffer>>>);

impl Outbox {
    fn drain(&self) -> Vec<AudioBuffer> {
        match self.0.lock() {
            Ok(mut buffers) => std::mem::take(&mut *buffers),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

impl AudioBufferSender for Outbox {
    fn send(&mut self, buffer: &AudioBuffer) {
        if let Ok(mut buffers) = self.0.lock() {
            buffers.push(buffer.clone());
        }
    }
}

struct SinkEntry {
    sink: Arc<Sink>,
    reader: Reader<Buffer>,
    receivers: Receivers,
    encoder: Encoder<Outbox>,
    outbox: Outbox,
}

struct SourceEntry {
    source: Arc<Source>,
    decoder: PcmDecoder,
}

struct EngineState {
    node_id: NodeId,
    session_id: SessionId,
    peer_name: String,
    gateway_addr: Ipv4Addr,
    peers: Vec<PeerReceiver>,
    channels: Channels,
    sinks: Vec<SinkEntry>,
    sources: Vec<SourceEntry>,
    announced_channels: Vec<ChannelAnnouncement>,
}

impl EngineState {
    fn channel_announcements(&self) -> Vec<ChannelAnnouncement> {
        self.sinks
            .iter()
            .map(|entry| ChannelAnnouncement {
                name: entry.sink.name(),
                id: entry.sink.id(),
            })
            .collect()
    }

    /// Splits the announcement into messages that fit the payload budget,
    /// mirroring upstream's `updateAnnouncement`.
    fn announcements(&self) -> Vec<PeerAnnouncement> {
        let base = PeerAnnouncement {
            node_id: self.node_id,
            session_id: self.session_id,
            peer_info: PeerInfo::new(self.peer_name.clone()),
            channels: ChannelAnnouncements::default(),
        };

        let ping_size = HostTime::default().size_in_byte_stream();
        let mut announcements = vec![base.clone()];

        for channel in self.channel_announcements() {
            let channel_size = channel.size_in_byte_stream();
            let added_size = if announcements.len() == 1 {
                channel_size + ping_size
            } else {
                channel_size
            };

            let current_size = announcements
                .last()
                .map(|a| a.payload_size())
                .unwrap_or_default();
            if current_size + added_size > MAX_PAYLOAD_SIZE as u32 {
                announcements.push(base.clone());
            }

            if let Some(last) = announcements.last_mut() {
                last.channels.channels.push(channel);
            }
        }

        announcements
    }
}

/// The LinkAudio engine: owns the socket, the discovered channels and the
/// sinks and sources of the local peer.
pub struct AudioEngine {
    socket: Arc<UdpSocket>,
    endpoint: SocketAddrV4,
    state: Arc<Mutex<EngineState>>,
    api_channels: Arc<Mutex<Vec<Channel>>>,
    channels_changed: Arc<Mutex<Option<ChannelsChangedCallback>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl AudioEngine {
    /// Binds a LinkAudio socket on `addr` and starts the engine tasks.
    pub async fn new(
        addr: Ipv4Addr,
        node_id: NodeId,
        session_id: SessionId,
        peer_name: impl Into<String>,
    ) -> std::io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(SocketAddrV4::new(addr, 0)).await?);
        let endpoint = match socket.local_addr()? {
            SocketAddr::V4(addr) => addr,
            SocketAddr::V6(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "LinkAudio requires an IPv4 endpoint",
                ))
            }
        };

        let state = Arc::new(Mutex::new(EngineState {
            node_id,
            session_id,
            peer_name: truncate_name(&peer_name.into()),
            gateway_addr: addr,
            peers: Vec::new(),
            channels: Channels::new(),
            sinks: Vec::new(),
            sources: Vec::new(),
            announced_channels: Vec::new(),
        }));

        let mut engine = AudioEngine {
            socket,
            endpoint,
            state,
            api_channels: Arc::new(Mutex::new(Vec::new())),
            channels_changed: Arc::new(Mutex::new(None)),
            tasks: Vec::new(),
        };

        engine.spawn_tasks();
        Ok(engine)
    }

    /// The endpoint other peers should send LinkAudio traffic to. This is what
    /// gets announced in the Link `aep4` payload entry.
    pub fn endpoint(&self) -> SocketAddrV4 {
        self.endpoint
    }

    pub fn peer_name(&self) -> String {
        self.with_state(|state| state.peer_name.clone())
    }

    pub fn set_peer_name(&self, name: impl Into<String>) {
        let name = truncate_name(&name.into());
        self.with_state(|state| state.peer_name = name);
    }

    /// Updates the Link node and session identity the engine announces.
    pub fn set_identity(&self, node_id: NodeId, session_id: SessionId) {
        self.with_state(|state| {
            state.node_id = node_id;
            state.session_id = session_id;
        });
    }

    pub fn set_channels_changed_callback(&self, callback: ChannelsChangedCallback) {
        if let Ok(mut current) = self.channels_changed.lock() {
            *current = Some(callback);
        }
    }

    /// The audio channels currently available in the session.
    pub fn channels(&self) -> Vec<Channel> {
        self.api_channels
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Registers or removes the LinkAudio endpoint of a Link peer. This is fed
    /// from the `aep4` entry of the peer's Link `PeerState`.
    pub fn saw_link_audio_endpoint(&self, peer_id: NodeId, endpoint: Option<SocketAddrV4>) {
        self.with_state(|state| match endpoint {
            Some(endpoint) => {
                if !state.peers.iter().any(|p| p.endpoint == endpoint) {
                    state.peers.push(PeerReceiver {
                        peer_id,
                        endpoint,
                        metrics: NetworkMetricsFilter::new(),
                    });
                }
            }
            None => state.peers.retain(|p| p.peer_id != peer_id),
        });
    }

    /// Prunes peers and channels that are no longer part of the session.
    pub fn update_session_peers(&self, peers: &[NodeId]) {
        let changed = self.with_state(|state| {
            state.peers.retain(|p| peers.contains(&p.peer_id));
            for sink in &mut state.sinks {
                sink.receivers.retain_peers(peers);
            }
            state.channels.prune_peer_channels(peers)
        });
        if changed {
            self.publish_channels();
        }
    }

    /// Adds a sink, publishing a new channel to the session.
    pub fn add_sink(&self, name: impl Into<String>, max_num_samples: usize) -> Arc<Sink> {
        let id = NodeId::new();
        let (sink, reader) = Sink::new(name, max_num_samples, id);
        let sink = Arc::new(sink);
        let outbox = Outbox::default();

        self.with_state(|state| {
            state.sinks.push(SinkEntry {
                sink: sink.clone(),
                reader,
                receivers: Receivers::new(),
                encoder: Encoder::new(outbox.clone(), id),
                outbox: outbox.clone(),
            });
        });

        sink
    }

    /// Removes a sink and says goodbye to its channel.
    pub fn remove_sink(&self, id: Id) {
        let removed = self.with_state(|state| {
            let before = state.sinks.len();
            state.sinks.retain(|entry| entry.sink.id() != id);
            state.sinks.len() != before
        });

        if removed {
            self.send_channel_byes(&[id]);
        }
    }

    /// Subscribes to a channel published by another peer.
    pub fn add_source(
        &self,
        channel_id: Id,
        callback: super::source::SourceCallback,
    ) -> Arc<Source> {
        let source = Arc::new(Source::new(channel_id, callback));
        self.with_state(|state| {
            state.sources.push(SourceEntry {
                source: source.clone(),
                decoder: PcmDecoder::default(),
            });
        });
        self.send_channel_requests();
        source
    }

    /// Unsubscribes from a channel, telling the publishing peer to stop.
    pub fn remove_source(&self, channel_id: Id) {
        let (removed, endpoint, node_id) = self.with_state(|state| {
            let before = state.sources.len();
            state.sources.retain(|e| e.source.id() != channel_id);
            (
                state.sources.len() != before,
                state.channels.channel_endpoint(channel_id),
                state.node_id,
            )
        });

        if let (true, Some(endpoint)) = (removed, endpoint) {
            let request = ChannelStopRequest {
                peer_id: node_id,
                channel_id,
            };
            self.send(STOP_CHANNEL_REQUEST, 0, &request.to_payload(), endpoint);
        }
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut EngineState) -> T) -> T {
        match self.state.lock() {
            Ok(mut state) => f(&mut state),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    fn send(&self, message_type: u8, ttl: u8, payload: &[u8], to: SocketAddrV4) {
        let node_id = self.with_state(|state| state.node_id);
        send_message(&self.socket, node_id, message_type, ttl, payload, to);
    }

    fn send_channel_byes(&self, ids: &[Id]) {
        let (node_id, endpoints) = self.with_state(|state| {
            (
                state.node_id,
                state.peers.iter().map(|p| p.endpoint).collect::<Vec<_>>(),
            )
        });

        for byes in split_byes(ids) {
            let payload = byes.to_payload();
            for endpoint in &endpoints {
                send_message(
                    &self.socket,
                    node_id,
                    CHANNEL_BYES,
                    TTL,
                    &payload,
                    *endpoint,
                );
            }
        }
    }

    fn send_channel_requests(&self) {
        let (node_id, requests) = self.with_state(|state| {
            let requests: Vec<(Id, Option<SocketAddrV4>)> = state
                .sources
                .iter()
                .map(|entry| {
                    let id = entry.source.id();
                    (id, state.channels.channel_endpoint(id))
                })
                .collect();
            (state.node_id, requests)
        });

        for (channel_id, endpoint) in requests {
            if let Some(endpoint) = endpoint {
                let request = ChannelRequest {
                    peer_id: node_id,
                    channel_id,
                };
                send_message(
                    &self.socket,
                    node_id,
                    CHANNEL_REQUEST,
                    TTL,
                    &request.to_payload(),
                    endpoint,
                );
            }
        }
    }

    fn publish_channels(&self) {
        let (session_id, channels) =
            self.with_state(|state| (state.session_id, state.channels.all_channels()));

        // Session channels first, so the API list is ordered like upstream's.
        let mut ordered: Vec<Channel> = channels
            .iter()
            .filter(|c| c.session_id == session_id)
            .cloned()
            .collect();
        ordered.extend(channels.into_iter().filter(|c| c.session_id != session_id));

        if let Ok(mut api_channels) = self.api_channels.lock() {
            if *api_channels == ordered {
                return;
            }
            *api_channels = ordered;
        }

        if let Ok(callback) = self.channels_changed.lock() {
            if let Some(callback) = callback.as_ref() {
                callback();
            }
        }
    }

    fn spawn_tasks(&mut self) {
        self.tasks.push(self.spawn_receive_task());
        self.tasks.push(self.spawn_announce_task());
        self.tasks.push(self.spawn_process_task());
        self.tasks.push(self.spawn_request_task());
    }

    fn spawn_receive_task(&self) -> JoinHandle<()> {
        let socket = self.socket.clone();
        let state = self.state.clone();
        let api_channels = self.api_channels.clone();
        let channels_changed = self.channels_changed.clone();

        tokio::spawn(async move {
            let mut buffer = vec![0u8; MAX_MESSAGE_SIZE];
            loop {
                let (num_bytes, from) = match socket.recv_from(&mut buffer).await {
                    Ok(result) => result,
                    Err(e) => {
                        debug!("link audio socket error: {}", e);
                        continue;
                    }
                };

                let from = match from {
                    SocketAddr::V4(addr) => addr,
                    SocketAddr::V6(_) => continue,
                };

                let changed = handle_message(&socket, &state, &buffer[..num_bytes], from);
                if changed {
                    publish(&state, &api_channels, &channels_changed);
                }
            }
        })
    }

    fn spawn_announce_task(&self) -> JoinHandle<()> {
        let socket = self.socket.clone();
        let state = self.state.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(NOMINAL_BROADCAST_PERIOD);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                let (node_id, announcements, endpoints, byes) = {
                    let mut state = match state.lock() {
                        Ok(state) => state,
                        Err(poisoned) => poisoned.into_inner(),
                    };

                    let announcements = state.announcements();
                    let current: Vec<ChannelAnnouncement> = announcements
                        .iter()
                        .flat_map(|a| a.channels.channels.clone())
                        .collect();

                    // Channels that disappeared since the last announcement.
                    let byes: Vec<Id> = state
                        .announced_channels
                        .iter()
                        .filter(|previous| !current.iter().any(|c| c.id == previous.id))
                        .map(|previous| previous.id)
                        .collect();
                    state.announced_channels = current;

                    (
                        state.node_id,
                        announcements,
                        state.peers.iter().map(|p| p.endpoint).collect::<Vec<_>>(),
                        byes,
                    )
                };

                for byes in split_byes(&byes) {
                    let payload = byes.to_payload();
                    for endpoint in &endpoints {
                        send_message(&socket, node_id, CHANNEL_BYES, TTL, &payload, *endpoint);
                    }
                }

                let ping_time = Duration::microseconds(now_micros());

                for endpoint in &endpoints {
                    // A ping accompanies the first announcement per receiver.
                    let mut should_send_ping = true;
                    for announcement in &announcements {
                        let mut payload = announcement.to_payload();
                        if should_send_ping {
                            HostTime { time: ping_time }.encode(&mut payload);
                            should_send_ping = false;
                        }
                        send_message(
                            &socket,
                            node_id,
                            PEER_ANNOUNCEMENT,
                            TTL,
                            &payload,
                            *endpoint,
                        );
                    }
                }
            }
        })
    }

    fn spawn_process_task(&self) -> JoinHandle<()> {
        let socket = self.socket.clone();
        let state = self.state.clone();
        let api_channels = self.api_channels.clone();
        let channels_changed = self.channels_changed.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PROCESS_PERIOD);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                let (node_id, messages, channels_changed_now) = {
                    let mut state = match state.lock() {
                        Ok(state) => state,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let now = Instant::now();
                    let node_id = state.node_id;
                    let mut messages: Vec<(Vec<u8>, SocketAddrV4)> = Vec::new();

                    let mut names_changed = false;
                    for entry in &mut state.sinks {
                        if entry.sink.name_changed() {
                            names_changed = true;
                        }

                        entry.receivers.prune_expired(now);
                        entry.sink.set_is_connected(!entry.receivers.is_empty());

                        let max_num_samples = entry.sink.max_num_samples();
                        while entry.reader.retain_slot() {
                            if let Some(buffer) = entry.reader.slot_mut() {
                                let has_audio = buffer.tempo.bpm() > 0.0;
                                if has_audio {
                                    let buffer = buffer.clone();
                                    entry.encoder.encode(&buffer);
                                }
                            }
                            if let Some(buffer) = entry.reader.slot_mut() {
                                if buffer.samples.len() < max_num_samples {
                                    buffer.samples.resize(max_num_samples, 0);
                                }
                            }
                            entry.reader.release_slot();
                        }

                        let endpoints: Vec<SocketAddrV4> = entry.receivers.endpoints().collect();
                        for audio_buffer in entry.outbox.drain() {
                            if endpoints.is_empty() {
                                continue;
                            }
                            match audio_buffer_message(node_id, &audio_buffer.to_payload()) {
                                Ok(message) => {
                                    for endpoint in &endpoints {
                                        messages.push((message.clone(), *endpoint));
                                    }
                                }
                                Err(e) => debug!("failed to encode audio buffer: {}", e),
                            }
                        }
                    }

                    let expired = state.channels.prune_expired(now);
                    (node_id, messages, names_changed || expired)
                };

                let _ = node_id;
                for (message, endpoint) in messages {
                    if let Err(e) = socket.try_send_to(&message, endpoint.into()) {
                        debug!("failed to send audio buffer: {}", e);
                    }
                }

                if channels_changed_now {
                    publish(&state, &api_channels, &channels_changed);
                }
            }
        })
    }

    fn spawn_request_task(&self) -> JoinHandle<()> {
        let socket = self.socket.clone();
        let state = self.state.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REQUEST_PERIOD);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                let (node_id, requests) = {
                    let state = match state.lock() {
                        Ok(state) => state,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let requests: Vec<(Id, Option<SocketAddrV4>)> = state
                        .sources
                        .iter()
                        .map(|entry| {
                            let id = entry.source.id();
                            (id, state.channels.channel_endpoint(id))
                        })
                        .collect();
                    (state.node_id, requests)
                };

                for (channel_id, endpoint) in requests {
                    if let Some(endpoint) = endpoint {
                        let request = ChannelRequest {
                            peer_id: node_id,
                            channel_id,
                        };
                        send_message(
                            &socket,
                            node_id,
                            CHANNEL_REQUEST,
                            TTL,
                            &request.to_payload(),
                            endpoint,
                        );
                    }
                }
            }
        })
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        let ids: Vec<Id> =
            self.with_state(|state| state.sinks.iter().map(|s| s.sink.id()).collect());
        if !ids.is_empty() {
            self.send_channel_byes(&ids);
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn send_message(
    socket: &UdpSocket,
    node_id: NodeId,
    message_type: u8,
    ttl: u8,
    payload: &[u8],
    to: SocketAddrV4,
) {
    match encode_message(node_id, ttl, message_type, payload) {
        Ok(message) => {
            if let Err(e) = socket.try_send_to(&message, to.into()) {
                debug!("failed to send link audio message: {}", e);
            }
        }
        Err(e) => debug!("failed to encode link audio message: {}", e),
    }
}

/// Splits channel byes into payloads that fit a single message.
fn split_byes(ids: &[Id]) -> Vec<ChannelByes> {
    let mut result = vec![ChannelByes::default()];
    for id in ids {
        let bye = ChannelBye { id: *id };
        let added_size = bye.size_in_byte_stream();
        if let Some(last) = result.last() {
            if last.size_in_byte_stream() + added_size > MAX_PAYLOAD_SIZE as u32 {
                result.push(ChannelByes::default());
            }
        }
        if let Some(last) = result.last_mut() {
            last.byes.push(bye);
        }
    }
    result.retain(|byes| !byes.byes.is_empty());
    result
}

fn publish(
    state: &Arc<Mutex<EngineState>>,
    api_channels: &Arc<Mutex<Vec<Channel>>>,
    channels_changed: &Arc<Mutex<Option<ChannelsChangedCallback>>>,
) {
    let (session_id, channels) = {
        let state = match state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        (state.session_id, state.channels.all_channels())
    };

    let mut ordered: Vec<Channel> = channels
        .iter()
        .filter(|c| c.session_id == session_id)
        .cloned()
        .collect();
    ordered.extend(channels.into_iter().filter(|c| c.session_id != session_id));

    match api_channels.lock() {
        Ok(mut current) => {
            if *current == ordered {
                return;
            }
            *current = ordered;
        }
        Err(poisoned) => *poisoned.into_inner() = ordered,
    }

    if let Ok(callback) = channels_changed.lock() {
        if let Some(callback) = callback.as_ref() {
            callback();
        }
    }
}

/// Dispatches a received message. Returns `true` if the visible set of
/// channels changed.
fn handle_message(
    socket: &UdpSocket,
    state: &Arc<Mutex<EngineState>>,
    data: &[u8],
    from: SocketAddrV4,
) -> bool {
    let (header, payload_offset) = match parse_message_header(data) {
        Ok(result) => result,
        Err(e) => {
            debug!("ignoring link audio message: {}", e);
            return false;
        }
    };

    let payload = &data[payload_offset..];

    let mut state = match state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Ignore messages from ourselves.
    if header.ident == state.node_id {
        return false;
    }

    match header.message_type {
        PEER_ANNOUNCEMENT => {
            let changed = receive_announcement(&mut state, header.ident, payload, from, header.ttl);
            receive_ping(socket, &state, payload, from);
            changed
        }
        CHANNEL_BYES => receive_channel_byes(&mut state, payload),
        PONG => {
            receive_pong(&mut state, payload, from);
            false
        }
        CHANNEL_REQUEST => {
            receive_channel_request(&mut state, header.ident, payload, header.ttl, from);
            false
        }
        STOP_CHANNEL_REQUEST => {
            receive_channel_stop_request(&mut state, header.ident, payload);
            false
        }
        AUDIO_BUFFER => {
            receive_audio_buffer(&mut state, payload);
            false
        }
        other => {
            debug!("unknown link audio message type {}", other);
            false
        }
    }
}

fn receive_announcement(
    state: &mut EngineState,
    peer_id: NodeId,
    payload: &[u8],
    from: SocketAddrV4,
    ttl: u8,
) -> bool {
    // Only peers whose endpoint we learned from Link discovery are accepted.
    let quality = match state.peers.iter().find(|p| p.endpoint == from) {
        Some(peer) => peer.metrics.metrics().quality(),
        None => return false,
    };

    let announcement = match PeerAnnouncement::from_payload(peer_id, payload) {
        Ok(announcement) => announcement,
        Err(e) => {
            debug!("ignoring peer announcement: {}", e);
            return false;
        }
    };

    let announced: Vec<AnnouncedChannel> = announcement
        .channels
        .channels
        .iter()
        .map(|c| AnnouncedChannel {
            id: c.id,
            name: c.name.clone(),
        })
        .collect();

    let gateway_addr = state.gateway_addr;
    state.channels.saw_announcement(
        peer_id,
        &announcement.peer_info.name,
        announcement.session_id,
        &announced,
        gateway_addr,
        from,
        quality,
        ttl,
        Instant::now(),
    )
}

fn receive_ping(socket: &UdpSocket, state: &EngineState, payload: &[u8], from: SocketAddrV4) {
    let mut host_time = None;
    let _ = parse_payload(payload, |key, reader| {
        if key == HOST_TIME_KEY {
            host_time = Some(HostTime::decode_body(reader)?);
        }
        Ok(())
    });

    if let Some(host_time) = host_time {
        send_message(
            socket,
            state.node_id,
            PONG,
            TTL,
            &host_time.to_payload(),
            from,
        );
    }
}

fn receive_pong(state: &mut EngineState, payload: &[u8], from: SocketAddrV4) {
    let mut send_time = None;
    let _ = parse_payload(payload, |key, reader| {
        if key == HOST_TIME_KEY {
            send_time = Some(HostTime::decode_body(reader)?.time);
        }
        Ok(())
    });

    if let Some(send_time) = send_time {
        let receive_time = Duration::microseconds(now_micros());
        if let Some(peer) = state.peers.iter_mut().find(|p| p.endpoint == from) {
            peer.metrics.push(receive_time - send_time);
        }
    }
}

fn receive_channel_byes(state: &mut EngineState, payload: &[u8]) -> bool {
    let mut byes = Vec::new();
    let _ = parse_payload(payload, |key, reader| {
        if key == CHANNEL_BYES_KEY {
            byes = ChannelByes::decode_body(reader)?
                .byes
                .into_iter()
                .map(|b| b.id)
                .collect();
        }
        Ok(())
    });

    if byes.is_empty() {
        return false;
    }

    let gateway_addr = state.gateway_addr;
    state.channels.channels_left(gateway_addr, &byes)
}

fn receive_channel_request(
    state: &mut EngineState,
    peer_id: NodeId,
    payload: &[u8],
    ttl: u8,
    from: SocketAddrV4,
) {
    let request = match ChannelRequest::from_payload(peer_id, payload) {
        Ok(request) => request,
        Err(e) => {
            debug!("ignoring channel request: {}", e);
            return;
        }
    };

    let now = Instant::now();
    if let Some(entry) = state
        .sinks
        .iter_mut()
        .find(|entry| entry.sink.id() == request.channel_id)
    {
        entry
            .receivers
            .receive_channel_request(&request, ttl, Some(from), now);
        entry.sink.set_is_connected(!entry.receivers.is_empty());
    }
}

fn receive_channel_stop_request(state: &mut EngineState, peer_id: NodeId, payload: &[u8]) {
    let request = match ChannelStopRequest::from_payload(peer_id, payload) {
        Ok(request) => request,
        Err(e) => {
            debug!("ignoring channel stop request: {}", e);
            return;
        }
    };

    if let Some(entry) = state
        .sinks
        .iter_mut()
        .find(|entry| entry.sink.id() == request.channel_id)
    {
        entry.receivers.receive_channel_stop_request(&request);
        entry.sink.set_is_connected(!entry.receivers.is_empty());
    }
}

fn receive_audio_buffer(state: &mut EngineState, payload: &[u8]) {
    let mut audio_buffer = None;
    let _ = parse_payload(payload, |key, reader| {
        if key == AUDIO_BUFFER_KEY {
            audio_buffer = Some(AudioBuffer::decode_body(reader)?);
        }
        Ok(())
    });

    let Some(audio_buffer) = audio_buffer else {
        return;
    };

    if let Some(entry) = state
        .sources
        .iter_mut()
        .find(|entry| entry.source.id() == audio_buffer.channel_id)
    {
        let source = entry.source.clone();
        entry
            .decoder
            .decode(&audio_buffer, |handle| source.invoke(handle));
    }
}

/// Type alias kept for parity with upstream's naming.
pub type SinkMap = HashMap<Id, Arc<Sink>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{beats::Beats, tempo::Tempo, timeline::Timeline};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn addr() -> Ipv4Addr {
        Ipv4Addr::LOCALHOST
    }

    fn timeline() -> Timeline {
        Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        }
    }

    async fn engine(name: &str) -> AudioEngine {
        AudioEngine::new(
            addr(),
            NodeId::new(),
            SessionId(NodeId::new()),
            name.to_string(),
        )
        .await
        .unwrap()
    }

    #[test]
    fn byes_are_split_across_messages() {
        let ids: Vec<Id> = (0..200).map(|i| NodeId::from_array([i as u8; 8])).collect();
        let messages = split_byes(&ids);
        assert!(messages.len() > 1);
        for message in &messages {
            assert!(message.size_in_byte_stream() as usize <= MAX_PAYLOAD_SIZE);
        }
        let total: usize = messages.iter().map(|m| m.byes.len()).sum();
        assert_eq!(total, ids.len());
    }

    #[test]
    fn no_byes_produce_no_messages() {
        assert!(split_byes(&[]).is_empty());
    }

    #[tokio::test]
    async fn engine_binds_an_endpoint() {
        let engine = engine("rust").await;
        assert_eq!(*engine.endpoint().ip(), addr());
        assert_ne!(engine.endpoint().port(), 0);
        assert_eq!(engine.peer_name(), "rust");
    }

    #[tokio::test]
    async fn sinks_announce_channels() {
        let engine = engine("rust").await;
        let sink = engine.add_sink("drums", 512);

        let announcements = engine.with_state(|state| state.announcements());
        assert_eq!(announcements.len(), 1);
        assert_eq!(announcements[0].channels.channels.len(), 1);
        assert_eq!(announcements[0].channels.channels[0].name, "drums");
        assert_eq!(announcements[0].channels.channels[0].id, sink.id());

        engine.remove_sink(sink.id());
        let announcements = engine.with_state(|state| state.announcements());
        assert!(announcements[0].channels.channels.is_empty());
    }

    #[tokio::test]
    async fn announcements_are_split_to_fit_the_payload_budget() {
        let engine = engine("rust").await;
        for i in 0..40 {
            engine.add_sink(format!("channel with a fairly long name number {}", i), 64);
        }

        let announcements = engine.with_state(|state| state.announcements());
        assert!(announcements.len() > 1);
        for announcement in &announcements {
            assert!(announcement.payload_size() as usize <= MAX_PAYLOAD_SIZE);
        }
    }

    #[tokio::test]
    async fn peer_endpoints_are_tracked() {
        let engine = engine("rust").await;
        let peer = NodeId::from_array([3; 8]);
        let endpoint = SocketAddrV4::new(addr(), 30303);

        engine.saw_link_audio_endpoint(peer, Some(endpoint));
        assert_eq!(engine.with_state(|s| s.peers.len()), 1);

        // Duplicate endpoints are not added twice.
        engine.saw_link_audio_endpoint(peer, Some(endpoint));
        assert_eq!(engine.with_state(|s| s.peers.len()), 1);

        engine.saw_link_audio_endpoint(peer, None);
        assert_eq!(engine.with_state(|s| s.peers.len()), 0);
    }

    #[tokio::test]
    async fn peers_that_left_the_session_are_dropped() {
        let engine = engine("rust").await;
        let peer = NodeId::from_array([3; 8]);
        engine.saw_link_audio_endpoint(peer, Some(SocketAddrV4::new(addr(), 30303)));

        engine.update_session_peers(&[NodeId::from_array([9; 8])]);
        assert_eq!(engine.with_state(|s| s.peers.len()), 0);
    }

    #[tokio::test]
    async fn audio_flows_from_a_sink_to_a_source() {
        let sender = engine("sender").await;
        let receiver = engine("receiver").await;

        // Peers learn about each other's endpoints via Link discovery.
        sender.saw_link_audio_endpoint(
            receiver.with_state(|s| s.node_id),
            Some(receiver.endpoint()),
        );
        receiver.saw_link_audio_endpoint(sender.with_state(|s| s.node_id), Some(sender.endpoint()));

        // Both peers must agree on the session for the channel to be visible.
        let session_id = sender.with_state(|s| s.session_id);
        receiver.with_state(|s| s.session_id = session_id);

        let sink = sender.add_sink("drums", 512);

        // Wait for the receiver to discover the channel.
        let channel_id = tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                if let Some(channel) = receiver.channels().first().cloned() {
                    return channel.id;
                }
                tokio::time::sleep(StdDuration::from_millis(20)).await;
            }
        })
        .await
        .expect("channel was not discovered");

        assert_eq!(channel_id, sink.id());
        assert_eq!(receiver.channels()[0].name, "drums");
        assert_eq!(receiver.channels()[0].peer_name, "sender");

        let received = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let sink_samples = received.clone();
        let sink_calls = calls.clone();
        receiver.add_source(
            channel_id,
            Box::new(move |handle| {
                sink_samples
                    .lock()
                    .unwrap()
                    .extend_from_slice(handle.samples);
                sink_calls.fetch_add(1, Ordering::SeqCst);
            }),
        );

        // Wait for the channel request to connect the sink.
        tokio::time::timeout(StdDuration::from_secs(10), async {
            while !sink.is_connected() {
                tokio::time::sleep(StdDuration::from_millis(20)).await;
            }
        })
        .await
        .expect("sink never connected");

        let expected: Vec<i16> = (0..64).map(|i| i as i16 - 32).collect();
        tokio::time::timeout(StdDuration::from_secs(10), async {
            while calls.load(Ordering::SeqCst) == 0 {
                if let Some(mut handle) = sink.retain_buffer() {
                    handle.samples_mut()[..expected.len()].copy_from_slice(&expected);
                    handle.commit(
                        &timeline(),
                        session_id.0,
                        0.0,
                        4.0,
                        expected.len() / 2,
                        2,
                        44100,
                    );
                }
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
        })
        .await
        .expect("no audio was received");

        let received = received.lock().unwrap();
        assert!(!received.is_empty());
        assert_eq!(
            &received[..expected.len().min(received.len())],
            &expected[..received.len().min(expected.len())]
        );
    }
}
