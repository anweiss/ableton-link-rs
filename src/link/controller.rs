use std::sync::{
    atomic::{AtomicBool, AtomicUsize},
    Arc,
};

use tokio::sync::Mutex;

use crate::{
    clock::Clock,
    discovery::{
        gateway::PeerGateway,
        peers::{ControllerPeer, GatewayObserver},
    },
};

use super::{
    ghostxform::GhostXForm,
    node::{NodeId, NodeState},
    sessions::{ControllerClientState, ControllerSessions, SessionId},
    state::{SessionState, StartStopState},
    tempo, PeerCountCallback, StartStopCallback, TempoCallback,
};

pub struct Controller {
    tempo_callback: Option<TempoCallback>,
    start_stop_callback: Option<StartStopCallback>,
    clock: Clock,
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
    sessions: ControllerSessions,
    discovery: PeerGateway,
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

        let timeline = session_state.lock().await.timeline;

        Self {
            tempo_callback,
            start_stop_callback,
            clock,
            node_id: node_id.clone(),
            session_id,
            session_state: session_state.clone(),
            client_state: Arc::new(Mutex::new(ControllerClientState)),
            // last_is_playing_for_start_stop_state_callback: false,
            has_pending_rt_client_states: Arc::new(AtomicBool::new(false)),
            session_peer_counter: SessionPeerCounter::new(peer_count_callback),
            start_stop_sync_enabled: Arc::new(AtomicBool::new(false)),
            rt_client_state_setter: RtClientStateSetter,
            peers: ControllerPeer::default(),
            sessions: ControllerSessions,
            discovery: PeerGateway::new(
                NodeState {
                    node_id,
                    session_id,
                    timeline,
                    start_stop_state: StartStopState::default(),
                },
                GhostXForm::default(),
            )
            .await,
        }
    }

    pub async fn enable(&mut self) {
        self.discovery.listen().await;
    }

    pub async fn update_discovery(&mut self) {
        self.discovery
            .update_node_state(
                NodeState {
                    node_id: self.node_id,
                    session_id: self.session_id,
                    timeline: self.session_state.lock().await.timeline,
                    start_stop_state: self.session_state.lock().await.start_stop_state,
                },
                self.session_state.lock().await.ghost_xform,
            )
            .await;
    }
}

fn init_session_state(tempo: tempo::Tempo, clock: Clock) -> SessionState {
    // todo!()
    SessionState::default()
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
