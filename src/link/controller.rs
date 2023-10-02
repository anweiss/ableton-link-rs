use std::sync::{
    atomic::{AtomicBool, AtomicUsize},
    Arc, Mutex,
};

use chrono::Duration;

use crate::discovery::{gateway::PeerGateway, peers::ControllerPeer};

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
    node_id: NodeId,
    session_id: Arc<Mutex<SessionId>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    // last_is_playing_for_start_stop_state_callback: bool,
    // has_pending_rt_client_states: Arc<AtomicBool>,
    session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    start_stop_sync_enabled: Arc<AtomicBool>,
    // rt_client_state_setter: RtClientStateSetter,
    // peers: ControllerPeer,
    sessions: Sessions,
    discovery: PeerGateway,
    clock: Clock,
}

impl Controller {
    pub async fn new(
        tempo: tempo::Tempo,
        // peer_count_callback: Option<PeerCountCallback>,
        // tempo_callback: Option<TempoCallback>,
        // start_stop_callback: Option<StartStopCallback>,
        clock: Clock,
    ) -> Self {
        let mut rng = rand::thread_rng();
        let node_id = NodeId::random(&mut rng);
        let session_peer_counter = Arc::new(Mutex::new(SessionPeerCounter::default()));
        let session_id = Arc::new(Mutex::new(SessionId(node_id)));
        let session_state = init_session_state(tempo, clock);
        let client_state = Arc::new(Mutex::new(init_client_state(session_state)));

        let timeline = session_state.timeline;

        let (tx_measure_peer, rx_measure_peer) = tokio::sync::mpsc::channel(1);

        Self {
            // tempo_callback,
            // start_stop_callback,
            node_id,
            session_id: session_id.clone(),
            session_state: Arc::new(Mutex::new(session_state)),
            client_state,
            // last_is_playing_for_start_stop_state_callback: false,
            // has_pending_rt_client_states: Arc::new(AtomicBool::new(false)),
            session_peer_counter: session_peer_counter.clone(),
            start_stop_sync_enabled: Arc::new(AtomicBool::new(false)),
            // rt_client_state_setter: RtClientStateSetter,
            // peers: ControllerPeer::default(),
            sessions: Sessions::new(
                Session {
                    session_id: session_id.clone(),
                    timeline,
                    measurement: SessionMeasurement::default(),
                },
                Arc::new(Mutex::new(Vec::new())),
                tx_measure_peer,
            ),
            discovery: PeerGateway::new(
                NodeState {
                    node_id,
                    session_id: session_id.clone(),
                    timeline,
                    start_stop_state: StartStopState::default(),
                },
                GhostXForm::default(),
                rx_measure_peer,
                clock,
                session_peer_counter,
            )
            .await,
            clock,
        }
    }

    pub async fn enable(&mut self) {
        self.discovery.listen().await;
    }

    pub async fn update_discovery(&mut self) {
        let timeline = self.session_state.lock().unwrap().timeline;
        let start_stop_state = self.session_state.lock().unwrap().start_stop_state;
        let ghost_xform = self.session_state.lock().unwrap().ghost_x_form;

        self.discovery
            .update_node_state(
                NodeState {
                    node_id: self.node_id,
                    session_id: self.session_id.clone(),
                    timeline,
                    start_stop_state,
                },
                ghost_xform,
            )
            .await;
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
    session_peer_count: usize,
}
