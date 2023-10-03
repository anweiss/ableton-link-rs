use std::sync::{
    atomic::{AtomicBool, AtomicUsize},
    Arc, Mutex,
};

use chrono::Duration;
use tokio::sync::mpsc::{self, Receiver};

use crate::discovery::{
    gateway::{OnEvent, PeerGateway},
    peers::{ControllerPeer, PeerStateChange},
};

use super::{
    beats::Beats,
    clock::Clock,
    ghostxform::GhostXForm,
    node::{NodeId, NodeState},
    sessions::{Session, SessionId, SessionMeasurement, Sessions},
    state::{
        ClientStartStopState, ClientState, ControllerClientState, SessionState, StartStopState,
    },
    tempo,
    timeline::{clamp_tempo, Timeline},
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
    start_stop_sync_enabled: Arc<AtomicBool>,
    // rt_client_state_setter: RtClientStateSetter,
    // peers: ControllerPeer,
    sessions: Arc<Mutex<Sessions>>,
    discovery: Arc<PeerGateway>,
    clock: Clock,
    rx_event: Option<Receiver<OnEvent>>,
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

        let timeline = s_state.timeline;

        let session_state = Arc::new(Mutex::new(s_state));

        let (tx_measure_peer, rx_measure_peer) = tokio::sync::mpsc::channel(1);
        let (tx_peer_state_change, mut rx_peer_state_change) = tokio::sync::mpsc::channel(1);
        let (tx_event, rx_event) = tokio::sync::mpsc::channel::<OnEvent>(1);

        let discovery = Arc::new(
            PeerGateway::new(
                NodeState {
                    node_id: n_id,
                    session_id: session_id.clone(),
                    timeline,
                    start_stop_state: StartStopState::default(),
                },
                GhostXForm::default(),
                rx_measure_peer,
                clock,
                session_peer_counter.clone(),
                tx_peer_state_change,
                tx_event,
            )
            .await,
        );

        let sessions = Arc::new(Mutex::new(Sessions::new(
            Session {
                session_id: session_id.clone(),
                timeline,
                measurement: SessionMeasurement::default(),
            },
            Arc::new(Mutex::new(Vec::new())),
            tx_measure_peer,
        )));

        let n_id_loop = node_id.clone();
        let discovery_loop = discovery.clone();
        let s_id_loop = session_id.clone();
        let s_state_loop = session_state.clone();
        let sessions_loop = sessions.clone();

        tokio::spawn(async move {
            loop {
                match rx_peer_state_change.recv().await {
                    Some(PeerStateChange::SessionMembership) => {
                        let n_id = NodeId::new();
                        *n_id_loop.lock().unwrap() = n_id;
                        *s_id_loop.lock().unwrap() = SessionId(n_id);

                        let x_form = init_x_form(clock);
                        let host_time = -x_form.intercept;

                        let new_tl = Timeline {
                            tempo: s_state_loop.lock().unwrap().timeline.tempo,
                            beat_origin: s_state_loop.lock().unwrap().timeline.to_beats(
                                s_state_loop
                                    .lock()
                                    .unwrap()
                                    .ghost_x_form
                                    .host_to_ghost(host_time),
                            ),
                            time_origin: x_form.host_to_ghost(host_time),
                        };

                        reset_session_start_stop_state(s_state_loop.clone());

                        update_session_timing(s_state_loop.clone(), new_tl, x_form);
                        update_discovery(
                            s_state_loop.clone(),
                            n_id_loop.clone(),
                            s_id_loop.clone(),
                            discovery_loop.clone(),
                        )
                        .await;

                        sessions_loop.lock().unwrap().reset_session(Session {
                            session_id: s_id_loop.clone(),
                            timeline: new_tl,
                            measurement: SessionMeasurement {
                                x_form,
                                timestamp: host_time,
                            },
                        });

                        discovery_loop.observer.reset_peers();
                    }
                    Some(PeerStateChange::SessionTimeline) => {}
                    _ => todo!(),
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
            start_stop_sync_enabled: Arc::new(AtomicBool::new(false)),
            // rt_client_state_setter: RtClientStateSetter,
            // peers: ControllerPeer::default(),
            sessions,
            discovery,
            clock,
            rx_event: Some(rx_event),
        }
    }

    pub async fn reset_state(&mut self) {}

    pub async fn enable(&mut self) {
        self.discovery.listen(self.rx_event.as_mut().unwrap()).await;
    }
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
    new_timeline: Timeline,
    new_x_form: GhostXForm,
) {
    todo!()
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

fn map_start_stop_state_form_session_to_client(
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
