use std::{
    net::{IpAddr, SocketAddrV4},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, Weak},
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
    AudioEndpointCallback, IncomingClientState, TempoCallback,
};

pub const LOCAL_MOD_GRACE_PERIOD: Duration = Duration::milliseconds(1000);

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
        if self.is_open() {
            publish();
            return;
        }
        let epoch = self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
        self.active.send_replace(DispatchState::Preparing(epoch));
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            {
                let mut subscribers = self
                    .subscribers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                subscribers.retain(|subscriber| subscriber.strong_count() != 0);
                let all_ready = subscribers.iter().all(|subscriber| {
                    subscriber.upgrade().is_none_or(|prepared| {
                        prepared.load(std::sync::atomic::Ordering::Acquire) >= epoch
                    })
                });
                if all_ready {
                    // Serialize the final readiness check and opening with
                    // subscribe(), including receivers created by interface scans.
                    publish();
                    self.active.send_replace(DispatchState::Open);
                    return;
                }
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

pub struct Controller {
    // Retains cancellation handles; Controller::drop also joins the IO runtime.
    dispatch: DispatchTasks,
    io: Option<crate::platform::io_context::IoContext>,
    callbacks_closed: Arc<AtomicBool>,
    managed_tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
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
        let tempo_callback: Arc<Mutex<Option<TempoCallback>>> = Arc::new(Mutex::new(None));
        let audio_endpoint_callback: Arc<Mutex<Option<AudioEndpointCallback>>> =
            Arc::new(Mutex::new(None));
        let callbacks_closed = Arc::new(AtomicBool::new(false));
        let callback = tempo_callback.clone();
        let closed = callbacks_closed.clone();
        let managed: TempoCallback = Arc::new(Mutex::new(Box::new(move |bpm| {
            let callback = callback
                .try_lock()
                .ok()
                .and_then(|callback| callback.clone());
            if let Some(callback) = callback {
                if let Ok(callback) = callback.try_lock() {
                    if !closed.load(Ordering::Acquire) {
                        callback(bpm);
                    }
                }
            }
        })));
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
        let session_id = SessionId(node_id);
        let s_state = init_session_state(tempo, clock);
        let client_state = Arc::new(Mutex::new(init_client_state(s_state, session_id)));

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

        let ip = list_afinet_netifas()
            .map_err(|e| {
                std::io::Error::other(format!("failed to enumerate network interfaces: {}", e))
            })?
            .iter()
            .find_map(|(_, ip)| match ip {
                IpAddr::V4(ipv4) if !ip.is_loopback() => Some(*ipv4),
                _ => None,
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "no non-loopback IPv4 interface found",
                )
            })?;

        let ping_responder_unicast_socket =
            Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0).into())?);

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
            .await?,
        );
        discovery.measurement_service.stop().await;
        discovery.stop().await;

        let mut dispatch = DispatchTasks::new();
        let (sessions, session_task) = Sessions::with_dispatch_gate(
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
            rx_measure_peer_result,
            dispatch.gate.clone(),
        );
        dispatch.tasks.push(session_task);

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

        // An audio-endpoint notification held back because the
        // `SessionMembership` change it was queued behind could not be applied
        // - every read in that arm is a `try_lock` that skips the change rather
        // than block. Dropping the notification instead is not an option:
        // `saw_peer` has already recorded the new endpoint, so an identical
        // later sighting is not a transition and will never re-fire the edge.
        // It is re-emitted the next time membership is applied successfully,
        // which is also what keeps it behind membership rather than ahead of
        // it. A newer edge supersedes an older held one; latest wins.
        //
        // Stamped with the gate epoch it was deferred in: this state outlives
        // the loop iteration and so is not reached by the channel drain a gate
        // close performs, and an endpoint held from a previous lifecycle must
        // not be released into the next one.
        let mut deferred_audio_endpoint: Option<(u64, NodeId, Option<SocketAddrV4>)> = None;

        let gate_loop = dispatch.gate.clone();

        let mut gate_open = gate_loop.subscribe();
        dispatch.tasks.push(tokio::spawn(async move {
            while let Some((_permit, epoch, peer_state_changes)) =
                gated_recv(&gate_loop, &mut gate_open, &mut rx_peer_state_change).await
            {
                // Anything held from a lifecycle that has since been torn down
                // and restarted is dropped rather than dispatched.
                if let Some((deferred_epoch, peer_id, _)) = deferred_audio_endpoint {
                    if deferred_epoch != epoch {
                        debug!(
                            "Discarding AudioEndpoint change for peer {} deferred in a \
                                 previous Link lifecycle",
                            peer_id
                        );
                        deferred_audio_endpoint = None;
                    }
                }
                debug!("controller received peer state changes");
                // Set when a `SessionMembership` change in this batch was
                // abandoned. Any audio-endpoint change queued behind it is
                // then held back rather than delivered against state the
                // abandoned change was supposed to update.
                let mut membership_abandoned = false;
                for peer_state_change in peer_state_changes.iter() {
                    match peer_state_change {
                        PeerStateChange::SessionMembership => {
                            debug!("Controller received SessionMembership change");
                            // Both reads come from one guard: taken
                            // separately they are two chances to bail, and
                            // two different snapshots of the same state.
                            let ids = peer_state_loop
                                .try_lock()
                                .map(|ps| (ps.session_id(), ps.ident()))
                                .ok();
                            let (session_id, self_node_id) = match ids {
                                Some(ids) => ids,
                                None => {
                                    membership_abandoned = true;
                                    continue;
                                }
                            };

                            let count =
                                unique_session_peer_count(session_id, p_loop.clone(), self_node_id);
                            let old_count = if let Ok(spc) = s_peer_counter_loop.try_lock() {
                                spc.session_peer_count
                            } else {
                                membership_abandoned = true;
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
                                    tempo_cb_loop.clone(),
                                )
                                .await
                            }

                            // Membership is now applied, so an endpoint
                            // edge held back by an earlier abandoned
                            // membership change can be delivered - still
                            // after membership, which is the point.
                            if let Some((_, peer_id, endpoint)) = deferred_audio_endpoint.take() {
                                debug!(
                                    "Controller releasing deferred AudioEndpoint change \
                                         for peer {}",
                                    peer_id
                                );
                                dispatch_audio_endpoint_change(
                                    &audio_endpoint_cb_loop,
                                    peer_id,
                                    endpoint,
                                );
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
                                tempo_cb_loop.clone(),
                                *peer_session,
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

                            let current_timestamp = if let Ok(s_state) = s_state_loop.try_lock() {
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
                            let count = unique_session_peer_count(s_id, p_loop.clone(), peer_ident);
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
                                    tempo_cb_loop.clone(),
                                )
                                .await;
                            }
                        }
                        PeerStateChange::AudioEndpoint(peer_id, endpoint) => {
                            debug!(
                                "Controller received AudioEndpoint change for peer {}",
                                peer_id
                            );
                            if membership_abandoned {
                                debug!(
                                    "Deferring AudioEndpoint change for peer {}: the \
                                         membership change it follows was not applied",
                                    peer_id
                                );
                                deferred_audio_endpoint = Some((epoch, *peer_id, *endpoint));
                                continue;
                            }
                            dispatch_audio_endpoint_change(
                                &audio_endpoint_cb_loop,
                                *peer_id,
                                *endpoint,
                            );
                        }
                    }
                }
            }
        }));

        Ok(Self {
            dispatch,
            io: None,
            callbacks_closed,
            managed_tempo_callback,
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
        if *self
            .enabled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            return;
        }

        // A cancelled enable/disable may leave only some gates open. Quiesce
        // them before retrying the reset instead of assuming an atomic startup.
        self.discovery.measurement_service.stop().await;
        self.discovery.stop().await;
        self.dispatch.gate.stop().await;
        self.session_peer_counter.lock().unwrap().session_peer_count = 0;

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
            .start_with(|| {
                self.discovery.discard_queued_datagrams();
                *self
                    .enabled
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
            })
            .await;
    }

    pub async fn disable(&mut self) {
        // Publish the transition before awaiting, so cancellation cannot leave
        // is_enabled true with only part of the pipeline running.
        *self
            .enabled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
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
        if let Ok(mut counter) = self.session_peer_counter.try_lock() {
            counter.session_peer_count = 0;
            info!("Reset session peer count to 0");
        }

        // Clear all peers from the discovery
        self.discovery.observer.reset_peers();
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
                self.managed_tempo_callback.clone(),
                peer_session_id,
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
    /// can discover where to send audio traffic.
    pub fn set_audio_endpoint(&self, endpoint: Option<SocketAddrV4>) {
        if let Ok(mut peer_state) = self.peer_state.try_lock() {
            peer_state.audio_endpoint = endpoint;
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
        tempo_callback.clone(),
        session.session_id,
    );

    // Verify that client state was actually updated
    if let Ok(client_state_check) = client_state.try_lock() {
        info!(
            "after joining session {}, client state tempo is now: {}",
            session.session_id,
            client_state_check.timeline.tempo.bpm()
        );
    }

    update_discovery(session_state.clone(), peer_state.clone(), discovery.clone()).await;

    if session_id_changed {
        info!(
            "joining session {} with tempo {}",
            session.session_id,
            session.timeline.tempo.bpm().round()
        );

        // session_peer_counter(session_id, peers, session_peer_count);

        let should_reset = if let (Ok(peer_state_guard), Ok(mut session_peer_count_guard)) =
            (peer_state.try_lock(), session_peer_count.try_lock())
        {
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
                tempo_callback,
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
    tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
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

    // Create a temporary session while waiting for discovery
    // This session will be replaced if we find a better session on the network
    let s_id = SessionId(n_id);

    if let Ok(mut peer_state_guard) = peer_state.try_lock() {
        peer_state_guard.node_state.node_id = n_id;
        peer_state_guard.node_state.session_id = s_id;
    }

    let x_form = init_x_form(clock);
    let host_time = -x_form.intercept;

    let (timeline, ghost_x_form) = if let Ok(session_state_guard) = session_state.try_lock() {
        (
            session_state_guard.timeline,
            session_state_guard.ghost_x_form,
        )
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
        "initializing temporary session {} with timeline {:?} (preserving NodeId: {})",
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
        tempo_callback,
        s_id,
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
    let (timeline, start_stop_state, ghost_xform) =
        if let Ok(session_state_guard) = session_state.try_lock() {
            (
                session_state_guard.timeline,
                session_state_guard.start_stop_state,
                session_state_guard.ghost_x_form,
            )
        } else {
            return; // Skip update if we can't get the lock
        };

    let (node_id, session_id, measurement_endpoint) =
        if let Ok(peer_state_guard) = peer_state.try_lock() {
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
    tempo_callback: Arc<Mutex<Option<TempoCallback>>>,
    session_id: SessionId,
) {
    let new_timeline = clamp_tempo(new_timeline);
    let mut changed_tempo = None;

    if let Ok(mut session_state) = session_state.try_lock() {
        let old_timeline = session_state.timeline;
        let old_x_form = session_state.ghost_x_form;

        if old_timeline != new_timeline || old_x_form != new_x_form {
            session_state.timeline = new_timeline;
            session_state.ghost_x_form = new_x_form;

            if let Ok(mut client_state_guard) = client_state.try_lock() {
                let old_client_timeline = client_state_guard.timeline;
                client_state_guard.timeline = update_client_timeline_from_session(
                    old_client_timeline, // Current client timeline
                    new_timeline,        // Session timeline to sync to
                    clock.micros(),
                    new_x_form,
                );
                client_state_guard.timeline_session_id = session_id;

                if let Ok(start_stop_enabled) = start_stop_sync_enabled.try_lock() {
                    if *start_stop_enabled
                        && session_state.start_stop_state != StartStopState::default()
                    {
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
                changed_tempo = Some(new_timeline.tempo.bpm());
            }
        }
    }
    if let Some(bpm) = changed_tempo {
        let callback = tempo_callback
            .try_lock()
            .ok()
            .and_then(|callback| callback.clone());
        if let Some(callback) = callback {
            if let Ok(callback) = callback.try_lock() {
                callback(bpm);
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
        .ghost_to_host(Duration::microseconds(0));

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

#[cfg(test)]
mod dispatch_gate_tests {
    use super::*;
    use std::{future::Future, task::Context, task::Waker, time::Duration};
    use tokio::sync::oneshot;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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

        let (dispatch, _, tx, mut joined) = session_dispatch();
        dispatch.gate.start().await;
        for cycle in 1..=3 {
            let x_form = GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(cycle),
            };
            tx.send(MeasurePeerEvent::XForm(SessionId::default(), x_form))
                .await
                .unwrap();
            let session = tokio::time::timeout(TEST_TIMEOUT, joined.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(session.measurement.x_form, x_form);

            dispatch.gate.stop().await;
            tx.send(MeasurePeerEvent::XForm(
                SessionId::default(),
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
        for seconds in 1..=2 {
            let x_form = GhostXForm {
                slope: 1.0,
                intercept: chrono::Duration::seconds(seconds),
            };
            tx.send(MeasurePeerEvent::XForm(SessionId::default(), x_form))
                .await
                .unwrap();
            tokio::time::timeout(TEST_TIMEOUT, async {
                while sessions.current.lock().unwrap().measurement.x_form != x_form {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        // The first join fills the queue; the second result has been applied
        // but its send is blocked. stop() must cancel that send, not deadlock.
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
        tx.send(MeasurePeerEvent::XForm(SessionId::default(), fresh))
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
            callback.lock().unwrap()(121.0);
            callback.lock().unwrap()(122.0);
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
            callback.lock().unwrap()(121.0);
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
            callback.lock().unwrap()(121.0);
            callback.lock().unwrap()(122.0);
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
        let local_ip = controller
            .discovery
            .measurement_service
            .shared_socket
            .local_addr()
            .unwrap()
            .ip();
        let peer_id = NodeId::from_array([1; 8]);
        let peer_session = SessionId(peer_id);
        // Keep source and destination on the same local interface on every platform.
        let socket = Arc::new(tokio::net::UdpSocket::bind((local_ip, 0)).await.unwrap());
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
        controller.peers.lock().unwrap().push(ControllerPeer {
            peer_state: PeerState {
                node_state: NodeState {
                    node_id: peer_id,
                    session_id: peer_session,
                    timeline: Timeline::default(),
                    start_stop_state: StartStopState::default(),
                },
                measurement_endpoint: Some(endpoint),
                audio_endpoint: None,
            },
        });
        controller
            .sessions
            .saw_session_timeline(peer_session, Timeline::default())
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
                tempo: tempo::Tempo::new(135.0 + f64::from(cycle)),
                ..Timeline::default()
            };
            controller.peers.lock().unwrap().push(ControllerPeer {
                peer_state: PeerState {
                    node_state: NodeState {
                        node_id: peer_id,
                        session_id: peer_session,
                        timeline,
                        start_stop_state: StartStopState::default(),
                    },
                    measurement_endpoint: Some(endpoint),
                    audio_endpoint: None,
                },
            });

            // Seed discovery without multicast; everything from requesting a
            // measurement through UDP ping/pong, regression, and joining is real.
            controller
                .sessions
                .saw_session_timeline(peer_session, timeline)
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
            assert_eq!(controller.dispatch.tasks.len(), 4);

            controller.enable().await;
            assert_eq!(controller.session_id(), peer_session);
            assert_eq!(controller.dispatch.tasks.len(), 4);

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
        assert_eq!(tasks.len(), 3);

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
