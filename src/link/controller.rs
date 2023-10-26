use std::sync::{Arc, Mutex};

use chrono::Duration;
use tokio::{
    select,
    sync::{mpsc::Receiver, Notify},
};
use tracing::info;

use crate::discovery::{
    gateway::{OnEvent, PeerGateway},
    peers::PeerStateChange,
};

use super::{
    beats::Beats,
    clock::Clock,
    ghostxform::GhostXForm,
    measurement::MeasurePeerEvent,
    node::{NodeId, NodeState},
    sessions::{Session, SessionId, SessionMeasurement, Sessions},
    state::{ClientStartStopState, ClientState, SessionState, StartStopState},
    tempo,
    timeline::{clamp_tempo, update_client_timeline_from_session, Timeline},
};

pub const LOCAL_MOD_GRACE_PERIOD: Duration = Duration::milliseconds(1000);

pub struct Controller {
    // tempo_callback: Option<TempoCallback>,
    // start_stop_callback: Option<StartStopCallback>,
    node_id: Arc<Mutex<NodeId>>,
    session_id: Arc<Mutex<SessionId>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    // last_is_playing_for_start_stop_state_callback: bool,
    // has_pending_rt_client_states: Arc<AtomicBool>,
    session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
    // rt_client_state_setter: RtClientStateSetter,
    // peers: ControllerPeer,
    sessions: Sessions,
    discovery: Arc<PeerGateway>,
    clock: Clock,
    rx_event: Option<Receiver<OnEvent>>,
    rx_measure_peer: Option<tokio::sync::broadcast::Receiver<MeasurePeerEvent>>,
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
        let n_id = NodeId::new();
        let node_id = Arc::new(Mutex::new(n_id));
        let session_peer_counter = Arc::new(Mutex::new(SessionPeerCounter::default()));
        let session_id = Arc::new(Mutex::new(SessionId(n_id)));
        let s_state = init_session_state(tempo, clock);
        let client_state = Arc::new(Mutex::new(init_client_state(s_state)));
        let start_stop_sync_enabled = Arc::new(Mutex::new(false));

        let timeline = s_state.timeline;

        let session_state = Arc::new(Mutex::new(s_state));

        let (tx_measure_peer, rx_measure_peer) = tokio::sync::broadcast::channel(1);
        let (tx_peer_state_change, mut rx_peer_state_change) = tokio::sync::mpsc::channel(1);
        let (tx_event, rx_event) = tokio::sync::mpsc::channel::<OnEvent>(1);
        let (tx_join_session, mut rx_join_session) = tokio::sync::mpsc::channel::<Session>(1);

        let peers = Arc::new(Mutex::new(vec![]));
        let notifier = Arc::new(Notify::new());

        let discovery = Arc::new(
            PeerGateway::new(
                NodeState {
                    node_id: n_id,
                    session_id: *session_id.lock().unwrap(),
                    timeline,
                    start_stop_state: StartStopState::default(),
                },
                GhostXForm::default(),
                clock,
                session_peer_counter.clone(),
                tx_peer_state_change,
                tx_event,
                tx_measure_peer.clone(),
                peers.clone(),
                notifier.clone(),
            )
            .await,
        );

        let s_id_loop = session_id.clone();
        let s_state_loop = session_state.clone();
        let c_state_loop = client_state.clone();
        let s_stop_sync_enabled_loop = start_stop_sync_enabled.clone();
        let n_id_loop = node_id.clone();
        let discovery_loop = discovery.clone();

        tokio::spawn(async move {
            loop {
                if let Some(session) = rx_join_session.recv().await {
                    join_session(
                        session,
                        s_id_loop.clone(),
                        s_state_loop.clone(),
                        c_state_loop.clone(),
                        clock,
                        s_stop_sync_enabled_loop.clone(),
                        n_id_loop.clone(),
                        discovery_loop.clone(),
                    )
                    .await;
                }
            }
        });

        let sessions = Sessions::new(
            Session {
                session_id: *session_id.lock().unwrap(),
                timeline,
                measurement: SessionMeasurement::default(),
            },
            tx_measure_peer,
            peers,
            clock,
            tx_join_session,
        );

        let n_id_loop = node_id.clone();
        let discovery_loop = discovery.clone();
        let s_id_loop = session_id.clone();
        let s_state_loop = session_state.clone();
        let c_state_loop = client_state.clone();
        let sessions_loop = sessions.clone();
        let s_stop_sync_enabled_loop = start_stop_sync_enabled.clone();

        tokio::spawn(async move {
            loop {
                if let Some(peer_state_changes) = rx_peer_state_change.recv().await {
                    info!("received peer state changes");
                    for peer_state_change in peer_state_changes.iter() {
                        match peer_state_change {
                            PeerStateChange::SessionMembership => {
                                reset_state(
                                    n_id_loop.clone(),
                                    s_id_loop.clone(),
                                    s_state_loop.clone(),
                                    c_state_loop.clone(),
                                    discovery_loop.clone(),
                                    sessions_loop.clone(),
                                    clock,
                                    s_stop_sync_enabled_loop.clone(),
                                )
                                .await
                            }
                            PeerStateChange::SessionTimeline(peer_session, timeline) => {
                                // handle_timeline_from_session

                                info!(
                                    "received timeline with tempo: {} for session: {}",
                                    timeline.tempo.bpm(),
                                    peer_session
                                );

                                let new_timeline = sessions_loop
                                    .saw_session_timeline(*peer_session, *timeline)
                                    .unwrap();

                                update_session_timing(
                                    s_state_loop.clone(),
                                    c_state_loop.clone(),
                                    new_timeline,
                                    None,
                                    clock,
                                    s_stop_sync_enabled_loop.clone(),
                                );

                                update_discovery(
                                    s_state_loop.clone(),
                                    n_id_loop.clone(),
                                    s_id_loop.clone(),
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
                                    "received start stop state. isPlaying: {}, beats: {}, time: {} for session: {}",
                                    peer_start_stop_state.is_playing,
                                    peer_start_stop_state.beats.floating(),
                                    peer_start_stop_state.timestamp.num_microseconds().unwrap(),
                                    peer_session,
                                );

                                if *peer_session == *s_id_loop.lock().unwrap()
                                    && peer_start_stop_state.timestamp
                                        > s_state_loop.lock().unwrap().start_stop_state.timestamp
                                {
                                    s_state_loop.lock().unwrap().start_stop_state =
                                        *peer_start_stop_state;

                                    update_discovery(
                                        s_state_loop.clone(),
                                        n_id_loop.clone(),
                                        s_id_loop.clone(),
                                        discovery_loop.clone(),
                                    )
                                    .await;

                                    if *s_stop_sync_enabled_loop.lock().unwrap() {
                                        c_state_loop.lock().unwrap().start_stop_state =
                                            map_start_stop_state_from_session_to_client(
                                                *peer_start_stop_state,
                                                s_state_loop.lock().unwrap().timeline,
                                                s_state_loop.lock().unwrap().ghost_x_form,
                                            )
                                    }
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
            node_id,
            session_id: session_id.clone(),
            session_state,
            client_state,
            // last_is_playing_for_start_stop_state_callback: false,
            // has_pending_rt_client_states: Arc::new(AtomicBool::new(false)),
            session_peer_counter: session_peer_counter.clone(),
            start_stop_sync_enabled,
            // rt_client_state_setter: RtClientStateSetter,
            // peers: ControllerPeer::default(),
            sessions,
            discovery,
            clock,
            rx_event: Some(rx_event),
            rx_measure_peer: Some(rx_measure_peer),
            notifier,
        }
    }

    pub async fn enable(&mut self) {
        self.discovery
            .listen(self.rx_event.take().unwrap(), self.notifier.clone())
            .await;
    }
}

pub async fn join_session(
    session: Session,
    session_id: Arc<Mutex<SessionId>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
    node_id: Arc<Mutex<NodeId>>,
    discovery: Arc<PeerGateway>,
) {
    let session_id_changed = *session_id.lock().unwrap() != session.session_id;
    *session_id.lock().unwrap() = session.session_id;

    if session_id_changed {
        reset_session_start_stop_state(session_state.clone())
    }

    update_session_timing(
        session_state.clone(),
        client_state,
        session.timeline,
        Some(session.measurement.x_form),
        clock,
        start_stop_sync_enabled,
    );

    update_discovery(session_state, node_id, session_id, discovery).await;

    if session_id_changed {
        info!(
            "joining session {} with tempo {}",
            session.session_id,
            session.timeline.tempo.bpm()
        );
    }
}

pub async fn reset_state(
    node_id: Arc<Mutex<NodeId>>,
    session_id: Arc<Mutex<SessionId>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    discovery: Arc<PeerGateway>,
    mut sessions: Sessions,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
) {
    let n_id = NodeId::new();
    *node_id.lock().unwrap() = n_id;
    *session_id.lock().unwrap() = SessionId(n_id);

    let x_form = init_x_form(clock);
    let host_time = -x_form.intercept;

    let new_tl = Timeline {
        tempo: session_state.lock().unwrap().timeline.tempo,
        beat_origin: session_state.lock().unwrap().timeline.to_beats(
            session_state
                .lock()
                .unwrap()
                .ghost_x_form
                .host_to_ghost(host_time),
        ),
        time_origin: x_form.host_to_ghost(host_time),
    };

    reset_session_start_stop_state(session_state.clone());

    update_session_timing(
        session_state.clone(),
        client_state.clone(),
        new_tl,
        Some(x_form),
        clock,
        start_stop_sync_enabled,
    );

    update_discovery(
        session_state.clone(),
        node_id.clone(),
        session_id.clone(),
        discovery.clone(),
    )
    .await;

    sessions.reset_session(Session {
        session_id: *session_id.lock().unwrap(),
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
    node_id: Arc<Mutex<NodeId>>,
    session_id: Arc<Mutex<SessionId>>,
    discovery: Arc<PeerGateway>,
) {
    let timeline = session_state.lock().unwrap().timeline;
    let start_stop_state = session_state.lock().unwrap().start_stop_state;
    let ghost_xform = session_state.lock().unwrap().ghost_x_form;

    let node_id = *node_id.lock().unwrap();
    let session_id = *session_id.lock().unwrap();

    discovery
        .update_node_state(
            NodeState {
                node_id,
                session_id,
                timeline,
                start_stop_state,
            },
            ghost_xform,
        )
        .await;
}

pub fn reset_session_start_stop_state(session_state: Arc<Mutex<SessionState>>) {
    session_state.lock().unwrap().start_stop_state = StartStopState::default();
}

pub fn update_session_timing(
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    new_timeline: Timeline,
    new_x_form: Option<GhostXForm>,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
) {
    let new_timeline = clamp_tempo(new_timeline);
    let old_timeline = session_state.try_lock().unwrap().timeline;
    let old_x_form = session_state.try_lock().unwrap().ghost_x_form;

    let new_x_form = new_x_form.unwrap_or(old_x_form);

    if old_timeline != new_timeline || old_x_form != new_x_form {
        session_state.lock().unwrap().timeline = new_timeline;
        session_state.lock().unwrap().ghost_x_form = new_x_form;

        let old_client_timeline = client_state.lock().unwrap().timeline;
        client_state.lock().unwrap().timeline = update_client_timeline_from_session(
            new_timeline,
            old_client_timeline,
            clock.micros(),
            new_x_form,
        );

        if *start_stop_sync_enabled.lock().unwrap()
            && session_state.lock().unwrap().start_stop_state != StartStopState::default()
        {
            client_state.lock().unwrap().start_stop_state =
                map_start_stop_state_from_session_to_client(
                    session_state.lock().unwrap().start_stop_state,
                    session_state.lock().unwrap().timeline,
                    session_state.lock().unwrap().ghost_x_form,
                );
        }

        if old_timeline.tempo != new_timeline.tempo {}
    }
}

fn init_x_form(clock: Clock) -> GhostXForm {
    GhostXForm {
        slope: 1.0,
        intercept: clock.micros(),
    }
}

fn init_session_state(tempo: tempo::Tempo, clock: Clock) -> SessionState {
    SessionState {
        timeline: clamp_tempo(Timeline {
            tempo,
            beat_origin: Beats::new(0f64),
            time_origin: Duration::microseconds(0),
        }),
        start_stop_state: StartStopState {
            is_playing: false,
            beats: Beats::new(0f64),
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
