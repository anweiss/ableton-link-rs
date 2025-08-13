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

        let enabled = Arc::new(Mutex::new(false));
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

        let ping_responder_unicast_socket = Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0).into()).unwrap());

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
                enabled.clone(),
            )
            .await,
        );

        let sessions = Sessions::new(
            Session {
                session_id,
                timeline,
                measurement: SessionMeasurement {
                    x_form: if let Ok(session_state) = session_state.try_lock() {
                        session_state.ghost_x_form
                    } else {
                        GhostXForm::default()
                    },
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
                                debug!("Controller received SessionMembership change");
                                let session_id = if let Ok(ps) = peer_state_loop.try_lock() {
                                    ps.session_id()
                                } else {
                                    continue;
                                };
                                let self_node_id = if let Ok(ps) = peer_state_loop.try_lock() {
                                    ps.ident()
                                } else {
                                    continue;
                                };

                                let count = unique_session_peer_count(
                                    session_id,
                                    p_loop.clone(),
                                    self_node_id,
                                );
                                let old_count = if let Ok(spc) = s_peer_counter_loop.try_lock() {
                                    spc.session_peer_count
                                } else {
                                    continue;
                                };

                                debug!(
                                    "SessionMembership: old_count={}, new_count={}",
                                    old_count, count
                                );

                                // Only update the session peer count if it has actually changed
                                if old_count != count {
                                    if let Ok(mut spc) = s_peer_counter_loop.try_lock() {
                                        spc.session_peer_count = count;
                                    }
                                    debug!(
                                        "Updated session peer count from {} to {}",
                                        old_count, count
                                    );
                                }

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

                                let ghost_x_form = if let Ok(state) = s_state_loop.try_lock() {
                                    state.ghost_x_form
                                } else {
                                    continue;
                                };

                                update_session_timing(
                                    s_state_loop.clone(),
                                    c_state_loop.clone(),
                                    new_timeline,
                                    ghost_x_form,
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

                                let peer_session_id = if let Ok(ps) = peer_state_loop.try_lock() {
                                    ps.session_id()
                                } else {
                                    continue;
                                };

                                let current_timestamp = if let Ok(s_state) = s_state_loop.try_lock()
                                {
                                    s_state.start_stop_state.timestamp
                                } else {
                                    continue;
                                };

                                if *peer_session == peer_session_id
                                    && peer_start_stop_state.timestamp > current_timestamp
                                {
                                    if let Ok(mut s_state) = s_state_loop.try_lock() {
                                        s_state.start_stop_state = *peer_start_stop_state;
                                    } else {
                                        continue;
                                    }

                                    update_discovery(
                                        s_state_loop.clone(),
                                        peer_state_loop.clone(),
                                        discovery_loop.clone(),
                                    )
                                    .await;

                                    let sync_enabled =
                                        if let Ok(enabled) = s_stop_sync_enabled_loop.try_lock() {
                                            *enabled
                                        } else {
                                            continue;
                                        };

                                    if sync_enabled {
                                        let (timeline, ghost_x_form) =
                                            if let Ok(s_state) = s_state_loop.try_lock() {
                                                (s_state.timeline, s_state.ghost_x_form)
                                            } else {
                                                continue;
                                            };

                                        if let Ok(mut c_state) = c_state_loop.try_lock() {
                                            c_state.start_stop_state =
                                                map_start_stop_state_from_session_to_client(
                                                    *peer_start_stop_state,
                                                    timeline,
                                                    ghost_x_form,
                                                );
                                        }
                                    }
                                }
                            }
                            PeerStateChange::PeerLeft => {
                                let s_id = if let Ok(ps) = peer_state_loop.try_lock() {
                                    ps.session_id()
                                } else {
                                    continue;
                                };
                                let peer_ident = if let Ok(ps) = peer_state_loop.try_lock() {
                                    ps.ident()
                                } else {
                                    continue;
                                };
                                let count =
                                    unique_session_peer_count(s_id, p_loop.clone(), peer_ident);
                                let old_count = if let Ok(spc) = s_peer_counter_loop.try_lock() {
                                    spc.session_peer_count
                                } else {
                                    continue;
                                };
                                if let Ok(mut spc) = s_peer_counter_loop.try_lock() {
                                    spc.session_peer_count = count;
                                }
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
            enabled,
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
        if let Ok(mut enabled) = self.enabled.try_lock() {
            *enabled = true;
        }

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
        // Send bye bye message before disabling to properly notify other peers
        use crate::discovery::messenger::send_byebye;
        let node_id = if let Ok(peer_state) = self.peer_state.try_lock() {
            peer_state.node_state.node_id
        } else {
            return; // If we can't get the node_id, we can't send bye message
        };
        send_byebye(node_id);

        if let Ok(mut enabled) = self.enabled.try_lock() {
            *enabled = false;
        }

        // Reset peer count to 0 when disabled, like the C++ implementation
        self.session_peer_counter
            .try_lock()
            .unwrap()
            .session_peer_count = 0;

        // Clear all peers from the discovery
        self.discovery.observer.reset_peers();

        // Give some time for the bye bye message to be sent and processed
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // TODO: Implement proper cleanup of discovery/networking
        // For now, just set enabled to false and reset peers
    }

    pub async fn set_state(&self, mut new_client_state: IncomingClientState) {
        info!("setting state");
        if let Some(timeline) = new_client_state.timeline.as_mut() {
            *timeline = clamp_tempo(*timeline);
            if let Ok(mut client_state) = self.client_state.try_lock() {
                client_state.timeline = *timeline;
            }
        }

        if let Some(mut start_stop_state) = new_client_state.start_stop_state {
            let current_start_stop_state = if let Ok(client_state) = self.client_state.try_lock() {
                client_state.start_stop_state
            } else {
                return; // If we can't access the state, exit early
            };

            start_stop_state =
                select_preferred_start_stop_state(current_start_stop_state, start_stop_state);

            if let Ok(mut client_state) = self.client_state.try_lock() {
                client_state.start_stop_state = start_stop_state;
            }
        }

        self.handle_client_state(new_client_state).await
    }

    pub async fn handle_client_state(&self, client_state: IncomingClientState) {
        let mut must_update_discovery = false;

        info!("client_state: {:?}", client_state);

        if let Some(timeline) = client_state.timeline {
            let (session_timeline, ghost_x_form) =
                if let Ok(session_state) = self.session_state.try_lock() {
                    (session_state.timeline, session_state.ghost_x_form)
                } else {
                    return; // If we can't access session state, exit early
                };

            let session_timeline = update_session_timeline_from_client(
                session_timeline,
                timeline,
                client_state.timeline_timestamp,
                ghost_x_form,
            );

            self.sessions.reset_timeline(session_timeline);

            // setSessionTimeline
            let peer_session_id = if let Ok(peer_state) = self.peer_state.try_lock() {
                peer_state.session_id()
            } else {
                return; // If we can't access peer state, exit early
            };

            if let Ok(mut peers) = self.peers.try_lock() {
                for peer in peers
                    .iter_mut()
                    .filter(|p| p.peer_state.session_id() == peer_session_id)
                {
                    peer.peer_state.node_state.timeline = session_timeline;
                }
            }

            let ghost_x_form = if let Ok(session_state) = self.session_state.try_lock() {
                session_state.ghost_x_form
            } else {
                return; // If we can't access session state, exit early
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
            let sync_enabled = if let Ok(enabled) = self.start_stop_sync_enabled.try_lock() {
                *enabled
            } else {
                return; // If we can't access sync enabled state, exit early
            };

            if sync_enabled {
                let new_ghost_time = if let Ok(session_state) = self.session_state.try_lock() {
                    session_state
                        .ghost_x_form
                        .host_to_ghost(client_start_stop_state.timestamp)
                } else {
                    return; // If we can't access session state, exit early
                };

                let current_timestamp = if let Ok(session_state) = self.session_state.try_lock() {
                    session_state.start_stop_state.timestamp
                } else {
                    return; // If we can't access session state, exit early
                };

                if new_ghost_time > current_timestamp {
                    if let Ok(mut session_state) = self.session_state.try_lock() {
                        session_state.start_stop_state =
                            map_start_stop_state_from_client_to_session(
                                client_start_stop_state,
                                session_state.timeline,
                                session_state.ghost_x_form,
                            );

                        if let Ok(mut client_state) = self.client_state.try_lock() {
                            client_state.start_stop_state = client_start_stop_state;
                        }

                        must_update_discovery = true;
                    }
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
        self.enabled
            .try_lock()
            .map(|enabled| *enabled)
            .unwrap_or(false)
    }

    pub fn is_start_stop_sync_enabled(&self) -> bool {
        self.start_stop_sync_enabled
            .try_lock()
            .map(|enabled| *enabled)
            .unwrap_or(false)
    }

    pub fn enable_start_stop_sync(&mut self, enable: bool) {
        if let Ok(mut sync_enabled) = self.start_stop_sync_enabled.try_lock() {
            *sync_enabled = enable;
        }
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
    let session_id_changed = if let Ok(ps) = peer_state.try_lock() {
        ps.session_id() != session.session_id
    } else {
        debug!("Failed to lock peer_state in join_session");
        return;
    };

    if let Ok(mut ps) = peer_state.try_lock() {
        ps.node_state.session_id = session.session_id;
    } else {
        debug!("Failed to lock peer_state to update session_id");
        return;
    };

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

        let should_reset = if let (Ok(peer_state_guard), Ok(mut session_peer_count_guard)) = 
            (peer_state.try_lock(), session_peer_count.try_lock()) {
            
            let s_id = peer_state_guard.session_id();
            let count = unique_session_peer_count(s_id, peers, peer_state_guard.ident());
            let old_count = session_peer_count_guard.session_peer_count;
            session_peer_count_guard.session_peer_count = count;
            
            old_count != count && count == 0
        } else {
            false
        };
        
        if should_reset {
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
    // Preserve the existing NodeId to maintain peer identity across enable/disable cycles
    let existing_node_id = if let Ok(peer_state_guard) = peer_state.try_lock() {
        peer_state_guard.node_state.node_id
    } else {
        NodeId::default()
    };

    // Only generate a new NodeId if this is the very first initialization
    let n_id = if existing_node_id == NodeId::default() {
        NodeId::new()
    } else {
        existing_node_id
    };

    // Always create a new session when resetting state - this allows joining other sessions
    let s_id = SessionId(n_id);

    if let Ok(mut peer_state_guard) = peer_state.try_lock() {
        peer_state_guard.node_state.node_id = n_id;
        peer_state_guard.node_state.session_id = s_id;
    }

    let x_form = init_x_form(clock);
    let host_time = -x_form.intercept;

    let (timeline, ghost_x_form) = if let Ok(session_state_guard) = session_state.try_lock() {
        (session_state_guard.timeline, session_state_guard.ghost_x_form)
    } else {
        // Fallback to default values if lock fails
        (Timeline::default(), GhostXForm::default())
    };

    let new_tl = Timeline {
        tempo: timeline.tempo,
        beat_origin: timeline.to_beats(ghost_x_form.host_to_ghost(host_time)),
        time_origin: x_form.host_to_ghost(host_time),
        // time_origin: Duration::zero(),
    };

    info!(
        "initializing session {} with timeline {:?} (preserving NodeId: {})",
        s_id, new_tl, n_id,
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
        session_id: s_id,
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
    let (timeline, start_stop_state, ghost_xform) = if let Ok(session_state_guard) = session_state.try_lock() {
        (
            session_state_guard.timeline,
            session_state_guard.start_stop_state,
            session_state_guard.ghost_x_form,
        )
    } else {
        return; // Skip update if we can't get the lock
    };

    let (node_id, session_id, measurement_endpoint) = if let Ok(peer_state_guard) = peer_state.try_lock() {
        (
            peer_state_guard.node_state.node_id,
            peer_state_guard.session_id(),
            peer_state_guard.measurement_endpoint,
        )
    } else {
        return; // Skip update if we can't get the lock
    };

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
    if let Ok(mut session_state_guard) = session_state.try_lock() {
        session_state_guard.start_stop_state = StartStopState::default();
    }
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

            if let Ok(mut client_state_guard) = client_state.try_lock() {
                let old_client_timeline = client_state_guard.timeline;
                client_state_guard.timeline = update_client_timeline_from_session(
                    new_timeline,
                    old_client_timeline,
                    clock.micros(),
                    new_x_form,
                );

                if let Ok(start_stop_enabled) = start_stop_sync_enabled.try_lock() {
                    if *start_stop_enabled && session_state.start_stop_state != StartStopState::default() {
                        client_state_guard.start_stop_state =
                            map_start_stop_state_from_session_to_client(
                                session_state.start_stop_state,
                                session_state.timeline,
                                session_state.ghost_x_form,
                            );
                    }
                }
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
