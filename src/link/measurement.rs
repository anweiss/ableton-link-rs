use std::{
    collections::HashMap,
    mem,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use async_recursion::async_recursion;
use chrono::Duration;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, Receiver, Sender},
};
use tracing::info;

use crate::{
    discovery::{messages::parse_payload, peers::PeerState, ENCODING_CONFIG},
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
    pingresponder::PingResponder,
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
            &(self.endpoint.unwrap().ip(), self.endpoint.unwrap().port()),
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

#[derive(Debug)]
pub struct MeasurementService {
    pub measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    pub clock: Clock,
    pub ping_responder: PingResponder,
}

impl MeasurementService {
    pub async fn new(
        unicast_socket: Arc<UdpSocket>,
        session_id: Arc<Mutex<SessionId>>,
        ghost_x_form: GhostXForm,
        clock: Clock,
    ) -> MeasurementService {
        MeasurementService {
            measurement_map: Arc::new(Mutex::new(HashMap::new())),
            clock,
            ping_responder: PingResponder::new(unicast_socket, session_id, ghost_x_form, clock)
                .await,
        }
    }

    pub async fn update_node_state(
        &mut self,
        session_id: Arc<Mutex<SessionId>>,
        x_form: GhostXForm,
    ) {
        self.ping_responder
            .update_node_state(session_id, x_form)
            .await;
    }
}

pub const NUMBER_DATA_POINTS: usize = 100;
pub const NUMBER_MEASUREMENTS: usize = 5;

enum TimerStatus {
    Finish,
    Reset,
}

#[derive(Debug)]
pub struct Measurement {
    pub socket: Option<Arc<UdpSocket>>,
    pub session_id: Arc<Mutex<SessionId>>,
    pub endpoint: Option<SocketAddrV4>,
    pub data: Arc<Mutex<Vec<f64>>>,
    pub clock: Clock,
    pub measurements_started: Arc<Mutex<usize>>,
    pub success: bool,
    rx_timer: Option<Receiver<TimerStatus>>,
    tx_timer: Sender<TimerStatus>,
}

impl Measurement {
    pub async fn new(socket: Arc<UdpSocket>, state: &PeerState, clock: Clock) -> Self {
        let (tx_timer, rx_timer) = mpsc::channel(1);

        let mut measurement = Measurement {
            socket: Some(socket.clone()),
            session_id: state.node_state.session_id.clone(),
            endpoint: state.endpoint,
            data: Arc::new(Mutex::new(vec![])),
            clock,
            measurements_started: Arc::new(Mutex::new(0)),
            success: false,
            tx_timer,
            rx_timer: Some(rx_timer),
        };

        info!("measurement on gateway");

        let ht = HostTime::new(clock.micros());
        send_ping(
            socket,
            &Payload {
                entries: vec![PayloadEntry::HostTime(ht)],
            },
        )
        .await;

        measurement.reset_timer().await;
        measurement.listen().await;

        measurement
    }

    #[async_recursion]
    pub async fn reset_timer(&mut self) {
        tokio::select! {
            _ = tokio::time::sleep(Duration::milliseconds(50).to_std().unwrap()) => {
                if *self.measurements_started.lock().unwrap() < NUMBER_MEASUREMENTS {
                    let ht = HostTime {
                        time: self.clock.micros(),
                    };
                    send_ping(
                        self.socket.as_ref().unwrap().clone(),
                        &Payload {
                            entries: vec![PayloadEntry::HostTime(ht)],
                        },
                    )
                    .await;

                    *self.measurements_started.lock().unwrap() += 1;
                    self.reset_timer().await;
                } else {
                    self.data.lock().unwrap().clear();
                    info!("measuring {} failed", self.endpoint.unwrap());
                }
            }
            Some(timer_status) = self.rx_timer.as_mut().unwrap().recv() => {
                match timer_status {
                    TimerStatus::Finish => {
                        self.finish();
                    }
                    TimerStatus::Reset => {
                        self.reset_timer().await;
                    }
                }
            }
        }
    }

    fn finish(&mut self) {
        self.success = true;
        info!("measuring {} done", self.endpoint.unwrap());
    }

    pub async fn listen(&mut self) {
        let socket = self.socket.as_ref().unwrap().clone();
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

                    if *s_id.lock().unwrap() == session_id {
                        let host_time = clock.micros();
                        let payload = Payload {
                            entries: vec![
                                PayloadEntry::HostTime(HostTime { time: host_time }),
                                PayloadEntry::PrevGhostTime(PrevGhostTime {
                                    time: prev_ghost_time,
                                }),
                            ],
                        };

                        send_ping(socket.clone(), &payload).await;

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

                        if data.lock().unwrap().len() > NUMBER_DATA_POINTS {
                            tx_timer.send(TimerStatus::Finish).await.unwrap();
                        } else {
                            tx_timer.send(TimerStatus::Reset).await.unwrap();
                        }
                    }
                }
            }
        });
    }
}

pub async fn send_ping(socket: Arc<UdpSocket>, payload: &Payload) {
    todo!()
}
