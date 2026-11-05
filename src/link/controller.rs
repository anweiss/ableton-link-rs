use crate::platform::io_context::{retry_state, try_state};
use std::{
    net::SocketAddrV4,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, Weak},
};

use chrono::Duration;
use tokio::sync::{mpsc::Receiver, Notify};
use tracing::{debug, info};

use crate::discovery::{
    gateway::{OnEvent, PeerGateway},
    messenger::new_udp_reuseport,
    peers::{count_session_peers, ControllerPeer, PeerState, PeerStateChange},
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
    AudioEndpointCallback, IncomingClientState, TempoCallback,
};

/// Start/stop gate for the background dispatch loops (measurement results,
/// join-session and peer-state-change) spawned in [`Controller::new`]. Rust analogue of
/// upstream's `RtClientStateSetter::start()`/`stop()` (see upstream commits
/// `57b77a8040d3` and `44d78f2cf3a4`): the loops are constructed once and
/// gated, rather than torn down, so that a [`Controller::disable`] followed by
/// a [`Controller::enable`] resumes dispatching instead of leaving the
/// receivers permanently closed.
///
/// [`DispatchGate::stop`] is *acknowledged*: it closes the gate against new
/// work and then waits for every consumer to release its permit. Once stop
/// returns, no admitted dispatch is running. Consumers discard queued work
/// while closed, and the next startup acknowledges that drain before opening.
///
/// A permit is held across the consumer's `recv()`, not taken after it, so
/// stop can wait for admitted dispatch to release its permit. Asynchronous
/// measurement forwarders additionally carry their originating epoch.
///
/// Startup first asks every registered consumer to drain its disabled queue.
/// Only after all consumers acknowledge preparation does it admit new work.
/// Register consumers before spawning their tasks, so the first enable also
/// waits for consumers that have not yet been polled.
///
/// The gate starts *closed*, matching `Controller`'s `enabled == false` at
/// construction: dispatch only ever runs between an [`Controller::enable`] and
/// the [`Controller::disable`] that follows it.
#[derive(Debug)]
pub(crate) struct DispatchGate {
    active: tokio::sync::watch::Sender<DispatchState>,
    in_flight: tokio::sync::RwLock<()>,
    subscribers: Mutex<Vec<Weak<std::sync::atomic::AtomicU64>>>,
    ready: Arc<Notify>,
    /// Bumped by every reopening in [`DispatchGate::start`]. Consumers that carry state
    /// across loop iterations - state the gate's own drain cannot reach,
    /// because it does not live in a channel - compare this against the epoch
    /// the state was recorded in and discard it when they differ.
    epoch: std::sync::atomic::AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchState {
    Closed,
    Preparing(u64),
    Open,
}

pub(crate) struct DispatchReceiver {
    state: tokio::sync::watch::Receiver<DispatchState>,
    prepared_epoch: Arc<std::sync::atomic::AtomicU64>,
    ready: Arc<Notify>,
}

impl DispatchReceiver {
    pub(crate) async fn wait_open<F, Fut>(
        &mut self,
        gate: &DispatchGate,
        mut drain: impl FnMut(),
        mut readable: F,
    ) where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = std::io::Result<()>>,
    {
        loop {
            {
                // A preparation drain must not continue past the final opening
                // transition and consume the first newly admitted packet.
                let _registration = gate
                    .subscribers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let state = *self.state.borrow_and_update();
                if state == DispatchState::Open {
                    return;
                }
                drain();
                if let DispatchState::Preparing(epoch) = state {
                    self.prepared_epoch
                        .store(epoch, std::sync::atomic::Ordering::Release);
                    self.ready.notify_one();
                }
            }
            tokio::select! {
                biased;
                changed = self.state.changed() => {
                    if changed.is_err() { return; }
                }
                result = readable() => {
                    if let Err(error) = result {
                        tracing::warn!("disabled ingress readiness failed: {}", error);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    pub(crate) async fn closed(&mut self) {
        let _ = self
            .state
            .wait_for(|state| *state != DispatchState::Open)
            .await;
    }
}

impl Drop for DispatchReceiver {
    fn drop(&mut self) {
        // Mark a departing subscriber ready for any pending start before its
        // weak registration expires, then wake the startup waiter.
        self.prepared_epoch
            .store(u64::MAX, std::sync::atomic::Ordering::Release);
        self.ready.notify_one();
    }
}

impl DispatchGate {
    pub(crate) fn new() -> Self {
        DispatchGate {
            active: tokio::sync::watch::Sender::new(DispatchState::Closed),
            in_flight: tokio::sync::RwLock::new(()),
            subscribers: Mutex::new(Vec::new()),
            ready: Arc::new(Notify::new()),
            epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) fn new_open() -> Self {
        let gate = Self::new();
        gate.active.send_replace(DispatchState::Open);
        gate
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Registers a consumer for the startup barrier and subscribes to state.
    pub(crate) fn subscribe(&self) -> DispatchReceiver {
        let prepared_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Interface receivers may churn without another start() to clean up.
        subscribers.retain(|subscriber| subscriber.strong_count() != 0);
        subscribers.push(Arc::downgrade(&prepared_epoch));
        DispatchReceiver {
            state: self.active.subscribe(),
            prepared_epoch,
            ready: self.ready.clone(),
        }
    }

    /// Acquires the right to receive and run dispatch work. The permit is held
    /// across the consumer's `recv()`, so [`DispatchGate::stop`] cannot return
    /// while a consumer still holds queued work it has not discarded.
    pub(crate) async fn permit(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.in_flight.read().await
    }

    pub(crate) fn is_open(&self) -> bool {
        *self.active.borrow() == DispatchState::Open
    }

    pub(crate) async fn start(&self) {
        self.start_with(|| {}).await;
    }

    async fn start_with(&self, publish: impl FnOnce()) {
        let mut publish = Some(publish);
        self.start_with_retry(|| {
            publish.take().unwrap()();
            Some(())
        })
        .await;
    }

    async fn start_with_retry(&self, mut publish: impl FnMut() -> Option<()>) {
        if self.is_open() {
            retry_state(&mut publish).await;
            return;
        }
        let epoch = self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
        self.active.send_replace(DispatchState::Preparing(epoch));
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            let opened = retry_state(|| {
                let mut subscribers = try_state(&self.subscribers)?;
                subscribers.retain(|subscriber| subscriber.strong_count() != 0);
                let all_ready = subscribers.iter().all(|subscriber| {
                    subscriber.upgrade().is_none_or(|prepared| {
                        prepared.load(std::sync::atomic::Ordering::Acquire) >= epoch
                    })
                });
                if all_ready {
                    // The complete drain/policy attempt and opening share subscribe's lock.
                    publish()?;
                    self.active.send_replace(DispatchState::Open);
                }
                Some(all_ready)
            })
            .await;
            if opened {
                return;
            }
            ready.await;
        }
    }

    pub(crate) async fn stop(&self) {
        self.close();
        // Wait for admitted work to complete or cancel. Startup separately
        // acknowledges draining the queues before admitting fresh work.
        let _ = self.in_flight.write().await;
    }

    /// Closes the gate against new dispatch work without waiting for any batch
    /// already in flight to finish. Idempotent: closing an already-closed gate
    /// is a no-op, mirroring upstream's `LockFreeCallbackDispatcher::stop()`
    /// (`53f0627c9cf8`) being safe to call more than once.
    ///
    /// This is the synchronous half of [`DispatchGate::stop`], used from
    /// [`DispatchTasks`]'s destructor where an `async` wait is not available -
    /// Rust analogue of upstream moving `stopIoService()` (renamed
    /// `shutdown()`) into `~SessionController()` in `cccaecc9e93b`, so
    /// teardown is requested from the destructor rather than relying solely
    /// on an explicit `disable()` having been called first.
    pub(crate) fn close(&self) {
        self.active.send_if_modified(|state| {
            let changed = *state != DispatchState::Closed;
            *state = DispatchState::Closed;
            changed
        });
    }

    pub(crate) async fn run_in_epoch<F: std::future::Future>(
        &self,
        epoch: u64,
        work: F,
    ) -> Option<F::Output> {
        let mut state = self.active.subscribe();
        let _permit = self.permit().await;
        if !self.is_open() || self.epoch() != epoch {
            return None;
        }
        tokio::select! {
            biased;
            _ = state.wait_for(|state| *state != DispatchState::Open) => None,
            result = work => Some(result),
        }
    }
}

/// Owns the controller's dispatch tasks and discovery listener.
/// Drop requests cancellation; it does not join a task currently being polled
/// on another runtime thread. Use `Controller::disable` to await dispatch.
struct DispatchTasks {
    gate: Arc<DispatchGate>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl DispatchTasks {
    fn new() -> Self {
        Self {
            gate: Arc::new(DispatchGate::new()),
            tasks: Vec::new(),
        }
    }
}

impl Drop for DispatchTasks {
    fn drop(&mut self) {
        self.gate.close();
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Waits for the next batch of dispatch work that may legitimately run in the
/// current lifecycle, returning it together with the gate permit that must be
/// held while it is dispatched.
///
/// The permit is taken *before* the `recv()`, not after: a batch is therefore
/// only ever returned while a permit covering the lifecycle it was received in
/// is held, which is what [`DispatchGate::stop`] waits on. When the gate closes
/// while this is parked, everything the channel still holds was produced by the
/// lifecycle being torn down, so it is discarded before the permit is released
/// and can never be admitted after a later [`DispatchGate::start`].
///
/// Returns `None` only when the producers are gone for good.
///
/// The gate epoch the work was admitted under is returned alongside it, so a
/// consumer holding state across loop iterations - state no channel drain can
/// reach - can tell that the batch belongs to a later lifecycle and drop that
/// state instead of releasing it into it.
pub(crate) async fn gated_recv<'a, T>(
    gate: &'a DispatchGate,
    open: &mut DispatchReceiver,
    rx: &mut tokio::sync::mpsc::Receiver<T>,
) -> Option<(tokio::sync::RwLockReadGuard<'a, ()>, u64, T)> {
    loop {
        let state = *open.state.borrow_and_update();
        if state != DispatchState::Open {
            drain_pending(rx);
            if let DispatchState::Preparing(epoch) = state {
                open.prepared_epoch
                    .store(epoch, std::sync::atomic::Ordering::Release);
                open.ready.notify_one();
            }
            // Preparation is acknowledged before opening. Draining after open
            // can erase the first legitimate result produced by a new lifecycle.
            tokio::select! {
                biased;
                changed = open.state.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                }
                work = rx.recv() => {
                    work?;
                }
            }
            continue;
        }

        let permit = gate.permit().await;
        if !gate.is_open() {
            drop(permit);
            continue;
        }
        // Read under the permit: stop() cannot finish (and enable() cannot
        // restart the controller) while it is held. Drop can close the gate
        // without waiting for the permit.
        let epoch = gate.epoch();

        tokio::select! {
            biased;
            _ = open.closed() => {
                drain_pending(rx);
                drop(permit);
            }
            work = rx.recv() => return work.map(|work| (permit, epoch, work)),
        }
    }
}

/// Discards everything currently queued on `rx` without dispatching it. Used by
/// the dispatch loops when the gate closes, so work produced in the lifecycle
/// being torn down cannot be delivered into the next one.
fn drain_pending<T>(rx: &mut tokio::sync::mpsc::Receiver<T>) {
    while rx.try_recv().is_ok() {}
}

/// Invokes the registered audio-endpoint callback, if any. The registered
/// callback is cloned out from under the outer guard so it is never dropped
/// just because registration is momentarily in flight.
pub(crate) fn dispatch_audio_endpoint_change(
    callback: &Arc<Mutex<Option<AudioEndpointCallback>>>,
    peer_id: NodeId,
    endpoint: Option<SocketAddrV4>,
) {
    let callback = callback.lock().ok().and_then(|guard| guard.clone());
    if let Some(callback) = callback {
        if let Ok(callback) = callback.lock() {
            callback(peer_id, endpoint);
        }
    }
}

#[cfg(test)]
type PublicationPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

type WeakTempoCallback = std::sync::Weak<Mutex<Box<dyn Fn(f64) + Send>>>;

struct TempoNotifications {
    callback: Arc<Mutex<Option<TempoCallback>>>,
    client: Arc<Mutex<ClientState>>,
    closed: Arc<AtomicBool>,
    registration: Mutex<Option<TempoCallback>>,
    delivered: Mutex<(Option<WeakTempoCallback>, f64)>,
    dispatch: tokio::sync::Mutex<()>,
}

impl TempoNotifications {
    fn dispatch(&self, initial_requested: bool) {
        let Ok(_dispatch) = self.dispatch.try_lock() else {
            return;
        };
        let Some(registration) = try_state(&self.registration) else {
            return;
        };
        let Some(mut installed) = try_state(&self.callback) else {
            return;
        };
        let initial = registration.is_some();
        let retired = registration
            .as_ref()
            .and_then(|callback| installed.replace(callback.clone()));
        let callback = installed.clone();
        drop(installed);
        drop(registration);
        drop(retired);
        let Some(callback) = callback else { return };
        let Some(invoke) = try_state(&callback) else {
            return;
        };
        // Select the value only after serializing callback delivery.
        let Some(bpm) = try_state(&self.client).map(|state| state.timeline.tempo.bpm()) else {
            return;
        };
        let delivered = self.delivered.lock().unwrap_or_else(|e| e.into_inner());
        let same_callback = delivered
            .0
            .as_ref()
            .is_none_or(|old| std::sync::Weak::ptr_eq(old, &Arc::downgrade(&callback)));
        let initial = initial || (initial_requested && delivered.0.is_none());
        if (!initial && same_callback && delivered.1 == bpm) || self.closed.load(Ordering::Acquire)
        {
            return;
        }
        drop(delivered);
        invoke(bpm);
        *self.delivered.lock().unwrap_or_else(|e| e.into_inner()) =
            (Some(Arc::downgrade(&callback)), bpm);
        if let Some(mut registration) = try_state(&self.registration) {
            if registration
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &callback))
            {
                *registration = None;
            }
        }
    }
}

pub struct Controller {
    #[cfg(test)]
    publication_pause: Arc<Mutex<Option<PublicationPause>>>,
    // Retains cancellation handles; Controller::drop also joins the IO runtime.
    dispatch: DispatchTasks,
    io: Option<crate::platform::io_context::IoContext>,
    callbacks_closed: Arc<AtomicBool>,
    managed_tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
    tempo_notifications: Arc<TempoNotifications>,
    pub tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
    /// Invoked whenever a peer's discovered audio endpoint changes. Rust
    /// analogue of upstream's `Controller::SawAudioEndpointCallback`. Set via
    /// [`crate::link::BasicLink::set_audio_endpoint_callback`] and invoked
    /// directly from the peer-state-change consumption loop below; this port
    /// has no separate session-controller component to forward to.
    pub audio_endpoint_callback: Arc<Mutex<Option<AudioEndpointCallback>>>,
    pub peer_state: Arc<Mutex<PeerState>>,
    pub session_state: Arc<Mutex<SessionState>>,
    pub client_state: Arc<Mutex<ClientState>>,
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

impl Drop for Controller {
    fn drop(&mut self) {
        self.callbacks_closed.store(true, Ordering::Release);
        self.dispatch.gate.close();
        self.discovery.gate.close();
        if let Some(mut io) = self.io.take() {
            io.shutdown();
        }
    }
}

impl Controller {
    pub(crate) fn register_tempo_callback(&self, callback: TempoCallback) {
        let retired = self
            .tempo_notifications
            .registration
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(callback);
        drop(retired);
        self.tempo_notifications.dispatch(true);
    }

    #[cfg(feature = "audio")]
    pub(crate) fn spawn_on_io<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.io
            .as_ref()
            .expect("published Controller owns its IO context")
            .spawn(future)
    }

    pub async fn new(tempo: tempo::Tempo, clock: Clock) -> Result<Self, std::io::Error> {
        let io = crate::platform::io_context::IoContext::new()?;
        let mut controller = io
            .spawn(Self::new_on_io(tempo, clock))
            .await
            .map_err(|error| std::io::Error::other(format!("Link IO startup failed: {error}")))??;
        controller.io = Some(io);
        Ok(controller)
    }

    /// Requests/restores priority only on this controller's owned IO thread.
    /// Defaults to ordinary scheduling. Permission errors are returned.
    pub async fn set_io_thread_priority(&self, high: bool) -> std::io::Result<()> {
        self.io
            .as_ref()
            .ok_or_else(|| std::io::Error::other("Link IO context unavailable"))?
            .set_priority(high)
            .await
    }

    async fn new_on_io(tempo: tempo::Tempo, clock: Clock) -> Result<Self, std::io::Error> {
        let node_id = NodeId::new();
        let session_id = SessionId(node_id);
        let s_state = init_session_state(tempo, clock);
        let client_state = Arc::new(Mutex::new(init_client_state(s_state, session_id)));
        let tempo_callback: Arc<Mutex<Option<TempoCallback>>> = Arc::new(Mutex::new(None));
        let audio_endpoint_callback: Arc<Mutex<Option<AudioEndpointCallback>>> =
            Arc::new(Mutex::new(None));
        let callbacks_closed = Arc::new(AtomicBool::new(false));
        let tempo_notifications = Arc::new(TempoNotifications {
            callback: tempo_callback.clone(),
            client: client_state.clone(),
            closed: callbacks_closed.clone(),
            registration: Mutex::new(None),
            delivered: Mutex::new((None, s_state.timeline.tempo.bpm())),
            dispatch: tokio::sync::Mutex::new(()),
        });
        let notifications = tempo_notifications.clone();
        let managed: TempoCallback =
            Arc::new(Mutex::new(Box::new(move |_| notifications.dispatch(true))));
        let managed_tempo_callback = Arc::new(Mutex::new(Some(managed)));
        let callback = audio_endpoint_callback.clone();
        let closed = callbacks_closed.clone();
        let managed: AudioEndpointCallback =
            Arc::new(Mutex::new(Box::new(move |peer, endpoint| {
                let callback = callback.lock().unwrap().clone();
                if let Some(callback) = callback {
                    let callback = callback.lock().unwrap();
                    if !closed.load(Ordering::Acquire) {
                        callback(peer, endpoint);
                    }
                }
            })));
        let managed_audio_callback = Arc::new(Mutex::new(Some(managed)));
        let session_peer_counter = Arc::new(Mutex::new(SessionPeerCounter::default()));

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
            audio_endpoint: None,
        }));

        // Standalone measurement traffic retains a wildcard socket. Managed
        // measurements use discovery's learned interface registration for both
        // PING and PONG, rather than asking the OS to choose an ambiguous route.
        let ping_responder_unicast_socket = Arc::new(new_udp_reuseport(
            SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        )?);

        let mut dispatch = DispatchTasks::new();
        let notifications = tempo_notifications.clone();
        dispatch.tasks.push(tokio::spawn(async move {
            let mut retry = tokio::time::interval(std::time::Duration::from_millis(10));
            retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                retry.tick().await;
                if notifications.closed.load(Ordering::Acquire) {
                    break;
                }
                notifications.dispatch(false);
            }
        }));
        let (sessions, session_task) = Sessions::with_dispatch_gate(
            Session {
                incarnation: Arc::new(()),
                edit_revision: crate::link::sessions::new_edit_revision(),
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
            rx_measure_peer_result,
            dispatch.gate.clone(),
        );
        dispatch.tasks.push(session_task);

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
                Some(sessions.clone()),
            )
            .await?,
        );
        discovery.measurement_service.stop().await;
        discovery.stop().await;

        let s_state_loop = session_state.clone();
        let c_state_loop = client_state.clone();
        let s_stop_sync_enabled_loop = start_stop_sync_enabled.clone();
        let discovery_loop = discovery.clone();
        let peers_loop = peers.clone();
        let s_peer_counter_loop = session_peer_counter.clone();
        let s_loop = sessions.clone();
        let ps_loop = peer_state.clone();
        let tempo_cb_loop = managed_tempo_callback.clone();

        let gate_loop = dispatch.gate.clone();

        let mut gate_open = gate_loop.subscribe();
        dispatch.tasks.push(tokio::spawn(async move {
            while let Some((_permit, _epoch, session)) =
                gated_recv(&gate_loop, &mut gate_open, &mut rx_join_session).await
            {
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
                    tempo_cb_loop.clone(),
                )
                .await;
            }
        }));

        let discovery_loop = discovery.clone();
        let s_state_loop = session_state.clone();
        let c_state_loop = client_state.clone();
        let s_stop_sync_enabled_loop = start_stop_sync_enabled.clone();
        let sessions_loop = sessions.clone();
        let p_loop = peers.clone();
        let s_peer_counter_loop = session_peer_counter.clone();
        let peer_state_loop = peer_state.clone();
        let tempo_cb_loop = managed_tempo_callback.clone();
        let audio_endpoint_cb_loop = managed_audio_callback;

        // A contended membership update retains its entire ordered batch. The
        // gate cancels that wait on disable, so no deferred edge crosses epochs.
        let gate_loop = dispatch.gate.clone();

        let mut gate_open = gate_loop.subscribe();
        dispatch.tasks.push(tokio::spawn(async move {
            while let Some((_permit, _epoch, peer_state_changes)) =
                gated_recv(&gate_loop, &mut gate_open, &mut rx_peer_state_change).await
            {
                let apply_changes = async {
                for peer_state_change in peer_state_changes.iter() {
                    match peer_state_change {
                        PeerStateChange::SessionMembership => {
                            debug!("Controller received SessionMembership change");
                            let (session_id, old_count, count) = refresh_peer_count(&peer_state_loop, &p_loop, &s_peer_counter_loop).await;
                            debug!(
                                "SessionMembership: old_count={}, new_count={}",
                                old_count, count
                            );

                            if old_count != count && count == 0 {
                                reset_state_if_peerless(
                                    peer_state_loop.clone(),
                                    s_state_loop.clone(),
                                    c_state_loop.clone(),
                                    discovery_loop.clone(),
                                    sessions_loop.clone(),
                                    clock,
                                    s_stop_sync_enabled_loop.clone(),
                                    tempo_cb_loop.clone(),
                                    Some(session_id),
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

                            sessions_loop.saw_session_timeline(*peer_session, *timeline).await;
                            let _ = refresh_current_session_timing(
                                &sessions_loop, &peer_state_loop, &s_state_loop, &c_state_loop,
                                &s_stop_sync_enabled_loop, &discovery_loop, clock, &tempo_cb_loop,
                            ).await;
                        }
                        PeerStateChange::SessionStartStopState(
                            peer_session,
                            peer_timeline,
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

                            apply_received_start_stop(
                                *peer_session, *peer_timeline, *peer_start_stop_state, &sessions_loop,
                                &peer_state_loop, &s_state_loop, &c_state_loop,
                                &s_stop_sync_enabled_loop, &discovery_loop, clock, &tempo_cb_loop,
                            ).await;
                        }
                        PeerStateChange::PeerLeft => {
                            let (s_id, old_count, count) = refresh_peer_count(&peer_state_loop, &p_loop, &s_peer_counter_loop).await;
                            if old_count != count && count == 0 {
                                reset_state_if_peerless(
                                    peer_state_loop.clone(),
                                    s_state_loop.clone(),
                                    c_state_loop.clone(),
                                    discovery_loop.clone(),
                                    sessions_loop.clone(),
                                    clock,
                                    s_stop_sync_enabled_loop.clone(),
                                    tempo_cb_loop.clone(),
                                    Some(s_id),
                                )
                                .await;
                            }
                        }
                        PeerStateChange::AudioEndpoint(peer_id, endpoint) => {
                            debug!(
                                "Controller received AudioEndpoint change for peer {}",
                                peer_id
                            );
                            dispatch_audio_endpoint_change(
                                &audio_endpoint_cb_loop,
                                *peer_id,
                                *endpoint,
                            );
                        }
                    }
                }
                };
                tokio::select! { biased; _ = gate_open.closed() => {}, _ = apply_changes => {} }
            }
        }));

        Ok(Self {
            #[cfg(test)]
            publication_pause: Arc::new(Mutex::new(None)),
            dispatch,
            io: None,
            callbacks_closed,
            managed_tempo_callback,
            tempo_notifications,
            tempo_callback,
            audio_endpoint_callback,
            peer_state,
            session_state,
            client_state,
            session_peer_counter: session_peer_counter.clone(),
            enabled,
            start_stop_sync_enabled,
            peers: peers.clone(),
            sessions,
            discovery,
            clock,
            rx_event: Some(rx_event),
            notifier,
        })
    }

    pub async fn enable(&mut self) {
        if retry_state(|| Some(*try_state(&self.enabled)?)).await {
            return;
        }

        // A cancelled enable/disable may leave only some gates open. Quiesce
        // them before retrying the reset instead of assuming an atomic startup.
        self.discovery.measurement_service.stop().await;
        self.discovery.stop().await;
        self.dispatch.gate.stop().await;
        retry_state(|| {
            try_state(&self.session_peer_counter)?.session_peer_count = 0;
            Some(())
        })
        .await;

        // Reset while dispatch and discovery are still disabled. Opening first
        // lets a resumed result handler race this new lifecycle's state reset.
        reset_state(
            self.peer_state.clone(),
            self.session_state.clone(),
            self.client_state.clone(),
            self.discovery.clone(),
            self.sessions.clone(),
            self.clock,
            self.start_stop_sync_enabled.clone(),
            self.managed_tempo_callback.clone(),
        )
        .await;

        self.discovery.measurement_service.start().await;
        self.dispatch.gate.start().await;

        // Only start the discovery listener if it hasn't been started already
        if let Some(rx_event) = self.rx_event.take() {
            let discovery = self.discovery.clone();
            let notifier = self.notifier.clone();
            let events = discovery.gate.subscribe();

            self.dispatch
                .tasks
                .push(self.io.as_ref().expect("initialized IO").spawn(async move {
                    discovery
                        .listen_with_dispatch(rx_event, notifier, events)
                        .await;
                }));
        }
        self.discovery
            .gate
            .start_with_retry(|| {
                let mut enabled = try_state(&self.enabled)?;
                self.discovery.try_discard_queued_datagrams()?;
                *enabled = true;
                Some(())
            })
            .await;
    }

    pub async fn disable(&mut self) {
        // Commit the disabled flag before stopping any pipeline stage. Contention
        // must yield without starting a partially disabled lifecycle.
        retry_state(|| {
            *try_state(&self.enabled)? = false;
            Some(())
        })
        .await;
        // Stop request intake and cancel active measurements first. Its closed
        // consumer keeps draining sends from dispatch while those loops wind down.
        self.discovery.measurement_service.stop().await;
        self.discovery.stop().await;
        // Stop the background dispatch loops before anything else, mirroring
        // upstream's `mRtClientStateSetter.stop()` at the top of the async
        // shutdown handler (see `44d78f2cf3a4`, "Stop the
        // RtClientStateDispatcher on shutdown"). This closes the gate and
        // drains any batch already in flight, so once it returns no further
        // join-session or peer-state-change processing can run, rather than
        // relying solely on the loops to observe `enabled == false` on their
        // next iteration. The loops themselves stay alive so that a later
        // `enable()` can resume dispatching.
        self.dispatch.gate.stop().await;

        // Send bye bye message before disabling to properly notify other peers.
        // On lock contention the bye-bye is skipped - it is best-effort - but
        // the rest of the teardown must still run, otherwise `enabled` would be
        // left true while the dispatch gate is already closed.
        use crate::discovery::messenger::send_byebye;
        if let Ok(peer_state) = self.peer_state.try_lock() {
            let node_id = peer_state.node_state.node_id;
            drop(peer_state);
            info!(
                "Disabling Link instance, sending bye-bye message for node {}",
                node_id
            );
            send_byebye(node_id);
        } else {
            info!("Could not read node id, skipping bye-bye message");
        }

        // Cancel this lifecycle's measurements and wake the enabled-gated
        // broadcaster. The long-lived consumers remain available for enable().
        self.notifier.notify_waiters();
        info!("Notified background tasks of disable");

        // Reset peer count to 0 when disabled, like the C++ implementation
        retry_state(|| {
            try_state(&self.session_peer_counter)?.session_peer_count = 0;
            Some(())
        })
        .await;

        // Clear all peers from the discovery
        self.discovery.observer.reset_peers_async().await;
        // Closing the gate can cancel a promoted session's queued join. Publish
        // a coherent local incarnation before accepting further disabled edits.
        reset_state(
            self.peer_state.clone(),
            self.session_state.clone(),
            self.client_state.clone(),
            self.discovery.clone(),
            self.sessions.clone(),
            self.clock,
            self.start_stop_sync_enabled.clone(),
            self.managed_tempo_callback.clone(),
        )
        .await;
        info!("Reset discovery peers");

        // Give some time for the bye bye message to be sent and background tasks to wind down
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        info!("Completed Link disable process");

        // NOTE: The measurement-result, join-session and peer-state-change loops are
        // gated off and drained above, before the bye-bye message is sent,
        // matching upstream's shutdown ordering. Discovery stays alive but
        // suppresses traffic while disabled; measurement jobs observe the
        // notifier. Final drop cancels the owned long-lived tasks.
    }

    pub async fn set_state(
        &self,
        new_client_state: IncomingClientState,
    ) -> Result<(), super::error::CommitError> {
        self.handle_client_state(new_client_state).await
    }

    pub async fn handle_client_state(
        &self,
        client_state: IncomingClientState,
    ) -> Result<(), super::error::CommitError> {
        self.commit_client_state(client_state, None).await
    }

    /// Capture coherent accepted state and opaque provenance using only try_lock.
    pub fn capture_client_state(
        &self,
    ) -> Option<(ClientState, super::sessions::SessionEditOrigin)> {
        let current = try_state(&self.sessions.current)?;
        let published = try_state(&self.sessions.published)?;
        let session = try_state(&self.session_state)?;
        let client = try_state(&self.client_state)?;
        if published.matches(&current)
            && current.session_id == client.timeline_session_id
            && current.timeline == session.timeline
            && current.measurement.x_form == session.ghost_x_form
        {
            Some((*client, current.edit_origin()?))
        } else {
            None
        }
    }

    /// Reject a deferred proposal if its capture was overtaken or replaced.
    pub async fn set_state_checked(
        &self,
        client_state: IncomingClientState,
        origin: &super::sessions::SessionEditOrigin,
    ) -> Result<(), super::error::CommitError> {
        self.commit_client_state(client_state, Some(origin)).await
    }

    async fn commit_client_state(
        &self,
        client_state: IncomingClientState,
        origin: Option<&super::sessions::SessionEditOrigin>,
    ) -> Result<(), super::error::CommitError> {
        loop {
            let result = retry_state(|| {
                try_session_commit(
                    &self.sessions,
                    &self.peers,
                    &self.peer_state,
                    &self.session_state,
                    &self.client_state,
                    &self.session_peer_counter,
                    &self.start_stop_sync_enabled,
                    &self.discovery,
                    |state| state.client_edit_from_origin(&client_state, self.clock, origin),
                )
            })
            .await;
            match result {
                ClientCommit::WaitingForPublication => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await
                }
                ClientCommit::Complete(changed) => {
                    if let Some(tempo) = changed {
                        dispatch_current_tempo(
                            &self.session_state,
                            &self.managed_tempo_callback,
                            tempo,
                        );
                    }
                    return Ok(());
                }
                ClientCommit::Rejected(error) => return Err(error),
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        *self
            .enabled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            .map(|counter| counter.session_peer_count)
            .unwrap_or(0) // Return 0 if lock is contended
    }

    /// The peers this node currently knows about. Used by the LinkAudio
    /// subsystem to learn peers' announced audio endpoints.
    pub fn peers(&self) -> Arc<Mutex<Vec<ControllerPeer>>> {
        self.peers.clone()
    }

    /// This node's identifier.
    pub fn node_id(&self) -> NodeId {
        self.peer_state
            .try_lock()
            .map(|peer_state| peer_state.ident())
            .unwrap_or_default()
    }

    /// The session this node currently belongs to.
    pub fn session_id(&self) -> SessionId {
        self.peer_state
            .try_lock()
            .map(|peer_state| peer_state.session_id())
            .unwrap_or_default()
    }

    /// Announces a LinkAudio endpoint in this node's peer state, so that peers
    /// can discover where to send audio traffic. Passing `None` withdraws it.
    ///
    /// Unlike the best-effort peer snapshot accessors above, this blocks on the lock
    /// rather than skipping on contention. A dropped read returns a stale
    /// answer to one caller; a dropped write is a lost state transition: the
    /// audio-sharing lifecycle in `LinkAudio` would believe the endpoint
    /// withdrawn while peers kept seeing it announced, and no later call would
    /// correct it. Callers must not hold `peer_state` while invoking this
    /// setter. Final `LinkAudio` destruction uses a best-effort withdrawal
    /// instead, before closing and joining discovery.
    pub fn set_audio_endpoint(&self, endpoint: Option<SocketAddrV4>) {
        let mut peer_state = self
            .peer_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        peer_state.audio_endpoint = endpoint;
    }
}

async fn refresh_peer_count(
    peer_state: &Mutex<PeerState>,
    peers: &Mutex<Vec<ControllerPeer>>,
    counter: &Mutex<SessionPeerCounter>,
) -> (SessionId, usize, usize) {
    retry_state(|| {
        let state = try_state(peer_state)?;
        let peers = try_state(peers)?;
        let mut counter = try_state(counter)?;
        let count = count_session_peers(state.session_id(), &peers, state.ident());
        let old = counter.session_peer_count;
        counter.session_peer_count = count;
        Some((state.session_id(), old, count))
    })
    .await
}

enum ClientCommit {
    WaitingForPublication,
    Complete(Option<tempo::Tempo>),
    Rejected(super::error::CommitError),
}

struct SessionCommit<'a> {
    published: &'a mut Session,
    current: &'a mut Session,
    others: &'a mut Vec<Session>,
    peers: &'a mut Vec<ControllerPeer>,
    peer: &'a mut PeerState,
    session: &'a mut SessionState,
    client: &'a mut ClientState,
    counter: &'a mut SessionPeerCounter,
    sync_enabled: bool,
    responder_id: &'a mut SessionId,
    responder_form: &'a mut GhostXForm,
    gateway_peer: Option<&'a mut PeerState>,
}

fn try_session_commit<R>(
    sessions: &Sessions,
    peers: &Mutex<Vec<ControllerPeer>>,
    peer_state: &Mutex<PeerState>,
    session_state: &Mutex<SessionState>,
    client_state: &Mutex<ClientState>,
    counter: &Mutex<SessionPeerCounter>,
    sync_enabled: &Mutex<bool>,
    discovery: &PeerGateway,
    commit: impl FnOnce(&mut SessionCommit<'_>) -> R,
) -> Option<R> {
    let mut current = try_state(&sessions.current)?;
    let mut others = try_state(&sessions.other_sessions)?;
    let mut peers = try_state(peers)?;
    let mut published = try_state(&sessions.published)?;
    let mut peer = try_state(peer_state)?;
    let mut session = try_state(session_state)?;
    let mut client = try_state(client_state)?;
    let mut counter = try_state(counter)?;
    let sync_enabled = try_state(sync_enabled)?;
    let responder = &discovery.measurement_service.ping_responder;
    let mut responder_id = try_state(&responder.session_id)?;
    let mut responder_form = try_state(&responder.ghost_x_form)?;
    // Public helpers can be given a separate caller-side discovery snapshot.
    let mut gateway_peer = if std::ptr::eq(peer_state, discovery.peer_state.as_ref()) {
        None
    } else {
        Some(try_state(&discovery.peer_state)?)
    };
    Some(commit(&mut SessionCommit {
        published: &mut published,
        current: &mut current,
        others: &mut others,
        peers: &mut peers,
        peer: &mut peer,
        session: &mut session,
        client: &mut client,
        counter: &mut counter,
        sync_enabled: *sync_enabled,
        responder_id: &mut responder_id,
        responder_form: &mut responder_form,
        gateway_peer: gateway_peer.as_deref_mut(),
    }))
}

impl SessionCommit<'_> {
    fn publish_discovery(&mut self) {
        *self.published = self.current.clone();
        self.peer.node_state.timeline = self.session.timeline;
        self.peer.node_state.start_stop_state = self.session.start_stop_state;
        *self.responder_id = self.peer.session_id();
        *self.responder_form = self.session.ghost_x_form;
        if let Some(peer) = &mut self.gateway_peer {
            peer.node_state = self.peer.node_state.clone();
        }
    }

    fn reject_pending_timing(&mut self) {
        let rejected = std::mem::replace(self.current, self.published.clone());
        if !self.published.matches(&rejected) {
            self.others
                .retain(|other| !self.published.matches(other) && !rejected.matches(other));
        } else {
            for other in self
                .others
                .iter_mut()
                .filter(|other| self.published.matches(other))
            {
                *other = self.published.clone();
            }
        }
    }

    fn reset(&mut self, clock: Clock) -> Result<Option<tempo::Tempo>, super::error::CommitError> {
        let node = if self.peer.ident() == NodeId::default() {
            NodeId::new()
        } else {
            self.peer.ident()
        };
        let id = SessionId(node);
        let x_form = init_x_form(clock);
        let host_time = -x_form.intercept;
        let timeline = Timeline {
            tempo: self.session.timeline.tempo,
            beat_origin: self
                .session
                .timeline
                .to_beats(self.session.ghost_x_form.host_to_ghost_nanos(host_time)),
            time_origin: x_form.host_to_ghost_nanos(host_time),
        };
        let mut session = *self.session;
        let mut client = *self.client;
        session.start_stop_state = StartStopState::default();
        let changed = match update_timing_guarded(
            &mut session,
            &mut client,
            timeline,
            x_form,
            clock,
            self.sync_enabled,
            id,
        ) {
            Ok(changed) => changed,
            Err(error) => {
                self.reject_pending_timing();
                return Err(error);
            }
        };
        self.peer.node_state.node_id = node;
        self.peer.node_state.session_id = id;
        *self.session = session;
        *self.client = client;
        *self.current = Session::new(
            id,
            timeline,
            SessionMeasurement {
                x_form,
                timestamp: host_time,
            },
        );
        self.others.clear();
        self.peers.clear();
        self.counter.session_peer_count = 0;
        self.publish_discovery();
        Ok(changed)
    }

    fn invalidate_client_captures(&mut self) {
        self.current.invalidate_edit_origin();
        let key = self.current.key();
        for other in self.others.iter_mut().filter(|other| key.matches(other)) {
            other.edit_revision = self.current.edit_revision;
        }
    }

    fn refresh_timing(
        &mut self,
        clock: Clock,
    ) -> Result<Option<tempo::Tempo>, super::error::CommitError> {
        // A promotion's pending join owns publication of its identity. A timeline
        // announcement may refresh only the session already published locally.
        if !self.published.matches(self.current)
            || self.current.session_id != self.peer.session_id()
        {
            return Ok(None);
        }
        let session = self.current.clone();
        let before = *self.client;
        let changed = match update_timing_guarded(
            self.session,
            self.client,
            session.timeline,
            session.measurement.x_form,
            clock,
            self.sync_enabled,
            session.session_id,
        ) {
            Ok(changed) => changed,
            Err(error) => {
                self.reject_pending_timing();
                return Err(error);
            }
        };
        if *self.client != before {
            self.invalidate_client_captures();
        }
        self.publish_discovery();
        Ok(changed)
    }

    #[cfg(test)]
    fn client_edit(&mut self, incoming: &IncomingClientState, clock: Clock) -> ClientCommit {
        self.client_edit_from_origin(incoming, clock, None)
    }

    fn client_edit_from_origin(
        &mut self,
        incoming: &IncomingClientState,
        clock: Clock,
        origin: Option<&super::sessions::SessionEditOrigin>,
    ) -> ClientCommit {
        if origin.is_some_and(|origin| !origin.matches(self.current)) {
            return ClientCommit::Rejected(super::error::CommitError::StaleClientState);
        }
        if !self.published.matches(self.current)
            || self.current.session_id != self.peer.session_id()
            || self.current.measurement.x_form != self.session.ghost_x_form
            || (incoming.timeline.is_none() && self.current.timeline != self.session.timeline)
        {
            return ClientCommit::WaitingForPublication;
        }
        // Calculation and installation share the publication/ownership guards;
        // no A-domain edit can be installed into a pending B incarnation.
        let key = self.current.key();
        let target = match incoming.timeline {
            Some(timeline) => match update_session_timeline_from_client(
                self.current.timeline,
                clamp_tempo(timeline),
                incoming.timeline_timestamp,
                self.session.ghost_x_form,
            ) {
                Ok(target) => Some(target),
                Err(error) => return ClientCommit::Rejected(error),
            },
            None => None,
        };
        let proposed_start_stop = match incoming.start_stop_state {
            Some(state) => {
                if state.timestamp <= self.client.start_stop_state.timestamp {
                    return ClientCommit::Rejected(super::error::CommitError::StaleClientState);
                }
                if self.sync_enabled {
                    let Some(successor) =
                        super::micros(self.session.start_stop_state.timestamp).checked_add(1)
                    else {
                        return ClientCommit::Rejected(
                            super::error::CommitError::StartStopTimestampExhausted,
                        );
                    };
                    let mut mapped = map_start_stop_state_from_client_to_session(
                        state,
                        target.unwrap_or(self.session.timeline),
                        self.session.ghost_x_form,
                    );
                    mapped.timestamp = chrono::Duration::microseconds(
                        super::micros(mapped.timestamp).max(successor),
                    );
                    Some(mapped)
                } else {
                    None
                }
            }
            None => None,
        };
        let prepared = if let Some(timeline) = incoming.timeline {
            let mut session = *self.session;
            let mut client = *self.client;
            client.timeline = clamp_tempo(timeline);
            match update_timing_guarded(
                &mut session,
                &mut client,
                target.expect("timeline preflight succeeded"),
                self.session.ghost_x_form,
                clock,
                self.sync_enabled,
                key.session_id,
            ) {
                Ok(changed) => Some((session, client, changed)),
                Err(error) => return ClientCommit::Rejected(error),
            }
        } else {
            None
        };
        let mut changed = None;
        if incoming.timeline.is_some() || incoming.start_stop_state.is_some() {
            self.current.invalidate_edit_origin();
        }
        if incoming.timeline.is_some() {
            let target = target.expect("timeline preflight succeeded");
            self.current.set_timeline(target);
            for other in self.others.iter_mut().filter(|other| key.matches(other)) {
                other.timeline = target;
                other.edit_revision = self.current.edit_revision;
            }
            for peer in self
                .peers
                .iter_mut()
                .filter(|peer| peer.peer_state.session_id() == key.session_id)
            {
                peer.peer_state.node_state.timeline = target;
            }
            let (session, client, tempo) =
                prepared.expect("client composition preflight succeeded");
            *self.session = session;
            *self.client = client;
            changed = tempo;
        }
        if let Some(state) = incoming.start_stop_state {
            self.client.start_stop_state = state;
            for other in self.others.iter_mut().filter(|other| key.matches(other)) {
                other.edit_revision = self.current.edit_revision;
            }
            if let Some(mapped) = proposed_start_stop {
                self.session.start_stop_state = mapped;
            }
        }
        self.publish_discovery();
        ClientCommit::Complete(changed)
    }

    fn received_start_stop(
        &mut self,
        source: SessionId,
        advertised: Timeline,
        incoming: StartStopState,
        clock: Clock,
    ) -> ClientCommit {
        if source != self.current.session_id {
            return ClientCommit::Complete(None);
        }
        if !self.published.matches(self.current)
            || self.peer.session_id() != source
            || self.current.measurement.x_form != self.session.ghost_x_form
            || self.current.timeline != self.session.timeline
        {
            return ClientCommit::WaitingForPublication;
        }
        let advertised = clamp_tempo(advertised);
        if advertised.beat_origin != self.session.timeline.beat_origin
            || advertised.tempo.period_micros() != self.session.timeline.tempo.period_micros()
            || super::micros(advertised.time_origin)
                != super::micros(self.session.timeline.time_origin)
        {
            return ClientCommit::Complete(None);
        }
        if incoming.timestamp <= self.session.start_stop_state.timestamp {
            return ClientCommit::Complete(None);
        }
        let mut session = *self.session;
        let mut client = *self.client;
        session.start_stop_state = incoming;
        let changed = match update_timing_guarded(
            &mut session,
            &mut client,
            self.current.timeline,
            self.current.measurement.x_form,
            clock,
            self.sync_enabled,
            source,
        ) {
            Ok(changed) => changed,
            Err(error) => return ClientCommit::Rejected(error),
        };
        if self.sync_enabled {
            let observation = clock.micros().max(client.start_stop_state.timestamp);
            client.start_stop_state = map_start_stop_state_from_session_to_client(
                incoming,
                session.timeline,
                session.ghost_x_form,
            );
            client.start_stop_state.timestamp = observation;
        }
        self.current.invalidate_edit_origin();
        let key = self.current.key();
        for other in self.others.iter_mut().filter(|other| key.matches(other)) {
            other.edit_revision = self.current.edit_revision;
        }
        *self.session = session;
        *self.client = client;
        self.publish_discovery();
        ClientCommit::Complete(changed)
    }

    fn join(
        &mut self,
        queued: &Session,
        clock: Clock,
    ) -> Result<Option<tempo::Tempo>, super::error::CommitError> {
        if !queued.key().matches(self.current) {
            return Ok(None);
        }
        // All required guards are held: contention cannot masquerade as a stale
        // notification or leave only part of this transition published.
        let session = self.current.clone();
        let count = count_session_peers(session.session_id, self.peers, self.peer.ident());
        if session.session_id.0 != self.peer.ident() && count == 0 {
            return self.reset(clock);
        }
        let mut session_state = *self.session;
        let mut client = *self.client;
        if self.peer.session_id() != session.session_id {
            session_state.start_stop_state = StartStopState::default();
        }
        let before = *self.client;
        let changed = match update_timing_guarded(
            &mut session_state,
            &mut client,
            session.timeline,
            session.measurement.x_form,
            clock,
            self.sync_enabled,
            session.session_id,
        ) {
            Ok(changed) => changed,
            Err(error) => {
                self.reject_pending_timing();
                return Err(error);
            }
        };
        self.peer.node_state.session_id = session.session_id;
        *self.session = session_state;
        *self.client = client;
        if *self.client != before {
            self.invalidate_client_captures();
        }
        self.publish_discovery();
        self.counter.session_peer_count = count;
        Ok(changed)
    }
}

async fn refresh_current_session_timing(
    sessions: &Sessions,
    peer_state: &Arc<Mutex<PeerState>>,
    session_state: &Arc<Mutex<SessionState>>,
    client_state: &Arc<Mutex<ClientState>>,
    sync_enabled: &Arc<Mutex<bool>>,
    discovery: &Arc<PeerGateway>,
    clock: Clock,
    tempo_callback: &Arc<Mutex<Option<TempoCallback>>>,
) -> Result<(), super::error::CommitError> {
    let changed = retry_state(|| {
        try_session_commit(
            sessions,
            &sessions.peers,
            peer_state,
            session_state,
            client_state,
            &discovery.session_peer_counter,
            sync_enabled,
            discovery,
            |state| state.refresh_timing(clock),
        )
    })
    .await;
    match changed {
        Ok(Some(tempo)) => dispatch_current_tempo(session_state, tempo_callback, tempo),
        Ok(None) => {}
        Err(error) => {
            info!("Rejected received session timing: {}", error);
            return Err(error);
        }
    }
    Ok(())
}

async fn apply_received_start_stop(
    source: SessionId,
    advertised: Timeline,
    incoming: StartStopState,
    sessions: &Sessions,
    peer: &Arc<Mutex<PeerState>>,
    session: &Arc<Mutex<SessionState>>,
    client: &Arc<Mutex<ClientState>>,
    sync: &Arc<Mutex<bool>>,
    discovery: &Arc<PeerGateway>,
    clock: Clock,
    callback: &Arc<Mutex<Option<TempoCallback>>>,
) {
    loop {
        let result = retry_state(|| {
            try_session_commit(
                sessions,
                &sessions.peers,
                peer,
                session,
                client,
                &discovery.session_peer_counter,
                sync,
                discovery,
                |state| state.received_start_stop(source, advertised, incoming, clock),
            )
        })
        .await;
        match result {
            ClientCommit::WaitingForPublication => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await
            }
            ClientCommit::Complete(changed) => {
                if let Some(tempo) = changed {
                    dispatch_current_tempo(session, callback, tempo);
                }
                return;
            }
            ClientCommit::Rejected(_) => return,
        }
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
    tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
) {
    let changed = retry_state(|| {
        try_session_commit(
            &sessions,
            &peers,
            &peer_state,
            &session_state,
            &client_state,
            &session_peer_count,
            &start_stop_sync_enabled,
            &discovery,
            |state| state.join(&session, clock),
        )
    })
    .await;
    match changed {
        Ok(Some(tempo)) => dispatch_current_tempo(&session_state, &tempo_callback, tempo),
        Ok(None) => {}
        Err(error) => info!("Rejected session join timing: {}", error),
    }
}

pub async fn reset_state(
    peer_state: Arc<Mutex<PeerState>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    discovery: Arc<PeerGateway>,
    sessions: Sessions,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
    tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
) {
    reset_state_if_peerless(
        peer_state,
        session_state,
        client_state,
        discovery,
        sessions,
        clock,
        start_stop_sync_enabled,
        tempo_callback,
        None,
    )
    .await;
}

async fn reset_state_if_peerless(
    peer_state: Arc<Mutex<PeerState>>,
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    discovery: Arc<PeerGateway>,
    sessions: Sessions,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
    tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
    expected: Option<SessionId>,
) {
    let changed = retry_state(|| {
        try_session_commit(
            &sessions,
            &sessions.peers,
            &peer_state,
            &session_state,
            &client_state,
            &discovery.session_peer_counter,
            &start_stop_sync_enabled,
            &discovery,
            |state| {
                if let Some(expected) = expected {
                    if state.current.session_id != expected
                        || state.peer.session_id() != expected
                        || count_session_peers(expected, state.peers, state.peer.ident()) != 0
                    {
                        return Ok(None);
                    }
                }
                state.reset(clock)
            },
        )
    })
    .await;
    match changed {
        Ok(Some(tempo)) => dispatch_current_tempo(&session_state, &tempo_callback, tempo),
        Ok(None) => {}
        Err(error) => info!("Rejected session reset timing: {}", error),
    }
}

/// Refresh discovery through its controller's complete publication boundary.
/// Independent state Arcs cannot establish session/clock snapshot authority.
pub async fn update_discovery(controller: &Controller) {
    #[cfg(test)]
    pause_publication(controller).await;
    let _ = refresh_current_session_timing(
        &controller.sessions,
        &controller.peer_state,
        &controller.session_state,
        &controller.client_state,
        &controller.start_stop_sync_enabled,
        &controller.discovery,
        controller.clock,
        &controller.managed_tempo_callback,
    )
    .await;
}

#[cfg(test)]
async fn pause_publication(controller: &Controller) {
    let pause = controller.publication_pause.lock().unwrap().take();
    if let Some((ready, resume)) = pause {
        ready.send(()).unwrap();
        resume.await.unwrap();
    }
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
    tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
    session_id: SessionId,
) -> Result<(), super::error::CommitError> {
    if let Some(tempo) = update_session_timing_inner(
        session_state.clone(),
        client_state,
        new_timeline,
        new_x_form,
        clock,
        start_stop_sync_enabled,
        session_id,
    )? {
        dispatch_current_tempo(&session_state, &tempo_callback, tempo);
    }
    Ok(())
}

fn update_session_timing_inner(
    session_state: Arc<Mutex<SessionState>>,
    client_state: Arc<Mutex<ClientState>>,
    new_timeline: Timeline,
    new_x_form: GhostXForm,
    clock: Clock,
    start_stop_sync_enabled: Arc<Mutex<bool>>,
    session_id: SessionId,
) -> Result<Option<tempo::Tempo>, super::error::CommitError> {
    let Some(mut session_state) = try_state(&session_state) else {
        return Ok(None);
    };
    let Some(mut client_state) = try_state(&client_state) else {
        return Ok(None);
    };
    let Some(sync_enabled) = try_state(&start_stop_sync_enabled) else {
        return Ok(None);
    };
    update_timing_guarded(
        &mut session_state,
        &mut client_state,
        new_timeline,
        new_x_form,
        clock,
        *sync_enabled,
        session_id,
    )
}

fn update_timing_guarded(
    session: &mut SessionState,
    client: &mut ClientState,
    new_timeline: Timeline,
    new_x_form: GhostXForm,
    clock: Clock,
    sync_enabled: bool,
    session_id: SessionId,
) -> Result<Option<tempo::Tempo>, super::error::CommitError> {
    let timeline = clamp_tempo(new_timeline);
    if timeline.time_origin.num_microseconds().is_none() {
        return Err(super::error::CommitError::TimelineOriginExhausted);
    }
    let old = session.timeline;
    let changed = old != timeline || session.ghost_x_form != new_x_form;
    if changed {
        let client_timeline = update_client_timeline_from_session(
            client.timeline,
            timeline,
            clock.micros(),
            new_x_form,
        )?;
        session.timeline = timeline;
        session.ghost_x_form = new_x_form;
        client.timeline = client_timeline;
        if sync_enabled && session.start_stop_state != StartStopState::default() {
            let observation = client.start_stop_state.timestamp;
            client.start_stop_state = map_start_stop_state_from_session_to_client(
                session.start_stop_state,
                timeline,
                new_x_form,
            );
            client.start_stop_state.timestamp = observation;
        }
    }
    client.timeline_session_id = session_id;
    Ok((old.tempo != timeline.tempo).then_some(timeline.tempo))
}

fn dispatch_current_tempo(
    session_state: &Arc<Mutex<SessionState>>,
    tempo_callback: &Arc<Mutex<Option<TempoCallback>>>,
    tempo: tempo::Tempo,
) {
    let callback = tempo_callback
        .try_lock()
        .ok()
        .and_then(|callback| callback.clone());
    if let Some(callback) = callback {
        if let Ok(callback) = callback.try_lock() {
            // Validate after serializing notifications, but release the state
            // guard before user code. An overtaken update must not arrive last.
            let current = session_state
                .try_lock()
                .is_ok_and(|state| state.timeline.tempo == tempo);
            if current {
                callback(tempo.bpm());
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

fn init_client_state(session_state: SessionState, session_id: SessionId) -> ClientState {
    let host_time = session_state
        .ghost_x_form
        .ghost_to_host_nanos(Duration::microseconds(0));

    ClientState {
        timeline: Timeline {
            tempo: session_state.timeline.tempo,
            beat_origin: session_state.timeline.beat_origin,
            time_origin: host_time,
        },
        timeline_session_id: session_id,
        start_stop_state: ClientStartStopState {
            is_playing: session_state.start_stop_state.is_playing,
            time: host_time,
            timestamp: host_time,
        },
    }
}

fn map_start_stop_state_from_session_to_client(
    session_start_stop_state: StartStopState,
    session_timeline: Timeline,
    x_form: GhostXForm,
) -> ClientStartStopState {
    let time = x_form.ghost_nanos_to_host(
        session_timeline.nanos_at_microbeats(session_start_stop_state.beats.value.into()),
    );
    let timestamp = x_form.ghost_to_host_nanos(session_start_stop_state.timestamp);
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
        session_timeline.to_beats(x_form.host_to_ghost_nanos(client_start_stop_state.time));
    let timestamp = x_form.host_to_ghost_nanos(client_start_stop_state.timestamp);
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

#[cfg(test)]
mod dispatch_gate_tests {
    #[tokio::test]
    async fn ordering_value_only_captures_survive_unused_and_busy_drop_after_cloned_reset() {
        use crate::link::{AudioQueueError, BasicLink, CommitError};
        assert!(!std::mem::needs_drop::<crate::link::SessionState>());
        let link = BasicLink::new(120.0).await.unwrap();
        let old = link.capture_audio_session_state().unwrap();
        let origin = old.origin.unwrap();
        let ticket = link.commit_audio_session_state(old.clone()).unwrap();
        let mut sessions = link.controller.sessions.clone();
        let prior = sessions.current.lock().unwrap().clone();
        sessions.reset_session(prior);
        assert!(!origin.matches(&sessions.current.lock().unwrap()));
        assert_eq!(
            link.commit_audio_session_state(old),
            Err(AudioQueueError::Busy)
        );
        assert_eq!(
            link.process_audio_session_state_updates().await,
            Some((ticket, Err(CommitError::StaleClientState)))
        );
    }

    #[tokio::test]
    async fn ordering_normalized_tempo_and_pending_publication_keep_captures_usable() {
        use crate::link::{BasicLink, CommitError};
        let mut link = BasicLink::new(f64::NAN).await.unwrap();
        assert_eq!(link.capture_app_session_state().tempo(), 120.0);
        for (bpm, expected) in [
            (f64::INFINITY, 999.0),
            (f64::NEG_INFINITY, 20.0),
            (f64::NAN, 120.0),
        ] {
            let prior = *link.controller.client_state.lock().unwrap();
            let current = link.controller.sessions.current.lock().unwrap().clone();
            let timeline = Timeline {
                tempo: tempo::Tempo::new(bpm),
                beat_origin: Beats::from_microbeats(current.timeline.beat_origin.value + 1),
                ..current.timeline
            };
            link.controller
                .sessions
                .saw_session_timeline(current.session_id, timeline)
                .await;
            let untracked = link.capture_app_session_state();
            assert_eq!(untracked.timeline(), prior.timeline);
            assert!(untracked.origin.is_none());
            assert_eq!(
                link.commit_app_session_state(untracked).await,
                Err(CommitError::StaleClientState)
            );
            super::update_discovery(&link.controller).await;
            let mut captured = link.capture_app_session_state();
            assert_eq!(captured.tempo(), expected);
            assert!(captured.origin.is_some());
            captured.set_tempo(f64::NAN, link.clock.micros());
            link.commit_app_session_state(captured).await.unwrap();
            assert_eq!(link.capture_app_session_state().tempo(), 120.0);
        }
    }

    #[tokio::test]
    async fn ordering_queued_audio_cannot_cross_join_or_same_id_reset() {
        use crate::link::{BasicLink, CommitError};
        for different_id in [false, true] {
            let mut link = BasicLink::new(120.0).await.unwrap();
            let mut old = link.capture_audio_session_state().unwrap();
            old.set_tempo(150.0, link.clock.micros());
            let ticket = link.commit_audio_session_state(old).unwrap();
            let initial = link.controller.sessions.current.lock().unwrap().clone();
            let id = if different_id {
                SessionId(NodeId::from_array([172; 8]))
            } else {
                initial.session_id
            };
            let replacement = Session::new(
                id,
                Timeline {
                    tempo: tempo::Tempo::new(180.0),
                    ..initial.timeline
                },
                initial.measurement,
            );
            if different_id {
                link.controller.peers.lock().unwrap().push(ControllerPeer {
                    peer_state: PeerState {
                        node_state: NodeState {
                            node_id: id.0,
                            session_id: id,
                            timeline: replacement.timeline,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                });
            }
            link.controller.sessions.reset_session(replacement);
            let queued = link.controller.sessions.current.lock().unwrap().clone();
            join_session(
                queued,
                link.controller.peer_state.clone(),
                link.controller.session_state.clone(),
                link.controller.client_state.clone(),
                link.clock,
                link.controller.start_stop_sync_enabled.clone(),
                link.controller.discovery.clone(),
                link.controller.peers.clone(),
                link.controller.session_peer_counter.clone(),
                link.controller.sessions.clone(),
                link.controller.tempo_callback.clone(),
            )
            .await;
            let before = *link.controller.client_state.lock().unwrap();
            let before_session = *link.controller.session_state.lock().unwrap();
            assert_eq!(before.timeline.tempo.bpm(), 180.0);
            assert_eq!(
                link.process_audio_session_state_updates().await,
                Some((ticket, Err(CommitError::StaleClientState)))
            );
            assert_eq!(
                link.take_audio_commit_result(ticket),
                Ok(Some(Err(CommitError::StaleClientState)))
            );
            assert_eq!(*link.controller.client_state.lock().unwrap(), before);
            assert_eq!(
                *link.controller.session_state.lock().unwrap(),
                before_session
            );
            assert_eq!(link.controller.peer_state.lock().unwrap().session_id(), id);
        }
    }

    fn ordering_fixture(run: impl FnOnce(&mut SessionCommit<'_>)) {
        let id = SessionId(NodeId::from_array([140; 8]));
        let timeline = Timeline {
            tempo: tempo::Tempo::new(120.0),
            ..Timeline::default()
        };
        let form = GhostXForm {
            slope: 1.0,
            intercept: chrono::Duration::zero(),
        };
        let mut current = Session::new(
            id,
            timeline,
            SessionMeasurement {
                x_form: form,
                timestamp: chrono::Duration::zero(),
            },
        );
        let mut published = current.clone();
        let mut others = vec![current.clone()];
        let mut peer = PeerState {
            node_state: NodeState {
                node_id: id.0,
                session_id: id,
                timeline,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut peers = vec![ControllerPeer {
            peer_state: peer.clone(),
        }];
        let mut session = SessionState {
            timeline,
            ghost_x_form: form,
            ..Default::default()
        };
        let mut client = ClientState {
            timeline,
            timeline_session_id: id,
            ..Default::default()
        };
        let mut counter = SessionPeerCounter::default();
        let mut responder_id = id;
        let mut responder_form = form;
        run(&mut SessionCommit {
            published: &mut published,
            current: &mut current,
            others: &mut others,
            peers: &mut peers,
            peer: &mut peer,
            session: &mut session,
            client: &mut client,
            counter: &mut counter,
            sync_enabled: true,
            responder_id: &mut responder_id,
            responder_form: &mut responder_form,
            gateway_peer: None,
        });
    }

    #[test]
    fn ordering_composition_exhaustion_rejects_both_fields_before_mutation() {
        use crate::link::error::CommitError;
        for time_origin in [chrono::Duration::MIN, chrono::Duration::MAX] {
            ordering_fixture(|state| {
                state.session.ghost_x_form.slope = 2.0;
                state.current.measurement.x_form = state.session.ghost_x_form;
                state.published.measurement.x_form = state.session.ghost_x_form;
                *state.responder_form = state.session.ghost_x_form;
                let before = (
                    state.current.edit_origin(),
                    state.published.edit_origin(),
                    *state.session,
                    *state.client,
                    state.peer.timeline(),
                    state.others[0].edit_origin(),
                    state.peers[0].peer_state.timeline(),
                    *state.responder_id,
                    *state.responder_form,
                );
                let incoming = IncomingClientState {
                    timeline: Some(Timeline {
                        tempo: tempo::Tempo::new(150.0),
                        time_origin,
                        ..state.client.timeline
                    }),
                    timeline_timestamp: chrono::Duration::nanoseconds(1),
                    start_stop_state: Some(ClientStartStopState {
                        is_playing: true,
                        timestamp: chrono::Duration::nanoseconds(1),
                        ..Default::default()
                    }),
                };
                assert!(matches!(
                    state.client_edit(&incoming, Clock::new()),
                    ClientCommit::Rejected(CommitError::TimelineOriginExhausted)
                ));
                assert_eq!(
                    before,
                    (
                        state.current.edit_origin(),
                        state.published.edit_origin(),
                        *state.session,
                        *state.client,
                        state.peer.timeline(),
                        state.others[0].edit_origin(),
                        state.peers[0].peer_state.timeline(),
                        *state.responder_id,
                        *state.responder_form,
                    )
                );
            });
        }
    }

    #[test]
    fn ordering_wire_origin_exhaustion_rejects_both_fields_before_mutation() {
        use crate::link::error::CommitError;
        for sign in [-1, 1] {
            ordering_fixture(|state| {
                let accepted = Timeline {
                    beat_origin: Beats::from_microbeats(sign * 4_000_000_000_000_000_000),
                    ..state.session.timeline
                };
                state.current.timeline = accepted;
                state.published.timeline = accepted;
                state.others[0].timeline = accepted;
                state.session.timeline = accepted;
                state.peer.node_state.timeline = accepted;
                state.peers[0].peer_state.node_state.timeline = accepted;
                let before = (
                    state.current.edit_origin(),
                    state.published.edit_origin(),
                    *state.session,
                    *state.client,
                    state.peer.timeline(),
                    state.others[0].edit_origin(),
                    state.peers[0].peer_state.timeline(),
                    *state.responder_id,
                    *state.responder_form,
                );
                let incoming = IncomingClientState {
                    timeline: Some(Timeline {
                        tempo: tempo::Tempo::new(20.0),
                        ..Timeline::default()
                    }),
                    timeline_timestamp: chrono::Duration::zero(),
                    start_stop_state: Some(ClientStartStopState {
                        is_playing: true,
                        timestamp: chrono::Duration::nanoseconds(1),
                        ..Default::default()
                    }),
                };
                let raw = update_session_timeline_from_client(
                    accepted,
                    incoming.timeline.unwrap(),
                    incoming.timeline_timestamp,
                    state.session.ghost_x_form,
                )
                .unwrap();
                assert_eq!(
                    super::super::duration_nanos(raw.time_origin),
                    i128::from(sign) * 12_000_000_000_000_000_000_000 + 3_000
                );
                assert!(raw.time_origin.num_microseconds().is_none());
                assert!(matches!(
                    state.client_edit(&incoming, Clock::new()),
                    ClientCommit::Rejected(CommitError::TimelineOriginExhausted)
                ));
                assert_eq!(
                    before,
                    (
                        state.current.edit_origin(),
                        state.published.edit_origin(),
                        *state.session,
                        *state.client,
                        state.peer.timeline(),
                        state.others[0].edit_origin(),
                        state.peers[0].peer_state.timeline(),
                        *state.responder_id,
                        *state.responder_form,
                    )
                );
            });
        }
    }

    #[test]
    fn ordering_wire_origin_guard_preserves_fractional_endpoint_precision() {
        for endpoint in [i64::MIN, i64::MAX] {
            let sign = i128::from(endpoint.signum());
            for extra in [0, 999, 1_000] {
                ordering_fixture(|state| {
                    let nanos = i128::from(endpoint) * 1_000 + sign * extra;
                    let timeline = Timeline {
                        time_origin: super::super::duration_from_nanos(nanos),
                        ..state.session.timeline
                    };
                    state.session.timeline = timeline;
                    let before = (*state.session, *state.client);
                    let result = update_timing_guarded(
                        state.session,
                        state.client,
                        timeline,
                        *state.responder_form,
                        Clock::new(),
                        true,
                        *state.responder_id,
                    );
                    if extra < 1_000 {
                        assert_eq!(result, Ok(None));
                        assert_eq!(
                            super::super::duration_nanos(state.session.timeline.time_origin),
                            nanos
                        );
                    } else {
                        assert_eq!(
                            result,
                            Err(super::super::CommitError::TimelineOriginExhausted)
                        );
                    }
                    assert_eq!(before, (*state.session, *state.client));
                });
            }
        }
    }

    #[test]
    fn ordering_received_composition_rollback_preserves_accepted_revision() {
        ordering_fixture(|state| {
            let before = (
                state.current.edit_origin(),
                state.published.edit_origin(),
                *state.session,
                *state.client,
                state.peer.timeline(),
                *state.responder_id,
                *state.responder_form,
            );
            state.current.set_timeline(Timeline {
                tempo: tempo::Tempo::new(999.0),
                beat_origin: Beats::from_microbeats(1),
                time_origin: chrono::Duration::microseconds(i64::MAX),
            });
            let pending = IncomingClientState {
                timeline: None,
                timeline_timestamp: chrono::Duration::nanoseconds(1),
                start_stop_state: Some(ClientStartStopState {
                    is_playing: true,
                    timestamp: chrono::Duration::nanoseconds(1),
                    ..Default::default()
                }),
            };
            assert!(matches!(
                state.client_edit(&pending, Clock::new()),
                ClientCommit::WaitingForPublication
            ));
            assert_eq!(
                state.refresh_timing(Clock::new()),
                Err(super::super::CommitError::TimelineOriginExhausted)
            );
            assert_eq!(
                before,
                (
                    state.current.edit_origin(),
                    state.published.edit_origin(),
                    *state.session,
                    *state.client,
                    state.peer.timeline(),
                    *state.responder_id,
                    *state.responder_form,
                )
            );
        });
    }

    #[test]
    fn ordering_rejected_promotion_restores_the_published_incarnation() {
        ordering_fixture(|state| {
            let before = (
                state.current.edit_origin(),
                *state.session,
                *state.client,
                state.peer.session_id(),
                *state.responder_id,
                *state.responder_form,
                state.counter.session_peer_count,
            );
            let candidate = Session::new(
                SessionId(NodeId::from_array([141; 8])),
                Timeline {
                    tempo: tempo::Tempo::new(999.0),
                    beat_origin: Beats::from_microbeats(1),
                    time_origin: chrono::Duration::microseconds(i64::MAX),
                },
                state.current.measurement,
            );
            let mut remote = state.peer.clone();
            remote.node_state.node_id = candidate.session_id.0;
            remote.node_state.session_id = candidate.session_id;
            state.peers.push(ControllerPeer { peer_state: remote });
            *state.current = candidate.clone();
            assert_eq!(
                state.join(&candidate, Clock::new()),
                Err(super::super::CommitError::TimelineOriginExhausted)
            );
            assert!(state.published.matches(state.current));
            assert!(!state.others.iter().any(|other| candidate.matches(other)));
            assert_eq!(
                before,
                (
                    state.current.edit_origin(),
                    *state.session,
                    *state.client,
                    state.peer.session_id(),
                    *state.responder_id,
                    *state.responder_form,
                    state.counter.session_peer_count,
                )
            );
        });
    }

    #[tokio::test]
    async fn ordering_rejected_timeline_packet_cannot_publish_its_playback_state() {
        rejected_timeline_packet_fixture(false).await;
    }

    #[tokio::test]
    async fn ordering_ignored_timeline_packet_cannot_publish_its_playback_state() {
        rejected_timeline_packet_fixture(true).await;
    }

    async fn rejected_timeline_packet_fixture(include_ignored: bool) {
        async fn receive(
            controller: &Controller,
            local: std::net::Ipv4Addr,
            peer: PeerState,
            acknowledged: &mut tokio::sync::mpsc::Receiver<()>,
        ) {
            let packet = crate::discovery::messages::encode_message(
                peer.node_state.node_id,
                30,
                crate::discovery::messages::RESPONSE,
                &crate::link::payload::Payload::from(peer),
                0,
            )
            .unwrap();
            controller
                .discovery
                .receive_peer_state_fixture(local, &packet)
                .await;
            tokio::time::timeout(TEST_TIMEOUT, acknowledged.recv())
                .await
                .unwrap()
                .unwrap();
        }

        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        *controller.start_stop_sync_enabled.lock().unwrap() = true;
        let node = NodeId::from_array([142; 8]);
        let other_node = NodeId::from_array([143; 8]);
        let (ack, mut acknowledged) = tokio::sync::mpsc::channel(1);
        *controller.audio_endpoint_callback.lock().unwrap() =
            Some(Arc::new(Mutex::new(Box::new(move |peer, _| {
                if peer == node || peer == other_node {
                    ack.try_send(()).unwrap();
                }
            }))));
        controller.enable().await;
        let before = *controller.session_state.lock().unwrap();
        let capture = controller.capture_client_state().unwrap();
        let id = controller.sessions.current.lock().unwrap().session_id;
        let local = controller
            .discovery
            .socket_probes()
            .into_iter()
            .filter_map(|socket| socket.upgrade())
            .find_map(|socket| match socket.local_addr().unwrap() {
                std::net::SocketAddr::V4(address) if !address.ip().is_unspecified() => {
                    Some(*address.ip())
                }
                _ => None,
            })
            .unwrap();
        let mut peer = PeerState {
            node_state: NodeState {
                node_id: node,
                session_id: id,
                timeline: Timeline {
                    tempo: tempo::Tempo::new(999.0),
                    beat_origin: Beats::from_microbeats(
                        before.timeline.beat_origin.value.checked_add(1).unwrap(),
                    ),
                    time_origin: chrono::Duration::microseconds(i64::MAX),
                },
                start_stop_state: StartStopState {
                    is_playing: true,
                    beats: Beats::new(4.0),
                    timestamp: chrono::Duration::seconds(1),
                },
            },
            measurement_endpoint: Some("127.0.0.1:12345".parse().unwrap()),
            audio_endpoint: Some("127.0.0.1:12346".parse().unwrap()),
        };
        for port in [12346_u16, 12347] {
            peer.audio_endpoint = Some(format!("127.0.0.1:{port}").parse().unwrap());
            peer.node_state.start_stop_state.timestamp =
                chrono::Duration::seconds(i64::from(port - 12345));
            receive(&controller, local, peer.clone(), &mut acknowledged).await;
            assert_eq!(*controller.session_state.lock().unwrap(), before);
            assert_eq!(controller.capture_client_state().unwrap(), capture);
        }

        let mut other = peer.clone();
        other.node_state.node_id = other_node;
        if include_ignored {
            other.node_state.timeline.beat_origin = before.timeline.beat_origin;
            other.node_state.start_stop_state.timestamp = chrono::Duration::seconds(3);
            other.audio_endpoint = Some("127.0.0.1:12348".parse().unwrap());
            receive(&controller, local, other.clone(), &mut acknowledged).await;
            assert_eq!(*controller.session_state.lock().unwrap(), before);
            assert_eq!(controller.capture_client_state().unwrap(), capture);
        }

        other.node_state.timeline = Timeline {
            beat_origin: Beats::from_microbeats(
                before.timeline.beat_origin.value.checked_add(2).unwrap(),
            ),
            ..before.timeline
        };
        other.audio_endpoint = Some("127.0.0.1:12349".parse().unwrap());
        receive(&controller, local, other.clone(), &mut acknowledged).await;
        let accepted = *controller.session_state.lock().unwrap();
        let accepted_capture = controller.capture_client_state().unwrap();
        assert_eq!(accepted.timeline, other.node_state.timeline);
        assert_eq!(accepted.start_stop_state, other.node_state.start_stop_state);

        peer.node_state.start_stop_state.timestamp = chrono::Duration::seconds(5);
        peer.audio_endpoint = Some("127.0.0.1:12350".parse().unwrap());
        receive(&controller, local, peer.clone(), &mut acknowledged).await;
        assert_eq!(*controller.session_state.lock().unwrap(), accepted);
        assert_eq!(controller.capture_client_state().unwrap(), accepted_capture);

        peer.node_state.timeline = Timeline {
            beat_origin: Beats::from_microbeats(
                before.timeline.beat_origin.value.checked_add(3).unwrap(),
            ),
            ..before.timeline
        };
        peer.node_state.start_stop_state.is_playing = false;
        peer.node_state.start_stop_state.timestamp = chrono::Duration::seconds(6);
        peer.audio_endpoint = Some("127.0.0.1:12351".parse().unwrap());
        receive(&controller, local, peer.clone(), &mut acknowledged).await;
        let recovered = *controller.session_state.lock().unwrap();
        assert_eq!(recovered.timeline, peer.node_state.timeline);
        assert_eq!(recovered.start_stop_state, peer.node_state.start_stop_state);
        assert_ne!(controller.capture_client_state().unwrap(), capture);
        controller.disable().await;
    }

    #[tokio::test]
    async fn ordering_rejected_receipt_restores_capturable_accepted_state() {
        let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let before = controller.capture_client_state().unwrap();
        let accepted = *controller.session_state.lock().unwrap();
        let id = controller.sessions.current.lock().unwrap().session_id;
        controller
            .sessions
            .saw_session_timeline(
                id,
                Timeline {
                    tempo: tempo::Tempo::new(999.0),
                    beat_origin: Beats::from_microbeats(1),
                    time_origin: chrono::Duration::microseconds(i64::MAX),
                },
            )
            .await;
        assert!(controller.capture_client_state().is_none());
        update_discovery(&controller).await;
        assert_eq!(controller.capture_client_state().unwrap(), before);
        assert_eq!(*controller.session_state.lock().unwrap(), accepted);
    }

    #[test]
    fn ordering_received_playback_requires_wire_equivalent_timing() {
        for bpm in [120.0, 128.5] {
            ordering_fixture(|state| {
                let timeline = Timeline {
                    tempo: tempo::Tempo::new(bpm),
                    time_origin: chrono::Duration::nanoseconds(1_500),
                    ..state.current.timeline
                };
                state.current.set_timeline(timeline);
                state.session.timeline = timeline;
                state.client.timeline = timeline;
                *state.published = state.current.clone();
                let bytes = crate::encoding::encode_to_vec(&timeline).unwrap();
                let advertised = <Timeline as crate::encoding::Decode>::decode_from(&bytes)
                    .unwrap()
                    .0;
                assert_ne!(advertised, timeline);
                let incoming = StartStopState {
                    is_playing: true,
                    timestamp: chrono::Duration::seconds(1),
                    ..Default::default()
                };
                let before = (*state.session, *state.client, state.current.edit_origin());
                for incompatible in [
                    Timeline {
                        time_origin: advertised.time_origin + chrono::Duration::microseconds(1),
                        ..advertised
                    },
                    Timeline {
                        tempo: tempo::Tempo::new(121.0),
                        ..advertised
                    },
                ] {
                    assert!(matches!(
                        state.received_start_stop(
                            state.current.session_id,
                            incompatible,
                            incoming,
                            Clock::new()
                        ),
                        ClientCommit::Complete(None)
                    ));
                    assert_eq!(
                        (*state.session, *state.client, state.current.edit_origin()),
                        before
                    );
                }
                assert!(matches!(
                    state.received_start_stop(
                        state.current.session_id,
                        advertised,
                        incoming,
                        Clock::new()
                    ),
                    ClientCommit::Complete(_)
                ));
                assert_eq!(state.session.start_stop_state, incoming);
                assert_ne!(state.current.edit_origin(), before.2);
            });
        }
    }

    #[test]
    fn ordering_playback_revision_rejects_equal_observation_aba() {
        ordering_fixture(|state| {
            state.client.start_stop_state.timestamp = chrono::Duration::seconds(100);
            let before = *state.client;
            let origin = state.current.edit_origin().unwrap();
            for (is_playing, timestamp) in [(true, 1), (false, 2)] {
                assert!(matches!(
                    state.received_start_stop(
                        state.current.session_id,
                        state.current.timeline,
                        StartStopState {
                            is_playing,
                            timestamp: chrono::Duration::microseconds(timestamp),
                            ..Default::default()
                        },
                        Clock::new()
                    ),
                    ClientCommit::Complete(_)
                ));
            }
            assert_eq!(*state.client, before);
            let incoming = IncomingClientState {
                timeline: None,
                timeline_timestamp: chrono::Duration::seconds(101),
                start_stop_state: Some(ClientStartStopState {
                    is_playing: true,
                    timestamp: chrono::Duration::seconds(101),
                    ..before.start_stop_state
                }),
            };
            assert!(matches!(
                state.client_edit_from_origin(&incoming, Clock::new(), Some(&origin)),
                ClientCommit::Rejected(super::super::CommitError::StaleClientState)
            ));
            assert_eq!(*state.client, before);
        });
    }

    #[test]
    fn ordering_client_alignment_revision_rejects_aba_without_wire_change() {
        ordering_fixture(|state| {
            let origin = state.current.edit_origin().unwrap();
            let before = *state.client;
            let session = state.current.timeline;
            for beat_origin in [Beats::new(4.0), before.timeline.beat_origin] {
                let incoming = IncomingClientState {
                    timeline: Some(Timeline {
                        beat_origin,
                        ..before.timeline
                    }),
                    timeline_timestamp: chrono::Duration::zero(),
                    start_stop_state: None,
                };
                assert!(matches!(
                    state.client_edit(&incoming, Clock::new()),
                    ClientCommit::Complete(_)
                ));
                assert_eq!(state.current.timeline, session);
            }
            assert_eq!(*state.client, before);
            let incoming = IncomingClientState {
                timeline: Some(Timeline {
                    tempo: tempo::Tempo::new(150.0),
                    ..before.timeline
                }),
                timeline_timestamp: chrono::Duration::zero(),
                start_stop_state: None,
            };
            assert!(matches!(
                state.client_edit_from_origin(&incoming, Clock::new(), Some(&origin)),
                ClientCommit::Rejected(super::super::CommitError::StaleClientState)
            ));
            assert_eq!(*state.client, before);
        });
    }

    #[test]
    fn ordering_timeline_revision_does_not_reuse_an_old_capture_after_aba() {
        ordering_fixture(|state| {
            let origin = state.current.edit_origin().unwrap();
            let initial = state.current.timeline;
            state.current.set_timeline(Timeline {
                tempo: tempo::Tempo::new(180.0),
                ..initial
            });
            state.current.set_timeline(initial);
            let before = (*state.session, *state.client, state.current.timeline);
            let incoming = IncomingClientState {
                timeline: Some(Timeline {
                    tempo: tempo::Tempo::new(150.0),
                    ..initial
                }),
                start_stop_state: Some(ClientStartStopState {
                    is_playing: true,
                    timestamp: chrono::Duration::nanoseconds(1),
                    ..Default::default()
                }),
                timeline_timestamp: chrono::Duration::nanoseconds(1),
            };
            assert!(matches!(
                state.client_edit_from_origin(&incoming, Clock::new(), Some(&origin)),
                ClientCommit::Rejected(super::super::error::CommitError::StaleClientState)
            ));
            assert_eq!(
                (*state.session, *state.client, state.current.timeline),
                before
            );
        });
    }

    #[test]
    fn ordering_exhaustion_rejects_combined_edit_without_partial_publication() {
        use crate::link::error::CommitError;
        for timeline_exhausted in [false, true] {
            ordering_fixture(|state| {
                if timeline_exhausted {
                    state.current.timeline.beat_origin = Beats::from_microbeats(i64::MAX);
                } else {
                    state.session.start_stop_state.timestamp =
                        chrono::Duration::microseconds(i64::MAX);
                }
                let before = (
                    state.current.timeline,
                    *state.session,
                    *state.client,
                    state.peer.timeline(),
                    state.others[0].timeline,
                    state.peers[0].peer_state.timeline(),
                    *state.responder_id,
                    *state.responder_form,
                );
                let incoming = IncomingClientState {
                    timeline: Some(Timeline {
                        tempo: tempo::Tempo::new(150.0),
                        ..state.client.timeline
                    }),
                    timeline_timestamp: chrono::Duration::nanoseconds(1),
                    start_stop_state: Some(ClientStartStopState {
                        is_playing: true,
                        timestamp: chrono::Duration::nanoseconds(1),
                        ..ClientStartStopState::default()
                    }),
                };
                let expected = if timeline_exhausted {
                    CommitError::TimelineOriginExhausted
                } else {
                    CommitError::StartStopTimestampExhausted
                };
                assert!(
                    matches!(state.client_edit(&incoming, Clock::new()), ClientCommit::Rejected(error) if error == expected)
                );
                assert_eq!(
                    before,
                    (
                        state.current.timeline,
                        *state.session,
                        *state.client,
                        state.peer.timeline(),
                        state.others[0].timeline,
                        state.peers[0].peer_state.timeline(),
                        *state.responder_id,
                        *state.responder_form,
                    )
                );
            });
        }
    }

    #[test]
    fn ordering_fractional_wire_ties_advance_without_poisoning_local_order() {
        ordering_fixture(|state| {
            for (timestamp, expected_wire) in [(750, 1), (1000, 2)] {
                let incoming = IncomingClientState {
                    timeline: None,
                    timeline_timestamp: chrono::Duration::zero(),
                    start_stop_state: Some(ClientStartStopState {
                        is_playing: expected_wire == 1,
                        time: chrono::Duration::zero(),
                        timestamp: chrono::Duration::nanoseconds(timestamp),
                    }),
                };
                assert!(matches!(
                    state.client_edit(&incoming, Clock::new()),
                    ClientCommit::Complete(_)
                ));
                assert_eq!(
                    state.session.start_stop_state.timestamp,
                    chrono::Duration::microseconds(expected_wire)
                );
                assert_eq!(
                    state.client.start_stop_state.timestamp,
                    chrono::Duration::nanoseconds(timestamp)
                );
                let bytes =
                    crate::encoding::encode_to_vec(&state.session.start_stop_state).unwrap();
                let (decoded, _) =
                    crate::encoding::decode_from_slice::<StartStopState>(&bytes).unwrap();
                assert_eq!(
                    decoded.timestamp,
                    chrono::Duration::microseconds(expected_wire)
                );
            }
        });
    }

    #[test]
    fn ordering_terminal_remote_timestamp_reports_exhaustion_not_false_success() {
        for terminal in [
            chrono::Duration::microseconds(i64::MAX),
            chrono::Duration::MAX,
        ] {
            ordering_fixture(|state| {
                let clock = Clock::new();
                let remote = StartStopState {
                    is_playing: true,
                    timestamp: terminal,
                    ..Default::default()
                };
                assert!(matches!(
                    state.received_start_stop(
                        state.current.session_id,
                        state.current.timeline,
                        remote,
                        clock
                    ),
                    ClientCommit::Complete(_)
                ));
                let observed = state.client.start_stop_state.timestamp;
                assert!(observed < terminal);
                for (offset, expected) in [
                    (
                        1,
                        super::super::error::CommitError::StartStopTimestampExhausted,
                    ),
                    (-1, super::super::error::CommitError::StaleClientState),
                ] {
                    let before = (*state.client, *state.session);
                    let incoming = IncomingClientState {
                        timeline: None,
                        timeline_timestamp: observed,
                        start_stop_state: Some(ClientStartStopState {
                            is_playing: false,
                            timestamp: observed + chrono::Duration::nanoseconds(offset),
                            ..state.client.start_stop_state
                        }),
                    };
                    assert!(
                        matches!(state.client_edit(&incoming, clock), ClientCommit::Rejected(error) if error == expected)
                    );
                    assert_eq!((*state.client, *state.session), before);
                }
            });
        }
    }

    #[test]
    fn nanosecond_alignment_survives_guarded_controller_publication() {
        use crate::link::phase::{force_beat_at_time_impl, to_phase_encoded_beats};
        use chrono::Duration as Delta;

        for (bpm, microbeats) in [(120.0, 1), (150.0, 2)] {
            let initial = Timeline {
                tempo: tempo::Tempo::new(bpm),
                ..Timeline::default()
            };
            let at = Delta::zero();
            let beat = Beats::from_microbeats(microbeats);
            let quantum = Beats::new(4.0);
            let mut forced = initial;
            force_beat_at_time_impl(&mut forced, beat, at, quantum);
            for (slope, intercept) in [
                (1.0, 0),
                (1.001, 137),
                (0.999, -137),
                (2.0, 137),
                (-1.0, -137),
            ] {
                let form = GhostXForm {
                    slope,
                    intercept: Delta::nanoseconds(intercept),
                };
                let target =
                    update_session_timeline_from_client(initial, forced, at, form).unwrap();
                let mut session = SessionState {
                    timeline: initial,
                    ghost_x_form: form,
                    ..SessionState::default()
                };
                let mut client = ClientState {
                    timeline: forced,
                    ..ClientState::default()
                };
                update_timing_guarded(
                    &mut session,
                    &mut client,
                    target,
                    form,
                    Clock::new(),
                    false,
                    SessionId::default(),
                )
                .unwrap();
                assert_eq!(to_phase_encoded_beats(&client.timeline, at, quantum), beat);
            }
        }
    }

    #[test]
    fn nanosecond_alignment_survives_start_stop_mapping() {
        use chrono::Duration as Delta;

        for (bpm, origin, microbeat) in [(120.0, -500, 2), (150.0, -800, 3)] {
            let timeline = Timeline {
                tempo: tempo::Tempo::new(bpm),
                beat_origin: Beats::default(),
                time_origin: Delta::nanoseconds(origin),
            };
            let beat = Beats::from_microbeats(microbeat);
            let time = timeline.from_beats(beat);
            let form = GhostXForm {
                slope: 1.0,
                intercept: Delta::zero(),
            };
            let client = ClientStartStopState {
                is_playing: true,
                time,
                timestamp: time,
            };
            let session = map_start_stop_state_from_client_to_session(client, timeline, form);
            assert_eq!(session.beats, beat);
            assert_eq!(
                map_start_stop_state_from_session_to_client(session, timeline, form),
                client
            );
        }
    }

    #[tokio::test]
    async fn admission_received_start_stop_waits_coherently_and_cancels_without_parking_io() {
        for hold_form in [false, true] {
            for cancel in [false, true] {
                let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                    .await
                    .unwrap();
                *controller.start_stop_sync_enabled.lock().unwrap() = true;
                let (ack, mut acknowledged) = tokio::sync::mpsc::channel(2);
                *controller.audio_endpoint_callback.lock().unwrap() =
                    Some(Arc::new(Mutex::new(Box::new(move |_, _| {
                        ack.try_send(()).unwrap();
                    }))));
                controller.enable().await;
                let local = controller
                    .discovery
                    .socket_probes()
                    .into_iter()
                    .filter_map(|socket| socket.upgrade())
                    .find_map(|socket| match socket.local_addr().unwrap() {
                        std::net::SocketAddr::V4(addr) if !addr.ip().is_unspecified() => {
                            Some(*addr.ip())
                        }
                        _ => None,
                    })
                    .unwrap();
                let current = controller.sessions.current.lock().unwrap().clone();
                let node = NodeId::from_array([97; 8]);
                let incoming = StartStopState {
                    is_playing: true,
                    beats: Beats::new(4.0),
                    timestamp: chrono::Duration::seconds(1),
                };
                let packet = |second: bool| {
                    let peer = PeerState {
                        node_state: NodeState {
                            node_id: node,
                            session_id: current.session_id,
                            timeline: current.timeline,
                            start_stop_state: if second {
                                incoming
                            } else {
                                StartStopState::default()
                            },
                        },
                        audio_endpoint: second.then(|| "127.0.0.1:12347".parse().unwrap()),
                        ..Default::default()
                    };
                    crate::discovery::messages::encode_message(
                        node,
                        30,
                        crate::discovery::messages::RESPONSE,
                        &crate::link::payload::Payload::from(peer),
                        0,
                    )
                    .unwrap()
                };
                controller
                    .discovery
                    .receive_peer_state_fixture(local, &packet(false))
                    .await;
                tokio::time::timeout(TEST_TIMEOUT, acknowledged.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let before = *controller.session_state.lock().unwrap();
                let before_client = *controller.client_state.lock().unwrap();
                let responder = &controller.discovery.measurement_service.ping_responder;
                let id = responder.session_id.clone();
                let form = responder.ghost_x_form.clone();
                let (ready, ready_rx) = std::sync::mpsc::channel();
                let (release, release_rx) = std::sync::mpsc::channel();
                fn hold<T>(
                    state: Arc<Mutex<T>>,
                    ready: std::sync::mpsc::Sender<()>,
                    release: std::sync::mpsc::Receiver<()>,
                ) {
                    let _guard = state.lock().unwrap();
                    ready.send(()).unwrap();
                    release.recv_timeout(TEST_TIMEOUT).unwrap();
                }
                let holder = std::thread::spawn(move || {
                    if hold_form {
                        hold(form, ready, release_rx)
                    } else {
                        hold(id, ready, release_rx)
                    }
                });
                ready_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                controller
                    .discovery
                    .receive_peer_state_fixture(local, &packet(true))
                    .await;
                tokio::time::timeout(TEST_TIMEOUT, async {
                    loop {
                        if controller.peers.lock().unwrap().iter().any(|peer| {
                            peer.peer_state.ident() == node
                                && peer.peer_state.start_stop_state() == incoming
                        }) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .unwrap();
                let heartbeat = controller.io.as_ref().unwrap().spawn(async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                });
                let responsive = matches!(
                    tokio::time::timeout(Duration::from_millis(100), heartbeat).await,
                    Ok(Ok(()))
                );
                let unchanged = *controller.session_state.lock().unwrap() == before
                    && *controller.client_state.lock().unwrap() == before_client;
                let stopped = !cancel
                    || tokio::time::timeout(
                        Duration::from_millis(100),
                        controller.dispatch.gate.stop(),
                    )
                    .await
                    .is_ok();
                release.send(()).unwrap();
                holder.join().unwrap();
                if cancel {
                    tokio::time::timeout(TEST_TIMEOUT, controller.dispatch.gate.stop())
                        .await
                        .unwrap();
                    assert_eq!(
                        *controller.session_state.lock().unwrap(),
                        before,
                        "cancelled received state must not publish after guard release"
                    );
                    assert!(acknowledged.try_recv().is_err());
                } else {
                    tokio::time::timeout(TEST_TIMEOUT, acknowledged.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        controller.session_state.lock().unwrap().start_stop_state,
                        incoming
                    );
                    let actual = controller.client_state.lock().unwrap().start_stop_state;
                    let mapped = map_start_stop_state_from_session_to_client(
                        incoming,
                        current.timeline,
                        current.measurement.x_form,
                    );
                    assert_eq!(
                        (actual.is_playing, actual.time),
                        (mapped.is_playing, mapped.time)
                    );
                    assert!(actual.timestamp >= before_client.start_stop_state.timestamp);
                    assert!(actual.timestamp <= controller.clock.micros());
                    assert_eq!(*responder.session_id.lock().unwrap(), current.session_id);
                    assert_eq!(
                        *responder.ghost_x_form.lock().unwrap(),
                        current.measurement.x_form
                    );
                }
                assert!(responsive && unchanged && stopped, "received start/stop must wait without parking, partial mutation or uncancellable work: form={hold_form}, cancel={cancel}");
                controller.disable().await;
            }
        }
    }

    #[tokio::test]
    async fn admission_public_discovery_refresh_cannot_overwrite_intervening_join() {
        let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let before = *controller.session_state.lock().unwrap();
        let (ready, mut ready_rx) = tokio::sync::oneshot::channel();
        let (resume, resume_rx) = tokio::sync::oneshot::channel();
        *controller.publication_pause.lock().unwrap() = Some((ready, resume_rx));
        let mut refresh = Box::pin(update_discovery(&controller));
        assert!(refresh
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        ready_rx.try_recv().unwrap();
        let remote = Session::new(
            SessionId(NodeId::from_array([95; 8])),
            Timeline {
                tempo: tempo::Tempo::new(138.0),
                ..before.timeline
            },
            SessionMeasurement {
                x_form: GhostXForm {
                    slope: 1.0,
                    intercept: before.ghost_x_form.intercept + chrono::Duration::seconds(60),
                },
                timestamp: controller.clock.micros(),
            },
        );
        controller
            .sessions
            .other_sessions
            .lock()
            .unwrap()
            .push(remote.clone());
        controller.peers.lock().unwrap().push(ControllerPeer {
            peer_state: PeerState {
                node_state: NodeState {
                    node_id: remote.session_id.0,
                    session_id: remote.session_id,
                    timeline: remote.timeline,
                    ..Default::default()
                },
                ..Default::default()
            },
        });
        let (joins, mut queued) = tokio::sync::mpsc::channel(1);
        let _owner = crate::link::sessions::handle_successful_measurement(
            remote.key(),
            remote.measurement.x_form,
            controller.sessions.other_sessions.clone(),
            controller.sessions.current.clone(),
            controller.clock,
            joins,
            controller.peers.clone(),
            controller.sessions.tx_measure_peer_state.clone(),
        )
        .await;
        join_session(
            queued.recv().await.unwrap(),
            controller.peer_state.clone(),
            controller.session_state.clone(),
            controller.client_state.clone(),
            controller.clock,
            controller.start_stop_sync_enabled.clone(),
            controller.discovery.clone(),
            controller.peers.clone(),
            controller.session_peer_counter.clone(),
            controller.sessions.clone(),
            controller.tempo_callback.clone(),
        )
        .await;
        resume.send(()).unwrap();
        refresh.await;
        let state = *controller.session_state.lock().unwrap();
        let peer = controller.peer_state.lock().unwrap().clone();
        let responder = &controller.discovery.measurement_service.ping_responder;
        assert_eq!(state.timeline, remote.timeline);
        assert_eq!(state.ghost_x_form, remote.measurement.x_form);
        assert_eq!(peer.session_id(), remote.session_id);
        assert_eq!(
            peer.timeline(),
            state.timeline,
            "stale A timing must not overwrite B gateway publication"
        );
        assert_eq!(*responder.session_id.lock().unwrap(), remote.session_id);
        assert_eq!(
            *responder.ghost_x_form.lock().unwrap(),
            state.ghost_x_form,
            "B identity must not be paired with A's mapping"
        );
    }

    #[tokio::test]
    async fn admission_client_edit_uses_received_timeline_ahead_of_publication() {
        for changed_tempo in [false, true] {
            let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                .await
                .unwrap();
            let (ack, mut acknowledged) = tokio::sync::mpsc::channel(1);
            *controller.audio_endpoint_callback.lock().unwrap() =
                Some(Arc::new(Mutex::new(Box::new(move |_, _| {
                    ack.try_send(()).unwrap();
                }))));
            controller.enable().await;
            let original = controller.sessions.current.lock().unwrap().clone();
            let published = *controller.session_state.lock().unwrap();
            let client = *controller.client_state.lock().unwrap();
            let accepted = Timeline {
                beat_origin: Beats::new(100.0),
                time_origin: original.timeline.from_beats(Beats::new(100.0)),
                ..original.timeline
            };
            let local = controller
                .discovery
                .socket_probes()
                .into_iter()
                .filter_map(|socket| socket.upgrade())
                .find_map(|socket| match socket.local_addr().unwrap() {
                    std::net::SocketAddr::V4(addr) if !addr.ip().is_unspecified() => {
                        Some(*addr.ip())
                    }
                    _ => None,
                })
                .unwrap();
            let state = controller.session_state.clone();
            let (held, held_rx) = std::sync::mpsc::channel();
            let (release_state, state_release) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _guard = state.lock().unwrap();
                held.send(()).unwrap();
                state_release.recv_timeout(TEST_TIMEOUT).unwrap();
            });
            held_rx.recv_timeout(TEST_TIMEOUT).unwrap();
            let node = NodeId::from_array([96; 8]);
            let peer = PeerState {
                node_state: NodeState {
                    node_id: node,
                    session_id: original.session_id,
                    timeline: accepted,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bytes = crate::discovery::messages::encode_message(
                node,
                30,
                crate::discovery::messages::RESPONSE,
                &crate::link::payload::Payload::from(peer),
                0,
            )
            .unwrap();
            controller
                .discovery
                .receive_peer_state_fixture(local, &bytes)
                .await;
            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    if controller.sessions.current.lock().unwrap().timeline == accepted {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            // Hold the executor only as a test scheduling barrier: publication is
            // already awaiting the public state guard, and the app edit wins next.
            let (paused, paused_rx) = std::sync::mpsc::channel();
            let (resume, resume_rx) = std::sync::mpsc::channel();
            let barrier = controller.io.as_ref().unwrap().spawn(async move {
                paused.send(()).unwrap();
                resume_rx.recv_timeout(TEST_TIMEOUT).unwrap();
            });
            paused_rx.recv_timeout(TEST_TIMEOUT).unwrap();
            release_state.send(()).unwrap();
            holder.join().unwrap();
            assert_eq!(*controller.session_state.lock().unwrap(), published);
            assert_eq!(*controller.client_state.lock().unwrap(), client);
            let edit = Timeline {
                tempo: if changed_tempo {
                    tempo::Tempo::new(145.0)
                } else {
                    published.timeline.tempo
                },
                beat_origin: Beats::default(),
                time_origin: published
                    .ghost_x_form
                    .ghost_to_host(published.timeline.from_beats(Beats::default())),
            };
            let at = controller.clock.micros();
            controller
                .set_state(IncomingClientState {
                    timeline: Some(edit),
                    start_stop_state: None,
                    timeline_timestamp: at,
                })
                .await
                .unwrap();
            let actual = controller.sessions.current.lock().unwrap().timeline;
            resume.send(()).unwrap();
            barrier.await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, acknowledged.recv())
                .await
                .unwrap()
                .unwrap();
            let expected =
                update_session_timeline_from_client(accepted, edit, at, published.ghost_x_form)
                    .unwrap();
            assert_eq!(
                actual, expected,
                "client edit rolled back the already accepted wire timeline"
            );
            assert_eq!(controller.session_state.lock().unwrap().timeline, expected);
            if changed_tempo {
                assert!(expected.beat_origin > accepted.beat_origin);
            } else {
                assert_eq!(expected, accepted);
            }
            controller.disable().await;
        }
    }

    #[tokio::test]
    async fn wide_boundary_gateway_update_waits_atomically_and_cancels_on_owned_io() {
        for held_kind in 0..3 {
            for cancel in [false, true] {
                let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                    .await
                    .unwrap();
                let gateway = controller.discovery.clone();
                let responder = &gateway.measurement_service.ping_responder;
                let old_peer = gateway.peer_state.lock().unwrap().clone();
                let old_id = *responder.session_id.lock().unwrap();
                let old_form = *responder.ghost_x_form.lock().unwrap();
                let target = NodeState {
                    session_id: SessionId(NodeId::from_array([171; 8])),
                    ..old_peer.node_state.clone()
                };
                let target_form = GhostXForm {
                    slope: 1.0,
                    intercept: chrono::Duration::seconds(73),
                };
                let peer = gateway.peer_state.clone();
                let id = responder.session_id.clone();
                let form = responder.ghost_x_form.clone();
                let (ready, ready_rx) = std::sync::mpsc::channel();
                let (release, release_rx) = std::sync::mpsc::channel();
                fn hold<T>(
                    state: Arc<Mutex<T>>,
                    ready: std::sync::mpsc::Sender<()>,
                    release: std::sync::mpsc::Receiver<()>,
                ) {
                    let _guard = state.lock().unwrap();
                    ready.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                let holder = std::thread::spawn(move || match held_kind {
                    0 => hold(peer, ready, release_rx),
                    1 => hold(id, ready, release_rx),
                    _ => hold(form, ready, release_rx),
                });
                ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                let writer = gateway.clone();
                let update = target.clone();
                let io = controller.io.as_ref().unwrap();
                let work = io.spawn(async move {
                    writer.update_node_state(update, None, target_form).await;
                });
                let heartbeat = io.spawn(async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                });
                let live = matches!(
                    tokio::time::timeout(Duration::from_millis(100), heartbeat).await,
                    Ok(Ok(()))
                );
                let pending = !work.is_finished();
                let untouched = (held_kind == 0
                    || gateway
                        .peer_state
                        .try_lock()
                        .is_ok_and(|p| p.node_state == old_peer.node_state))
                    && (held_kind == 1
                        || responder
                            .session_id
                            .try_lock()
                            .is_ok_and(|id| *id == old_id))
                    && (held_kind == 2
                        || responder
                            .ghost_x_form
                            .try_lock()
                            .is_ok_and(|form| *form == old_form));
                if cancel {
                    work.abort();
                }
                let cancelled_before_release = if cancel {
                    tokio::time::timeout(Duration::from_millis(100), async {
                        while !work.is_finished() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .is_ok()
                } else {
                    true
                };
                release.send(()).unwrap();
                holder.join().unwrap();
                let outcome = tokio::time::timeout(Duration::from_secs(1), work)
                    .await
                    .unwrap();
                assert!(live && pending && untouched && cancelled_before_release,"gateway must acquire all guards before mutation without parking IO: {held_kind}, cancel={cancel}");
                if cancel {
                    assert!(outcome.unwrap_err().is_cancelled());
                    assert_eq!(
                        gateway.peer_state.lock().unwrap().node_state,
                        old_peer.node_state
                    );
                    assert_eq!(*responder.session_id.lock().unwrap(), old_id);
                    assert_eq!(*responder.ghost_x_form.lock().unwrap(), old_form);
                    gateway
                        .update_node_state(target.clone(), None, target_form)
                        .await;
                } else {
                    outcome.unwrap();
                }
                assert_eq!(gateway.peer_state.lock().unwrap().node_state, target);
                assert_eq!(*responder.session_id.lock().unwrap(), target.session_id);
                assert_eq!(*responder.ghost_x_form.lock().unwrap(), target_form);
            }
        }
    }

    async fn check_broadcast_epoch(rapid: bool) {
        use crate::discovery::messages::{parse_message_header, parse_payload};
        use crate::link::payload::PayloadEntry;
        use std::net::{Ipv4Addr, SocketAddr};
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        controller.enable().await;
        let receiver = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let SocketAddr::V4(destination) = receiver.local_addr().unwrap() else {
            panic!("IPv4")
        };
        let gateway = controller.discovery.clone();
        let fixture = controller
            .io
            .as_ref()
            .unwrap()
            .spawn(async move { gateway.broadcast_epoch_fixture(destination) })
            .await
            .unwrap();
        let old_epoch = controller.discovery.gate.epoch();
        tokio::time::timeout(Duration::from_secs(2), fixture.ready)
            .await
            .unwrap()
            .unwrap();
        let sockets = fixture.sockets.clone();
        let (held, held_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = sockets.lock().unwrap();
            held.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(4)).unwrap();
        });
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        fixture.resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), fixture.blocked)
            .await
            .unwrap()
            .unwrap();
        let io = controller.io.take().unwrap();
        let work = io.spawn(async move {
            controller.disable().await;
            controller
        });
        let completed = tokio::time::timeout(Duration::from_millis(300), work).await;
        let disabled_before_release = completed.is_ok();
        let mut controller = completed.unwrap().unwrap();
        controller.io = Some(io);
        assert!(!controller.discovery.gate.is_open());
        assert!(!fixture.lease_cancelled.load(Ordering::SeqCst));
        let mut edit = controller.client_state.lock().unwrap().timeline;
        edit.tempo = tempo::Tempo::new(150.0);
        controller
            .set_state(IncomingClientState {
                timeline: Some(edit),
                start_stop_state: None,
                timeline_timestamp: controller.clock.micros(),
            })
            .await
            .unwrap();
        if rapid {
            controller.enable().await;
            assert!(controller.discovery.gate.epoch() > old_epoch);
        }
        release.send(()).unwrap();
        holder.join().unwrap();
        let mut bytes = [0; crate::discovery::messages::MAX_MESSAGE_SIZE];
        let silent_when_disabled = if rapid {
            true
        } else {
            tokio::time::timeout(Duration::from_millis(100), receiver.recv_from(&mut bytes))
                .await
                .is_err()
        };
        if !rapid {
            controller.enable().await;
            assert!(controller.discovery.gate.epoch() > old_epoch);
        }
        controller.notifier.notify_waiters();
        let (size, source) =
            tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut bytes))
                .await
                .unwrap()
                .unwrap();
        let (header, offset) = parse_message_header(&bytes[..size]).unwrap();
        let payload = parse_payload(&bytes[offset..size]).unwrap();
        let tempo = payload.entries.iter().find_map(|entry| match entry {
            PayloadEntry::Timeline(timeline) => Some(timeline.tempo),
            _ => None,
        });
        let lease_reused = !fixture.lease_cancelled.load(Ordering::SeqCst)
            && fixture.sockets.lock().unwrap().len() == 1;
        fixture.task.abort();
        let _ = fixture.task.await;
        controller.disable().await;
        assert!(
            disabled_before_release && silent_when_disabled && lease_reused,
            "old epoch cannot survive disable; the registration remains reusable: rapid={rapid}"
        );
        assert_eq!(source, fixture.source);
        assert_eq!(header.message_type, crate::discovery::messages::ALIVE);
        assert_eq!(
            tempo,
            Some(tempo::Tempo::new(150.0)),
            "rapid re-enable must not revive the captured120BPM announcement"
        );
    }

    #[tokio::test]
    async fn wide_broadcast_disabled_epoch_sends_nothing_with_live_lease() {
        check_broadcast_epoch(false).await;
    }

    #[tokio::test]
    async fn wide_broadcast_rapid_reenable_does_not_revive_old_announcement() {
        check_broadcast_epoch(true).await;
    }

    #[tokio::test]
    async fn wide_socket_enable_waits_for_complete_drain_without_parking_io() {
        for held_kind in 0..3 {
            for cancel in [false, true] {
                let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                    .await
                    .unwrap();
                controller.enable().await;
                controller.disable().await;
                let enabled = controller.enabled.clone();
                let gate = controller.discovery.gate.clone();
                let (ready, ready_rx) = std::sync::mpsc::channel();
                let (release, release_rx) = std::sync::mpsc::channel();
                let holder = if held_kind < 2 {
                    controller.discovery.hold_discovery_boundary_fixture(
                        held_kind == 1,
                        ready,
                        release_rx,
                    )
                } else {
                    let flag = enabled.clone();
                    std::thread::spawn(move || {
                        let _guard = flag.lock().unwrap();
                        ready.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                    })
                };
                ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                let io = controller.io.take().unwrap();
                let mut work = io.spawn(async move {
                    controller.enable().await;
                    controller
                });
                let heartbeat = io.spawn(async {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                });
                let responsive = matches!(
                    tokio::time::timeout(Duration::from_millis(150), heartbeat).await,
                    Ok(Ok(()))
                );
                let pending = !work.is_finished() && !gate.is_open();
                let not_enabled = enabled.try_lock().map(|value| !*value).unwrap_or(true);
                let mut consumed = false;
                let cancelled = if cancel {
                    work.abort();
                    match tokio::time::timeout(Duration::from_millis(150), &mut work).await {
                        Ok(Err(error)) => {
                            consumed = true;
                            error.is_cancelled()
                        }
                        _ => false,
                    }
                } else {
                    true
                };
                release.send(()).unwrap();
                holder.join().unwrap();
                if !cancel {
                    let mut controller = tokio::time::timeout(Duration::from_secs(2), work)
                        .await
                        .unwrap()
                        .unwrap();
                    controller.io = Some(io);
                    assert!(*enabled.lock().unwrap() && gate.is_open());
                    controller.disable().await;
                } else {
                    if !consumed {
                        assert!(matches!(work.await,Err(error) if error.is_cancelled()));
                    }
                    assert!(!*enabled.lock().unwrap());
                    drop(io);
                }
                assert!(
                    responsive && pending && not_enabled && cancelled,
                    "enable drain remains owned and atomic: guard={held_kind}, cancel={cancel}"
                );
            }
        }
    }

    #[tokio::test]
    async fn admission_disable_waits_for_public_guards_without_parking_owned_io() {
        for held_kind in 0..4 {
            let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                .await
                .unwrap();
            let before = controller.sessions.current.lock().unwrap().key();
            let current = controller.sessions.current.clone();
            let others = controller.sessions.other_sessions.clone();
            let peers = controller.peers.clone();
            let enabled = controller.enabled.clone();
            let (ready, ready_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            fn hold<T>(
                state: Arc<Mutex<T>>,
                ready: std::sync::mpsc::Sender<()>,
                release: std::sync::mpsc::Receiver<()>,
            ) {
                let _guard = state.lock().unwrap();
                ready.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            let holder = std::thread::spawn(move || match held_kind {
                0 => hold(current, ready, release_rx),
                1 => hold(others, ready, release_rx),
                2 => hold(peers, ready, release_rx),
                _ => hold(enabled, ready, release_rx),
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let io = controller.io.take().unwrap();
            let work = io.spawn(async move {
                controller.disable().await;
                controller
            });
            let heartbeat = io.spawn(async {
                tokio::time::sleep(Duration::from_millis(15)).await;
            });
            let responsive = matches!(
                tokio::time::timeout(Duration::from_millis(100), heartbeat).await,
                Ok(Ok(()))
            );
            let pending = !work.is_finished();
            release.send(()).unwrap();
            holder.join().unwrap();
            let mut controller = tokio::time::timeout(Duration::from_secs(2), work)
                .await
                .unwrap()
                .unwrap();
            controller.io = Some(io);
            assert!(
                responsive && pending,
                "disable must await lock contention without parking IO: {held_kind}"
            );
            assert!(!before.matches(&controller.sessions.current.lock().unwrap()));
            assert!(controller
                .sessions
                .published
                .lock()
                .unwrap()
                .matches(&controller.sessions.current.lock().unwrap()));
            assert!(controller.peers.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn admission_disable_cancels_pending_promotion_without_stranding_disabled_edits() {
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let local = controller.peer_state.lock().unwrap().ident();
        let remote = Session::new(
            SessionId(NodeId::from_array([93; 8])),
            Timeline::default(),
            SessionMeasurement::default(),
        );
        controller
            .sessions
            .other_sessions
            .lock()
            .unwrap()
            .push(remote.clone());
        let (joins, mut queued) = tokio::sync::mpsc::channel(1);
        let _owner = crate::link::sessions::handle_successful_measurement(
            remote.key(),
            GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(100),
            },
            controller.sessions.other_sessions.clone(),
            controller.sessions.current.clone(),
            controller.clock,
            joins,
            controller.peers.clone(),
            controller.sessions.tx_measure_peer_state.clone(),
        )
        .await;
        queued.recv().await.unwrap();
        assert!(!controller
            .sessions
            .published
            .lock()
            .unwrap()
            .matches(&controller.sessions.current.lock().unwrap()));
        controller.disable().await;
        let timeline = Timeline {
            tempo: tempo::Tempo::new(135.0),
            ..controller.client_state.lock().unwrap().timeline
        };
        tokio::time::timeout(
            TEST_TIMEOUT,
            controller.set_state(IncomingClientState {
                timeline: Some(timeline),
                start_stop_state: None,
                timeline_timestamp: controller.clock.micros(),
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            controller.sessions.current.lock().unwrap().session_id,
            SessionId(local)
        );
        assert!(controller
            .sessions
            .published
            .lock()
            .unwrap()
            .matches(&controller.sessions.current.lock().unwrap()));
        assert_eq!(
            controller.client_state.lock().unwrap().timeline.tempo,
            tempo::Tempo::new(135.0)
        );
    }

    #[tokio::test]
    async fn admission_client_edit_waits_for_promoted_clock_publication() {
        for same_wire_id in [false, true] {
            let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                .await
                .unwrap();
            let before = *controller.session_state.lock().unwrap();
            let original = controller.sessions.current.lock().unwrap().clone();
            let remote = Session::new(
                if same_wire_id {
                    original.session_id
                } else {
                    SessionId(NodeId::from_array([94; 8]))
                },
                Timeline {
                    tempo: tempo::Tempo::new(130.0),
                    ..before.timeline
                },
                SessionMeasurement {
                    x_form: GhostXForm {
                        slope: 1.0,
                        intercept: before.ghost_x_form.intercept + chrono::Duration::seconds(10),
                    },
                    timestamp: controller.clock.micros(),
                },
            );
            controller
                .sessions
                .other_sessions
                .lock()
                .unwrap()
                .push(remote.clone());
            controller.peers.lock().unwrap().push(ControllerPeer {
                peer_state: PeerState {
                    node_state: NodeState {
                        node_id: NodeId::from_array([95; 8]),
                        session_id: remote.session_id,
                        timeline: remote.timeline,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            });
            let (joins, mut queued) = tokio::sync::mpsc::channel(1);
            let _owner = crate::link::sessions::handle_successful_measurement(
                remote.key(),
                remote.measurement.x_form,
                controller.sessions.other_sessions.clone(),
                controller.sessions.current.clone(),
                controller.clock,
                joins,
                controller.peers.clone(),
                controller.sessions.tx_measure_peer_state.clone(),
            )
            .await;
            let promoted = queued.recv().await.unwrap();
            let host_edit = Timeline {
                tempo: tempo::Tempo::new(155.0),
                ..controller.client_state.lock().unwrap().timeline
            };
            let at = controller.clock.micros();
            let mut edit = Box::pin(controller.set_state(IncomingClientState {
                timeline: Some(host_edit),
                start_stop_state: None,
                timeline_timestamp: at,
            }));
            assert!(edit
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending());
            assert_eq!(
                controller.sessions.current.lock().unwrap().timeline,
                remote.timeline,
                "an A-clock calculation must not mutate pending B"
            );
            assert_eq!(*controller.session_state.lock().unwrap(), before);
            join_session(
                promoted,
                controller.peer_state.clone(),
                controller.session_state.clone(),
                controller.client_state.clone(),
                controller.clock,
                controller.start_stop_sync_enabled.clone(),
                controller.discovery.clone(),
                controller.peers.clone(),
                controller.session_peer_counter.clone(),
                controller.sessions.clone(),
                controller.tempo_callback.clone(),
            )
            .await;
            tokio::time::timeout(TEST_TIMEOUT, edit)
                .await
                .unwrap()
                .unwrap();
            let expected = update_session_timeline_from_client(
                remote.timeline,
                host_edit,
                at,
                remote.measurement.x_form,
            )
            .unwrap();
            assert_eq!(
                controller.sessions.current.lock().unwrap().timeline,
                expected
            );
            assert_eq!(controller.session_state.lock().unwrap().timeline, expected);
            assert_eq!(controller.peer_state.lock().unwrap().timeline(), expected);
            assert_eq!(
                controller.client_state.lock().unwrap().timeline_session_id,
                remote.session_id
            );
        }
    }

    #[tokio::test]
    async fn admission_received_extreme_beats_survive_join_start_stop_and_client_edit() {
        for origin in [i64::MIN, i64::MAX] {
            let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                .await
                .unwrap();
            *controller.start_stop_sync_enabled.lock().unwrap() = true;
            let (ack, mut acknowledged) = tokio::sync::mpsc::channel(4);
            *controller.audio_endpoint_callback.lock().unwrap() =
                Some(Arc::new(Mutex::new(Box::new(move |_, _| {
                    ack.try_send(()).unwrap();
                }))));
            controller.enable().await;
            let local = controller
                .discovery
                .socket_probes()
                .into_iter()
                .filter_map(|socket| socket.upgrade())
                .find_map(|socket| match socket.local_addr().unwrap() {
                    std::net::SocketAddr::V4(addr) if !addr.ip().is_unspecified() => {
                        Some(*addr.ip())
                    }
                    _ => None,
                })
                .unwrap();
            let node = NodeId::from_array([91; 8]);
            let id = SessionId(node);
            let timeline_origin =
                chrono::Duration::seconds(if origin == i64::MAX { 200 } else { 0 });
            let packet = |second: bool| {
                let state = PeerState {
                    node_state: NodeState {
                        node_id: node,
                        session_id: id,
                        timeline: Timeline {
                            tempo: tempo::Tempo::new(120.0),
                            time_origin: timeline_origin,
                            ..Default::default()
                        },
                        start_stop_state: StartStopState {
                            is_playing: second,
                            timestamp: chrono::Duration::seconds(if second { 2 } else { 1 }),
                            ..Default::default()
                        },
                    },
                    measurement_endpoint: Some("127.0.0.1:12345".parse().unwrap()),
                    audio_endpoint: second.then(|| "127.0.0.1:12346".parse().unwrap()),
                };
                let mut bytes = crate::discovery::messages::encode_message(
                    node,
                    30,
                    crate::discovery::messages::RESPONSE,
                    &crate::link::payload::Payload::from(state),
                    0,
                )
                .unwrap();
                let timeline = bytes.windows(4).position(|entry| entry == b"tmln").unwrap() + 16;
                bytes[timeline..timeline + 8].copy_from_slice(&origin.to_be_bytes());
                let start_stop = bytes.windows(4).position(|entry| entry == b"stst").unwrap() + 9;
                bytes[start_stop..start_stop + 8].copy_from_slice(
                    &(if origin == i64::MIN {
                        i64::MAX
                    } else {
                        i64::MIN
                    })
                    .to_be_bytes(),
                );
                bytes
            };
            controller
                .discovery
                .receive_peer_state_fixture(local, &packet(false))
                .await;
            tokio::time::timeout(TEST_TIMEOUT, acknowledged.recv())
                .await
                .unwrap()
                .unwrap();
            let candidate = controller
                .sessions
                .other_sessions
                .lock()
                .unwrap()
                .iter()
                .find(|session| session.session_id == id)
                .unwrap()
                .clone();
            assert_eq!(
                candidate.timeline.beat_origin.value, origin,
                "raw decoder must have accepted the adversarial beat value"
            );
            assert_eq!(candidate.timeline.time_origin, timeline_origin);
            let (joins, mut queued) = tokio::sync::mpsc::channel(1);
            let _owner = crate::link::sessions::handle_successful_measurement(
                candidate.key(),
                GhostXForm {
                    slope: 1.0,
                    intercept: chrono::Duration::seconds(100),
                },
                controller.sessions.other_sessions.clone(),
                controller.sessions.current.clone(),
                controller.clock,
                joins,
                controller.peers.clone(),
                controller.sessions.tx_measure_peer_state.clone(),
            )
            .await;
            let promoted = queued.recv().await.unwrap();
            join_session(
                promoted,
                controller.peer_state.clone(),
                controller.session_state.clone(),
                controller.client_state.clone(),
                controller.clock,
                controller.start_stop_sync_enabled.clone(),
                controller.discovery.clone(),
                controller.peers.clone(),
                controller.session_peer_counter.clone(),
                controller.sessions.clone(),
                controller.tempo_callback.clone(),
            )
            .await;
            controller
                .discovery
                .receive_peer_state_fixture(local, &packet(true))
                .await;
            tokio::time::timeout(TEST_TIMEOUT, acknowledged.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                controller
                    .session_state
                    .lock()
                    .unwrap()
                    .start_stop_state
                    .beats
                    .value,
                if origin == i64::MIN {
                    i64::MAX
                } else {
                    i64::MIN
                }
            );
            assert_eq!(
                controller.client_state.lock().unwrap().timeline_session_id,
                id
            );
            let received_host_time = controller
                .client_state
                .lock()
                .unwrap()
                .start_stop_state
                .time;
            let beat_difference = i128::from(if origin == i64::MIN {
                i64::MAX
            } else {
                i64::MIN
            }) - i128::from(origin);
            let expected_nanos = crate::link::duration_nanos(timeline_origin)
                + beat_difference * 500
                - 100_000_000_000;
            assert_eq!(
                crate::link::duration_nanos(received_host_time),
                expected_nanos,
                "received start/stop must add the timeline origin and full beat difference before subtracting the 100-second intercept"
            );
            let edit = Timeline {
                tempo: tempo::Tempo::new(150.0),
                ..controller.client_state.lock().unwrap().timeline
            };
            let before = *controller.session_state.lock().unwrap();
            let before_client = *controller.client_state.lock().unwrap();
            let result = controller
                .set_state(IncomingClientState {
                    timeline: Some(edit),
                    start_stop_state: None,
                    timeline_timestamp: controller.clock.micros(),
                })
                .await;
            if origin == i64::MAX {
                assert_eq!(
                    result,
                    Err(super::super::error::CommitError::TimelineOriginExhausted)
                );
                assert_eq!(*controller.session_state.lock().unwrap(), before);
                assert_eq!(*controller.client_state.lock().unwrap(), before_client);
            } else {
                result.unwrap();
                assert_eq!(
                    controller.session_state.lock().unwrap().timeline.tempo,
                    tempo::Tempo::new(150.0)
                );
            }
            controller.disable().await;
        }
    }

    #[tokio::test]
    async fn owned_io_candidate_timeline_cannot_relabel_current_client() {
        use crate::discovery::peers::PeerStateMessageType;
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let candidate = SessionId(NodeId::from_array([93; 8]));
        let (sent, mut received) = tokio::sync::mpsc::channel(1);
        *controller.audio_endpoint_callback.lock().unwrap() =
            Some(Arc::new(Mutex::new(Box::new(move |node, _| {
                if node == candidate.0 {
                    sent.try_send(()).unwrap();
                }
            }))));
        controller.enable().await;
        let initial = controller.sessions.current.lock().unwrap().clone();
        let before = *controller.session_state.lock().unwrap();
        let client = *controller.client_state.lock().unwrap();
        controller
            .discovery
            .event_sender()
            .send(OnEvent::PeerState(PeerStateMessageType {
                expires_at: None,
                observation: None,
                ttl: 30,
                node_state: NodeState {
                    node_id: candidate.0,
                    session_id: candidate,
                    timeline: Timeline {
                        tempo: tempo::Tempo::new(148.0),
                        beat_origin: Beats::new(2.0),
                        ..Default::default()
                    },
                    start_stop_state: Default::default(),
                },
                measurement_endpoint: Some(SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 9)),
                audio_endpoint: None,
            }))
            .await
            .unwrap();
        // Audio is last in the real observer/controller batch, after SessionTimeline.
        tokio::time::timeout(TEST_TIMEOUT, received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            initial
                .key()
                .matches(&controller.sessions.current.lock().unwrap()),
            "candidate was not promoted"
        );
        assert!(controller
            .sessions
            .other_sessions
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.session_id == candidate));
        assert_eq!(
            controller.client_state.lock().unwrap().timeline_session_id,
            initial.session_id,
            "candidate identity must not relabel the accepted current timeline"
        );
        assert_eq!(
            controller.client_state.lock().unwrap().timeline,
            client.timeline
        );
        assert_eq!(
            controller.session_state.lock().unwrap().timeline,
            before.timeline
        );
        assert_eq!(
            controller.peer_state.lock().unwrap().session_id(),
            initial.session_id
        );
        assert_eq!(
            *controller
                .discovery
                .measurement_service
                .ping_responder
                .session_id
                .lock()
                .unwrap(),
            initial.session_id
        );
        controller.disable().await;
    }

    #[tokio::test]
    async fn owned_io_equal_timing_join_publishes_identity_only_at_owned_join() {
        let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let before = *controller.session_state.lock().unwrap();
        let previous = controller.peer_state.lock().unwrap().session_id();
        let remote = Session::new(
            SessionId(NodeId::from_array([1; 8])),
            before.timeline,
            SessionMeasurement::default(),
        );
        assert_ne!(remote.session_id, previous);
        controller
            .sessions
            .other_sessions
            .lock()
            .unwrap()
            .push(remote.clone());
        controller.peers.lock().unwrap().push(ControllerPeer {
            peer_state: PeerState {
                node_state: NodeState {
                    node_id: remote.session_id.0,
                    session_id: remote.session_id,
                    timeline: before.timeline,
                    ..Default::default()
                },
                ..Default::default()
            },
        });
        let (joins, mut queued) = tokio::sync::mpsc::channel(1);
        let _remeasurement = crate::link::sessions::handle_successful_measurement(
            remote.key(),
            before.ghost_x_form,
            controller.sessions.other_sessions.clone(),
            controller.sessions.current.clone(),
            controller.clock,
            joins,
            controller.peers.clone(),
            controller.sessions.tx_measure_peer_state.clone(),
        )
        .await;
        let promoted = tokio::time::timeout(TEST_TIMEOUT, queued.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(remote
            .key()
            .matches(&controller.sessions.current.lock().unwrap()));
        refresh_current_session_timing(
            &controller.sessions,
            &controller.peer_state,
            &controller.session_state,
            &controller.client_state,
            &controller.start_stop_sync_enabled,
            &controller.discovery,
            controller.clock,
            &controller.tempo_callback,
        )
        .await
        .unwrap();
        assert_eq!(
            controller.client_state.lock().unwrap().timeline_session_id,
            previous,
            "pending promotion cannot publish identity via a timeline refresh"
        );
        assert_eq!(controller.peer_state.lock().unwrap().session_id(), previous);
        join_session(
            promoted,
            controller.peer_state.clone(),
            controller.session_state.clone(),
            controller.client_state.clone(),
            controller.clock,
            controller.start_stop_sync_enabled.clone(),
            controller.discovery.clone(),
            controller.peers.clone(),
            controller.session_peer_counter.clone(),
            controller.sessions.clone(),
            controller.tempo_callback.clone(),
        )
        .await;
        assert_eq!(
            controller.session_state.lock().unwrap().timeline,
            before.timeline
        );
        assert_eq!(
            controller.session_state.lock().unwrap().ghost_x_form,
            before.ghost_x_form
        );
        assert_eq!(
            controller.client_state.lock().unwrap().timeline_session_id,
            remote.session_id
        );
        assert_eq!(
            controller.peer_state.lock().unwrap().session_id(),
            remote.session_id
        );
        assert_eq!(
            *controller
                .discovery
                .measurement_service
                .ping_responder
                .session_id
                .lock()
                .unwrap(),
            remote.session_id
        );
    }

    #[test]
    fn owned_io_timing_update_does_not_partially_mutate_contended_client() {
        let clock = Clock::new();
        let initial = init_session_state(tempo::Tempo::new(120.0), clock);
        let id = SessionId(NodeId::from_array([98; 8]));
        let session = Arc::new(Mutex::new(initial));
        let client = Arc::new(Mutex::new(init_client_state(initial, SessionId::default())));
        let sync = Arc::new(Mutex::new(false));
        let held = client.lock().unwrap();
        let timeline = Timeline {
            tempo: tempo::Tempo::new(147.0),
            ..initial.timeline
        };
        assert!(update_session_timing_inner(
            session.clone(),
            client.clone(),
            timeline,
            initial.ghost_x_form,
            clock,
            sync.clone(),
            id
        )
        .unwrap()
        .is_none());
        assert_eq!(
            session.lock().unwrap().timeline,
            initial.timeline,
            "no session mutation before obtaining the client guard"
        );
        drop(held);
        assert!(update_session_timing_inner(
            session.clone(),
            client.clone(),
            timeline,
            initial.ghost_x_form,
            clock,
            sync.clone(),
            id
        )
        .unwrap()
        .is_some());
        let same_timing_id = SessionId(NodeId::from_array([99; 8]));
        assert!(update_session_timing_inner(
            session,
            client.clone(),
            timeline,
            initial.ghost_x_form,
            clock,
            sync,
            same_timing_id
        )
        .unwrap()
        .is_none());
        assert_eq!(
            client.lock().unwrap().timeline_session_id,
            same_timing_id,
            "equal timing still publishes the new session identity"
        );
    }

    #[tokio::test]
    async fn owned_io_live_join_waits_for_every_publication_guard() {
        check_join_commit_guards(false).await;
    }

    #[tokio::test]
    async fn owned_io_peerless_reset_waits_for_every_publication_guard() {
        check_join_commit_guards(true).await;
    }

    async fn check_join_commit_guards(peerless: bool) {
        use crate::discovery::peers::{GatewayObserver, PeerEvent};
        use tokio::sync::mpsc;
        fn hold<T>(
            state: Arc<Mutex<T>>,
            ready: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
        ) {
            let _guard = state.lock().unwrap();
            ready.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for held_kind in 0..11 {
            let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                .await
                .unwrap();
            let local = controller.peer_state.lock().unwrap().ident();
            let remote = Session::new(
                SessionId(NodeId::from_array([96; 8])),
                Timeline {
                    tempo: tempo::Tempo::new(146.0),
                    ..Default::default()
                },
                SessionMeasurement::default(),
            );
            controller
                .sessions
                .other_sessions
                .lock()
                .unwrap()
                .push(remote.clone());
            controller.peers.lock().unwrap().push(ControllerPeer {
                peer_state: PeerState {
                    node_state: NodeState {
                        node_id: NodeId::from_array([97; 8]),
                        session_id: remote.session_id,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            });
            let (joins, mut queued) = mpsc::channel(1);
            let _remeasurement = crate::link::sessions::handle_successful_measurement(
                remote.key(),
                GhostXForm {
                    slope: 1.0,
                    intercept: chrono::Duration::seconds(100),
                },
                controller.sessions.other_sessions.clone(),
                controller.sessions.current.clone(),
                controller.clock,
                joins,
                controller.peers.clone(),
                controller.sessions.tx_measure_peer_state.clone(),
            )
            .await;
            let promoted = queued.recv().await.unwrap();
            let (events, rx_events) = mpsc::channel(1);
            let (changes, mut changed) = mpsc::channel(1);
            let _observer = GatewayObserver::new(
                rx_events,
                controller.peer_state.clone(),
                controller.session_peer_counter.clone(),
                changes,
                controller.peers.clone(),
                Arc::new(Notify::new()),
                Some(controller.sessions.clone()),
            )
            .await;
            if peerless {
                events
                    .send(PeerEvent::PeerLeft(NodeId::from_array([97; 8])))
                    .await
                    .unwrap();
                changed.recv().await.unwrap();
            }
            let before_peer = controller.peer_state.lock().unwrap().clone();
            let before_session = *controller.session_state.lock().unwrap();
            let before_client = *controller.client_state.lock().unwrap();
            let before_current = controller.sessions.current.lock().unwrap().clone();
            let before_others = controller.sessions.other_sessions.lock().unwrap().clone();
            let before_peers = controller.peers.lock().unwrap().clone();
            let before_count = controller
                .session_peer_counter
                .lock()
                .unwrap()
                .session_peer_count;
            let responder = &controller.discovery.measurement_service.ping_responder;
            let before_id = *responder.session_id.lock().unwrap();
            let before_form = *responder.ghost_x_form.lock().unwrap();
            let peer = controller.peer_state.clone();
            let session = controller.session_state.clone();
            let client = controller.client_state.clone();
            let enabled = controller.start_stop_sync_enabled.clone();
            let count = controller.session_peer_counter.clone();
            let pong_id = responder.session_id.clone();
            let pong_form = responder.ghost_x_form.clone();
            let current = controller.sessions.current.clone();
            let others = controller.sessions.other_sessions.clone();
            let peers = controller.peers.clone();
            let published = controller.sessions.published.clone();
            let (ready, ready_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || match held_kind {
                0 => hold(peer, ready, release_rx),
                1 => hold(session, ready, release_rx),
                2 => hold(client, ready, release_rx),
                3 => hold(enabled, ready, release_rx),
                4 => hold(count, ready, release_rx),
                5 => hold(pong_id, ready, release_rx),
                6 => hold(pong_form, ready, release_rx),
                7 => hold(current, ready, release_rx),
                8 => hold(others, ready, release_rx),
                9 => hold(peers, ready, release_rx),
                _ => hold(published, ready, release_rx),
            });
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let peer = controller.peer_state.clone();
            let session = controller.session_state.clone();
            let client = controller.client_state.clone();
            let enabled = controller.start_stop_sync_enabled.clone();
            let discovery = controller.discovery.clone();
            let peers = controller.peers.clone();
            let count = controller.session_peer_counter.clone();
            let sessions = controller.sessions.clone();
            let callback = controller.tempo_callback.clone();
            let clock = controller.clock;
            let work = controller.io.as_ref().unwrap().spawn(async move {
                join_session(
                    promoted, peer, session, client, clock, enabled, discovery, peers, count,
                    sessions, callback,
                )
                .await
            });
            let mut heartbeat = controller.io.as_ref().unwrap().spawn(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
            });
            let responsive = matches!(
                tokio::time::timeout(Duration::from_millis(80), &mut heartbeat).await,
                Ok(Ok(()))
            );
            let pending = !work.is_finished();
            let unchanged = tokio::time::timeout(
                Duration::from_millis(80),
                retry_state(|| {
                    let peer = if held_kind == 0 {
                        None
                    } else {
                        Some(try_state(&controller.peer_state)?)
                    };
                    let session = if held_kind == 1 {
                        None
                    } else {
                        Some(try_state(&controller.session_state)?)
                    };
                    let client = if held_kind == 2 {
                        None
                    } else {
                        Some(try_state(&controller.client_state)?)
                    };
                    let count = if held_kind == 4 {
                        None
                    } else {
                        Some(try_state(&controller.session_peer_counter)?)
                    };
                    let id = if held_kind == 5 {
                        None
                    } else {
                        Some(try_state(&responder.session_id)?)
                    };
                    let form = if held_kind == 6 {
                        None
                    } else {
                        Some(try_state(&responder.ghost_x_form)?)
                    };
                    let current = if held_kind == 7 {
                        None
                    } else {
                        Some(try_state(&controller.sessions.current)?)
                    };
                    let others = if held_kind == 8 {
                        None
                    } else {
                        Some(try_state(&controller.sessions.other_sessions)?)
                    };
                    let peers = if held_kind == 9 {
                        None
                    } else {
                        Some(try_state(&controller.peers)?)
                    };
                    Some(
                        peer.as_ref().is_none_or(|p| **p == before_peer)
                            && session.as_ref().is_none_or(|s| {
                                s.timeline == before_session.timeline
                                    && s.ghost_x_form == before_session.ghost_x_form
                                    && s.start_stop_state == before_session.start_stop_state
                            })
                            && client.as_ref().is_none_or(|c| {
                                c.timeline == before_client.timeline
                                    && c.timeline_session_id == before_client.timeline_session_id
                                    && c.start_stop_state == before_client.start_stop_state
                            })
                            && count
                                .as_ref()
                                .is_none_or(|c| c.session_peer_count == before_count)
                            && id.as_ref().is_none_or(|id| **id == before_id)
                            && form.as_ref().is_none_or(|form| **form == before_form)
                            && current.as_ref().is_none_or(|c| {
                                before_current.key().matches(c)
                                    && c.timeline == before_current.timeline
                                    && c.measurement.x_form == before_current.measurement.x_form
                            })
                            && others.as_ref().is_none_or(|o| {
                                o.len() == before_others.len()
                                    && o.iter()
                                        .zip(&before_others)
                                        .all(|(a, b)| b.key().matches(a))
                            })
                            && peers.as_ref().is_none_or(|p| **p == before_peers),
                    )
                }),
            )
            .await
                == Ok(true);
            release.send(()).unwrap();
            holder.join().unwrap();
            tokio::time::timeout(Duration::from_secs(2), work)
                .await
                .unwrap()
                .unwrap();
            assert!(responsive && pending && unchanged,"all publication must wait without parking or partial mutation: peerless={peerless}, held={held_kind}, responsive={responsive}, pending={pending}, unchanged={unchanged}");
            let expected = if peerless {
                SessionId(local)
            } else {
                remote.session_id
            };
            let peer = controller.peer_state.lock().unwrap();
            let session = controller.session_state.lock().unwrap();
            let client = controller.client_state.lock().unwrap();
            let current = controller.sessions.current.lock().unwrap();
            assert_eq!(current.session_id, expected);
            assert_eq!(peer.session_id(), expected);
            assert_eq!(peer.ident(), local);
            assert_eq!(client.timeline_session_id, expected);
            assert_eq!(peer.timeline(), session.timeline);
            assert_eq!(*responder.session_id.lock().unwrap(), expected);
            assert_eq!(
                *responder.ghost_x_form.lock().unwrap(),
                session.ghost_x_form
            );
            assert_eq!(
                controller
                    .session_peer_counter
                    .lock()
                    .unwrap()
                    .session_peer_count,
                usize::from(!peerless)
            );
            assert_eq!(peer.measurement_endpoint, before_peer.measurement_endpoint);
            assert_eq!(peer.audio_endpoint, before_peer.audio_endpoint);
            assert_eq!(remote.key().matches(&current), !peerless);
        }
    }

    #[tokio::test]
    async fn owned_io_managed_count_preserves_survivor_without_parking_executor() {
        let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let id = controller.peer_state.lock().unwrap().session_id();
        controller.peers.lock().unwrap().push(ControllerPeer {
            peer_state: PeerState {
                node_state: NodeState {
                    node_id: NodeId::from_array([93; 8]),
                    session_id: id,
                    ..Default::default()
                },
                ..Default::default()
            },
        });
        controller
            .session_peer_counter
            .lock()
            .unwrap()
            .session_peer_count = 2;
        let held = controller.peers.clone();
        let (ready, ready_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            ready.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let state = controller.peer_state.clone();
        let peers = controller.peers.clone();
        let counter = controller.session_peer_counter.clone();
        let count = controller
            .io
            .as_ref()
            .unwrap()
            .spawn(async move { refresh_peer_count(&state, &peers, &counter).await });
        let mut heartbeat = controller.io.as_ref().unwrap().spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        });
        let responsive = matches!(
            tokio::time::timeout(Duration::from_millis(100), &mut heartbeat).await,
            Ok(Ok(()))
        );
        let premature = count.is_finished();
        release.send(()).unwrap();
        holder.join().unwrap();
        let (actual, old, new) = count.await.unwrap();
        assert!(responsive && !premature,"managed counting must wait cooperatively instead of reporting zero or parking owned IO");
        assert_eq!((actual, old, new), (id, 2, 1));
        assert_eq!(
            controller
                .session_peer_counter
                .lock()
                .unwrap()
                .session_peer_count,
            1
        );
        assert_eq!(controller.peer_state.lock().unwrap().session_id(), id);
    }

    #[tokio::test]
    async fn owned_io_join_rechecks_last_peer_after_actual_candidate_promotion() {
        use crate::discovery::peers::{GatewayObserver, PeerEvent};
        use tokio::sync::mpsc;
        for leave_one in [false, true] {
            tokio::time::timeout(TEST_TIMEOUT, async {
                let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                    .await
                    .unwrap();
                let local_id = controller.peer_state.lock().unwrap().ident();
                let remote = Session::new(
                    SessionId(NodeId::from_array([92; 8])),
                    Timeline {
                        tempo: tempo::Tempo::new(143.0),
                        ..Default::default()
                    },
                    SessionMeasurement::default(),
                );
                controller
                    .sessions
                    .other_sessions
                    .lock()
                    .unwrap()
                    .push(remote.clone());
                let peers = [10u8, 11]
                    .into_iter()
                    .map(|node| ControllerPeer {
                        peer_state: PeerState {
                            node_state: NodeState {
                                node_id: NodeId::from_array([node; 8]),
                                session_id: remote.session_id,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    })
                    .collect::<Vec<_>>();
                *controller.peers.lock().unwrap() = peers;
                let (joins, mut queued) = mpsc::channel(1);
                let _remeasurement = crate::link::sessions::handle_successful_measurement(
                    remote.key(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: chrono::Duration::seconds(100),
                    },
                    controller.sessions.other_sessions.clone(),
                    controller.sessions.current.clone(),
                    controller.clock,
                    joins,
                    controller.peers.clone(),
                    controller.sessions.tx_measure_peer_state.clone(),
                )
                .await;
                let promoted = queued.recv().await.unwrap();
                assert!(remote
                    .key()
                    .matches(&controller.sessions.current.lock().unwrap()));
                let (events, rx_events) = mpsc::channel(1);
                let (changes, mut changed) = mpsc::channel(1);
                let _observer = GatewayObserver::new(
                    rx_events,
                    controller.peer_state.clone(),
                    controller.session_peer_counter.clone(),
                    changes,
                    controller.peers.clone(),
                    Arc::new(Notify::new()),
                    Some(controller.sessions.clone()),
                )
                .await;
                for node in if leave_one { vec![10u8] } else { vec![10, 11] } {
                    events
                        .send(PeerEvent::PeerLeft(NodeId::from_array([node; 8])))
                        .await
                        .unwrap();
                    changed.recv().await.unwrap();
                }
                assert_eq!(
                    controller
                        .session_peer_counter
                        .lock()
                        .unwrap()
                        .session_peer_count,
                    0
                );
                assert!(
                    remote
                        .key()
                        .matches(&controller.sessions.current.lock().unwrap()),
                    "observer keeps promoted current ownership until join consumption"
                );
                join_session(
                    promoted,
                    controller.peer_state.clone(),
                    controller.session_state.clone(),
                    controller.client_state.clone(),
                    controller.clock,
                    controller.start_stop_sync_enabled.clone(),
                    controller.discovery.clone(),
                    controller.peers.clone(),
                    controller.session_peer_counter.clone(),
                    controller.sessions.clone(),
                    controller.tempo_callback.clone(),
                )
                .await;
                let current = controller.sessions.current.lock().unwrap();
                if leave_one {
                    assert!(remote.key().matches(&current));
                    assert_eq!(
                        controller
                            .session_peer_counter
                            .lock()
                            .unwrap()
                            .session_peer_count,
                        1
                    );
                } else {
                    assert_eq!(
                        current.session_id,
                        SessionId(local_id),
                        "peerless remote promotion must reset even when counts were both zero"
                    );
                    assert!(!remote.key().matches(&current));
                    assert_eq!(
                        controller.peer_state.lock().unwrap().session_id(),
                        SessionId(local_id)
                    );
                }
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn origin_finality_queued_join_uses_latest_same_incarnation_snapshot() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                .await
                .unwrap();
            let queued = controller.sessions.current.lock().unwrap().clone();
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.send(queued.clone()).await.unwrap();
            let mut latest = queued.clone();
            latest.timeline.tempo = tempo::Tempo::new(141.0);
            latest.measurement.x_form = GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(3),
            };
            *controller.sessions.current.lock().unwrap() = latest.clone();
            assert!(queued.key().matches(&latest));
            join_session(
                rx.recv().await.unwrap(),
                controller.peer_state.clone(),
                controller.session_state.clone(),
                controller.client_state.clone(),
                controller.clock,
                controller.start_stop_sync_enabled.clone(),
                controller.discovery.clone(),
                controller.peers.clone(),
                controller.session_peer_counter.clone(),
                controller.sessions.clone(),
                controller.managed_tempo_callback.clone(),
            )
            .await;
            assert_eq!(
                controller.session_state.lock().unwrap().timeline.tempo,
                latest.timeline.tempo
            );
            assert_eq!(
                controller.session_state.lock().unwrap().ghost_x_form,
                latest.measurement.x_form
            );
            assert_eq!(
                controller.peer_state.lock().unwrap().timeline().tempo,
                latest.timeline.tempo
            );
            assert_eq!(
                controller.client_state.lock().unwrap().timeline.tempo,
                latest.timeline.tempo
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn queued_join_cannot_restore_an_incarnation_replaced_by_live_reset() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
                .await
                .unwrap();
            let mut queued = controller.sessions.current.lock().unwrap().clone();
            queued.timeline.tempo = tempo::Tempo::new(95.0);
            queued.measurement.x_form = GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(30),
            };
            *controller.sessions.current.lock().unwrap() = queued.clone();
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.send(queued.clone()).await.unwrap();
            reset_state(
                controller.peer_state.clone(),
                controller.session_state.clone(),
                controller.client_state.clone(),
                controller.discovery.clone(),
                controller.sessions.clone(),
                controller.clock,
                controller.start_stop_sync_enabled.clone(),
                controller.managed_tempo_callback.clone(),
            )
            .await;
            let expected_peer = controller.peer_state.lock().unwrap().clone();
            let expected_session = *controller.session_state.lock().unwrap();
            let expected_client = *controller.client_state.lock().unwrap();
            assert!(!queued
                .key()
                .matches(&controller.sessions.current.lock().unwrap()));
            assert_eq!(
                queued.session_id,
                controller.sessions.current.lock().unwrap().session_id,
                "fixture must exercise same wire ID"
            );
            join_session(
                rx.recv().await.unwrap(),
                controller.peer_state.clone(),
                controller.session_state.clone(),
                controller.client_state.clone(),
                controller.clock,
                controller.start_stop_sync_enabled.clone(),
                controller.discovery.clone(),
                controller.peers.clone(),
                controller.session_peer_counter.clone(),
                controller.sessions.clone(),
                controller.managed_tempo_callback.clone(),
            )
            .await;
            assert_eq!(*controller.peer_state.lock().unwrap(), expected_peer);
            assert_eq!(
                controller.session_state.lock().unwrap().ghost_x_form,
                expected_session.ghost_x_form
            );
            assert_eq!(
                controller.session_state.lock().unwrap().timeline,
                expected_session.timeline
            );
            assert_eq!(
                controller.client_state.lock().unwrap().timeline,
                expected_client.timeline
            );
            assert_eq!(
                controller.client_state.lock().unwrap().timeline_session_id,
                expected_client.timeline_session_id
            );
        })
        .await
        .unwrap();
    }

    use super::*;
    use std::{future::Future, task::Context, task::Waker, time::Duration};
    use tokio::sync::oneshot;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn ordering_tempo_registration_and_latest_delivery_survive_contention() {
        for registry_busy in [true, false] {
            let mut link = crate::link::BasicLink::new(120.0).await.unwrap();
            let observed = Arc::new(Mutex::new(Vec::new()));
            let values = observed.clone();
            let registry = link.controller.tempo_callback.clone();
            let registry_guard = registry_busy.then(|| registry.lock().unwrap());
            link.set_tempo_callback(move |bpm| values.lock().unwrap().push(bpm));
            let callback = link.tempo_callback.clone().unwrap();
            let callback_guard = (!registry_busy).then(|| callback.lock().unwrap());
            let mut expected = if registry_busy { vec![] } else { vec![120.0] };
            for bpm in [150.0, 175.0] {
                let current = *link.controller.client_state.lock().unwrap();
                link.controller
                    .set_state(IncomingClientState {
                        timeline: Some(Timeline {
                            tempo: tempo::Tempo::new(bpm),
                            ..current.timeline
                        }),
                        start_stop_state: None,
                        timeline_timestamp: link.clock().micros(),
                    })
                    .await
                    .unwrap();
            }
            assert_eq!(*observed.lock().unwrap(), expected);
            drop(registry_guard);
            drop(callback_guard);
            tokio::time::timeout(TEST_TIMEOUT, async {
                while observed.lock().unwrap().last() != Some(&175.0) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            expected.push(175.0);
            assert_eq!(*observed.lock().unwrap(), expected);
        }
    }

    #[test]
    fn ordering_playback_time_maps_wide_beats_before_duration_clamping() {
        let session = Timeline {
            tempo: tempo::Tempo::new(60.0),
            beat_origin: Beats::new(-1.0),
            time_origin: chrono::Duration::MAX,
        };
        let form = GhostXForm {
            slope: 2.0,
            intercept: chrono::Duration::zero(),
        };
        let received = StartStopState {
            is_playing: true,
            beats: Beats::default(),
            timestamp: chrono::Duration::zero(),
        };
        let mapped = map_start_stop_state_from_session_to_client(received, session, form);
        let expected = crate::link::duration_from_nanos(crate::link::round_ratio(
            crate::link::duration_nanos(chrono::Duration::MAX) + 1_000_000_000,
            2,
        ));
        assert_eq!(mapped.time, expected);
    }

    #[tokio::test]
    async fn managed_tempo_cannot_follow_a_newer_public_app_callback() {
        let mut link = crate::link::BasicLink::new(121.0).await.unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let values = observed.clone();
        link.set_tempo_callback(move |bpm| values.lock().unwrap().push(bpm));
        let pending = link.controller.session_state.lock().unwrap().timeline.tempo;
        let managed = link
            .controller
            .managed_tempo_callback
            .lock()
            .unwrap()
            .clone()
            .unwrap();

        let mut state = link.capture_app_session_state();
        state.set_tempo(122.0, link.clock().micros());
        link.commit_app_session_state(state).await.unwrap();
        let delivered = observed.lock().unwrap().clone();
        assert_eq!(delivered.last(), Some(&122.0));
        // Resume at the production wrapper after an earlier session check.
        managed.lock().unwrap()(pending.bpm());
        assert_eq!(*observed.lock().unwrap(), delivered);
    }

    #[test]
    fn overtaken_tempo_notification_cannot_follow_the_newer_callback() {
        let clock = Clock::new();
        let state = Arc::new(Mutex::new(init_session_state(
            tempo::Tempo::new(121.0),
            clock,
        )));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let values = observed.clone();
        let callback: TempoCallback = Arc::new(Mutex::new(Box::new(move |bpm| {
            values.lock().unwrap().push(bpm);
        })));
        let callback = Arc::new(Mutex::new(Some(callback)));
        let (ready, overtaken) = std::sync::mpsc::sync_channel(1);
        let (release, proceed) = std::sync::mpsc::sync_channel(1);
        let old_state = state.clone();
        let old_callback = callback.clone();
        let old = std::thread::spawn(move || {
            let tempo = old_state.lock().unwrap().timeline.tempo;
            ready.send(()).unwrap();
            proceed.recv_timeout(TEST_TIMEOUT).unwrap();
            dispatch_current_tempo(&old_state, &old_callback, tempo);
        });
        overtaken.recv_timeout(TEST_TIMEOUT).unwrap();
        state.lock().unwrap().timeline.tempo = tempo::Tempo::new(122.0);
        dispatch_current_tempo(&state, &callback, tempo::Tempo::new(122.0));
        release.send(()).unwrap();
        old.join().unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![122.0]);
    }

    #[test]
    fn tempo_callback_runs_without_the_session_state_lock() {
        let state = Arc::new(Mutex::new(init_session_state(
            tempo::Tempo::new(121.0),
            Clock::new(),
        )));
        let observed = Arc::new(AtomicBool::new(false));
        let called = observed.clone();
        let callback_state = state.clone();
        let callback: TempoCallback = Arc::new(Mutex::new(Box::new(move |_| {
            assert!(callback_state.try_lock().is_ok());
            called.store(true, Ordering::Release);
        })));
        dispatch_current_tempo(
            &state,
            &Arc::new(Mutex::new(Some(callback))),
            tempo::Tempo::new(121.0),
        );
        assert!(observed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn restart_waits_for_every_consumer_before_admitting_fresh_work() {
        let gate = DispatchGate::new();
        let mut first = gate.subscribe();
        let mut second = gate.subscribe();
        let (tx_first, mut rx_first) = tokio::sync::mpsc::channel(2);
        let (tx_second, mut rx_second) = tokio::sync::mpsc::channel(2);
        tx_first.try_send(1).unwrap();
        tx_second.try_send(2).unwrap();
        let mut first_recv = Box::pin(gated_recv(&gate, &mut first, &mut rx_first));
        let mut second_recv = Box::pin(gated_recv(&gate, &mut second, &mut rx_second));
        let mut start = Box::pin(gate.start());
        let mut context = Context::from_waker(Waker::noop());

        assert!(start.as_mut().poll(&mut context).is_pending());
        assert!(first_recv.as_mut().poll(&mut context).is_pending());
        assert!(start.as_mut().poll(&mut context).is_pending());
        assert!(!gate.is_open());
        assert!(second_recv.as_mut().poll(&mut context).is_pending());
        assert!(start.as_mut().poll(&mut context).is_ready());
        assert!(gate.is_open());

        tx_first.try_send(3).unwrap();
        tx_second.try_send(4).unwrap();
        assert_eq!(first_recv.await.unwrap().2, 3);
        assert_eq!(second_recv.await.unwrap().2, 4);
    }

    #[tokio::test]
    async fn restart_does_not_wait_forever_for_a_departed_consumer() {
        let gate = DispatchGate::new();
        let receiver = gate.subscribe();
        let mut start = Box::pin(gate.start());
        let mut context = Context::from_waker(Waker::noop());
        assert!(start.as_mut().poll(&mut context).is_pending());
        drop(receiver);
        assert!(start.as_mut().poll(&mut context).is_ready());
        assert!(gate.is_open());
    }

    fn session_dispatch() -> (
        DispatchTasks,
        Sessions,
        tokio::sync::mpsc::Sender<super::super::measurement::MeasurePeerEvent>,
        tokio::sync::mpsc::Receiver<Session>,
    ) {
        let mut dispatch = DispatchTasks::new();
        let (tx_request, _rx_request) = tokio::sync::mpsc::channel(1);
        let (tx_result, rx_result) = tokio::sync::mpsc::channel(1);
        let (tx_join, rx_join) = tokio::sync::mpsc::channel(1);
        let (sessions, task) = Sessions::with_dispatch_gate(
            Session {
                incarnation: Arc::new(()),
                edit_revision: crate::link::sessions::new_edit_revision(),
                session_id: SessionId::default(),
                timeline: Timeline::default(),
                measurement: SessionMeasurement::default(),
            },
            tx_request,
            Arc::new(Mutex::new(Vec::new())),
            Clock::new(),
            tx_join,
            rx_result,
            dispatch.gate.clone(),
        );
        dispatch.tasks.push(task);
        (dispatch, sessions, tx_result, rx_join)
    }

    #[tokio::test]
    async fn restart_discards_results_buffered_while_disabled() {
        use crate::link::measurement::MeasurePeerEvent;

        let (dispatch, sessions, tx, mut joined) = session_dispatch();
        dispatch.gate.start().await;
        let key = sessions.current.lock().unwrap().key();
        for cycle in 1..=3 {
            let x_form = GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(cycle),
            };
            tx.send(MeasurePeerEvent::XForm(key.clone(), x_form))
                .await
                .unwrap();
            let session = tokio::time::timeout(TEST_TIMEOUT, joined.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(session.measurement.x_form, x_form);

            dispatch.gate.stop().await;
            tx.send(MeasurePeerEvent::XForm(
                key.clone(),
                GhostXForm {
                    slope: 1.0,
                    intercept: chrono::Duration::seconds(100),
                },
            ))
            .await
            .unwrap();
            assert!(matches!(
                joined.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            assert!(!dispatch.tasks[0].is_finished());
            dispatch.gate.start().await;
        }
        drop(dispatch);
        tokio::time::timeout(TEST_TIMEOUT, tx.closed())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restart_cancels_a_result_handler_blocked_on_a_full_join_queue() {
        use crate::link::measurement::MeasurePeerEvent;

        let (dispatch, sessions, tx, mut joined) = session_dispatch();
        dispatch.gate.start().await;
        let key = sessions.current.lock().unwrap().key();
        for seconds in 1..=2 {
            let x_form = GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(seconds),
            };
            tx.send(MeasurePeerEvent::XForm(key.clone(), x_form))
                .await
                .unwrap();
            tokio::time::timeout(TEST_TIMEOUT, async {
                if seconds == 1 {
                    while sessions.current.lock().unwrap().measurement.x_form != x_form {
                        tokio::task::yield_now().await;
                    }
                } else {
                    while tx.capacity() == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
            .await
            .unwrap();
        }
        // The first join fills the queue; the second result was consumed but
        // cannot commit until notification capacity exists. stop() cancels it.
        assert_eq!(joined.len(), 1);
        assert!(dispatch.gate.in_flight.try_write().is_err());
        assert_eq!(
            sessions
                .current
                .lock()
                .unwrap()
                .measurement
                .x_form
                .intercept,
            chrono::Duration::seconds(1)
        );
        tokio::time::timeout(TEST_TIMEOUT, dispatch.gate.stop())
            .await
            .unwrap();
        assert!(!dispatch.tasks[0].is_finished());
        let first = joined.try_recv().unwrap();
        assert_eq!(
            first.measurement.x_form.intercept,
            chrono::Duration::seconds(1)
        );
        assert!(joined.try_recv().is_err());

        dispatch.gate.start().await;
        let fresh = GhostXForm {
            slope: 1.0,
            intercept: chrono::Duration::seconds(3),
        };
        tx.send(MeasurePeerEvent::XForm(key.clone(), fresh))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(TEST_TIMEOUT, joined.recv())
                .await
                .unwrap()
                .unwrap()
                .measurement
                .x_form,
            fresh
        );
        drop(tx);
        tokio::time::timeout(TEST_TIMEOUT, async {
            while !dispatch.tasks[0].is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn restart_measures_and_joins_a_peer_after_each_enable() {
        measures_and_joins_after_each_enable().await;
    }

    #[tokio::test]
    async fn enabled_state_read_waits_for_contention() {
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        controller.enable().await;
        let enabled = controller.enabled.clone();
        let guard = enabled.lock().unwrap();
        let (started, starting) = std::sync::mpsc::channel();
        let (finished, finishing) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            started.send(()).unwrap();
            finished.send(controller.is_enabled()).unwrap();
            controller
        });
        let start = starting.recv_timeout(TEST_TIMEOUT);
        let early = finishing.recv_timeout(TEST_TIMEOUT);
        drop(guard);
        let mut controller = reader.join().unwrap();
        start.unwrap();
        assert!(matches!(
            early,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(finishing.recv_timeout(TEST_TIMEOUT).unwrap());
        controller.disable().await;
    }

    #[tokio::test]
    async fn cancelled_enable_does_not_publish_success_and_can_be_retried() {
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let gate = controller.discovery.gate.clone();
        let enabled = controller.enabled.clone();
        let blocker = gate.subscribe();
        let mut enable = Box::pin(controller.enable());
        tokio::time::timeout(TEST_TIMEOUT, async {
            tokio::select! {
                _ = &mut enable => panic!("unacknowledged discovery consumer must block enable"),
                _ = async {
                    while gate.epoch() == 0 { tokio::task::yield_now().await; }
                } => {}
            }
        })
        .await
        .unwrap();
        assert!(!*enabled.lock().unwrap());
        drop(enable);
        drop(blocker);
        tokio::time::timeout(TEST_TIMEOUT, controller.enable())
            .await
            .unwrap();
        assert!(controller.is_enabled());
        assert!(gate.is_open());
        controller.disable().await;
    }

    #[tokio::test]
    async fn enabled_publication_happens_after_preparation_but_before_admission() {
        let gate = DispatchGate::new();
        let mut open = gate.subscribe();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let published = std::sync::atomic::AtomicBool::new(false);
        let mut start = Box::pin(gate.start_with(|| {
            assert!(
                gate.subscribers.try_lock().is_err(),
                "registration must remain serialized through publication and opening"
            );
            assert!(
                !gate.is_open(),
                "publication must precede receiver admission"
            );
            published.store(true, std::sync::atomic::Ordering::Release);
        }));
        let mut receive = Box::pin(gated_recv(&gate, &mut open, &mut rx));
        let mut context = Context::from_waker(Waker::noop());
        assert!(start.as_mut().poll(&mut context).is_pending());
        assert!(!published.load(std::sync::atomic::Ordering::Acquire));
        assert!(receive.as_mut().poll(&mut context).is_pending());
        assert!(start.as_mut().poll(&mut context).is_ready());
        tx.send(1).await.unwrap();
        assert_eq!(receive.await.unwrap().2, 1);
        assert!(published.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn open_gate_prunes_departed_receiver_registrations_during_churn() {
        let gate = DispatchGate::new_open();
        let retained = gate.subscribe();
        for _ in 0..1000 {
            let receiver = gate.subscribe();
            assert_eq!(gate.subscribers.lock().unwrap().len(), 2);
            drop(receiver);
        }
        drop(retained);
        let _receiver = gate.subscribe();
        assert_eq!(gate.subscribers.lock().unwrap().len(), 1);
        assert!(gate.is_open());
    }

    #[tokio::test]
    async fn retry_enable_clears_peer_count_after_cancelled_disable() {
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        controller.enable().await;
        controller
            .session_peer_counter
            .lock()
            .unwrap()
            .session_peer_count = 3;
        let gate = controller.discovery.gate.clone();
        let permit = gate.permit().await;
        let mut disable = Box::pin(controller.disable());
        let mut context = Context::from_waker(Waker::noop());
        assert!(disable.as_mut().poll(&mut context).is_pending());
        drop(disable);
        drop(permit);
        assert_eq!(controller.num_peers(), 3);
        controller.enable().await;
        assert_eq!(controller.num_peers(), 0);
        assert!(controller.peers.lock().unwrap().is_empty());
        controller.disable().await;
    }

    #[tokio::test]
    async fn drop_joins_running_callback_and_rejects_the_next_invocation() {
        use std::sync::{atomic::AtomicUsize, mpsc};
        let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let (started, running) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        *controller.tempo_callback.lock().unwrap() =
            Some(Arc::new(Mutex::new(Box::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                started.send(()).unwrap();
                released.recv_timeout(TEST_TIMEOUT).unwrap();
            }))));
        let callback = controller
            .managed_tempo_callback
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        controller.io.as_ref().unwrap().spawn(async move {
            callback.lock().unwrap()(120.0);
            callback.lock().unwrap()(120.0);
        });
        running.recv_timeout(TEST_TIMEOUT).unwrap();
        let closed = controller.callbacks_closed.clone();
        let (finished, completion) = mpsc::sync_channel(1);
        let dropper = std::thread::spawn(move || {
            drop(controller);
            finished.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + TEST_TIMEOUT;
        while !closed.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(
            completion.try_recv().is_err(),
            "drop must await the running callback"
        );
        release.send(()).unwrap();
        completion.recv_timeout(TEST_TIMEOUT).unwrap();
        dropper.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn contended_tempo_callback_does_not_deadlock_external_drop() {
        use std::sync::{atomic::AtomicUsize, mpsc};
        let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let user: TempoCallback = Arc::new(Mutex::new(Box::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        })));
        *controller.tempo_callback.lock().unwrap() = Some(user.clone());
        let lock = user.lock().unwrap();
        let callback = controller
            .managed_tempo_callback
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        let (admitted, admission) = mpsc::sync_channel(1);
        controller.io.as_ref().unwrap().spawn(async move {
            admitted.send(()).unwrap();
            callback.lock().unwrap()(120.0);
        });
        admission.recv_timeout(TEST_TIMEOUT).unwrap();
        let closed = controller.callbacks_closed.clone();
        let (finished, completion) = mpsc::sync_channel(1);
        let dropper = std::thread::spawn(move || {
            drop(controller);
            finished.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + TEST_TIMEOUT;
        while !closed.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        completion.recv_timeout(TEST_TIMEOUT).unwrap();
        drop(lock);
        dropper.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn callback_can_drop_its_controller_without_self_join_or_later_callbacks() {
        use std::sync::{atomic::AtomicUsize, mpsc};
        let controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let slot = Arc::new(Mutex::new(None::<Controller>));
        let owner = slot.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        *controller.tempo_callback.lock().unwrap() =
            Some(Arc::new(Mutex::new(Box::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                drop(owner.lock().unwrap().take());
            }))));
        let callback = controller
            .managed_tempo_callback
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        let session = Arc::downgrade(&controller.session_state);
        let (release, released) = tokio::sync::oneshot::channel();
        let (finished, done) = mpsc::sync_channel(1);
        controller.io.as_ref().unwrap().spawn(async move {
            released.await.unwrap();
            callback.lock().unwrap()(120.0);
            callback.lock().unwrap()(120.0);
            finished.send(()).unwrap();
        });
        *slot.lock().unwrap() = Some(controller);
        release.send(()).unwrap();
        done.recv_timeout(TEST_TIMEOUT).unwrap();
        assert!(slot.lock().unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let deadline = std::time::Instant::now() + TEST_TIMEOUT;
        while session.upgrade().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "runtime retained controller state"
            );
            std::thread::yield_now();
        }
    }

    #[tokio::test]
    async fn public_gateway_listen_preserves_notifier_cancellation() {
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let notifier = controller.notifier.clone();
        let mut listen = Box::pin(
            controller
                .discovery
                .listen(controller.rx_event.take().unwrap(), notifier.clone()),
        );
        let mut context = Context::from_waker(Waker::noop());
        assert!(listen.as_mut().poll(&mut context).is_pending());
        notifier.notify_waiters();
        assert!(listen.as_mut().poll(&mut context).is_ready());
    }

    #[tokio::test]
    async fn numeric_cleanup_signal_retries_identity_and_sends_unlocked_byebye() {
        use crate::discovery::{
            messages::{parse_message_header, BYEBYE, MAX_MESSAGE_SIZE},
            messenger::send_byebye_to,
        };
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let io = controller.io.take().unwrap();
        let gateway = controller.discovery.clone();
        let state = gateway.peer_state.clone();
        let check_unlocked = state.clone();
        let notifier = controller.notifier.clone();
        let open = gateway.gate.subscribe();
        let events = controller.rx_event.take().unwrap();
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let std::net::SocketAddr::V4(destination) = receiver.local_addr().unwrap() else {
            panic!("IPv4")
        };
        let (signal, signalled) = tokio::sync::oneshot::channel();
        let (seen, observed) = tokio::sync::oneshot::channel();
        let work = io.spawn(async move {
            gateway
                .listen_with_signal_and_departure(
                    events,
                    notifier,
                    open,
                    async {
                        signalled.await.unwrap();
                        seen.send(()).unwrap();
                        Ok(())
                    },
                    move |id| {
                        assert!(
                            check_unlocked.try_lock().is_ok(),
                            "release identity before network cleanup"
                        );
                        send_byebye_to(id, destination);
                    },
                )
                .await;
        });
        let id = NodeId::from_array([94; 8]);
        let (held, held_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let mut guard = state.lock().unwrap();
            held.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            guard.node_state.node_id = id;
        });
        held_rx.await.unwrap();
        signal.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), observed)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(
            Duration::from_millis(200),
            io.spawn(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let pending_while_locked = !work.is_finished();
        release.send(()).unwrap();
        holder.join().unwrap();
        tokio::time::timeout(Duration::from_secs(1), work)
            .await
            .unwrap()
            .unwrap();
        let mut bytes = [0; MAX_MESSAGE_SIZE];
        let wire =
            tokio::time::timeout(Duration::from_millis(300), receiver.recv_from(&mut bytes)).await;
        assert!(
            pending_while_locked && wire.is_ok(),
            "Ctrl-C must wait cooperatively and deliver BYEBYE"
        );
        let (size, _) = wire.unwrap().unwrap();
        let (header, _) = parse_message_header(&bytes[..size]).unwrap();
        assert_eq!(header.message_type, BYEBYE);
        assert_eq!(header.ttl, 0);
        assert_eq!(
            header.ident, id,
            "sample the current identity only after acquiring its guard"
        );
        drop(controller);
        drop(io);
    }

    #[tokio::test]
    async fn signal_setup_error_does_not_terminate_the_discovery_listener() {
        let mut controller = Controller::new(tempo::Tempo::new(120.0), Clock::new())
            .await
            .unwrap();
        let gate = controller.discovery.gate.clone();
        let mut listen = Box::pin(controller.discovery.listen_with_signal(
            controller.rx_event.take().unwrap(),
            controller.notifier.clone(),
            gate.subscribe(),
            std::future::ready(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
        ));
        let mut context = Context::from_waker(Waker::noop());
        // Ready errors must neither exit the listener nor be polled repeatedly.
        assert!(listen.as_mut().poll(&mut context).is_pending());
        assert!(listen.as_mut().poll(&mut context).is_pending());
        tokio::time::timeout(TEST_TIMEOUT, gate.start())
            .await
            .unwrap();
        assert!(listen.as_mut().poll(&mut context).is_pending());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_measures_and_joins_on_a_multi_thread_runtime() {
        measures_and_joins_after_each_enable().await;
    }

    async fn measures_and_joins_after_each_enable() {
        use crate::link::pingresponder::{PingResponder, MAX_MESSAGE_SIZE};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let clock = Clock::new();
        let mut controller = Controller::new(tempo::Tempo::new(120.0), clock)
            .await
            .unwrap();
        let peer_id = NodeId::from_array([1; 8]);
        let peer_session = SessionId(peer_id);
        let local = controller
            .discovery
            .socket_probes()
            .into_iter()
            .filter_map(|socket| socket.upgrade())
            .find_map(|socket| match socket.local_addr().unwrap() {
                std::net::SocketAddr::V4(addr) if !addr.ip().is_unspecified() => Some(*addr.ip()),
                _ => None,
            })
            .expect("identified local discovery interface");
        let socket = Arc::new(tokio::net::UdpSocket::bind((local, 0)).await.unwrap());
        let std::net::SocketAddr::V4(endpoint) = socket.local_addr().unwrap() else {
            panic!("expected an IPv4 measurement endpoint");
        };
        let responder = PingResponder::new(
            socket.clone(),
            peer_session,
            GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(60) - clock.micros(),
            },
            clock,
        );
        let pings = Arc::new(AtomicUsize::new(0));
        let peer_pings = pings.clone();
        let received_ping = Arc::new(Notify::new());
        let received = received_ping.clone();
        let responding = Arc::new(AtomicBool::new(false));
        let peer_responding = responding.clone();
        let peer = tokio::spawn(async move {
            let mut buf = [0; MAX_MESSAGE_SIZE];
            loop {
                let (size, from) = socket.recv_from(&mut buf).await.unwrap();
                peer_pings.fetch_add(1, Ordering::Relaxed);
                received.notify_one();
                if peer_responding.load(Ordering::Relaxed) {
                    responder.handle_ping(&buf[..size], from).await;
                }
            }
        });

        let node_id = controller.peer_state.lock().unwrap().ident();
        // Even disable-before-first-enable must not kill the only result consumer.
        controller.disable().await;

        controller.enable().await;
        let announce = |timeline| {
            let state = PeerState {
                node_state: NodeState {
                    node_id: peer_id,
                    session_id: peer_session,
                    timeline,
                    start_stop_state: StartStopState::default(),
                },
                measurement_endpoint: Some(endpoint),
                audio_endpoint: None,
            };
            crate::discovery::messages::encode_message(
                peer_id,
                30,
                crate::discovery::messages::RESPONSE,
                &crate::link::payload::Payload::from(state),
                0,
            )
            .unwrap()
        };
        controller
            .discovery
            .receive_peer_state_fixture(local, &announce(Timeline::default()))
            .await;
        tokio::time::timeout(TEST_TIMEOUT, async {
            while pings.load(Ordering::Relaxed) == 0 {
                received_ping.notified().await;
            }
        })
        .await
        .unwrap();
        // Cancel a measurement that has started but has not received a reply.
        controller.disable().await;
        responding.store(true, Ordering::Relaxed);

        for cycle in 0..3 {
            controller
                .discovery
                .event_sender()
                .send(OnEvent::PeerState(
                    crate::discovery::peers::PeerStateMessageType {
                        expires_at: None,
                        observation: None,
                        node_state: NodeState {
                            node_id: NodeId::from_array([99; 8]),
                            session_id: SessionId(NodeId::from_array([99; 8])),
                            timeline: Timeline::default(),
                            start_stop_state: StartStopState::default(),
                        },
                        ttl: 3,
                        measurement_endpoint: Some(endpoint),
                        audio_endpoint: None,
                    },
                ))
                .await
                .unwrap();
            controller.enable().await;
            assert!(
                controller.peers.lock().unwrap().is_empty(),
                "disabled discovery events must not repopulate reset peers"
            );
            assert_eq!(controller.session_id(), SessionId(node_id));
            let previous_pings = pings.load(Ordering::Relaxed);
            let timeline = Timeline {
                tempo: tempo::Tempo::new([100.0, 125.0, 150.0][cycle as usize]),
                ..Timeline::default()
            };
            // Decode accepted discovery bytes with the current registration;
            // this lifecycle test does not require the host to route multicast.
            controller
                .discovery
                .receive_peer_state_fixture(local, &announce(timeline))
                .await;
            tokio::time::timeout(TEST_TIMEOUT, async {
                while controller.session_id() != peer_session
                    || controller.num_peers() != 1
                    || controller.client_state.lock().unwrap().timeline.tempo != timeline.tempo
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("a fresh measurement must join the peer in every lifecycle");
            assert!(pings.load(Ordering::Relaxed) > previous_pings);
            assert_eq!(
                controller.client_state.lock().unwrap().timeline.tempo,
                timeline.tempo
            );
            assert_eq!(controller.num_peers(), 1);
            assert_eq!(controller.dispatch.tasks.len(), 5);

            controller.enable().await;
            assert_eq!(controller.session_id(), peer_session);
            assert_eq!(controller.dispatch.tasks.len(), 5);

            controller.disable().await;
            assert!(!controller.is_enabled());
            assert!(controller
                .dispatch
                .tasks
                .iter()
                .all(|task| !task.is_finished()));
        }

        let sockets = controller.discovery.socket_probes();
        let event_state = controller.discovery.event_state_probe();
        let peer_state = Arc::downgrade(&controller.discovery.peer_state);
        let peer_counter = Arc::downgrade(&controller.discovery.session_peer_counter);
        let peers = Arc::downgrade(&controller.peers);
        let tasks: Vec<_> = controller
            .dispatch
            .tasks
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect();
        drop(controller);
        tokio::time::timeout(TEST_TIMEOUT, async {
            while tasks.iter().any(|task| !task.is_finished())
                || sockets.iter().any(|socket| socket.strong_count() != 0)
                || event_state.strong_count() != 0
                || peer_state.strong_count() != 0
                || peer_counter.strong_count() != 0
                || peers.strong_count() != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        peer.abort();
        assert!(peer.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn restart_rejects_late_work_from_an_earlier_measurement_epoch() {
        let gate = DispatchGate::new();
        gate.start().await;
        let old_epoch = gate.epoch();
        gate.stop().await;
        gate.start().await;
        let old_work = gate.run_in_epoch(old_epoch, async { panic!("stale result ran") });
        assert!(old_work.await.is_none());
        assert_eq!(gate.run_in_epoch(gate.epoch(), async { 7 }).await, Some(7));
    }

    #[tokio::test]
    async fn restart_cancels_a_measurement_result_blocked_on_a_full_channel() {
        let gate = DispatchGate::new();
        gate.start().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(1).unwrap();
        let mut result = Box::pin(gate.run_in_epoch(gate.epoch(), tx.send(2)));
        let mut context = Context::from_waker(Waker::noop());
        assert!(result.as_mut().poll(&mut context).is_pending());
        let mut stop = Box::pin(gate.stop());
        assert!(stop.as_mut().poll(&mut context).is_pending());
        assert!(matches!(
            result.as_mut().poll(&mut context),
            std::task::Poll::Ready(None)
        ));
        assert!(stop.as_mut().poll(&mut context).is_ready());
        assert_eq!(rx.try_recv().unwrap(), 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn close_is_idempotent() {
        let gate = DispatchGate::new();
        gate.start().await;
        assert!(gate.is_open());
        gate.close();
        assert!(!gate.is_open());
        // Mirrors upstream's `LockFreeCallbackDispatcher::stop()`
        // (`53f0627c9cf8`) being safe to call more than once - including from
        // both an explicit `disable()` and the destructor afterwards.
        gate.close();
        assert!(!gate.is_open());
        gate.stop().await;
        assert!(!gate.is_open());
    }

    #[tokio::test]
    async fn close_stops_admitting_queued_work() {
        let gate = DispatchGate::new();
        gate.start().await;
        let mut open = gate.subscribe();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.try_send(1).unwrap();
        gate.close();

        let mut recv = Box::pin(gated_recv(&gate, &mut open, &mut rx));
        let mut context = Context::from_waker(Waker::noop());
        assert!(recv.as_mut().poll(&mut context).is_pending());
        tx.try_send(2).unwrap();
        assert!(recv.as_mut().poll(&mut context).is_pending());

        // Reopening drains both pre-close and disabled-lifecycle work.
        let mut start = Box::pin(gate.start());
        assert!(start.as_mut().poll(&mut context).is_pending());
        assert!(recv.as_mut().poll(&mut context).is_pending());
        assert!(start.as_mut().poll(&mut context).is_ready());
        tx.try_send(3).unwrap();
        let (permit, epoch, work) = recv.await.unwrap();
        assert_eq!(work, 3);
        assert_eq!(epoch, gate.epoch());
        drop(permit);
        gate.stop().await;
    }

    #[tokio::test]
    async fn drop_cancels_queued_work_before_first_poll() {
        let mut dispatch = DispatchTasks::new();
        let gate = dispatch.gate.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (callback_tx, mut callback_rx) = tokio::sync::mpsc::channel(4);
        gate.start().await;
        tx.try_send(7).unwrap();
        let task_gate = gate.clone();
        dispatch.tasks.push(tokio::spawn(async move {
            let mut open = task_gate.subscribe();
            while let Some((_permit, _, work)) = gated_recv(&task_gate, &mut open, &mut rx).await {
                callback_tx.try_send(work).unwrap();
            }
        }));

        // Current-thread runtime: the task cannot run before this drop.
        drop(dispatch);
        assert!(!gate.is_open());
        tokio::time::timeout(TEST_TIMEOUT, tx.closed())
            .await
            .unwrap();
        assert_eq!(callback_rx.recv().await, None);
    }

    #[tokio::test]
    async fn drop_releases_parked_tasks_in_each_gate_state() {
        for enabled in [false, true] {
            let mut dispatch = DispatchTasks::new();
            if enabled {
                dispatch.gate.start().await;
            }
            let gate = dispatch.gate.clone();
            let task_gate = gate.clone();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(1);
            let (started_tx, started_rx) = oneshot::channel();
            dispatch.tasks.push(tokio::spawn(async move {
                let mut open = task_gate.subscribe();
                started_tx.send(()).unwrap();
                assert!(gated_recv(&task_gate, &mut open, &mut rx).await.is_none());
            }));
            // The task runs until gated_recv yields before we can resume.
            tokio::time::timeout(TEST_TIMEOUT, started_rx)
                .await
                .unwrap()
                .unwrap();
            drop(dispatch);
            assert!(!gate.is_open());
            tokio::time::timeout(TEST_TIMEOUT, tx.closed())
                .await
                .unwrap();
            assert_eq!(Arc::strong_count(&gate), 1);
        }
    }

    #[tokio::test]
    async fn drop_cancels_admitted_work_suspended_before_callback() {
        let mut dispatch = DispatchTasks::new();
        let gate = dispatch.gate.clone();
        gate.start().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (admitted_tx, admitted_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let (callback_tx, callback_rx) = oneshot::channel();
        let task_gate = gate.clone();
        dispatch.tasks.push(tokio::spawn(async move {
            let mut open = task_gate.subscribe();
            let (_permit, _, work) = gated_recv(&task_gate, &mut open, &mut rx).await.unwrap();
            admitted_tx.send(()).unwrap();
            resume_rx.await.unwrap();
            callback_tx.send(work).unwrap();
        }));
        tx.try_send(7).unwrap();
        tokio::time::timeout(TEST_TIMEOUT, admitted_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(gate.in_flight.try_write().is_err());
        drop(dispatch);
        assert!(!gate.is_open());
        // Make the suspended future ready after cancellation was requested.
        // The cancelled task must not resume to invoke its callback.
        resume_tx.send(()).unwrap();
        assert!(tokio::time::timeout(TEST_TIMEOUT, callback_rx)
            .await
            .unwrap()
            .is_err());
        assert!(gate.in_flight.try_write().is_ok());
        tokio::time::timeout(TEST_TIMEOUT, tx.closed())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn drop_without_disable_closes_the_gate() {
        let clock = Clock::new();
        let controller = Controller::new(tempo::Tempo::new(120.0), clock)
            .await
            .expect("Controller::new must succeed for this test to prove anything about drop");
        let gate = controller.dispatch.gate.clone();
        let tasks: Vec<_> = controller
            .dispatch
            .tasks
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect();
        assert_eq!(tasks.len(), 4);

        // Stand in for `enable()`, which additionally starts discovery: the
        // gate state this test is about is exactly what `enable()` sets.
        gate.start().await;
        assert!(gate.is_open());
        drop(controller);
        assert!(!gate.is_open());
        tokio::time::timeout(TEST_TIMEOUT, async {
            while tasks.iter().any(|task| !task.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
