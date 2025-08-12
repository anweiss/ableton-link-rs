use std::{
    net::{IpAddr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use chrono::Duration;
use local_ip_address::list_afinet_netifas;
use tokio::sync::{mpsc::Receiver, Notify};
use tracing::{debug, info};

use crate::discovery::{
    gateway::{OnEvent, PeerGateway},
    messenger::new_udp_reuseport,
    peers::{unique_session_peer_count, ControllerPeer, PeerState, PeerStateChange},
};

use super::{
    beats::Beats,
    clock::Clock,
    ghostxform::GhostXForm,
    node::{NodeId, NodeState},
    sessions::{Session, SessionId, SessionMeasurement, Sessions},
    state::{ClientStartStopState, ClientState, SessionState, StartStopState},
    tempo,
    timeline::{
        clamp_tempo, update_client_timeline_from_session, update_session_timeline_from_client,
        Timeline,
    },
    IncomingClientState,
};

pub const LOCAL_MOD_GRACE_PERIOD: Duration = Duration::milliseconds(1000);

pub struct Controller {
    // tempo_callback: Option<TempoCallback>,
    // start_stop_callback: Option<StartStopCallback>,
    pub peer_state: Arc<Mutex<PeerState>>,
    pub session_state: Arc<Mutex<SessionState>>,
    pub client_state: Arc<Mutex<ClientState>>,
    // last_is_playing_for_start_stop_state_callback: bool,
    session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    enabled: Arc<Mutex<bool>>,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    sessions: Sessions,
    discovery: Arc<PeerGateway>,
    clock: Clock,
    rx_event: Option<Receiver<OnEvent>>,
    notifier: Arc<Notify>,
}

impl Controller {
    pub async fn new(
        tempo: tempo::Tempo,
        // peer_count_callback: Option<PeerCountCallback>,
        // tempo_callback: Option<TempoCallback>,
        // start_stop_callback: Option<StartStopCallback>,
        clock: Clock,
    ) -> Self {
        let node_id = NodeId::new();
        let session_peer_counter = Arc::new(Mutex::new(SessionPeerCounter::default()));
        let session_id = SessionId(node_id);
        let s_state = init_session_state(tempo, clock);
        let client_state = Arc::new(Mutex::new(init_client_state(s_state)));

        let start_stop_sync_enabled = Arc::new(Mutex::new(false));

        let timeline = s_state.timeline;

        let session_state = Arc::new(Mutex::new(s_state));

        let (tx_measure_peer_state, rx_measure_peer_state) = tokio::sync::mpsc::channel(1);
        let (tx_measure_peer_result, rx_measure_peer_result) = tokio::sync::mpsc::channel(1);
        let (tx_peer_state_change, mut rx_peer_state_change) = tokio::sync::mpsc::channel(1);
        let (tx_event, rx_event) = tokio::sync::mpsc::channel::<OnEvent>(1);
        let (tx_join_session, mut rx_join_session) = tokio::sync::mpsc::channel::<Session>(1);

        let peers = Arc::new(Mutex::new(vec![]));
        let notifier = Arc::new(Notify::new());

        let peer_state = Arc::new(Mutex::new(PeerState {
            node_state: NodeState {
                node_id,
                session_id,
                timeline,
                start_stop_state: StartStopState::default(),
            },
            measurement_endpoint: None,
        }));

        let ip = list_afinet_netifas()
            .unwrap()
            .iter()
            .find_map(|(_, ip)| match ip {
                IpAddr::V4(ipv4) if !ip.is_loopback() => Some(*ipv4),
                _ => None,
            })
            .unwrap();

        let ping_responder_unicast_socket = Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0)));

        let discovery = Arc::new(
            PeerGateway::new(
                peer_state.clone(),
                session_state.clone(),
                clock,
                session_peer_counter.clone(),
                tx_peer_state_change,
                tx_event,
                tx_measure_peer_result.clone(),
                peers.clone(),
                notifier.clone(),
                rx_measure_peer_state,
                ping_responder_unicast_socket,
            )
            .await,
        );

        let sessions = Sessions::new(
            Session {
                session_id,
                timeline,
                measurement: SessionMeasurement {
                    x_form: session_state.try_lock().unwrap().ghost_x_form,
                    timestamp: clock.micros(),
                },
            },
            tx_measure_peer_state,
            peers.clone(),
            clock,
            tx_join_session,
            notifier.clone(),
            rx_measure_peer_result,
        );

        let s_state_loop = session_state.clone();
        let c_state_loop = client_state.clone();
        let s_stop_sync_enabled_loop = start_stop_sync_enabled.clone();
        let discovery_loop = discovery.clone();
        let peers_loop = peers.clone();
        let s_peer_counter_loop = session_peer_counter.clone();
        let s_loop = sessions.clone();
        let ps_loop = peer_state.clone();

        tokio::spawn(async move {
            loop {
                if let Some(session) = rx_join_session.recv().await {
                    join_session(
                        session,
                        ps_loop.clone(),
                        s_state_loop.clone(),
                        c_state_loop.clone(),
                        clock,
                        s_stop_sync_enabled_loop.clone(),
                        discovery_loop.clone(),
                        peers_loop.clone(),
                        s_peer_counter_loop.clone(),
                        s_loop.clone(),
                    )
                    .await;
                }
            }
        });

        let discovery_loop = discovery.clone();
        let s_state_loop = session_state.clone();
        let c_state_loop = client_state.clone();
        let s_stop_sync_enabled_loop = start_stop_sync_enabled.clone();
        let sessions_loop = sessions.clone();
        let p_loop = peers.clone();
        let s_peer_counter_loop = session_peer_counter.clone();
        let peer_state_loop = peer_state.clone();

        tokio::spawn(async move {
            loop {
                if let Some(peer_state_changes) = rx_peer_state_change.recv().await {
                    debug!("controller received peer state changes");
                    for peer_state_change in peer_state_changes.iter() {
                        match peer_state_change {
                            PeerStateChange::SessionMembership => {
                                let count = unique_session_peer_count(
                                    peer_state_loop.try_lock().unwrap().session_id(),
                                    p_loop.clone(),
                                );
                                let old_count =
                                    s_peer_counter_loop.try_lock().unwrap().session_peer_count;

                                if old_count != count && count == 0 {
                                    reset_state(
                                        peer_state_loop.clone(),
                                        s_state_loop.clone(),
                                        c_state_loop.clone(),
                                        discovery_loop.clone(),
                                        sessions_loop.clone(),
                                        clock,
                                        s_stop_sync_enabled_loop.clone(),
                                    )
                                    .await
                                }
                            }
                            PeerStateChange::SessionTimeline(peer_session, timeline) => {
                                // handle_timeline_from_session

                                debug!(
                                    "controller received timeline with tempo: {} for session: {}",
                                    timeline.tempo, peer_session
                                );

                                let new_timeline = sessions_loop
                                    .saw_session_timeline(*peer_session, *timeline)
                                    .await;

                                update_session_timing(
                                    s_state_loop.clone(),
                                    c_state_loop.clone(),
                                    new_timeline,
                                    s_state_loop.try_lock().unwrap().ghost_x_form,
                                    clock,
                                    s_stop_sync_enabled_loop.clone(),
                                );

                                update_discovery(
                                    s_state_loop.clone(),
                                    peer_state_loop.clone(),
                                    discovery_loop.clone(),
                                )
                                .await;
                            }
                            PeerStateChange::SessionStartStopState(
                                peer_session,
                                peer_start_stop_state,
                            ) => {
                                // handle_start_stop_state_from_session

                                info!(
                                    "controller received start stop state. isPlaying: {}, beats: {}, time: {} for session: {}",
                                    peer_start_stop_state.is_playing,
                                    peer_start_stop_state.beats.floating(),
                                    peer_start_stop_state.timestamp.num_microseconds().unwrap(),
                                    peer_session,
                                );

                                if *peer_session == peer_state_loop.try_lock().unwrap().session_id()
                                    && peer_start_stop_state.timestamp
                                        > s_state_loop
                                            .try_lock()
                                            .unwrap()
                                            .start_stop_state
                                            .timestamp
                                {
                                    s_state_loop.try_lock().unwrap().start_stop_state =
                                        *peer_start_stop_state;

                                    update_discovery(
                                        s_state_loop.clone(),
                                        peer_state_loop.clone(),
                                        discovery_loop.clone(),
                                    )
                                    .await;

                                    if *s_stop_sync_enabled_loop.try_lock().unwrap() {
                                        let (timeline, ghost_x_form) = {
                                            let s_state = s_state_loop.try_lock().unwrap();
                                            (s_state.timeline, s_state.ghost_x_form)
                                        };
                                        c_state_loop.try_lock().unwrap().start_stop_state =
                                            map_start_stop_state_from_session_to_client(
                                                *peer_start_stop_state,
                                                timeline,
                                                ghost_x_form,
                                            )
                                    }
                                }
                            }
                            PeerStateChange::PeerLeft => {
                                let s_id = peer_state_loop.try_lock().unwrap().session_id();
                                let count = unique_session_peer_count(s_id, p_loop.clone());
                                let old_count =
                                    s_peer_counter_loop.try_lock().unwrap().session_peer_count;
                                s_peer_counter_loop.try_lock().unwrap().session_peer_count = count;
                                if old_count != count && count == 0 {
                                    reset_state(
                                        peer_state_loop.clone(),
                                        s_state_loop.clone(),
                                        c_state_loop.clone(),
                                        discovery_loop.clone(),
                                        sessions_loop.clone(),
                                        clock,
                                        s_stop_sync_enabled_loop.clone(),
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
            }
        });

        Self {
            // tempo_callback,
            // start_stop_callback,
            peer_state,
            session_state,
            client_state,
            // last_is_playing_for_start_stop_state_callback: false,
            session_peer_counter: session_peer_counter.clone(),
            enabled: Arc::new(Mutex::new(false)),
            start_stop_sync_enabled,
            peers: peers.clone(),
            sessions,
            discovery,
            clock,
            rx_event: Some(rx_event),
            notifier,
        }
    }

    pub async fn enable(&mut self) {
        *self.enabled.try_lock().unwrap() = true;
        
        reset_state(
            self.peer_state.clone(),
            self.session_state.clone(),
            self.client_state.clone(),
            self.discovery.clone(),
            self.sessions.clone(),
            self.clock,
            self.start_stop_sync_enabled.clone(),
        )
        .await;

        // Only start the discovery listener if it hasn't been started already
        if let Some(rx_event) = self.rx_event.take() {
            let discovery = self.discovery.clone();
            let notifier = self.notifier.clone();

            tokio::spawn(async move {
                discovery.listen(rx_event, notifier).await;
            });
        }
    }

    pub async fn disable(&mut self) {
        *self.enabled.try_lock().unwrap() = false;
        
        // TODO: Implement proper cleanup of discovery/networking
        // For now, just set enabled to false
    }

    pub async fn set_state(&self, mut new_client_state: IncomingClientState) {
        info!("setting state");
        if let Some(timeline) = new_client_state.timeline.as_mut() {
            *timeline = clamp_tempo(*timeline);
            self.client_state.try_lock().unwrap().timeline = *timeline;
        }

        if let Some(mut start_stop_state) = new_client_state.start_stop_state {
            start_stop_state = select_preferred_start_stop_state(
                self.client_state.try_lock().unwrap().start_stop_state,
                start_stop_state,
            );
            self.client_state.try_lock().unwrap().start_stop_state = start_stop_state;
        }

        self.handle_client_state(new_client_state).await
    }

    pub async fn handle_client_state(&self, client_state: IncomingClientState) {
        let mut must_update_discovery = false;

        info!("client_state: {:?}", client_state);

        if let Some(timeline) = client_state.timeline {
            let (session_timeline, ghost_x_form) = {
                let session_state = self.session_state.try_lock().unwrap();
                (session_state.timeline, session_state.ghost_x_form)
            };
            
            let session_timeline = update_session_timeline_from_client(
                session_timeline,
                timeline,
                client_state.timeline_timestamp,
                ghost_x_form,
            );

            self.sessions.reset_timeline(session_timeline);

            // setSessionTimeline
            let peer_session_id = self.peer_state.try_lock().unwrap().session_id();
            for peer in self.peers.try_lock().unwrap().iter_mut().filter(|p| {
                p.peer_state.session_id() == peer_session_id
            }) {
                peer.peer_state.node_state.timeline = session_timeline;
            }
            
            let ghost_x_form = {
                let session_state = self.session_state.try_lock().unwrap();
                session_state.ghost_x_form
            };
            
            update_session_timing(
                self.session_state.clone(),
                self.client_state.clone(),
                session_timeline,
                ghost_x_form,
                self.clock,
                self.start_stop_sync_enabled.clone(),
            );

            must_update_discovery = true;
        }

        if let Some(client_start_stop_state) = client_state.start_stop_state {
            if *self.start_stop_sync_enabled.try_lock().unwrap() {
                let new_ghost_time = self
                    .session_state
                    .try_lock()
                    .unwrap()
                    .ghost_x_form
                    .host_to_ghost(client_start_stop_state.timestamp);

                if new_ghost_time
                    > self
                        .session_state
                        .try_lock()
                        .unwrap()
                        .start_stop_state
                        .timestamp
                {
                    let mut session_state = self.session_state.try_lock().unwrap();
                    session_state.start_stop_state =
                        map_start_stop_state_from_client_to_session(
                            client_start_stop_state,
                            session_state.timeline,
                            session_state.ghost_x_form,
                        );

                    self.client_state.try_lock().unwrap().start_stop_state =
                        client_start_stop_state;

                    must_update_discovery = true;
                }
            }
        }

        if must_update_discovery {
            info!("updating discovery");
            update_discovery(
                self.session_state.clone(),
                self.peer_state.clone(),
                self.discovery.clone(),
            )
            .await;
        }
    }

    pub fn is_enabled(&self) -> bool {
        *self.enabled.try_lock().unwrap()
    }

    pub fn is_start_stop_sync_enabled(&self) -> bool {
        *self.start_stop_sync_enabled.try_lock().unwrap()
    }

    pub fn enable_start_stop_sync(&mut self, enable: bool) {
        *self.start_stop_sync_enabled.try_lock().unwrap() = enable;
    }

    pub fn num_peers(&self) -> usize {
        self.session_peer_counter
            .try_lock()
            .unwrap()
            .session_peer_count
    }
}

pub async fn join_session(
    session: Session,
    peer_state: Arc<Mutex<PeerState>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
    discovery: Arc<PeerGateway>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_peer_count: Arc<Mutex<SessionPeerCounter>>,
    sessions: Sessions,
) {
    let session_id_changed = peer_state.try_lock().unwrap().session_id() != session.session_id;
    peer_state.try_lock().unwrap().node_state.session_id = session.session_id;

    if session_id_changed {
        reset_session_start_stop_state(session_state.clone())
    }

    update_session_timing(
        session_state.clone(),
        client_state.clone(),
        session.timeline,
        session.measurement.x_form,
        clock,
        start_stop_sync_enabled.clone(),
    );

    update_discovery(session_state.clone(), peer_state.clone(), discovery.clone()).await;

    if session_id_changed {
        info!(
            "joining session {} with tempo {}",
            session.session_id,
            session.timeline.tempo.bpm().round()
        );

        // session_peer_counter(session_id, peers, session_peer_count);

        let s_id = peer_state.try_lock().unwrap().session_id();
        let count = unique_session_peer_count(s_id, peers);
        let old_count = session_peer_count.try_lock().unwrap().session_peer_count;
        session_peer_count.try_lock().unwrap().session_peer_count = count;
        if old_count != count && count == 0 {
            reset_state(
                peer_state.clone(),
                session_state.clone(),
                client_state,
                discovery,
                sessions,
                clock,
                start_stop_sync_enabled,
            )
            .await;
        }
    }
}

pub async fn reset_state(
    peer_state: Arc<Mutex<PeerState>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    discovery: Arc<PeerGateway>,
    mut sessions: Sessions,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
) {
    let n_id = NodeId::new();
    peer_state.try_lock().unwrap().node_state.node_id = n_id;
    peer_state.try_lock().unwrap().node_state.session_id = SessionId(n_id);

    let x_form = init_x_form(clock);
    let host_time = -x_form.intercept;

    let timeline = session_state.try_lock().unwrap().timeline;

    let new_tl = Timeline {
        tempo: timeline.tempo,
        beat_origin: timeline.to_beats(
            session_state
                .try_lock()
                .unwrap()
                .ghost_x_form
                .host_to_ghost(host_time),
        ),
        time_origin: x_form.host_to_ghost(host_time),
        // time_origin: Duration::zero(),
    };

    info!(
        "initializing new session {} with timeline {:?}",
        peer_state.try_lock().unwrap().node_state.session_id,
        new_tl,
    );

    reset_session_start_stop_state(session_state.clone());

    update_session_timing(
        session_state.clone(),
        client_state.clone(),
        new_tl,
        x_form,
        clock,
        start_stop_sync_enabled,
    );

    update_discovery(session_state.clone(), peer_state.clone(), discovery.clone()).await;

    sessions.reset_session(Session {
        session_id: peer_state.try_lock().unwrap().session_id(),
        timeline: new_tl,
        measurement: SessionMeasurement {
            x_form,
            timestamp: host_time,
        },
    });

    discovery.observer.reset_peers();
}

pub async fn update_discovery(
    session_state: Arc<Mutex<SessionState>>,
    peer_state: Arc<Mutex<PeerState>>,
    discovery: Arc<PeerGateway>,
) {
    let timeline = session_state.try_lock().unwrap().timeline;
    let start_stop_state = session_state.try_lock().unwrap().start_stop_state;
    let ghost_xform = session_state.try_lock().unwrap().ghost_x_form;

    let node_id = peer_state.try_lock().unwrap().node_state.node_id;
    let session_id = peer_state.try_lock().unwrap().session_id();
    let measurement_endpoint = peer_state.try_lock().unwrap().measurement_endpoint;

    discovery
        .update_node_state(
            NodeState {
                node_id,
                session_id,
                timeline,
                start_stop_state,
            },
            measurement_endpoint,
            ghost_xform,
        )
        .await;
}

pub fn reset_session_start_stop_state(session_state: Arc<Mutex<SessionState>>) {
    session_state.try_lock().unwrap().start_stop_state = StartStopState::default();
}

pub fn update_session_timing(
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    new_timeline: Timeline,
    new_x_form: GhostXForm,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
) {
    let new_timeline = clamp_tempo(new_timeline);

    if let Ok(mut session_state) = session_state.try_lock() {
        let old_timeline = session_state.timeline;
        let old_x_form = session_state.ghost_x_form;

        if old_timeline != new_timeline || old_x_form != new_x_form {
            session_state.timeline = new_timeline;
            session_state.ghost_x_form = new_x_form;

            let old_client_timeline = client_state.try_lock().unwrap().timeline;
            client_state.try_lock().unwrap().timeline = update_client_timeline_from_session(
                new_timeline,
                old_client_timeline,
                clock.micros(),
                new_x_form,
            );

            if *start_stop_sync_enabled.try_lock().unwrap()
                && session_state.start_stop_state != StartStopState::default()
            {
                client_state.try_lock().unwrap().start_stop_state =
                    map_start_stop_state_from_session_to_client(
                        session_state.start_stop_state,
                        session_state.timeline,
                        session_state.ghost_x_form,
                    );
            }

            if old_timeline.tempo != new_timeline.tempo {
                // TODO: user callback
            }
        }
    }
}

fn init_x_form(clock: Clock) -> GhostXForm {
    GhostXForm {
        slope: 1.0,
        intercept: -clock.micros(),
    }
}

fn init_session_state(tempo: tempo::Tempo, clock: Clock) -> SessionState {
    SessionState {
        timeline: clamp_tempo(Timeline {
            tempo,
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        }),
        start_stop_state: StartStopState {
            is_playing: false,
            beats: Beats::new(0.0),
            timestamp: Duration::microseconds(0),
        },
        ghost_x_form: init_x_form(clock),
    }
}

fn init_client_state(session_state: SessionState) -> ClientState {
    let host_time = session_state
        .ghost_x_form
        .ghost_to_host(Duration::microseconds(0));

    ClientState {
        timeline: Timeline {
            tempo: session_state.timeline.tempo,
            beat_origin: session_state.timeline.beat_origin,
            time_origin: host_time,
        },
        start_stop_state: ClientStartStopState {
            is_playing: session_state.start_stop_state.is_playing,
            time: host_time,
            timestamp: host_time,
        },
    }
}

fn select_preferred_start_stop_state(
    current_start_stop_state: ClientStartStopState,
    start_stop_state: ClientStartStopState,
) -> ClientStartStopState {
    if start_stop_state.timestamp > current_start_stop_state.timestamp {
        return start_stop_state;
    }

    current_start_stop_state
}

fn map_start_stop_state_from_session_to_client(
    session_start_stop_state: StartStopState,
    session_timeline: Timeline,
    x_form: GhostXForm,
) -> ClientStartStopState {
    let time = x_form.ghost_to_host(session_timeline.from_beats(session_start_stop_state.beats));
    let timestamp = x_form.ghost_to_host(session_start_stop_state.timestamp);
    ClientStartStopState {
        is_playing: session_start_stop_state.is_playing,
        time,
        timestamp,
    }
}

fn map_start_stop_state_from_client_to_session(
    client_start_stop_state: ClientStartStopState,
    session_timeline: Timeline,
    x_form: GhostXForm,
) -> StartStopState {
    let session_beats =
        session_timeline.to_beats(x_form.host_to_ghost(client_start_stop_state.time));
    let timestamp = x_form.host_to_ghost(client_start_stop_state.timestamp);
    StartStopState {
        is_playing: client_start_stop_state.is_playing,
        beats: session_beats,
        timestamp,
    }
}

#[derive(Debug, Default)]
pub struct SessionPeerCounter {
    // callback: Option<PeerCountCallback>,
    pub session_peer_count: usize,
}
