use std::{
    collections::HashMap,
    mem,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use chrono::Duration;
use tokio::{
    net::UdpSocket,
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        Notify,
    },
};
use tracing::info;

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
    pub tx_measure_peer: tokio::sync::broadcast::Sender<MeasurePeerEvent>,
}

impl MeasurementService {
    pub async fn new(
        ping_responder_unicast_socket: Arc<UdpSocket>,
        session_id: SessionId,
        ghost_x_form: GhostXForm,
        clock: Clock,
        tx_measure_peer: tokio::sync::broadcast::Sender<MeasurePeerEvent>,
        notifier: Arc<Notify>,
    ) -> MeasurementService {
        let mut rx_measure_peer = tx_measure_peer.subscribe();
        let measurement_map = Arc::new(Mutex::new(HashMap::new()));

        let m_map = measurement_map.clone();
        let t_peer = tx_measure_peer.clone();

        let n = notifier.clone();

        tokio::spawn(async move {
            loop {
                select! {
                    Ok(peer) = rx_measure_peer.recv() => {
                        if let MeasurePeerEvent::PeerState(session_id, peer) = peer {
                            measure_peer(
                                clock,
                                m_map.clone(),
                                t_peer.clone(),
                                session_id,
                                peer,
                            )
                            .await;
                        }
                    }
                    // _ = n.notified() => {
                    //     break;
                    // }
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
            tx_measure_peer,
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
    tx_measure_peer: tokio::sync::broadcast::Sender<MeasurePeerEvent>,
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

    measurement_map.lock().unwrap().insert(node_id, measurement);

    let tx_measure_peer = tx_measure_peer.clone();

    let measurement_map = measurement_map.clone();

    tokio::spawn(async move {
        loop {
            if let Some(data) = rx_measurement.recv().await {
                if data.is_empty() {
                    let _ = tx_measure_peer
                        .send(MeasurePeerEvent::XForm(session_id, GhostXForm::default()))
                        .unwrap();
                } else {
                    let _ = tx_measure_peer
                        .send(MeasurePeerEvent::XForm(
                            session_id,
                            GhostXForm {
                                slope: 1.0,
                                intercept: Duration::microseconds(median(data).round() as i64),
                            },
                        ))
                        .unwrap();
                }

                measurement_map.lock().unwrap().remove(&node_id);
            }
        }
    });
}

pub const NUMBER_DATA_POINTS: usize = 100;
pub const NUMBER_MEASUREMENTS: usize = 5;

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
    tx_measurement: Sender<Vec<f64>>,
}

impl Measurement {
    pub async fn new(state: PeerState, clock: Clock, tx_measurement: Sender<Vec<f64>>) -> Self {
        let (tx_timer, mut rx_timer) = mpsc::channel(1);

        info!("peer state: {:?}", state);

        let unicast_socket = Arc::new(new_udp_reuseport(UNICAST_IP_ANY));

        let success = Arc::new(Mutex::new(false));
        let data = Arc::new(Mutex::new(vec![]));

        let mut measurement = Measurement {
            unicast_socket: Some(unicast_socket.clone()),
            session_id: state.node_state.session_id.clone(),
            measurement_endpoint: state.measurement_endpoint,
            data: data.clone(),
            clock,
            measurements_started: Arc::new(Mutex::new(0)),
            success: success.clone(),
            tx_timer,
            tx_measurement: tx_measurement.clone(),
        };

        info!("measurement on gateway");

        let ht = HostTime::new(clock.micros());

        let s = success.clone();
        let s_me = state.measurement_endpoint.clone();
        let d = data.clone();
        let t = tx_measurement.clone();

        tokio::spawn(async move {
            loop {
                if let Some(timer_status) = rx_timer.recv().await {
                    match timer_status {
                        TimerStatus::Finish => {
                            finish(s.clone(), s_me.unwrap(), d.clone(), t.clone()).await;
                        }
                        TimerStatus::Reset => {
                            todo!();
                        }
                    }
                }
            }
        });

        measurement.listen().await;

        send_ping(
            unicast_socket,
            state.measurement_endpoint.as_ref().unwrap().clone(),
            &Payload {
                entries: vec![PayloadEntry::HostTime(ht)],
            },
        )
        .await;

        measurement.reset_timer().await;

        measurement
    }

    pub async fn reset_timer(&mut self) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::milliseconds(50).to_std().unwrap()) => {
                    info!("measurements_start {}", self.measurements_started.lock().unwrap());
                    if *self.measurements_started.lock().unwrap() < NUMBER_MEASUREMENTS {
                        let ht = HostTime {
                            time: self.clock.micros(),
                        };

                        send_ping(
                            self.unicast_socket.as_ref().unwrap().clone(),
                            self.measurement_endpoint.as_ref().unwrap().clone(),
                            &Payload {
                                entries: vec![PayloadEntry::HostTime(ht)],
                            },
                        )
                        .await;

                        *self.measurements_started.lock().unwrap() += 1;
                    } else {
                        self.data.lock().unwrap().clear();
                        info!("measuring {} failed", self.measurement_endpoint.unwrap());

                        let data = self.data.lock().unwrap().clone();
                        self.tx_measurement.send(data).await.unwrap();
                        break;
                    }
                }
            }
        }
    }

    pub async fn listen(&mut self) {
        let socket = self.unicast_socket.as_ref().unwrap().clone();
        let endpoint = self.measurement_endpoint.as_ref().unwrap().clone();
        socket.connect(endpoint).await.unwrap();
        let clock = self.clock;
        let s_id = self.session_id.clone();
        let data = self.data.clone();
        let tx_timer = self.tx_timer.clone();

        info!(
            "listening for pong messages on {}",
            socket.local_addr().unwrap()
        );

        tokio::spawn(async move {
            loop {
                let mut buf = [0; MAX_MESSAGE_SIZE];

                let (amt, src) = socket.recv_from(&mut buf).await.unwrap();

                let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();
                if header.message_type == PONG {
                    info!("received pong message from {}", src);

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
                            data.lock().unwrap().push(
                                ghost_time.num_microseconds().unwrap() as f64
                                    - ((host_time + prev_host_time).num_microseconds().unwrap()
                                        as f64
                                        * 0.5),
                            );

                            if prev_ghost_time != Duration::microseconds(0) {
                                data.lock().unwrap().push(
                                    ((ghost_time + prev_ghost_time).num_microseconds().unwrap()
                                        as f64
                                        * 0.5)
                                        - prev_ghost_time.num_microseconds().unwrap() as f64,
                                );
                            }
                        }

                        info!("data {:?}", data.lock().unwrap());

                        if data.lock().unwrap().len() > NUMBER_DATA_POINTS {
                            info!("finishing measurement");
                            tx_timer.send(TimerStatus::Finish).await.unwrap();
                        } else {
                            info!("resetting measurement");
                            tx_timer.send(TimerStatus::Reset).await.unwrap();
                        }
                    }
                }
            }
        });
    }
}

async fn finish(
    success: Arc<Mutex<bool>>,
    measurement_endpoint: SocketAddrV4,
    data: Arc<Mutex<Vec<f64>>>,
    tx_measurement: Sender<Vec<f64>>,
) {
    *success.lock().unwrap() = true;
    info!("measuring {} done", measurement_endpoint);

    let data = data.lock().unwrap().clone();
    tx_measurement.send(data).await.unwrap();
}

pub async fn send_ping(
    socket: Arc<UdpSocket>,
    measurement_endpoint: SocketAddrV4,
    payload: &Payload,
) {
    let message = encode_message(PING, payload).unwrap();
    info!(
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
        let median = (sorted_values[mid1] + sorted_values[mid2]) / 2.0;

        median
    } else {
        let mid = n / 2;

        sorted_values[mid]
    }
}
