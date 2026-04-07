use core::mem;

use alloc::vec::Vec;
use chrono::Duration;

use crate::ENCODING_CONFIG;

use super::{
    beats::{Beats, BEATS_SIZE},
    encoding::PayloadEntryHeader,
    ghostxform::GhostXForm,
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

impl Default for Timeline {
    fn default() -> Self {
        Self {
            tempo: Default::default(),
            beat_origin: Default::default(),
            time_origin: Duration::zero(),
        }
    }
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

impl bincode::Encode for Timeline {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> core::result::Result<(), bincode::error::EncodeError> {
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

impl bincode::Decode<()> for Timeline {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> core::result::Result<Self, bincode::error::DecodeError> {
        // Decode the raw i64 values as they are encoded
        let tempo_micros: i64 = bincode::Decode::decode(decoder)?;
        let beat_origin_micro_beats: i64 = bincode::Decode::decode(decoder)?;
        let time_origin_micros: i64 = bincode::Decode::decode(decoder)?;

        Ok(Self {
            tempo: Tempo::from(Duration::microseconds(tempo_micros)),
            beat_origin: Beats {
                value: beat_origin_micro_beats,
            },
            time_origin: Duration::microseconds(time_origin_micros),
        })
    }
}

pub fn clamp_tempo(timeline: Timeline) -> Timeline {
    const MIN_BPM: f64 = 20.0;
    const MAX_BPM: f64 = 999.0;

    Timeline {
        tempo: Tempo {
            value: timeline.tempo.bpm().clamp(MIN_BPM, MAX_BPM),
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

pub fn shift_client_timeline(client: Timeline, shift: Beats) -> Timeline {
    use super::phase::shift_client_timeline;
    shift_client_timeline(client, shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_to_client_updates_tempo() {
        let b0 = Beats::new(-1.0);
        let t0 = Duration::microseconds(-1);
        let x_form = GhostXForm {
            slope: 1.0,
            intercept: Duration::microseconds(-1000000),
        };

        let cur_client = Timeline {
            tempo: Tempo { value: 60.0 },
            beat_origin: b0,
            time_origin: t0,
        };
        let session = Timeline {
            tempo: Tempo { value: 90.0 },
            beat_origin: b0,
            time_origin: x_form.host_to_ghost(t0),
        };
        let new_client = update_client_timeline_from_session(cur_client, session, t0, x_form);
        assert_eq!(new_client.tempo, Tempo { value: 90.0 });
    }

    #[test]
    fn time_to_beats() {
        let t160 = Timeline {
            tempo: Tempo::new(60.0),
            beat_origin: Beats::new(-1.0),
            time_origin: Duration::microseconds(1000000),
        };

        assert_eq!(
            t160.to_beats(Duration::microseconds(4500000)),
            Beats::new(2.5)
        );
    }

    #[test]
    fn beats_to_time() {
        let t160 = Timeline {
            tempo: Tempo::new(60.0),
            beat_origin: Beats::new(-1.0),
            time_origin: Duration::microseconds(1000000),
        };

        assert_eq!(
            t160.from_beats(Beats::new(3.2)),
            Duration::microseconds(5200000)
        );
    }

    #[test]
    fn clamp_tempo_within_range() {
        let tl = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        let clamped = clamp_tempo(tl);
        assert_eq!(clamped.tempo.bpm(), 120.0);
    }

    #[test]
    fn clamp_tempo_below_minimum() {
        let tl = Timeline {
            tempo: Tempo::new(5.0),
            beat_origin: Beats::new(1.0),
            time_origin: Duration::microseconds(100),
        };
        let clamped = clamp_tempo(tl);
        assert_eq!(clamped.tempo.bpm(), 20.0);
        // beat_origin and time_origin should be preserved
        assert_eq!(clamped.beat_origin, tl.beat_origin);
        assert_eq!(clamped.time_origin, tl.time_origin);
    }

    #[test]
    fn clamp_tempo_above_maximum() {
        let tl = Timeline {
            tempo: Tempo::new(1500.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        let clamped = clamp_tempo(tl);
        assert_eq!(clamped.tempo.bpm(), 999.0);
    }

    #[test]
    fn clamp_tempo_at_boundaries() {
        let tl_min = Timeline {
            tempo: Tempo::new(20.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        assert_eq!(clamp_tempo(tl_min).tempo.bpm(), 20.0);

        let tl_max = Timeline {
            tempo: Tempo::new(999.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        assert_eq!(clamp_tempo(tl_max).tempo.bpm(), 999.0);
    }

    #[test]
    fn timeline_default_values() {
        let tl = Timeline::default();
        assert_eq!(tl.tempo.bpm(), 0.0);
        assert_eq!(tl.beat_origin, Beats::default());
        assert_eq!(tl.time_origin, Duration::zero());
    }

    #[test]
    fn timeline_encode_roundtrip() {
        let tl = Timeline {
            tempo: Tempo::new(128.0),
            beat_origin: Beats::new(4.0),
            time_origin: Duration::microseconds(2_000_000),
        };
        let encoded = tl.encode().unwrap();
        assert!(!encoded.is_empty());
    }

    #[test]
    fn to_beats_and_from_beats_roundtrip() {
        let tl = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        let time = Duration::microseconds(3_000_000);
        let beats = tl.to_beats(time);
        let recovered = tl.from_beats(beats);
        assert_eq!(recovered, time);
    }

    #[test]
    fn to_beats_with_offset_origin() {
        let tl = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(8.0),
            time_origin: Duration::microseconds(1_000_000),
        };
        // At 120 BPM, 1 second later (t=2s) should be 2 beats later → beat 10.0
        let beats = tl.to_beats(Duration::microseconds(2_000_000));
        assert!((beats.floating() - 10.0).abs() < 1e-6);
    }
}
