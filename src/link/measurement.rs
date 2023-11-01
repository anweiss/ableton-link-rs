use std::{
    collections::HashMap,
    mem,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use chrono::Duration;
use tokio::{
    net::UdpSocket,
    select,
    sync::{
        mpsc::{self, Sender},
        Notify,
    },
};
use tracing::{debug, info};

use crate::{
    discovery::{
        messages::parse_payload, messenger::new_udp_reuseport, peers::PeerState, ENCODING_CONFIG,
        UNICAST_IP_ANY,
    },
    link::{
        payload::PrevGhostTime,
        pingresponder::{parse_message_header, MAX_MESSAGE_SIZE, PONG},
        Result,
    },
};

use super::{
    clock::Clock,
    ghostxform::GhostXForm,
    node::NodeId,
    payload::{HostTime, Payload, PayloadEntry, PayloadEntryHeader},
    pingresponder::{encode_message, PingResponder, PING},
    sessions::SessionId,
};

pub const MEASUREMENT_ENDPOINT_V4_HEADER_KEY: u32 = u32::from_be_bytes(*b"mep4");
pub const MEASUREMENT_ENDPOINT_V4_SIZE: u32 =
    (mem::size_of::<Ipv4Addr>() + mem::size_of::<u16>()) as u32;
pub const MEASUREMENT_ENDPOINT_V4_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: MEASUREMENT_ENDPOINT_V4_HEADER_KEY,
    size: MEASUREMENT_ENDPOINT_V4_SIZE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementEndpointV4 {
    pub endpoint: Option<SocketAddrV4>,
}

impl bincode::Encode for MeasurementEndpointV4 {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(
            &(
                u32::from(*self.endpoint.unwrap().ip()),
                self.endpoint.unwrap().port(),
            ),
            encoder,
        )
    }
}

impl bincode::Decode for MeasurementEndpointV4 {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        let (ip, port) = bincode::Decode::decode(decoder)?;
        Ok(Self {
            endpoint: Some(SocketAddrV4::new(ip, port)),
        })
    }
}

impl MeasurementEndpointV4 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = MEASUREMENT_ENDPOINT_V4_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(self.endpoint, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}

#[derive(Clone, Debug)]
pub enum MeasurePeerEvent {
    PeerState(SessionId, PeerState),
    XForm(SessionId, GhostXForm),
}

#[derive(Debug, Clone)]
pub struct MeasurementService {
    pub measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    pub clock: Clock,
    pub ping_responder: PingResponder,
    pub tx_measure_peer: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
}

impl MeasurementService {
    pub async fn new(
        ping_responder_unicast_socket: Arc<UdpSocket>,
        session_id: SessionId,
        ghost_x_form: GhostXForm,
        clock: Clock,
        tx_measure_peer_result: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
        notifier: Arc<Notify>,
        mut rx_measure_peer_state: tokio::sync::mpsc::Receiver<MeasurePeerEvent>,
    ) -> MeasurementService {
        let measurement_map = Arc::new(Mutex::new(HashMap::new()));

        let m_map = measurement_map.clone();
        let t_peer = tx_measure_peer_result.clone();

        tokio::spawn(async move {
            loop {
                let event = rx_measure_peer_state.recv().await;
                if let Some(MeasurePeerEvent::PeerState(session_id, peer)) = event {
                    measure_peer(clock, m_map.clone(), t_peer.clone(), session_id, peer).await;
                }
            }
        });

        MeasurementService {
            measurement_map,
            clock,
            ping_responder: PingResponder::new(
                ping_responder_unicast_socket,
                session_id,
                ghost_x_form,
                clock,
                notifier,
            )
            .await,
            tx_measure_peer: tx_measure_peer_result,
        }
    }

    pub async fn update_node_state(&self, session_id: SessionId, x_form: GhostXForm) {
        self.ping_responder
            .update_node_state(session_id, x_form)
            .await;
    }
}

pub async fn measure_peer(
    clock: Clock,
    measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    tx_measure_peer_result: tokio::sync::mpsc::Sender<MeasurePeerEvent>,
    session_id: SessionId,
    state: PeerState,
) {
    info!(
        "measuring peer {} for session {}",
        state.node_state.node_id, session_id
    );

    let node_id = state.node_state.node_id;

    let (tx_measurement, mut rx_measurement) = mpsc::channel(1);

    let measurement = Measurement::new(state, clock, tx_measurement).await;

    measurement_map
        .try_lock()
        .unwrap()
        .insert(node_id, measurement);

    let tx_measure_peer_result_loop = tx_measure_peer_result.clone();

    let measurement_map = measurement_map.clone();

    tokio::spawn(async move {
        loop {
            if let Some(data) = rx_measurement.recv().await {
                if data.is_empty() {
                    tx_measure_peer_result_loop
                        .send(MeasurePeerEvent::XForm(session_id, GhostXForm::default()))
                        .await
                        .unwrap();
                } else {
                    tx_measure_peer_result_loop
                        .send(MeasurePeerEvent::XForm(
                            session_id,
                            GhostXForm {
                                slope: 1.0,
                                intercept: Duration::microseconds(median(data).round() as i64),
                            },
                        ))
                        .await
                        .unwrap();
                }

                measurement_map
                    .try_lock()
                    .unwrap()
                    .remove(&node_id)
                    .unwrap();
            }
        }
    });
}

pub const NUMBER_DATA_POINTS: usize = 100;
pub const NUMBER_MEASUREMENTS: usize = 5;

#[derive(Debug)]
enum TimerStatus {
    Finish,
    Reset,
}

#[derive(Debug)]
pub struct Measurement {
    pub unicast_socket: Option<Arc<UdpSocket>>,
    pub session_id: SessionId,
    pub measurement_endpoint: Option<SocketAddrV4>,
    pub data: Arc<Mutex<Vec<f64>>>,
    pub clock: Clock,
    pub measurements_started: Arc<Mutex<usize>>,
    pub success: Arc<Mutex<bool>>,
    tx_timer: Sender<TimerStatus>,
}

impl Measurement {
    pub async fn new(state: PeerState, clock: Clock, tx_measurement: Sender<Vec<f64>>) -> Self {
        let (tx_timer, mut rx_timer) = mpsc::channel(1);

        let unicast_socket = Arc::new(new_udp_reuseport(UNICAST_IP_ANY));

        let success = Arc::new(Mutex::new(false));
        let data = Arc::new(Mutex::new(vec![]));

        let mut measurement = Measurement {
            unicast_socket: Some(unicast_socket.clone()),
            session_id: state.node_state.session_id,
            measurement_endpoint: state.measurement_endpoint,
            data: data.clone(),
            clock,
            measurements_started: Arc::new(Mutex::new(0)),
            success: success.clone(),
            tx_timer,
        };

        info!("measurement on gateway");

        let ht = HostTime::new(clock.micros());

        let s = success.clone();
        let d = data.clone();
        let t = tx_measurement.clone();
        let ms = measurement.measurements_started.clone();
        let us = unicast_socket.clone();

        let finished_notifier = Arc::new(Notify::new());

        let fn_loop = finished_notifier.clone();

        tokio::spawn(async move {
            loop {
                if let Some(timer_status) = rx_timer.recv().await {
                    debug!("timer status {:?}", timer_status);
                    match timer_status {
                        TimerStatus::Finish => {
                            fn_loop.notify_one();

                            finish(
                                s.clone(),
                                state.measurement_endpoint.unwrap(),
                                d.clone(),
                                t.clone(),
                            )
                            .await;
                        }
                        TimerStatus::Reset => {
                            reset_timer(
                                ms.clone(),
                                clock,
                                Some(us.clone()),
                                state.measurement_endpoint.unwrap(),
                                d.clone(),
                                t.clone(),
                                fn_loop.clone(),
                            )
                            .await;
                        }
                    }
                }
            }
        });

        measurement.listen().await;

        send_ping(
            unicast_socket.clone(),
            *state.measurement_endpoint.as_ref().unwrap(),
            &Payload {
                entries: vec![PayloadEntry::HostTime(ht)],
            },
        )
        .await;

        reset_timer(
            measurement.measurements_started.clone(),
            clock,
            Some(unicast_socket.clone()),
            state.measurement_endpoint.unwrap(),
            data.clone(),
            tx_measurement.clone(),
            finished_notifier.clone(),
        )
        .await;

        measurement
    }

    pub async fn listen(&mut self) {
        let socket = self.unicast_socket.as_ref().unwrap().clone();
        let endpoint = *self.measurement_endpoint.as_ref().unwrap();
        socket.connect(endpoint).await.unwrap();
        let clock = self.clock;
        let s_id = self.session_id;
        let data = self.data.clone();
        let tx_timer = self.tx_timer.clone();

        info!(
            "listening for pong messages on {}",
            socket.local_addr().unwrap()
        );

        tokio::spawn(async move {
            let mut pong_received = false;
            loop {
                let mut buf = [0; MAX_MESSAGE_SIZE];

                // TODO: check to see if peer has left before receiving pong
                let (amt, src) = socket.recv_from(&mut buf).await.unwrap();

                let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();
                if header.message_type == PONG {
                    if !pong_received {
                        info!("received pong message from {}", src);
                        pong_received = true;
                    }

                    let payload = parse_payload(&buf[header_len..amt]).unwrap();

                    let mut session_id = SessionId::default();
                    let mut ghost_time = Duration::zero();
                    let mut prev_ghost_time = Duration::zero();
                    let mut prev_host_time = Duration::zero();

                    for entry in payload.entries.iter() {
                        match entry {
                            PayloadEntry::SessionMembership(id) => session_id = id.session_id,
                            PayloadEntry::GhostTime(gt) => ghost_time = gt.time,
                            PayloadEntry::PrevGhostTime(gt) => prev_ghost_time = gt.time,
                            PayloadEntry::HostTime(ht) => prev_host_time = ht.time,
                            _ => continue,
                        }
                    }

                    if s_id == session_id {
                        let host_time = clock.micros();
                        let payload = Payload {
                            entries: vec![
                                PayloadEntry::HostTime(HostTime { time: host_time }),
                                PayloadEntry::PrevGhostTime(PrevGhostTime {
                                    time: prev_ghost_time,
                                }),
                            ],
                        };

                        send_ping(socket.clone(), endpoint, &payload).await;

                        if ghost_time != Duration::microseconds(0)
                            && prev_host_time != Duration::microseconds(0)
                        {
                            data.try_lock().unwrap().push(
                                ghost_time.num_microseconds().unwrap() as f64
                                    - ((host_time + prev_host_time).num_microseconds().unwrap()
                                        as f64
                                        * 0.5),
                            );

                            if prev_ghost_time != Duration::microseconds(0) {
                                data.try_lock().unwrap().push(
                                    ((ghost_time + prev_ghost_time).num_microseconds().unwrap()
                                        as f64
                                        * 0.5)
                                        - prev_host_time.num_microseconds().unwrap() as f64,
                                );
                            }
                        }

                        if data.try_lock().unwrap().len() > NUMBER_DATA_POINTS {
                            tx_timer.send(TimerStatus::Finish).await.unwrap();
                            break;
                        }
                    }
                }
            }
        });
    }
}

async fn reset_timer(
    measurements_started: Arc<Mutex<usize>>,
    clock: Clock,
    unicast_socket: Option<Arc<UdpSocket>>,
    measurement_endpoint: SocketAddrV4,
    data: Arc<Mutex<Vec<f64>>>,
    tx_measurement: Sender<Vec<f64>>,
    finished_notifier: Arc<Notify>,
) {
    loop {
        select! {
            _  = tokio::time::sleep(Duration::milliseconds(50).to_std().unwrap()) => {
                info!(
                    "measurements_start {}",
                    measurements_started.try_lock().unwrap()
                );
                if *measurements_started.try_lock().unwrap() < NUMBER_MEASUREMENTS {
                    let ht = HostTime {
                        time: clock.micros(),
                    };

                    send_ping(
                        unicast_socket.as_ref().unwrap().clone(),
                        measurement_endpoint,
                        &Payload {
                            entries: vec![PayloadEntry::HostTime(ht)],
                        },
                    )
                    .await;

                    *measurements_started.try_lock().unwrap() += 1;
                } else {
                    data.try_lock().unwrap().clear();
                    info!("measuring {} failed", measurement_endpoint);

                    let data = data.try_lock().unwrap().clone();
                    tx_measurement.send(data).await.unwrap();
                }
            }
            _ = finished_notifier.notified() => {
                break;
            }
        }
    }
}

async fn finish(
    success: Arc<Mutex<bool>>,
    measurement_endpoint: SocketAddrV4,
    data: Arc<Mutex<Vec<f64>>>,
    tx_measurement: Sender<Vec<f64>>,
) {
    *success.try_lock().unwrap() = true;
    debug!("measuring {} done", measurement_endpoint);

    let d = data.try_lock().unwrap().clone();
    tx_measurement.send(d).await.unwrap();
    data.try_lock().unwrap().clear();
}

pub async fn send_ping(
    socket: Arc<UdpSocket>,
    measurement_endpoint: SocketAddrV4,
    payload: &Payload,
) {
    let message = encode_message(PING, payload).unwrap();
    debug!(
        "sending ping message to measurement endpoint {}",
        measurement_endpoint
    );
    let _ = socket.send(&message).await.unwrap();
}

pub fn median(values: Vec<f64>) -> f64 {
    let n = values.len();

    if n <= 2 {
        return 0.0;
    }

    let mut sorted_values = values;
    sorted_values.sort_by(|a, b| a.partial_cmp(b).unwrap());

    if n % 2 == 0 {
        let mid1 = n / 2;
        let mid2 = (n - 1) / 2;

        (sorted_values[mid1] + sorted_values[mid2]) / 2.0
    } else {
        let mid = n / 2;

        sorted_values[mid]
    }
}
