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
    state::{ControllerClientState, SessionState, StartStopState},
    tempo,
    timeline::{clamp_tempo, Timeline},
    PeerCountCallback, StartStopCallback, TempoCallback,
};

pub struct Controller {
    tempo_callback: Option<TempoCallback>,
    start_stop_callback: Option<StartStopCallback>,
    node_id: NodeId,
    session_id: SessionId,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ControllerClientState>>,
    // last_is_playing_for_start_stop_state_callback: bool,
    has_pending_rt_client_states: Arc<AtomicBool>,
    session_peer_counter: SessionPeerCounter,
    start_stop_sync_enabled: Arc<AtomicBool>,
    rt_client_state_setter: RtClientStateSetter,
    peers: ControllerPeer,
    sessions: Sessions,
    discovery: PeerGateway,
    clock: Clock,
}

impl Controller {
    pub async fn new(
        tempo: tempo::Tempo,
        peer_count_callback: Option<PeerCountCallback>,
        tempo_callback: Option<TempoCallback>,
        start_stop_callback: Option<StartStopCallback>,
        clock: Clock,
    ) -> Self {
        let mut rng = rand::thread_rng();
        let node_id = NodeId::random(&mut rng);
        let session_id = SessionId(node_id);
        let session_state = Arc::new(Mutex::new(init_session_state(tempo, clock)));

        let timeline = session_state.lock().unwrap().timeline;

        let (tx_measure_peer, rx_measure_peer) = tokio::sync::mpsc::channel(1);

        Self {
            tempo_callback,
            start_stop_callback,
            node_id,
            session_id,
            session_state: session_state.clone(),
            client_state: Arc::new(Mutex::new(ControllerClientState::default())),
            // last_is_playing_for_start_stop_state_callback: false,
            has_pending_rt_client_states: Arc::new(AtomicBool::new(false)),
            session_peer_counter: SessionPeerCounter::new(peer_count_callback),
            start_stop_sync_enabled: Arc::new(AtomicBool::new(false)),
            rt_client_state_setter: RtClientStateSetter,
            peers: ControllerPeer::default(),
            sessions: Sessions::new(
                Session {
                    session_id,
                    timeline,
                    measurement: SessionMeasurement::default(),
                },
                Arc::new(Mutex::new(Vec::new())),
                tx_measure_peer,
            ),
            discovery: PeerGateway::new(
                NodeState {
                    node_id,
                    session_id,
                    timeline,
                    start_stop_state: StartStopState::default(),
                },
                GhostXForm::default(),
                rx_measure_peer,
                clock,
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
                    session_id: self.session_id,
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

pub struct SessionPeerCounter {
    callback: Option<PeerCountCallback>,
    session_peer_count: AtomicUsize,
}

impl SessionPeerCounter {
    pub fn new(callback: Option<PeerCountCallback>) -> Self {
        Self {
            callback,
            session_peer_count: AtomicUsize::new(0),
        }
    }
}

pub struct RtClientStateSetter;
