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
    AudioEndpointCallback, IncomingClientState, TempoCallback,
};

pub const LOCAL_MOD_GRACE_PERIOD: Duration = Duration::milliseconds(1000);

/// Start/stop gate for the background dispatch loops (join-session and
/// peer-state-change) spawned in [`Controller::new`]. Rust analogue of
/// upstream's `RtClientStateSetter::start()`/`stop()` (see upstream commits
/// `57b77a8040d3` and `44d78f2cf3a4`): the loops are constructed once and
/// gated, rather than torn down, so that a [`Controller::disable`] followed by
/// a [`Controller::enable`] resumes dispatching instead of leaving the
/// receivers permanently closed.
///
/// [`DispatchGate::stop`] is *acknowledged*: it closes the gate against new
/// work and then waits for every consumer to release its permit, which a
/// consumer does only after discarding whatever its queue still held. So once
/// `stop()` returns, no dispatch work is running against a `Controller` that
/// has begun tearing down, and nothing produced before the shutdown is left
/// sitting in a channel waiting to be dispatched into the next lifecycle.
///
/// A permit is held across the consumer's `recv()`, not taken after it: a
/// permit acquired after `start()` can only carry work observed in the current
/// lifecycle, so no generation stamp on the work itself is needed.
///
/// The gate starts *closed*, matching `Controller`'s `enabled == false` at
/// construction: dispatch only ever runs between an [`Controller::enable`] and
/// the [`Controller::disable`] that follows it.
#[derive(Debug)]
pub(crate) struct DispatchGate {
    active: tokio::sync::watch::Sender<bool>,
    in_flight: tokio::sync::RwLock<()>,
    /// Bumped by every [`DispatchGate::start`]. Consumers that carry state
    /// across loop iterations - state the gate's own drain cannot reach,
    /// because it does not live in a channel - compare this against the epoch
    /// the state was recorded in and discard it when they differ.
    epoch: std::sync::atomic::AtomicU64,
}

impl DispatchGate {
    fn new() -> Self {
        DispatchGate {
            active: tokio::sync::watch::Sender::new(false),
            in_flight: tokio::sync::RwLock::new(()),
            epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A view of the open/closed state for a dispatch loop to park on. `watch`
    /// is lossless with respect to transitions the receiver has not yet seen,
    /// so a close that lands between a consumer's open check and its wait
    /// cannot be missed.
    fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.active.subscribe()
    }

    /// Acquires the right to receive and run dispatch work. The permit is held
    /// across the consumer's `recv()`, so [`DispatchGate::stop`] cannot return
    /// while a consumer still holds queued work it has not discarded.
    async fn permit(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.in_flight.read().await
    }

    fn is_open(&self) -> bool {
        *self.active.borrow()
    }

    fn start(&self) {
        self.epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.active.send_if_modified(|open| {
            let changed = !*open;
            *open = true;
            changed
        });
    }

    async fn stop(&self) {
        self.active.send_if_modified(|open| {
            let changed = *open;
            *open = false;
            changed
        });
        // Waiting for exclusive access waits out any batch already in flight,
        // and - because a consumer drains its queue before releasing its permit
        // - also for the pre-stop queue contents to have been discarded.
        let _ = self.in_flight.write().await;
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
async fn gated_recv<'a, T>(
    gate: &'a DispatchGate,
    open: &mut tokio::sync::watch::Receiver<bool>,
    rx: &mut tokio::sync::mpsc::Receiver<T>,
) -> Option<(tokio::sync::RwLockReadGuard<'a, ()>, u64, T)> {
    loop {
        // Park without a permit while the gate is closed, so a disabled
        // controller's consumers never hold `stop()` up. Anything produced
        // while it was closed is discarded on the way back in, alongside
        // whatever was already queued when it closed.
        if !*open.borrow_and_update() {
            if open.wait_for(|open| *open).await.is_err() {
                return None;
            }
            drain_pending(rx);
        }

        let permit = gate.permit().await;
        if !gate.is_open() {
            drop(permit);
            continue;
        }
        // Read under the permit: the gate cannot close (and so cannot be
        // restarted) while it is held, so this epoch stays valid for as long
        // as the returned work is being dispatched.
        let epoch = gate.epoch();

        tokio::select! {
            biased;
            closed = open.wait_for(|open| !*open) => {
                drain_pending(rx);
                drop(permit);
                if closed.is_err() {
                    return None;
                }
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
    /// Gate for the background dispatch loops (join-session and
    /// peer-state-change) spawned in [`Controller::new`]. Rust analogue of
    /// upstream's `RtClientStateSetter::stop()`
    /// (`ableton::link::Controller::CallbackDispatcher`, see upstream commit
    /// `44d78f2cf3a4`): closed and drained from [`Controller::disable`] so that
    /// no dispatch work can run after shutdown has begun, rather than merely
    /// relying on the loops to observe `enabled == false`. Reopened by
    /// [`Controller::enable`], so disable/re-enable cycles keep working. Starts
    /// closed, matching `enabled == false` at construction, so no dispatch work
    /// runs before the first `enable()`.
    dispatch_gate: Arc<DispatchGate>,
}

impl Controller {
    pub async fn new(tempo: tempo::Tempo, clock: Clock) -> Result<Self, std::io::Error> {
        let node_id = NodeId::new();
        let tempo_callback: Arc<Mutex<Option<TempoCallback>>> = Arc::new(Mutex::new(None));
        let audio_endpoint_callback: Arc<Mutex<Option<AudioEndpointCallback>>> =
            Arc::new(Mutex::new(None));
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
        let tempo_cb_loop = tempo_callback.clone();

        let dispatch_gate = Arc::new(DispatchGate::new());
        let gate_loop = dispatch_gate.clone();

        tokio::spawn(async move {
            let mut gate_open = gate_loop.subscribe();
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
        });

        let discovery_loop = discovery.clone();
        let s_state_loop = session_state.clone();
        let c_state_loop = client_state.clone();
        let s_stop_sync_enabled_loop = start_stop_sync_enabled.clone();
        let sessions_loop = sessions.clone();
        let p_loop = peers.clone();
        let s_peer_counter_loop = session_peer_counter.clone();
        let peer_state_loop = peer_state.clone();
        let tempo_cb_loop = tempo_callback.clone();
        let audio_endpoint_cb_loop = audio_endpoint_callback.clone();

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

        let gate_loop = dispatch_gate.clone();

        tokio::spawn(async move {
            let mut gate_open = gate_loop.subscribe();
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
        });

        Ok(Self {
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
            dispatch_gate,
        })
    }

    pub async fn enable(&mut self) {
        // Flip the enabled flag *before* reopening the gate, and do it with a
        // blocking lock rather than a `try_lock`, so the two lifecycle controls
        // cannot diverge: a failed `try_lock` here used to leave dispatch active
        // while `is_enabled()` still reported false. Every holder of this lock
        // takes it for a single bool read or write and never across an await,
        // so this cannot deadlock; a poisoned lock is recovered from rather
        // than skipped, since the value it guards is a plain bool.
        *self
            .enabled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;

        // Reopen the dispatch gate closed by a previous `disable()`, mirroring
        // upstream's `RtClientStateSetter::start()` being paired with its
        // `stop()`. Without this, a disable/re-enable cycle would leave the
        // join-session and peer-state-change loops permanently gated off.
        self.dispatch_gate.start();

        reset_state(
            self.peer_state.clone(),
            self.session_state.clone(),
            self.client_state.clone(),
            self.discovery.clone(),
            self.sessions.clone(),
            self.clock,
            self.start_stop_sync_enabled.clone(),
            self.tempo_callback.clone(),
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
        // Stop the background dispatch loops before anything else, mirroring
        // upstream's `mRtClientStateSetter.stop()` at the top of the async
        // shutdown handler (see `44d78f2cf3a4`, "Stop the
        // RtClientStateDispatcher on shutdown"). This closes the gate and
        // drains any batch already in flight, so once it returns no further
        // join-session or peer-state-change processing can run, rather than
        // relying solely on the loops to observe `enabled == false` on their
        // next iteration. The loops themselves stay alive so that a later
        // `enable()` can resume dispatching.
        self.dispatch_gate.stop().await;

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

        // Symmetrically with `enable()`, take the lock properly rather than
        // best-effort: a skipped `try_lock` here would leave `enabled` true
        // behind an already-closed gate.
        {
            *self
                .enabled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
            info!("Set Link enabled state to false");
        }

        // Signal background tasks (broadcast loop, discovery listener) to stop
        self.notifier.notify_waiters();
        info!("Notified background tasks to stop");

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

        // NOTE: The join-session and peer-state-change dispatch loops are
        // gated off and drained above, before the bye-bye message is sent,
        // matching upstream's shutdown ordering. Other spawned tasks
        // (discovery listener, measurement tasks) still observe
        // `enabled=false` and the notifier signal to wind down on their own,
        // since they have no equivalent race window during startup.
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
                self.tempo_callback.clone(),
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
                if let Ok(callback_guard) = tempo_callback.try_lock() {
                    if let Some(ref callback) = *callback_guard {
                        if let Ok(cb) = callback.try_lock() {
                            cb(new_timeline.tempo.bpm());
                        }
                    }
                }
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
