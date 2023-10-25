use std::mem;

use chrono::Duration;
use tracing::info;

use crate::discovery::ENCODING_CONFIG;

use super::{
    beats::{Beats, BEATS_SIZE},
    ghostxform::GhostXForm,
    payload::PayloadEntryHeader,
    tempo::{Tempo, TEMPO_SIZE},
    Result,
};

pub const TIMELINE_HEADER_KEY: u32 = u32::from_be_bytes(*b"tmln");
pub const TIMELINE_SIZE: u32 = TEMPO_SIZE + BEATS_SIZE + mem::size_of::<u64>() as u32;
pub const TIMELINE_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: TIMELINE_HEADER_KEY,
    size: TIMELINE_SIZE,
};

#[derive(PartialEq, Clone, Debug, Copy)]
pub struct Timeline {
    pub tempo: Tempo,
    pub beat_origin: Beats,
    pub time_origin: Duration,
}

impl Timeline {
    pub fn to_beats(&self, time: Duration) -> Beats {
        self.beat_origin + self.tempo.micros_to_beats(time - self.time_origin)
    }

    pub fn from_beats(&self, beats: Beats) -> Duration {
        self.time_origin + self.tempo.beats_to_micros(beats - self.beat_origin)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = TIMELINE_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(self, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            tempo: Tempo::default(),
            beat_origin: Beats { value: 0i64 },
            time_origin: Duration::zero(),
        }
    }
}

impl bincode::Encode for Timeline {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(
            &(
                self.tempo,
                self.beat_origin,
                self.time_origin.num_microseconds().unwrap(),
            ),
            encoder,
        )
    }
}

impl bincode::Decode for Timeline {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        let (tempo, beat_origin, time_origin) = bincode::Decode::decode(decoder)?;
        Ok(Self {
            tempo,
            beat_origin,
            time_origin: Duration::microseconds(time_origin),
        })
    }
}

pub fn clamp_tempo(timeline: Timeline) -> Timeline {
    const MIN_BPM: f64 = 20.0;
    const MAX_BPM: f64 = 999.0;

    Timeline {
        tempo: Tempo {
            value: f64::min(f64::max(timeline.tempo.bpm(), MIN_BPM), MAX_BPM),
        },
        beat_origin: timeline.beat_origin,
        time_origin: timeline.time_origin,
    }
}

pub fn update_client_timeline_from_session(
    cur_client: Timeline,
    session: Timeline,
    at_time: Duration,
    x_form: GhostXForm,
) -> Timeline {
    let temp_tl = Timeline {
        tempo: session.tempo,
        beat_origin: cur_client.to_beats(at_time),
        time_origin: at_time,
    };

    let host_beat_zero = x_form.ghost_to_host(session.from_beats(Beats { value: 0i64 }));
    Timeline {
        tempo: temp_tl.tempo,
        beat_origin: temp_tl.to_beats(host_beat_zero),
        time_origin: host_beat_zero,
    }
}

pub fn update_session_timeline_from_client(
    cur_session: Timeline,
    client: Timeline,
    at_time: Duration,
    x_form: GhostXForm,
) -> Timeline {
    let ghost_beat_0 = x_form.host_to_ghost(client.time_origin);

    let zero = Beats { value: 0i64 };

    if cur_session.to_beats(ghost_beat_0) == zero && client.tempo == cur_session.tempo {
        return cur_session;
    }

    let temp_tl = Timeline {
        tempo: client.tempo,
        beat_origin: zero,
        time_origin: ghost_beat_0,
    };

    let new_beat_origin = Beats {
        value: i64::max(
            cur_session.to_beats(x_form.host_to_ghost(at_time)).value,
            cur_session.beat_origin.value + 1_i64,
        ),
    };

    Timeline {
        tempo: client.tempo,
        beat_origin: new_beat_origin,
        time_origin: temp_tl.from_beats(new_beat_origin),
    }
}

pub fn shift_client_timeline(mut client: Timeline, shift: Beats) -> Timeline {
    let time_delta = client.from_beats(shift) - client.from_beats(Beats { value: 0i64 });
    client.time_origin = client.time_origin - time_delta;
    client
}
