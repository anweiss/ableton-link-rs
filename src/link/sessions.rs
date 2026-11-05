use crate::platform::io_context::{retry_state, try_state};
use std::{
    fmt::{self, Display},
    mem,
    num::NonZeroU64,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use chrono::Duration;
use tokio::sync::Notify;
use tracing::{debug, info};

use crate::{
    discovery::peers::ControllerPeer,
    encoding::{self, Decode, Encode},
    sync::lock,
};

use super::{
    clock::Clock,
    controller::{gated_recv, DispatchGate},
    encoding::PayloadEntryHeader,
    ghostxform::GhostXForm,
    measurement::MeasurePeerEvent,
    micros,
    node::NodeId,
    timeline::{clamp_tempo, Timeline},
    Result,
};

pub const SESSION_MEMBERSHIP_HEADER_KEY: u32 = u32::from_be_bytes(*b"sess");
pub const SESSION_MEMBERSHIP_SIZE: u32 = mem::size_of::<SessionId>() as u32;
pub const SESSION_MEMBERSHIP_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: SESSION_MEMBERSHIP_HEADER_KEY,
    size: SESSION_MEMBERSHIP_SIZE,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Hash)]
pub struct SessionId(pub NodeId);

impl Encode for SessionId {
    fn encode_to(
        &self,
        out: &mut Vec<u8>,
    ) -> core::result::Result<(), crate::encoding::EncodeError> {
        self.0.encode_to(out)?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        self.0.encoded_size()
    }
}

impl Decode for SessionId {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (node_id, n) = NodeId::decode_from(bytes)?;
        Ok((SessionId(node_id), n))
    }
}

impl Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionMembership {
    pub session_id: SessionId,
}

impl Encode for SessionMembership {
    fn encode_to(
        &self,
        out: &mut Vec<u8>,
    ) -> core::result::Result<(), crate::encoding::EncodeError> {
        self.session_id.encode_to(out)?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        self.session_id.encoded_size()
    }
}

impl Decode for SessionMembership {
    fn decode_from(bytes: &[u8]) -> std::result::Result<(Self, usize), encoding::DecodeError> {
        let (session_id, n) = SessionId::decode_from(bytes)?;
        Ok((Self { session_id }, n))
    }
}

impl From<SessionId> for SessionMembership {
    fn from(session_id: SessionId) -> Self {
        SessionMembership { session_id }
    }
}

impl SessionMembership {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = SESSION_MEMBERSHIP_HEADER.encode()?;
        encoded.append(&mut encoding::encode_to_vec(&self.session_id)?);
        Ok(encoded)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionMeasurement {
    pub x_form: GhostXForm,
    pub timestamp: Duration,
}

impl Default for SessionMeasurement {
    fn default() -> Self {
        Self {
            x_form: GhostXForm::default(),
            timestamp: Duration::zero(),
        }
    }
}

/// Identity of one local session incarnation, captured when measurement starts.
/// Wire SessionId alone is not sufficient across resets or candidate replacement.
#[derive(Clone, Debug)]
pub struct SessionKey {
    pub session_id: SessionId,
    incarnation: Arc<()>,
}

impl SessionKey {
    /// A standalone incarnation. Managed requests use Session::key instead.
    /// Fresh identity for independently consumed results only. Session handlers
    /// require the key captured from `Session::key()` at request origin instead.
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            incarnation: Arc::new(()),
        }
    }
    pub(crate) fn matches(&self, session: &Session) -> bool {
        self.session_id == session.session_id
            && Arc::ptr_eq(&self.incarnation, &session.incarnation)
    }
}

impl Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.session_id.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub(crate) incarnation: Arc<()>,
    pub(crate) edit_revision: Option<NonZeroU64>,
    pub session_id: SessionId,
    pub timeline: Timeline,
    pub measurement: SessionMeasurement,
}

impl Session {
    pub(crate) fn invalidate_edit_origin(&mut self) {
        self.edit_revision = new_edit_revision();
    }

    pub(crate) fn set_timeline(&mut self, timeline: Timeline) {
        if self.timeline != timeline {
            self.invalidate_edit_origin();
            self.timeline = timeline;
        }
    }

    pub(crate) fn edit_origin(&self) -> Option<SessionEditOrigin> {
        Some(SessionEditOrigin {
            session_id: self.session_id,
            revision: self.edit_revision?,
            timeline: self.timeline,
            mapping: self.measurement.x_form,
        })
    }

    pub fn key(&self) -> SessionKey {
        SessionKey {
            session_id: self.session_id,
            incarnation: self.incarnation.clone(),
        }
    }
    /// A newly observed session incarnation. Cloning preserves that incarnation;
    /// reset_session always replaces it even when the wire SessionId is unchanged.
    pub fn new(session_id: SessionId, timeline: Timeline, measurement: SessionMeasurement) -> Self {
        Self {
            session_id,
            timeline,
            measurement,
            incarnation: Arc::new(()),
            edit_revision: new_edit_revision(),
        }
    }
}

/// Opaque capture-time provenance for a deferred client edit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionEditOrigin {
    session_id: SessionId,
    revision: NonZeroU64,
    timeline: Timeline,
    mapping: GhostXForm,
}

impl SessionEditOrigin {
    pub(crate) fn matches(&self, session: &Session) -> bool {
        self.session_id == session.session_id
            && Some(self.revision) == session.edit_revision
            && self.timeline == session.timeline
            && self.mapping == session.measurement.x_form
    }
}

fn allocate_edit_revision(counter: &AtomicU64) -> Option<NonZeroU64> {
    let mut value = counter.load(Ordering::Relaxed);
    loop {
        let next = value.checked_add(1)?;
        match counter.compare_exchange_weak(value, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(prior) => return NonZeroU64::new(prior),
            Err(current) => value = current,
        }
    }
}

#[cfg(test)]
#[test]
fn ordering_revision_exhaustion_fails_closed_without_reusing_a_token() {
    let counter = AtomicU64::new(u64::MAX - 1);
    let mut session = Session::new(
        SessionId::default(),
        Timeline::default(),
        SessionMeasurement::default(),
    );
    session.edit_revision = allocate_edit_revision(&counter);
    let origin = session.edit_origin().unwrap();
    assert_eq!(origin.revision.get(), u64::MAX - 1);
    session.edit_revision = allocate_edit_revision(&counter);
    assert!(!origin.matches(&session));
    assert!(session.edit_origin().is_none());
    assert!(allocate_edit_revision(&counter).is_none());
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

pub(crate) fn new_edit_revision() -> Option<NonZeroU64> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    allocate_edit_revision(&NEXT)
}

#[derive(Clone)]
pub struct Sessions {
    pub(crate) published: Arc<Mutex<SessionKey>>,
    pub other_sessions: Arc<Mutex<Vec<Session>>>,
    pub current: Arc<Mutex<Session>>,
    pub is_founding: Arc<Mutex<bool>>,
    pub tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    pub peers: Arc<Mutex<Vec<ControllerPeer>>>,
    pub clock: Clock,
    pub has_joined: Arc<Mutex<bool>>,
}

impl Sessions {
    pub fn new(
        init: Session,
        tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        clock: Clock,
        tx_join_session: tokio::sync::mpsc::Sender<Session>,
        notifier: Arc<Notify>,
        rx_measure_peer_result: tokio::sync::mpsc::Receiver<MeasurePeerEvent>,
    ) -> Self {
        let gate = Arc::new(DispatchGate::new_open());
        let (sessions, task) = Self::with_dispatch_gate(
            init,
            tx_measure_peer_state,
            peers,
            clock,
            tx_join_session,
            rx_measure_peer_result,
            gate,
        );
        tokio::spawn(async move {
            notifier.notified().await;
            task.abort();
        });
        sessions
    }

    pub(crate) fn with_dispatch_gate(
        init: Session,
        tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        clock: Clock,
        tx_join_session: tokio::sync::mpsc::Sender<Session>,
        mut rx_measure_peer_result: tokio::sync::mpsc::Receiver<MeasurePeerEvent>,
        gate: Arc<DispatchGate>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let other_sessions = Arc::new(Mutex::new(vec![init.clone()]));
        let published = Arc::new(Mutex::new(init.key()));
        let current = Arc::new(Mutex::new(init));

        let other_sessions_loop = other_sessions.clone();
        let current_loop = current.clone();
        let tx_join_session_loop = tx_join_session.clone();
        let peers_loop = peers.clone();
        let tx_measure_peer_state_loop = tx_measure_peer_state.clone();

        let mut open = gate.subscribe();
        let task = tokio::spawn(async move {
            let mut remeasurements = tokio::task::JoinSet::new();
            let mut retries = std::collections::HashMap::<usize, tokio::task::AbortHandle>::new();
            while let Some((_permit, epoch, event)) =
                gated_recv(&gate, &mut open, &mut rx_measure_peer_result).await
            {
                while remeasurements.try_join_next().is_some() {}
                retries.retain(|_, task| !task.is_finished());
                let (key, x_form, cancelled_peer) = match event {
                    MeasurePeerEvent::XForm(key, x_form) => (key, x_form, None),
                    MeasurePeerEvent::Cancelled(key, peer) => {
                        (key, GhostXForm::default(), Some(peer))
                    }
                    MeasurePeerEvent::PeerState(..) => continue,
                };
                // A handler may be blocked sending to another gated consumer.
                // Cancel that await on disable instead of holding stop() up.
                let handle = async {
                    if cancelled_peer.is_some() {
                        retry_state(|| {
                            let current = try_state(&current_loop)?;
                            if key.matches(&current) {
                                Some(Some(current.clone()))
                            } else {
                                Some(
                                    try_state(&other_sessions_loop)?
                                        .iter()
                                        .find(|session| key.matches(session))
                                        .cloned(),
                                )
                            }
                        })
                        .await
                    } else if x_form == GhostXForm::default() {
                        handle_failed_measurement_inner(
                            key.clone(),
                            other_sessions_loop.clone(),
                            current_loop.clone(),
                            peers_loop.clone(),
                        )
                        .await
                    } else {
                        handle_successful_measurement_inner(
                            key.clone(),
                            x_form,
                            other_sessions_loop.clone(),
                            current_loop.clone(),
                            clock,
                            tx_join_session_loop.clone(),
                        )
                        .await
                    }
                };
                tokio::select! {
                    biased;
                    _ = open.closed() => {}
                    session = handle => {
                        if let Some(session) = session {
                            let identity = Arc::as_ptr(&session.incarnation) as usize;
                            if let Some(old) = retries.remove(&identity) { old.abort(); }
                            let gate = gate.clone();
                            let peers = peers_loop.clone();
                            let sender = tx_measure_peer_state_loop.clone();
                            let current = current_loop.clone();
                            let others = other_sessions_loop.clone();
                            let delay = if cancelled_peer.is_some() { std::time::Duration::from_millis(100) } else { std::time::Duration::from_secs(30) };
                            retries.insert(identity, remeasurements.spawn(async move {
                                gate.run_in_epoch(epoch, async move {
                                    tokio::time::sleep(delay).await;
                                    loop {
                                        match admit_owned_request(&sender, &session.key(), &current, &others, &peers, cancelled_peer).await {
                                            Admission::Absent => {},
                                            Admission::Sent | Admission::Retired => break,
                                        }
                                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    }
                                }).await;
                            }));
                        }
                    }
                }
            }
            debug!("measure peer event channel closed");
        });

        (
            Self {
                published,
                other_sessions,
                current,
                tx_measure_peer_state,
                peers,
                clock,
                is_founding: Arc::new(Mutex::new(false)),
                has_joined: Arc::new(Mutex::new(false)),
            },
            task,
        )
    }

    pub fn reset_session(&mut self, mut session: Session) {
        session.incarnation = Arc::new(());
        session.set_timeline(clamp_tempo(session.timeline));
        session.invalidate_edit_origin();
        let mut current = lock(&self.current);
        let mut others = lock(&self.other_sessions);
        *current = session;
        others.clear();
    }

    /// Install a ghost-domain edit only for its captured incarnation and mapping.
    pub fn reset_timeline(
        &self,
        origin: &SessionKey,
        mapping: GhostXForm,
        timeline: Timeline,
    ) -> bool {
        let timeline = clamp_tempo(timeline);
        let mut current = lock(&self.current);
        let mut others = lock(&self.other_sessions);
        if !origin.matches(&current) || current.measurement.x_form != mapping {
            return false;
        }
        current.set_timeline(timeline);
        for session in others.iter_mut().filter(|session| origin.matches(session)) {
            session.set_timeline(timeline);
        }
        true
    }

    pub async fn saw_session_timeline(
        &self,
        session_id: SessionId,
        timeline: Timeline,
    ) -> Timeline {
        debug!(
            "saw session timeline {:?} for session {}",
            timeline, session_id,
        );

        let mut slot = None;
        match retry_state(|| self.try_sighting(session_id, timeline, &mut slot)).await {
            Sighting::Complete(timeline) => return timeline,
            Sighting::NeedsCapacity => {}
        }
        // No candidate exists yet. Cancellation while capacity is unavailable
        // leaves neither an incarnation nor an orphaned measurement obligation.
        if let Ok(permit) = self.tx_measure_peer_state.reserve().await {
            slot = Some(permit);
            if let Sighting::Complete(timeline) =
                retry_state(|| self.try_sighting(session_id, timeline, &mut slot)).await
            {
                return timeline;
            }
        }
        retry_state(|| Some(try_state(&self.current)?.timeline)).await
    }

    fn try_sighting(
        &self,
        session_id: SessionId,
        timeline: Timeline,
        slot: &mut Option<tokio::sync::mpsc::Permit<'_, MeasurePeerEvent>>,
    ) -> Option<Sighting> {
        let timeline = clamp_tempo(timeline);
        let mut current = try_state(&self.current)?;
        let mut others = try_state(&self.other_sessions)?;
        let mut joined = try_state(&self.has_joined)?;
        if current.session_id == session_id {
            *current = self.update_timeline(current.clone(), timeline);
            *joined = true;
        } else if let Some(session) = others
            .iter_mut()
            .find(|session| session.session_id == session_id)
        {
            *session = self.update_timeline(session.clone(), timeline);
        } else if slot.is_none() {
            return Some(Sighting::NeedsCapacity);
        } else {
            let peers = try_state(&self.peers)?;
            if let Some(peer) = measurement_peer(&peers, session_id, None) {
                let session = Session::new(session_id, timeline, SessionMeasurement::default());
                let request = MeasurePeerEvent::PeerState(session.key(), peer.peer_state.clone());
                others.push(session);
                slot.take().unwrap().send(request);
            }
        }
        Some(Sighting::Complete(current.timeline))
    }

    pub fn update_timeline(&self, mut session: Session, timeline: Timeline) -> Session {
        let timeline = clamp_tempo(timeline);
        if timeline.beat_origin > session.timeline.beat_origin {
            info!(
                "[adopting] updating peer timeline for session {} (bpm: {}, beat origin: {}, time: origin: {})",
                session.session_id,
                timeline.tempo.bpm().round(),
                timeline.beat_origin.floating(),
                timeline.time_origin,
            );
            session.set_timeline(timeline);
        } else {
            debug!(
                "[rejecting] updating peer timeline with beat origin: {}. current timeline beat origin: {}",
                timeline.beat_origin.floating(),
                session.timeline.beat_origin.floating()
            );
        }

        session
    }
}

enum Sighting {
    NeedsCapacity,
    Complete(Timeline),
}
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Sent,
    Absent,
    Retired,
}

async fn reserve_for_key<'a, T>(
    sender: &'a tokio::sync::mpsc::Sender<T>,
    key: &SessionKey,
    current: &Mutex<Session>,
    others: &Mutex<Vec<Session>>,
    current_only: bool,
) -> Option<tokio::sync::mpsc::Permit<'a, T>> {
    let retired = async {
        loop {
            let alive = retry_state(|| {
                let current = try_state(current)?;
                let others = try_state(others)?;
                Some(
                    key.matches(&current)
                        || (!current_only && others.iter().any(|session| key.matches(session))),
                )
            })
            .await;
            if !alive {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::select! { biased; _ = retired => None, permit = sender.reserve() => permit.ok() }
}

async fn admit_owned_request(
    sender: &tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    key: &SessionKey,
    current: &Mutex<Session>,
    others: &Mutex<Vec<Session>>,
    peers: &Mutex<Vec<ControllerPeer>>,
    after: Option<NodeId>,
) -> Admission {
    let Some(slot) = reserve_for_key(sender, key, current, others, false).await else {
        return Admission::Retired;
    };
    let mut slot = Some(slot);
    retry_state(|| {
        let current = try_state(current)?;
        let mut others = try_state(others)?;
        let peers = try_state(peers)?;
        let is_current = key.matches(&current);
        if !is_current && !others.iter().any(|session| key.matches(session)) {
            return Some(Admission::Retired);
        }
        if let Some(peer) = measurement_peer(&peers, key.session_id, after) {
            slot.take().unwrap().send(MeasurePeerEvent::PeerState(
                key.clone(),
                peer.peer_state.clone(),
            ));
            Some(Admission::Sent)
        } else if is_current {
            Some(Admission::Absent)
        } else {
            others.retain(|session| !key.matches(session));
            Some(Admission::Retired)
        }
    })
    .await
}

#[cfg(test)]
async fn owned_measurement_request(
    key: &SessionKey,
    current: &Mutex<Session>,
    others: &Mutex<Vec<Session>>,
    peers: &Mutex<Vec<ControllerPeer>>,
) -> Option<MeasurePeerEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let _ = admit_owned_request(&tx, key, current, others, peers, None).await;
    rx.try_recv().ok()
}

fn measurement_peer(
    peers: &[ControllerPeer],
    session: SessionId,
    after: Option<NodeId>,
) -> Option<&ControllerPeer> {
    let mut eligible = peers
        .iter()
        .filter(|peer| peer.peer_state.session_id() == session)
        .collect::<Vec<_>>();
    if eligible
        .iter()
        .any(|peer| peer.peer_state.measurement_endpoint.is_some())
    {
        eligible.retain(|peer| peer.peer_state.measurement_endpoint.is_some());
    }
    eligible.sort_by_key(|peer| {
        (
            peer.peer_state.ident() != session.0,
            peer.peer_state.ident(),
        )
    });
    let index = after
        .and_then(|previous| {
            eligible
                .iter()
                .position(|peer| peer.peer_state.ident() == previous)
        })
        .map_or(0, |index| (index + 1) % eligible.len());
    eligible.get(index).copied()
}

pub async fn launch_session_measurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
) {
    let Ok(slot) = tx_measure_peer_state.reserve().await else {
        return;
    };
    let mut slot = Some(slot);
    retry_state(|| {
        let peers = try_state(&peers)?;
        if let Some(peer) = measurement_peer(&peers, session.session_id, None) {
            slot.take().unwrap().send(MeasurePeerEvent::PeerState(
                session.key(),
                peer.peer_state.clone(),
            ));
        }
        Some(())
    })
    .await;
}

/// One independently owned retry. Dropping it aborts pending delay, peer wait,
/// or capacity wait; an already committed request is not retracted.
#[must_use = "retain the retry owner until completion, or drop it to cancel"]
pub struct RemeasurementTask(tokio::task::JoinHandle<()>);
impl RemeasurementTask {
    pub fn abort(&self) {
        self.0.abort();
    }
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}
impl core::future::Future for RemeasurementTask {
    type Output = std::result::Result<(), tokio::task::JoinError>;
    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        core::pin::Pin::new(&mut self.0).poll(cx)
    }
}
impl Drop for RemeasurementTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn handle_successful_measurement(
    key: SessionKey,
    x_form: GhostXForm,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    clock: Clock,
    tx_join_session: tokio::sync::mpsc::Sender<Session>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
) -> Option<RemeasurementTask> {
    handle_successful_measurement_inner(
        key,
        x_form,
        other_sessions,
        current.clone(),
        clock,
        tx_join_session,
    )
    .await
    .map(|session| {
        RemeasurementTask(tokio::spawn(remeasurement_once(
            peers,
            tx_measure_peer_state,
            session,
            Some(current),
            std::time::Duration::from_secs(30),
        )))
    })
}

async fn handle_successful_measurement_inner(
    key: SessionKey,
    x_form: GhostXForm,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    clock: Clock,
    tx_join_session: tokio::sync::mpsc::Sender<Session>,
) -> Option<Session> {
    let session_id = key.session_id;
    info!(
        "session {} measurement completed with result ({}, {})",
        session_id,
        x_form.slope,
        micros(x_form.intercept),
    );

    let measurement = SessionMeasurement {
        x_form,
        timestamp: clock.micros(),
    };

    // Decide under a consistent snapshot; do not mutate a session that requires
    // a join notification until capacity is reserved. Cancellation while waiting
    // must not commit an undelivered session transition.
    let was_current = retry_state(|| {
        let current = try_state(&current)?;
        let mut others = try_state(&other_sessions)?;
        Some((|| {
            if key.matches(&current) {
                return Some(true);
            }
            let candidate = others.iter_mut().find(|candidate| key.matches(candidate))?;
            if candidate.measurement.timestamp > measurement.timestamp {
                return None;
            }
            if !measurement_wins(&current, session_id, measurement, clock.micros()) {
                candidate.measurement = measurement;
                return None;
            }
            Some(false)
        })())
    })
    .await?;
    let slot = reserve_for_key(
        &tx_join_session,
        &key,
        &current,
        &other_sessions,
        was_current,
    )
    .await?;
    let mut slot = Some(slot);
    retry_state(|| {
        let mut current = try_state(&current)?;
        let mut others = try_state(&other_sessions)?;
        Some((|| {
            if was_current && !key.matches(&current) {
                return None;
            }
            if key.matches(&current) {
                if current.measurement.timestamp > measurement.timestamp {
                    return None;
                }
                if current.measurement.x_form != measurement.x_form {
                    current.invalidate_edit_origin();
                }
                current.measurement = measurement;
                slot.take().unwrap().send(current.clone());
                return Some(current.clone());
            }
            let candidate = others.iter_mut().find(|candidate| key.matches(candidate))?;
            if candidate.measurement.timestamp > measurement.timestamp {
                return None;
            }
            candidate.measurement = measurement;
            if measurement_wins(&current, session_id, measurement, clock.micros()) {
                let mut winner = candidate.clone();
                winner.invalidate_edit_origin();
                *candidate = std::mem::replace(&mut *current, winner.clone());
                slot.take().unwrap().send(winner.clone());
                return Some(winner);
            }
            None
        })())
    })
    .await
}

fn measurement_wins(
    current: &Session,
    candidate: SessionId,
    measurement: SessionMeasurement,
    host_time: Duration,
) -> bool {
    const SESSION_EPS_NANOS: i128 = 500_000_000;
    let diff = super::duration_nanos(measurement.x_form.host_to_ghost(host_time))
        - super::duration_nanos(current.measurement.x_form.host_to_ghost(host_time));
    diff > SESSION_EPS_NANOS || (diff.abs() < SESSION_EPS_NANOS && candidate < current.session_id)
}

pub async fn handle_failed_measurement(
    key: SessionKey,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
) -> Option<RemeasurementTask> {
    handle_failed_measurement_inner(key, other_sessions, current.clone(), peers.clone())
        .await
        .map(|session| {
            RemeasurementTask(tokio::spawn(remeasurement_once(
                peers,
                tx_measure_peer,
                session,
                Some(current),
                std::time::Duration::from_secs(30),
            )))
        })
}

async fn handle_failed_measurement_inner(
    key: SessionKey,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
) -> Option<Session> {
    retry_state(|| {
        let current = try_state(&current)?;
        if key.matches(&current) {
            return Some(Some(current.clone()));
        }
        let mut others = try_state(&other_sessions)?;
        if others.iter().any(|session| key.matches(session)) {
            let mut peers = try_state(&peers)?;
            others.retain(|session| !key.matches(session));
            // A distinct incarnation with the same wire ID still owns its peers.
            if current.session_id != key.session_id
                && !others
                    .iter()
                    .any(|session| session.session_id == key.session_id)
            {
                peers.retain(|peer| peer.peer_state.session_id() != key.session_id);
            }
        }
        Some(None)
    })
    .await
}

/// Schedule one request after 30 seconds. Retain the returned owner while the
/// independent session is alive; dropping it cancels without detaching a retry.
pub async fn schedule_remeasurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
) -> RemeasurementTask {
    RemeasurementTask(tokio::spawn(remeasurement_once(
        peers,
        tx_measure_peer,
        session,
        None,
        std::time::Duration::from_secs(30),
    )))
}

async fn remeasurement_once(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session: Session,
    current: Option<Arc<Mutex<Session>>>,
    delay: std::time::Duration,
) {
    let retired = async {
        let Some(current) = &current else {
            return std::future::pending::<()>().await;
        };
        loop {
            if !retry_state(|| Some(session.key().matches(&*try_state(current)?))).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    let once = async {
        tokio::time::sleep(delay).await;
        loop {
            // Capacity is an asynchronous channel wait, never a mutex retry.
            let Ok(slot) = tx_measure_peer.reserve().await else {
                return;
            };
            let mut slot = Some(slot);
            let outcome = retry_state(|| {
                let guard = match &current {
                    Some(current) => Some(try_state(current)?),
                    None => None,
                };
                if guard
                    .as_ref()
                    .is_some_and(|current| !session.key().matches(current))
                {
                    return Some(Admission::Retired);
                }
                let peers = try_state(&peers)?;
                if let Some(peer) = measurement_peer(&peers, session.session_id, None) {
                    slot.take().unwrap().send(MeasurePeerEvent::PeerState(
                        session.key(),
                        peer.peer_state.clone(),
                    ));
                    Some(Admission::Sent)
                } else {
                    Some(Admission::Absent)
                }
            })
            .await;
            drop(slot);
            if outcome != Admission::Absent {
                return;
            }
            // Peer absence is a bounded-rate lifecycle wait, not 5ms lock contention.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::select! { biased; _ = tx_measure_peer.closed() => {}, _ = retired => {}, _ = once => {} }
}

pub fn session_peers(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_id: SessionId,
) -> Vec<ControllerPeer> {
    let mut peers = lock(&peers)
        .iter()
        .filter(|p| p.peer_state.session_id() == session_id)
        .cloned()
        .collect::<Vec<_>>();
    peers.sort_by_key(|a| a.peer_state.ident());

    peers
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn numeric_cleanup_successful_measurement_handles_extreme_clock_difference() {
        let io = crate::platform::io_context::IoContext::new().unwrap();
        io.spawn(async {
            let clock = Clock::new();
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            let extreme = GhostXForm {
                slope: -1e30,
                intercept: Duration::zero(),
            };
            assert_eq!(extreme.host_to_ghost(clock.micros()), Duration::MIN);
            for extreme_current in [false, true] {
                let mut initial = admission_session(1, 0);
                if extreme_current {
                    initial.measurement.x_form = extreme;
                }
                let origin = initial.key();
                let candidate = admission_session(2, 0);
                let candidate_key = candidate.key();
                let current = Arc::new(Mutex::new(initial));
                let others = Arc::new(Mutex::new(vec![candidate]));
                let peers = Arc::new(Mutex::new(vec![]));
                let (joins, mut joined) = tokio::sync::mpsc::channel(1);
                let (requests, _requested) = tokio::sync::mpsc::channel(1);
                assert!(handle_successful_measurement(
                    candidate_key.clone(),
                    extreme,
                    others.clone(),
                    current.clone(),
                    clock,
                    joins.clone(),
                    peers.clone(),
                    requests.clone()
                )
                .await
                .is_none());
                assert!(origin.matches(&current.lock().unwrap()));
                assert_eq!(others.lock().unwrap()[0].measurement.x_form, extreme);
                assert!(joined.try_recv().is_err());
                let ordinary = GhostXForm {
                    slope: 1.0,
                    intercept: Duration::seconds(10),
                };
                let retry = handle_successful_measurement(
                    candidate_key.clone(),
                    ordinary,
                    others,
                    current.clone(),
                    clock,
                    joins,
                    peers,
                    requests,
                )
                .await
                .unwrap();
                let winner = joined.recv().await.unwrap();
                assert!(candidate_key.matches(&winner));
                assert!(candidate_key.matches(&current.lock().unwrap()));
                assert_eq!(winner.measurement.x_form, ordinary);
                drop(retry);
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn admission_standalone_absence_and_capacity_do_not_spin_as_mutex_contention() {
        use std::{
            future::Future,
            sync::atomic::{AtomicUsize, Ordering},
            task::{Context, Wake, Waker},
        };
        struct CountWake {
            count: Arc<AtomicUsize>,
            inner: Waker,
        }
        impl Wake for CountWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.count.fetch_add(1, Ordering::Relaxed);
                self.inner.wake_by_ref();
            }
        }
        for full in [false, true] {
            let session = admission_session(1, 0);
            let peer = admission_peer(session.session_id, 1);
            let peers = Arc::new(Mutex::new(if full { vec![peer.clone()] } else { vec![] }));
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            if full {
                tx.send(MeasurePeerEvent::PeerState(
                    session.key(),
                    peer.peer_state.clone(),
                ))
                .await
                .unwrap();
            }
            let wakeups = Arc::new(AtomicUsize::new(0));
            let count = wakeups.clone();
            let mut once = Box::pin(remeasurement_once(
                peers.clone(),
                tx,
                session.clone(),
                None,
                std::time::Duration::ZERO,
            ));
            let task = RemeasurementTask(tokio::spawn(std::future::poll_fn(move |cx| {
                let waker = Waker::from(Arc::new(CountWake {
                    count: count.clone(),
                    inner: cx.waker().clone(),
                }));
                once.as_mut().poll(&mut Context::from_waker(&waker))
            })));
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let before = wakeups.load(Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                wakeups.load(Ordering::Relaxed) - before <= 3,
                "idle peer/capacity waits must not rescan every5ms"
            );
            assert!(!task.is_finished());
            if full {
                rx.recv().await.unwrap();
            } else {
                peers.lock().unwrap().push(peer);
            }
            let MeasurePeerEvent::PeerState(key, _) =
                tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("request")
            };
            assert!(key.matches(&session));
            task.await.unwrap();
            assert!(
                rx.recv().await.is_none(),
                "one owner schedules exactly one request"
            );
        }
    }

    fn admission_session(id: u8, offset: i64) -> Session {
        Session::new(
            SessionId(NodeId::from_array([id; 8])),
            Timeline::default(),
            SessionMeasurement {
                x_form: GhostXForm {
                    slope: 1.0,
                    intercept: Duration::seconds(offset),
                },
                timestamp: Duration::zero(),
            },
        )
    }
    fn admission_peer(session: SessionId, node: u8) -> ControllerPeer {
        ControllerPeer {
            peer_state: crate::discovery::peers::PeerState {
                node_state: crate::link::node::NodeState {
                    node_id: NodeId::from_array([node; 8]),
                    session_id: session,
                    ..Default::default()
                },
                measurement_endpoint: Some("127.0.0.1:12345".parse().unwrap()),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn admission_low_level_timeline_edit_rejects_replaced_origin_and_mapping() {
        let init = admission_session(1, 0);
        let origin = init.key();
        let (requests, _rx) = tokio::sync::mpsc::channel(1);
        let (joins, _joined) = tokio::sync::mpsc::channel(1);
        let (_results, results) = tokio::sync::mpsc::channel(1);
        let (mut sessions, task) = Sessions::with_dispatch_gate(
            init.clone(),
            requests,
            Arc::new(Mutex::new(vec![])),
            Clock::new(),
            joins,
            results,
            Arc::new(DispatchGate::new_open()),
        );
        let edited = Timeline {
            tempo: crate::link::tempo::Tempo::new(150.0),
            ..init.timeline
        };
        assert!(!sessions.reset_timeline(&origin, GhostXForm::default(), edited));
        assert_eq!(sessions.current.lock().unwrap().timeline, init.timeline);
        sessions.reset_session(init.clone());
        assert!(!sessions.reset_timeline(&origin, init.measurement.x_form, edited));
        let current = sessions.current.lock().unwrap().clone();
        let distinct = admission_session(1, 0);
        *sessions.other_sessions.lock().unwrap() =
            vec![current.clone(), current.clone(), distinct.clone()];
        assert!(sessions.reset_timeline(&current.key(), current.measurement.x_form, edited));
        assert_eq!(sessions.current.lock().unwrap().timeline, edited);
        for session in sessions.other_sessions.lock().unwrap().iter() {
            assert_eq!(
                session.timeline,
                if current.key().matches(session) {
                    edited
                } else {
                    distinct.timeline
                }
            );
        }
        task.abort();
    }

    #[tokio::test]
    async fn admission_unknown_candidate_is_not_installed_before_capacity_or_after_departure() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let initial = admission_session(1, 0);
        let candidate = admission_session(2, 3);
        let peers = Arc::new(Mutex::new(vec![admission_peer(candidate.session_id, 2)]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(MeasurePeerEvent::PeerState(
            initial.key(),
            admission_peer(initial.session_id, 1).peer_state,
        ))
        .await
        .unwrap();
        let (joins, _joined) = tokio::sync::mpsc::channel(1);
        let (_results, result_rx) = tokio::sync::mpsc::channel(1);
        let (sessions, task) = Sessions::with_dispatch_gate(
            initial.clone(),
            tx,
            peers.clone(),
            Clock::new(),
            joins,
            result_rx,
            Arc::new(DispatchGate::new_open()),
        );
        let mut sighting =
            Box::pin(sessions.saw_session_timeline(candidate.session_id, candidate.timeline));
        assert!(sighting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert!(
            !sessions
                .other_sessions
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.session_id == candidate.session_id),
            "capacity wait must not install a candidate"
        );
        drop(sighting);
        let mut sighting =
            Box::pin(sessions.saw_session_timeline(candidate.session_id, candidate.timeline));
        assert!(sighting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        peers.lock().unwrap().clear();
        rx.recv().await.unwrap();
        sighting.await;
        assert!(rx.try_recv().is_err());
        assert!(sessions
            .other_sessions
            .lock()
            .unwrap()
            .iter()
            .all(|s| initial.key().matches(s)));
        task.abort();
    }

    #[tokio::test]
    async fn admission_capacity_wait_rechecks_origin_and_latest_peer() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let session = admission_session(2, 1);
        let current = Mutex::new(admission_session(1, 0));
        let others = Mutex::new(vec![session.clone()]);
        let peers = Arc::new(Mutex::new(vec![admission_peer(session.session_id, 2)]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let filler = || {
            MeasurePeerEvent::PeerState(
                session.key(),
                admission_peer(session.session_id, 2).peer_state,
            )
        };
        tx.send(filler()).await.unwrap();
        let key = session.key();
        let mut request = Box::pin(admit_owned_request(
            &tx, &key, &current, &others, &peers, None,
        ));
        assert!(request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        others.lock().unwrap().clear();
        rx.recv().await.unwrap();
        assert_eq!(request.await, Admission::Retired);
        assert!(
            rx.try_recv().is_err(),
            "retired origin must never enter the request channel"
        );
        tx.send(filler()).await.unwrap();
        let mut launch = Box::pin(launch_session_measurement(
            peers.clone(),
            tx.clone(),
            session.clone(),
        ));
        assert!(launch
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        *peers.lock().unwrap() = vec![admission_peer(session.session_id, 8)];
        rx.recv().await.unwrap();
        launch.await;
        let MeasurePeerEvent::PeerState(key, peer) = rx.recv().await.unwrap() else {
            panic!("request")
        };
        assert!(key.matches(&session));
        assert_eq!(peer.ident(), NodeId::from_array([8; 8]));
    }

    #[tokio::test]
    async fn admission_concurrent_public_candidates_compare_with_latest_current() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let a = admission_session(1, 0);
        let b = admission_session(2, 2);
        let c = admission_session(3, 4);
        let current = Arc::new(Mutex::new(a.clone()));
        let others = Arc::new(Mutex::new(vec![b.clone(), c.clone()]));
        let peers = Arc::new(Mutex::new(vec![
            admission_peer(b.session_id, 2),
            admission_peer(c.session_id, 3),
        ]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(a).await.unwrap();
        let (requests, _request_rx) = tokio::sync::mpsc::channel(1);
        let mut first = Box::pin(handle_successful_measurement(
            b.key(),
            b.measurement.x_form,
            others.clone(),
            current.clone(),
            Clock::new(),
            tx.clone(),
            peers.clone(),
            requests.clone(),
        ));
        let mut second = Box::pin(handle_successful_measurement(
            c.key(),
            c.measurement.x_form,
            others.clone(),
            current.clone(),
            Clock::new(),
            tx,
            peers,
            requests,
        ));
        let mut context = Context::from_waker(Waker::noop());
        assert!(first.as_mut().poll(&mut context).is_pending());
        assert!(second.as_mut().poll(&mut context).is_pending());
        rx.recv().await.unwrap();
        let _first_owner = first.await;
        assert!(b.key().matches(&rx.recv().await.unwrap()));
        let _second_owner = second.await;
        assert!(
            c.key().matches(&current.lock().unwrap()),
            "C remains valid after B replaces incidental current A"
        );
        assert!(c.key().matches(&rx.recv().await.unwrap()));
    }

    #[tokio::test]
    async fn admission_same_wire_candidates_keep_exact_keys_and_retire_old_current_results() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let a = admission_session(1, 0);
        let decoy = admission_session(2, 0);
        let target = admission_session(2, 3);
        let current = Arc::new(Mutex::new(a.clone()));
        let others = Arc::new(Mutex::new(vec![decoy.clone(), target.clone()]));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        assert!(handle_successful_measurement_inner(
            target.key(),
            target.measurement.x_form,
            others.clone(),
            current.clone(),
            Clock::new(),
            tx.clone()
        )
        .await
        .is_some());
        assert!(target.key().matches(&rx.recv().await.unwrap()));
        assert!(target.key().matches(&current.lock().unwrap()));
        assert!(others
            .lock()
            .unwrap()
            .iter()
            .any(|s| decoy.key().matches(s) && s.measurement.timestamp == Duration::zero()));
        tx.send(target.clone()).await.unwrap();
        let mut old = Box::pin(handle_successful_measurement_inner(
            target.key(),
            target.measurement.x_form,
            others.clone(),
            current.clone(),
            Clock::new(),
            tx,
        ));
        assert!(old
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        *current.lock().unwrap() = a.clone();
        others.lock().unwrap().push(target);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(1), old)
            .await
            .unwrap()
            .is_none());
        assert!(a.key().matches(&current.lock().unwrap()));
    }

    #[tokio::test]
    async fn admission_cancelled_route_rotates_every_eligible_peer() {
        let init = admission_session(1, 0);
        let peers = Arc::new(Mutex::new(
            (1..=3)
                .map(|n| admission_peer(init.session_id, n))
                .collect(),
        ));
        let (requests, mut rx) = tokio::sync::mpsc::channel(1);
        let (results, result_rx) = tokio::sync::mpsc::channel(1);
        let (joins, _joined) = tokio::sync::mpsc::channel(1);
        let (_sessions, task) = Sessions::with_dispatch_gate(
            init.clone(),
            requests,
            peers,
            Clock::new(),
            joins,
            result_rx,
            Arc::new(DispatchGate::new_open()),
        );
        for (failed, expected) in [(1, 2), (2, 3), (3, 1)] {
            results
                .send(MeasurePeerEvent::Cancelled(
                    init.key(),
                    NodeId::from_array([failed; 8]),
                ))
                .await
                .unwrap();
            let MeasurePeerEvent::PeerState(key, peer) =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("request")
            };
            assert!(key.matches(&init));
            assert_eq!(peer.ident(), NodeId::from_array([expected; 8]));
        }
        task.abort();
    }

    #[tokio::test]
    async fn admission_public_retry_owners_release_state_and_sender_on_drop() {
        for success in [false, true] {
            let init = admission_session(1, 0);
            let key = init.key();
            let current = Arc::new(Mutex::new(init));
            let others = Arc::new(Mutex::new(vec![]));
            let peers = Arc::new(Mutex::new(vec![]));
            let weak = Arc::downgrade(&peers);
            let (requests, mut rx) = tokio::sync::mpsc::channel(1);
            let (joins, _joined) = tokio::sync::mpsc::channel(1);
            let owner = if success {
                handle_successful_measurement(
                    key,
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::zero(),
                    },
                    others,
                    current,
                    Clock::new(),
                    joins,
                    peers,
                    requests,
                )
                .await
            } else {
                handle_failed_measurement(key, others, current, peers, requests).await
            };
            assert!(owner.is_some());
            drop(owner);
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                weak.upgrade().is_none(),
                "an ended independent owner must not retain empty peer state"
            );
        }
    }

    #[tokio::test]
    async fn owned_io_selection_prefers_measurable_members_and_then_leader() {
        use crate::discovery::peers::PeerState;
        let id = SessionId(NodeId::from_array([1; 8]));
        let session = Session::new(id, Timeline::default(), SessionMeasurement::default());
        let current = Mutex::new(Session::new(
            SessionId(NodeId::from_array([9; 8])),
            Timeline::default(),
            SessionMeasurement::default(),
        ));
        let others = Mutex::new(vec![session.clone()]);
        for (leader_endpoint, member_endpoint, expected) in
            [(false, true, 2), (true, true, 1), (false, false, 1)]
        {
            let peers = Arc::new(Mutex::new(
                [1u8, 2]
                    .into_iter()
                    .map(|node| ControllerPeer {
                        peer_state: PeerState {
                            node_state: crate::link::node::NodeState {
                                node_id: NodeId::from_array([node; 8]),
                                session_id: id,
                                ..Default::default()
                            },
                            measurement_endpoint: (if node == 1 {
                                leader_endpoint
                            } else {
                                member_endpoint
                            })
                            .then(|| "127.0.0.1:12345".parse().unwrap()),
                            ..Default::default()
                        },
                    })
                    .collect(),
            ));
            let request = owned_measurement_request(&session.key(), &current, &others, &peers)
                .await
                .unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            launch_session_measurement(peers, tx, session.clone()).await;
            for request in [request, rx.recv().await.unwrap()] {
                let MeasurePeerEvent::PeerState(key, peer) = request else {
                    panic!("request")
                };
                assert!(key.matches(&session));
                assert_eq!(peer.ident(), NodeId::from_array([expected; 8]));
                assert_eq!(
                    peer.measurement_endpoint.is_some(),
                    leader_endpoint || member_endpoint
                );
            }
        }
    }

    #[tokio::test]
    async fn owned_io_initial_switch_failure_reannouncement_has_fresh_origin() {
        use crate::discovery::peers::PeerState;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let a = Session::new(
                SessionId(NodeId::from_array([2; 8])),
                Timeline::default(),
                SessionMeasurement::default(),
            );
            let b_id = SessionId(NodeId::from_array([1; 8]));
            let peer_a = ControllerPeer {
                peer_state: PeerState {
                    node_state: crate::link::node::NodeState::new(a.session_id),
                    measurement_endpoint: Some("127.0.0.1:12345".parse().unwrap()),
                    ..Default::default()
                },
            };
            let peer_b = ControllerPeer {
                peer_state: PeerState {
                    node_state: crate::link::node::NodeState::new(b_id),
                    measurement_endpoint: Some("127.0.0.1:12346".parse().unwrap()),
                    ..Default::default()
                },
            };
            let peers = Arc::new(Mutex::new(vec![peer_a.clone(), peer_b]));
            let (tx, mut requests) = tokio::sync::mpsc::channel(4);
            let (tx_results, results) = tokio::sync::mpsc::channel(4);
            let (tx_joins, mut joins) = tokio::sync::mpsc::channel(4);
            let gate = Arc::new(DispatchGate::new_open());
            let (sessions, task) = Sessions::with_dispatch_gate(
                a.clone(),
                tx,
                peers.clone(),
                Clock::new(),
                tx_joins.clone(),
                results,
                gate.clone(),
            );
            sessions
                .saw_session_timeline(b_id, Timeline::default())
                .await;
            let MeasurePeerEvent::PeerState(b, _) = requests.recv().await.unwrap() else {
                panic!("B request")
            };
            tx_results
                .send(MeasurePeerEvent::XForm(
                    b,
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(2),
                    },
                ))
                .await
                .unwrap();
            assert_eq!(joins.recv().await.unwrap().session_id, b_id);
            assert_eq!(
                lock(&sessions.other_sessions)
                    .iter()
                    .filter(|s| a.key().matches(s))
                    .count(),
                2,
                "exercise initial clone plus switched-out copy"
            );
            handle_failed_measurement_inner(
                a.key(),
                sessions.other_sessions.clone(),
                sessions.current.clone(),
                peers.clone(),
            )
            .await;
            assert!(
                !lock(&sessions.other_sessions)
                    .iter()
                    .any(|s| a.key().matches(s)),
                "all copies of failed origin must be retired"
            );
            assert!(!lock(&peers)
                .iter()
                .any(|p| p.peer_state.session_id() == a.session_id));
            assert!(handle_successful_measurement_inner(
                a.key(),
                GhostXForm {
                    slope: 1.0,
                    intercept: Duration::seconds(20)
                },
                sessions.other_sessions.clone(),
                sessions.current.clone(),
                Clock::new(),
                tx_joins
            )
            .await
            .is_none());
            assert!(joins.try_recv().is_err());
            lock(&peers).push(peer_a);
            sessions
                .saw_session_timeline(a.session_id, Timeline::default())
                .await;
            let MeasurePeerEvent::PeerState(fresh, _) = requests.recv().await.unwrap() else {
                panic!("reannouncement request")
            };
            assert!(!fresh.matches(&a));
            assert!(lock(&sessions.other_sessions)
                .iter()
                .any(|s| fresh.matches(s)));
            gate.stop().await;
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn owned_io_failed_origin_preserves_other_incarnations_and_their_peers() {
        let id = SessionId(NodeId::from_array([2; 8]));
        let old = Session::new(id, Timeline::default(), SessionMeasurement::default());
        let replacement = Session::new(id, Timeline::default(), SessionMeasurement::default());
        let others = Arc::new(Mutex::new(vec![
            old.clone(),
            replacement.clone(),
            old.clone(),
        ]));
        let current = Arc::new(Mutex::new(Session::new(
            SessionId(NodeId::from_array([3; 8])),
            Timeline::default(),
            SessionMeasurement::default(),
        )));
        let peers = Arc::new(Mutex::new(vec![ControllerPeer {
            peer_state: crate::discovery::peers::PeerState {
                node_state: crate::link::node::NodeState::new(id),
                ..Default::default()
            },
        }]));
        handle_failed_measurement_inner(old.key(), others.clone(), current, peers.clone()).await;
        assert_eq!(lock(&others).len(), 1);
        assert!(replacement.key().matches(&lock(&others)[0]));
        assert_eq!(lock(&peers).len(), 1);
    }

    #[tokio::test]
    async fn owned_io_standalone_waits_for_peer_once_and_cancels_stale_ownership() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = SessionId(NodeId::from_array([1; 8]));
            let session = Session::new(id, Timeline::default(), SessionMeasurement::default());
            let current = Arc::new(Mutex::new(session.clone()));
            let peers = Arc::new(Mutex::new(Vec::new()));
            let peer = ControllerPeer { peer_state: crate::discovery::peers::PeerState { node_state: crate::link::node::NodeState::new(id), ..Default::default() }};
            let (tx, mut rx) = tokio::sync::mpsc::channel(4);
            let work = tokio::spawn(remeasurement_once(peers.clone(), tx.clone(), session.clone(), Some(current.clone()), std::time::Duration::from_millis(1)));
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            assert!(!work.is_finished(), "empty peer list must not end the standalone chain");
            lock(&peers).push(peer.clone());
            assert!(matches!(rx.recv().await, Some(MeasurePeerEvent::PeerState(key, _)) if key.matches(&session)));
            work.await.unwrap(); assert!(rx.try_recv().is_err());
            lock(&peers).clear();
            let stale = tokio::spawn(remeasurement_once(peers.clone(), tx, session, Some(current.clone()), std::time::Duration::from_millis(1)));
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            *lock(&current) = Session::new(id, Timeline::default(), SessionMeasurement::default());
            lock(&peers).push(peer);
            stale.await.unwrap(); assert!(rx.try_recv().is_err());
            lock(&peers).clear();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let cancelled = tokio::spawn(remeasurement_once(peers, tx, lock(&current).clone(), None, std::time::Duration::ZERO));
            drop(rx); cancelled.await.unwrap();
        }).await.unwrap();
    }

    #[tokio::test]
    async fn origin_finality_standalone_scheduler_is_one_shot() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let id = SessionId(NodeId::from_array([1; 8]));
            let session = Session::new(id, Timeline::default(), SessionMeasurement::default());
            let peers = Arc::new(Mutex::new(vec![ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(id),
                    ..Default::default()
                },
            }]));
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let mut work = tokio::spawn(remeasurement_once(
                peers,
                tx,
                session.clone(),
                None,
                std::time::Duration::from_millis(5),
            ));
            let MeasurePeerEvent::PeerState(key, _) = rx.recv().await.unwrap() else {
                panic!("request")
            };
            assert!(key.matches(&session));
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut work)
                .await
                .expect("public remeasurement must finish after one request")
                .unwrap();
            assert!(
                rx.recv().await.is_none(),
                "no recurring producer survives the result"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn origin_finality_current_retry_survives_absence_and_remains_unique() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = SessionId(NodeId::from_array([1; 8]));
            let init = Session::new(id, Timeline::default(), SessionMeasurement::default());
            let peer = ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(id),
                    ..Default::default()
                },
            };
            let peers = Arc::new(Mutex::new(Vec::new()));
            let gate = Arc::new(DispatchGate::new_open());
            let (tx_request, mut requests) = tokio::sync::mpsc::channel(8);
            let (tx_result, results) = tokio::sync::mpsc::channel(8);
            let (tx_join, _joins) = tokio::sync::mpsc::channel(1);
            let (sessions, task) = Sessions::with_dispatch_gate(
                init.clone(),
                tx_request,
                peers.clone(),
                Clock::new(),
                tx_join,
                results,
                gate.clone(),
            );
            for _ in 0..4 {
                tx_result
                    .send(MeasurePeerEvent::Cancelled(init.key(), init.session_id.0))
                    .await
                    .unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            assert!(requests.try_recv().is_err());
            peers.lock().unwrap().push(peer);
            sessions.saw_session_timeline(id, Timeline::default()).await;
            let request =
                tokio::time::timeout(std::time::Duration::from_millis(500), requests.recv())
                    .await
                    .expect("current ownership must survive temporary peer absence")
                    .unwrap();
            assert!(matches!(request, MeasurePeerEvent::PeerState(key, _) if key.matches(&init)));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), requests.recv())
                    .await
                    .is_err(),
                "one owned retry, not one per cancellation result"
            );
            tx_result
                .send(MeasurePeerEvent::Cancelled(init.key(), init.session_id.0))
                .await
                .unwrap();
            tokio::task::yield_now().await;
            gate.stop().await;
            assert!(requests.try_recv().is_err());
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn originating_incarnation_rejects_old_success_and_failure_after_replacement() {
        for current_target in [false, true] {
            let initial = Session::new(
                SessionId(NodeId::from_array([90; 8])),
                Timeline::default(),
                SessionMeasurement::default(),
            );
            let old = if current_target {
                initial.clone()
            } else {
                Session::new(
                    SessionId(NodeId::from_array([89; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                )
            };
            let replacement = Session::new(
                old.session_id,
                old.timeline,
                SessionMeasurement {
                    x_form: GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(7),
                    },
                    timestamp: Duration::seconds(1),
                },
            );
            let current = Arc::new(Mutex::new(if current_target {
                replacement.clone()
            } else {
                initial
            }));
            let others = Arc::new(Mutex::new(if current_target {
                Vec::new()
            } else {
                vec![replacement.clone()]
            }));
            let peers = Arc::new(Mutex::new(vec![ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(old.session_id),
                    ..Default::default()
                },
            }]));
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            assert!(handle_successful_measurement_inner(
                old.key(),
                GhostXForm {
                    slope: 1.0,
                    intercept: Duration::seconds(100)
                },
                others.clone(),
                current.clone(),
                Clock::new(),
                tx
            )
            .await
            .is_none());
            assert!(handle_failed_measurement_inner(
                old.key(),
                others.clone(),
                current.clone(),
                peers.clone()
            )
            .await
            .is_none());
            let live = if current_target {
                current.lock().unwrap().clone()
            } else {
                others.lock().unwrap()[0].clone()
            };
            assert!(replacement.key().matches(&live));
            assert_eq!(live.measurement.x_form, replacement.measurement.x_form);
            assert_eq!(peers.lock().unwrap().len(), 1);
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn three_gaps_candidate_rearms_after_absent_peer_and_rejects_retired_origin() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let id = SessionId(NodeId::from_array([1; 8]));
            let peer = ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(id),
                    ..Default::default()
                },
            };
            let peers = Arc::new(Mutex::new(vec![peer.clone()]));
            let (tx_requests, mut requests) = tokio::sync::mpsc::channel(1);
            let (tx_results, results) = tokio::sync::mpsc::channel(1);
            let (tx_joins, mut joins) = tokio::sync::mpsc::channel(1);
            let (sessions, task) = Sessions::with_dispatch_gate(
                Session::new(
                    SessionId(NodeId::from_array([2; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                ),
                tx_requests,
                peers.clone(),
                Clock::new(),
                tx_joins,
                results,
                Arc::new(DispatchGate::new_open()),
            );
            sessions.saw_session_timeline(id, Timeline::default()).await;
            let MeasurePeerEvent::PeerState(old, _) = requests.recv().await.unwrap() else {
                panic!("initial request")
            };
            peers.lock().unwrap().clear();
            tx_results
                .send(MeasurePeerEvent::Cancelled(old.clone(), old.session_id.0))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while sessions
                    .other_sessions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|session| old.matches(session))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("retry must retire an absent candidate rather than strand it");
            assert!(requests.try_recv().is_err());
            assert!(joins.try_recv().is_err());
            peers.lock().unwrap().push(peer);
            sessions.saw_session_timeline(id, Timeline::default()).await;
            let MeasurePeerEvent::PeerState(fresh, _) = requests.recv().await.unwrap() else {
                panic!("rearmed request")
            };
            assert!(!Arc::ptr_eq(&old.incarnation, &fresh.incarnation));
            tx_results
                .send(MeasurePeerEvent::XForm(
                    old.clone(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(2),
                    },
                ))
                .await
                .unwrap();
            tx_results
                .send(MeasurePeerEvent::XForm(old, GhostXForm::default()))
                .await
                .unwrap();
            tx_results
                .send(MeasurePeerEvent::XForm(
                    fresh.clone(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(1),
                    },
                ))
                .await
                .unwrap();
            let joined = joins.recv().await.unwrap();
            assert!(fresh.matches(&joined));
            assert_eq!(peers.lock().unwrap().len(), 1);
            assert!(joins.try_recv().is_err());
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_candidate_retries_original_key_without_waiting_for_changed_timeline() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let candidate_id = SessionId(NodeId::from_array([1; 8]));
            let peer = ControllerPeer {
                peer_state: crate::discovery::peers::PeerState {
                    node_state: crate::link::node::NodeState::new(candidate_id),
                    ..Default::default()
                },
            };
            let (tx_requests, mut requests) = tokio::sync::mpsc::channel(1);
            let (tx_results, results) = tokio::sync::mpsc::channel(1);
            let (tx_joins, mut joins) = tokio::sync::mpsc::channel(1);
            let (sessions, task) = Sessions::with_dispatch_gate(
                Session::new(
                    SessionId(NodeId::from_array([2; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                ),
                tx_requests,
                Arc::new(Mutex::new(vec![peer])),
                Clock::new(),
                tx_joins,
                results,
                Arc::new(DispatchGate::new_open()),
            );
            sessions
                .saw_session_timeline(candidate_id, Timeline::default())
                .await;
            let MeasurePeerEvent::PeerState(origin, _) = requests.recv().await.unwrap() else {
                panic!("request")
            };
            tx_results
                .send(MeasurePeerEvent::Cancelled(
                    origin.clone(),
                    origin.session_id.0,
                ))
                .await
                .unwrap();
            let MeasurePeerEvent::PeerState(retried, _) = requests.recv().await.unwrap() else {
                panic!("retry")
            };
            assert!(Arc::ptr_eq(&origin.incarnation, &retried.incarnation));
            tx_results
                .send(MeasurePeerEvent::XForm(
                    retried,
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(1),
                    },
                ))
                .await
                .unwrap();
            let joined = joins.recv().await.unwrap();
            assert!(origin.matches(&joined));
            assert_eq!(sessions.current.lock().unwrap().session_id, candidate_id);
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    use super::*;
    use crate::link::node::NodeId;

    #[tokio::test]
    async fn blocked_joins_reject_same_id_reset_and_candidate_reinsertion() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for reset_current in [true, false] {
                let a = Session::new(
                    SessionId(NodeId::from_array([1; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                );
                let b = Session::new(
                    SessionId(NodeId::from_array([2; 8])),
                    Timeline::default(),
                    SessionMeasurement::default(),
                );
                let current = Arc::new(Mutex::new(a.clone()));
                let others = Arc::new(Mutex::new(vec![b.clone()]));
                let (tx_request, _rx_request) = tokio::sync::mpsc::channel(1);
                let mut sessions = Sessions {
                    published: Arc::new(Mutex::new(a.key())),
                    current: current.clone(),
                    other_sessions: others.clone(),
                    is_founding: Arc::new(Mutex::new(false)),
                    has_joined: Arc::new(Mutex::new(false)),
                    tx_measure_peer_state: tx_request,
                    peers: Arc::new(Mutex::new(vec![ControllerPeer {
                        peer_state: crate::discovery::peers::PeerState {
                            node_state: crate::link::node::NodeState::new(b.session_id),
                            ..Default::default()
                        },
                    }])),
                    clock: Clock::new(),
                };
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                tx.send(a.clone()).await.unwrap();
                let target = if reset_current { a.key() } else { b.key() };
                let mut pending = Box::pin(handle_successful_measurement_inner(
                    target,
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(60),
                    },
                    others.clone(),
                    current.clone(),
                    Clock::new(),
                    tx.clone(),
                ));
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                if reset_current {
                    sessions.reset_session(a.clone());
                } else {
                    lock(&others).clear();
                    sessions
                        .saw_session_timeline(b.session_id, b.timeline)
                        .await;
                }
                rx.recv().await.unwrap();
                assert!(pending.await.is_none());
                assert!(
                    rx.try_recv().is_err(),
                    "old incarnation must not publish after same-ID replacement"
                );
                assert_eq!(lock(&current).measurement.x_form, GhostXForm::default());
                if !reset_current {
                    assert_eq!(lock(&others)[0].measurement.x_form, GhostXForm::default());
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn blocked_join_is_cancel_safe_and_rechecks_state_before_commit() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for outcome in 0..3 {
                let a = Session {
                    incarnation: Arc::new(()),
                    edit_revision: new_edit_revision(),
                    session_id: SessionId(NodeId::from_array([10; 8])),
                    timeline: Timeline::default(),
                    measurement: SessionMeasurement::default(),
                };
                let b = Session {
                    incarnation: Arc::new(()),
                    session_id: SessionId(NodeId::from_array([20; 8])),
                    ..a.clone()
                };
                let current = Arc::new(Mutex::new(a.clone()));
                let others = Arc::new(Mutex::new(vec![b.clone()]));
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                tx.send(a.clone()).await.unwrap();
                let mut pending = tokio::spawn(handle_successful_measurement_inner(
                    b.key(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(60),
                    },
                    others.clone(),
                    current.clone(),
                    Clock::new(),
                    tx.clone(),
                ));
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(30), &mut pending)
                        .await
                        .is_err()
                );
                assert_eq!(
                    lock(&current).session_id,
                    a.session_id,
                    "blocked notification must not partially commit a switch"
                );
                assert_eq!(lock(&others)[0].session_id, b.session_id);
                if outcome == 0 {
                    pending.abort();
                    let _ = pending.await;
                    assert_eq!(lock(&current).session_id, a.session_id);
                    rx.recv().await.unwrap();
                    assert!(rx.try_recv().is_err());
                } else if outcome == 1 {
                    {
                        let mut guard = lock(&current);
                        let mut candidates = lock(&others);
                        guard.session_id = SessionId(NodeId::from_array([30; 8]));
                        candidates.clear();
                    }
                    rx.recv().await.unwrap();
                    assert!(pending.await.unwrap().is_none());
                    assert!(
                        rx.try_recv().is_err(),
                        "pre-reset session must not be published after capacity returns"
                    );
                } else {
                    lock(&others)[0].timeline.tempo = crate::link::tempo::Tempo::new(150.0);
                    rx.recv().await.unwrap();
                    let winner = pending.await.unwrap().unwrap();
                    assert_eq!(winner.timeline.tempo.bpm(), 150.0);
                    assert_eq!(rx.recv().await.unwrap().session_id, b.session_id);
                    let guard = lock(&current);
                    let candidates = lock(&others);
                    assert_eq!(guard.session_id, b.session_id);
                    assert_eq!(guard.timeline.tempo.bpm(), 150.0);
                    assert_eq!(candidates.len(), 1);
                    assert_eq!(candidates[0].session_id, a.session_id);
                }
            }
        })
        .await
        .expect("blocked joins must be cancel-safe and use state current at publication");
    }

    #[test]
    fn resetting_timeline_updates_the_current_snapshot_without_an_other_copy() {
        let current = Session {
            incarnation: Arc::new(()),
            edit_revision: new_edit_revision(),
            session_id: SessionId(NodeId::from_array([1; 8])),
            timeline: Timeline::default(),
            measurement: SessionMeasurement::default(),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let sessions = Sessions {
            published: Arc::new(Mutex::new(current.key())),
            current: Arc::new(Mutex::new(current)),
            other_sessions: Arc::new(Mutex::new(Vec::new())),
            is_founding: Arc::new(Mutex::new(false)),
            has_joined: Arc::new(Mutex::new(false)),
            peers: Arc::new(Mutex::new(Vec::new())),
            tx_measure_peer_state: tx,
            clock: Clock::new(),
        };
        let timeline = Timeline {
            tempo: crate::link::tempo::Tempo::new(150.0),
            ..Timeline::default()
        };
        let (origin, mapping) = {
            let current = lock(&sessions.current);
            (current.key(), current.measurement.x_form)
        };
        assert!(sessions.reset_timeline(&origin, mapping, timeline));
        assert_eq!(lock(&sessions.current).timeline.tempo, timeline.tempo);
        assert!(lock(&sessions.other_sessions).is_empty());
    }

    #[tokio::test]
    async fn failed_other_session_removes_only_its_session_and_peers() {
        let current = Session {
            incarnation: Arc::new(()),
            edit_revision: crate::link::sessions::new_edit_revision(),
            session_id: SessionId(NodeId::from_array([1; 8])),
            timeline: Timeline::default(),
            measurement: SessionMeasurement::default(),
        };
        let failed = Session {
            incarnation: Arc::new(()),
            session_id: SessionId(NodeId::from_array([2; 8])),
            ..current.clone()
        };
        let other = Session {
            incarnation: Arc::new(()),
            session_id: SessionId(NodeId::from_array([3; 8])),
            ..current.clone()
        };
        let sessions = Arc::new(Mutex::new(vec![failed.clone(), other.clone()]));
        let peers = Arc::new(Mutex::new(
            vec![failed.session_id, failed.session_id, other.session_id]
                .into_iter()
                .map(|session_id| ControllerPeer {
                    peer_state: crate::discovery::peers::PeerState {
                        node_state: crate::link::node::NodeState::new(session_id),
                        ..Default::default()
                    },
                })
                .collect(),
        ));
        assert!(handle_failed_measurement_inner(
            failed.key(),
            sessions.clone(),
            Arc::new(Mutex::new(current)),
            peers.clone()
        )
        .await
        .is_none());
        assert_eq!(sessions.lock().unwrap().len(), 1);
        assert_eq!(sessions.lock().unwrap()[0].session_id, other.session_id);
        assert_eq!(peers.lock().unwrap().len(), 1);
        assert_eq!(
            peers.lock().unwrap()[0].peer_state.session_id(),
            other.session_id
        );
    }

    #[tokio::test]
    async fn remeasurement_scheduler_releases_peers_on_disable_and_final_drop() {
        let gate = Arc::new(DispatchGate::new());
        let peers = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::downgrade(&peers);
        let (tx_request, _rx_request) = tokio::sync::mpsc::channel(1);
        let (tx_result, rx_result) = tokio::sync::mpsc::channel(2);
        let (tx_join, mut rx_join) = tokio::sync::mpsc::channel(1);
        let session = Session {
            incarnation: Arc::new(()),
            edit_revision: crate::link::sessions::new_edit_revision(),
            session_id: SessionId::default(),
            timeline: Timeline::default(),
            measurement: SessionMeasurement::default(),
        };
        let (sessions, task) = Sessions::with_dispatch_gate(
            session.clone(),
            tx_request,
            peers.clone(),
            Clock::new(),
            tx_join,
            rx_result,
            gate.clone(),
        );
        let baseline = Arc::strong_count(&peers);
        for cycle in 0..2 {
            gate.start().await;
            for _ in 0..3 {
                tx_result
                    .send(MeasurePeerEvent::XForm(
                        session.key(),
                        GhostXForm::default(),
                    ))
                    .await
                    .unwrap();
            }
            // The following join acknowledges processing of the preceding failures.
            tx_result
                .send(MeasurePeerEvent::XForm(
                    session.key(),
                    GhostXForm {
                        slope: 1.0,
                        intercept: Duration::seconds(1),
                    },
                ))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), rx_join.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(Arc::strong_count(&peers), baseline + 1);
            if cycle == 0 {
                gate.stop().await;
                assert_eq!(Arc::strong_count(&peers), baseline);
            }
        }
        drop(sessions);
        drop(peers);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while probe.strong_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn test_key() {
        assert_eq!(SESSION_MEMBERSHIP_HEADER_KEY, 0x73657373);
    }

    #[test]
    fn session_id_equality() {
        let id1 = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let id2 = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let id3 = SessionId(NodeId::from_array([8, 7, 6, 5, 4, 3, 2, 1]));
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn session_id_ordering() {
        let id_low = SessionId(NodeId::from_array([0, 0, 0, 0, 0, 0, 0, 1]));
        let id_high = SessionId(NodeId::from_array([0, 0, 0, 0, 0, 0, 0, 2]));
        assert!(id_low < id_high);
    }

    #[test]
    fn session_id_display() {
        let id = SessionId(NodeId::from_array([
            0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44,
        ]));
        let display = format!("{}", id);
        assert_eq!(display, "0xaabbccdd11223344");
    }

    #[test]
    fn session_membership_from_session_id() {
        let id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let membership = SessionMembership::from(id);
        assert_eq!(membership.session_id, id);
    }

    #[test]
    fn session_membership_roundtrip_encode() {
        let id = SessionId(NodeId::from_array([10, 20, 30, 40, 50, 60, 70, 80]));
        let membership = SessionMembership::from(id);
        let encoded = membership.encode().unwrap();
        // Should include the header + SessionId bytes
        assert!(!encoded.is_empty());
    }

    #[test]
    fn session_measurement_default() {
        let sm = SessionMeasurement::default();
        assert_eq!(sm.x_form, GhostXForm::default());
        assert_eq!(sm.timestamp, Duration::zero());
    }

    #[test]
    fn session_peers_empty_when_no_match() {
        let peers = Arc::new(Mutex::new(vec![]));
        let session_id = SessionId(NodeId::from_array([1, 2, 3, 4, 5, 6, 7, 8]));
        let result = session_peers(peers, session_id);
        assert!(result.is_empty());
    }
}
