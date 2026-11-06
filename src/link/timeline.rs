use core::mem;

use alloc::vec::Vec;
use chrono::Duration;

use crate::encoding::{self, Decode, Encode};

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
        Beats::from_microbeats(
            self.microbeats_at(time)
                .clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        )
    }

    pub(crate) fn microbeats_at(&self, time: Duration) -> i128 {
        self.microbeats_at_nanos(super::duration_nanos(time))
    }

    fn microbeats_at_nanos(&self, nanos: i128) -> i128 {
        let elapsed = nanos.saturating_sub(super::duration_nanos(self.time_origin));
        self.tempo
            .microbeats_from_nanos(elapsed)
            .saturating_add(i128::from(self.beat_origin.value))
    }

    pub fn from_beats(&self, beats: Beats) -> Duration {
        self.time_at_microbeats(beats.value.into())
    }

    pub(crate) fn time_at_microbeats(&self, beats: i128) -> Duration {
        super::duration_from_nanos(self.nanos_at_microbeats(beats))
    }

    pub(crate) fn nanos_at_microbeats(&self, beats: i128) -> i128 {
        let difference = beats - i128::from(self.beat_origin.value);
        let elapsed = self.tempo.nanos_from_microbeats(difference);
        super::duration_nanos(self.time_origin).saturating_add(elapsed)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = TIMELINE_HEADER.encode()?;
        encoded.append(&mut encoding::encode_to_vec(self)?);
        Ok(encoded)
    }
}

impl Encode for Timeline {
    fn encode_to(
        &self,
        out: &mut Vec<u8>,
    ) -> core::result::Result<(), crate::encoding::EncodeError> {
        self.tempo.encode_to(out)?;
        self.beat_origin.encode_to(out)?;
        super::micros(self.time_origin).encode_to(out)?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        self.tempo.encoded_size() + self.beat_origin.encoded_size() + 8
    }
}

impl Decode for Timeline {
    fn decode_from(bytes: &[u8]) -> core::result::Result<(Self, usize), encoding::DecodeError> {
        let (tempo_micros, n1) = i64::decode_from(bytes)?;
        let (beat_origin_micro_beats, n2) = i64::decode_from(&bytes[n1..])?;
        let (time_origin_micros, n3) = i64::decode_from(&bytes[n1 + n2..])?;

        Ok((
            Self {
                tempo: Tempo::from(Duration::microseconds(tempo_micros)),
                beat_origin: Beats {
                    value: beat_origin_micro_beats,
                },
                time_origin: Duration::microseconds(time_origin_micros),
            },
            n1 + n2 + n3,
        ))
    }
}

pub fn clamp_tempo(timeline: Timeline) -> Timeline {
    const MIN_BPM: f64 = 20.0;
    const MAX_BPM: f64 = 999.0;
    let bpm = timeline.tempo.bpm();

    Timeline {
        tempo: Tempo {
            value: if bpm.is_nan() {
                120.0
            } else {
                bpm.clamp(MIN_BPM, MAX_BPM)
            },
        },
        beat_origin: timeline.beat_origin,
        time_origin: timeline.time_origin,
    }
}

/// Preserve the mapped session-zero phase boundary and the current beat.
/// Reject an unrepresentable origin rather than clamping or moving that boundary.
pub fn update_client_timeline_from_session(
    cur_client: Timeline,
    session: Timeline,
    at_time: Duration,
    x_form: GhostXForm,
) -> core::result::Result<Timeline, super::error::CommitError> {
    let host_beat_zero = x_form
        .checked_ghost_nanos_to_host(session.nanos_at_microbeats(0))
        .ok_or(super::error::CommitError::TimelineOriginExhausted)?;
    let elapsed = super::duration_nanos(host_beat_zero) - super::duration_nanos(at_time);
    let beat_origin = cur_client
        .microbeats_at(at_time)
        .checked_add(session.tempo.microbeats_from_nanos(elapsed))
        .and_then(|origin| i64::try_from(origin).ok())
        .ok_or(super::error::CommitError::TimelineOriginExhausted)?;
    let timeline = Timeline {
        tempo: session.tempo,
        beat_origin: Beats::from_microbeats(beat_origin),
        time_origin: host_beat_zero,
    };
    if timeline.microbeats_at(at_time) != cur_client.microbeats_at(at_time) {
        return Err(super::error::CommitError::TimelineOriginExhausted);
    }
    Ok(timeline)
}

pub fn update_session_timeline_from_client(
    cur_session: Timeline,
    client: Timeline,
    at_time: Duration,
    x_form: GhostXForm,
) -> core::result::Result<Timeline, super::error::CommitError> {
    let ghost_beat_0 = x_form.host_to_ghost_wide_nanos(client.time_origin);

    if cur_session.nanos_at_microbeats(0) == ghost_beat_0 && client.tempo == cur_session.tempo {
        return Ok(cur_session);
    }

    let new_beat_origin = Beats {
        value: i64::max(
            cur_session
                .microbeats_at_nanos(x_form.host_to_ghost_wide_nanos(at_time))
                .clamp(i64::MIN as i128, i64::MAX as i128) as i64,
            cur_session
                .beat_origin
                .value
                .checked_add(1)
                .ok_or(super::error::CommitError::TimelineOriginExhausted)?,
        ),
    };

    let nanos = ghost_beat_0
        .checked_add(
            client
                .tempo
                .nanos_from_microbeats(i128::from(new_beat_origin.value)),
        )
        .ok_or(super::error::CommitError::TimelineOriginExhausted)?;
    let time_origin = super::duration_from_nanos(nanos);
    if super::duration_nanos(time_origin) != nanos {
        return Err(super::error::CommitError::TimelineOriginExhausted);
    }
    Ok(Timeline {
        tempo: client.tempo,
        beat_origin: new_beat_origin,
        time_origin,
    })
}

pub fn shift_client_timeline(client: Timeline, shift: Beats) -> Timeline {
    use super::phase::shift_client_timeline;
    shift_client_timeline(client, shift)
}

#[cfg(test)]
mod tests {
    #[test]
    fn composition_preserves_fractional_canonical_zero() {
        let current = Timeline {
            tempo: Tempo::new(120.0),
            ..Timeline::default()
        };
        for sign in [-1, 1] {
            let client = Timeline {
                time_origin: Duration::nanoseconds(sign * 200),
                ..current
            };
            let updated = update_session_timeline_from_client(
                current,
                client,
                Duration::zero(),
                GhostXForm {
                    slope: 1.0,
                    intercept: Duration::zero(),
                },
            )
            .unwrap();

            assert_eq!(
                updated.from_beats(Beats::from_microbeats(0)),
                client.time_origin,
            );
            let probe = Duration::nanoseconds(sign * 300);
            assert_eq!(updated.to_beats(probe), client.to_beats(probe));
        }
    }

    #[test]
    fn precision_conversion_examples_match_documented_rounding() {
        let tempo = Tempo::new(120.0);
        let timeline = Timeline {
            tempo,
            ..Timeline::default()
        };
        let beat = Beats::from_microbeats(1);
        assert_eq!(Tempo::from(Duration::nanoseconds(500)).bpm(), 120_000_000.0);
        assert_eq!(tempo.micros_to_beats(Duration::nanoseconds(500)), beat);
        assert_eq!(timeline.to_beats(Duration::nanoseconds(500)), beat);
        assert_eq!(timeline.from_beats(beat), Duration::nanoseconds(500));
        assert_eq!(tempo.beats_to_micros(beat), Duration::microseconds(1));
    }

    #[test]
    fn composition_rejects_received_wire_endpoints_without_a_beat_jump() {
        let client = Timeline {
            tempo: Tempo::new(120.0),
            ..Timeline::default()
        };
        for sign in [-1, 1] {
            let session = Timeline {
                tempo: Tempo::new(120.0),
                beat_origin: Beats::from_microbeats(-sign * i64::MAX),
                time_origin: Duration::microseconds(sign * i64::MAX),
            };
            let mapped = update_client_timeline_from_session(
                client,
                session,
                Duration::zero(),
                GhostXForm {
                    slope: 1.0,
                    intercept: Duration::zero(),
                },
            );
            assert_eq!(
                mapped,
                Err(crate::link::error::CommitError::TimelineOriginExhausted)
            );
        }
    }

    #[test]
    fn composition_rejects_a_phase_changing_alternate_client_anchor() {
        let client = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::from_microbeats(i64::MAX),
            time_origin: Duration::zero(),
        };
        let session = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::from_microbeats(-i64::MAX),
            time_origin: Duration::microseconds(i64::MAX),
        };
        let at_time = Duration::microseconds(1);
        let mapped = update_client_timeline_from_session(
            client,
            session,
            at_time,
            GhostXForm {
                slope: 1.0,
                intercept: Duration::zero(),
            },
        );
        assert_eq!(
            mapped,
            Err(crate::link::error::CommitError::TimelineOriginExhausted)
        );
        let observation_anchor = Timeline {
            tempo: Tempo::new(120.0),
            ..Timeline::default()
        };
        assert_eq!(observation_anchor.microbeats_at(Duration::zero()), 0);
        let intended_origin = 3 * i128::from(i64::MAX);
        assert_eq!(intended_origin.rem_euclid(2), 1);
        assert_eq!(observation_anchor.beat_origin.value.rem_euclid(2), 0);
        let same_phase = Timeline {
            beat_origin: Beats::from_microbeats(1),
            time_origin: Duration::nanoseconds(500),
            ..observation_anchor
        };
        assert_eq!(same_phase.microbeats_at(Duration::zero()), 0);
        let quantum = Beats::from_microbeats(2);
        assert_ne!(
            crate::link::phase::to_phase_encoded_beats(&same_phase, Duration::zero(), quantum),
            crate::link::phase::to_phase_encoded_beats(
                &observation_anchor,
                Duration::zero(),
                quantum,
            )
        );
    }

    #[test]
    fn composition_rejects_a_clamped_mapped_boundary_despite_raw_continuity() {
        let client = Timeline {
            tempo: Tempo::new(120.0),
            time_origin: Duration::MAX,
            ..Timeline::default()
        };
        let session = Timeline {
            time_origin: Duration::microseconds(i64::MAX),
            ..client
        };
        assert_eq!(
            update_client_timeline_from_session(
                client,
                session,
                Duration::zero(),
                GhostXForm {
                    slope: 0.0001,
                    intercept: Duration::zero(),
                }
            ),
            Err(crate::link::error::CommitError::TimelineOriginExhausted)
        );
        for slope in [0.0, -0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                update_client_timeline_from_session(
                    client,
                    session,
                    Duration::zero(),
                    GhostXForm {
                        slope,
                        intercept: Duration::zero(),
                    }
                ),
                Err(crate::link::error::CommitError::TimelineOriginExhausted)
            );
        }
    }

    #[test]
    fn composition_rejects_an_unrepresentable_final_session_time() {
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            ..Timeline::default()
        };
        for time_origin in [Duration::MIN, Duration::MAX] {
            assert_eq!(
                update_session_timeline_from_client(
                    timeline,
                    Timeline {
                        time_origin,
                        ..timeline
                    },
                    Duration::zero(),
                    GhostXForm {
                        slope: 2.0,
                        intercept: Duration::zero(),
                    },
                ),
                Err(crate::link::error::CommitError::TimelineOriginExhausted)
            );
        }
    }

    #[test]
    fn duration_boundary_wide_forward_endpoint_survives_composition() {
        let session = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::from_microbeats(999),
            time_origin: Duration::zero(),
        };
        let client = Timeline {
            tempo: Tempo::new(1e-32),
            time_origin: Duration::nanoseconds(-1),
            ..Timeline::default()
        };
        let mapped = update_session_timeline_from_client(
            session,
            client,
            Duration::zero(),
            GhostXForm {
                slope: f64::from_bits(1150_u64 << 52),
                intercept: Duration::zero(),
            },
        )
        .unwrap();
        assert_eq!(mapped.beat_origin.value, 1_000);
        assert_eq!(mapped.time_origin, Duration::nanoseconds(-1));
    }

    #[test]
    fn duration_boundary_composition_does_not_narrow_the_client_beat() {
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            ..Timeline::default()
        };
        for at_time in [Duration::MIN, Duration::MAX] {
            let mapped = update_client_timeline_from_session(
                timeline,
                timeline,
                at_time,
                GhostXForm {
                    slope: 1.0,
                    intercept: Duration::zero(),
                },
            )
            .unwrap();
            assert_eq!(mapped, timeline);
        }
    }

    #[test]
    fn duration_boundary_composition_keeps_a_representable_session_origin() {
        for (edge, sign) in [(Duration::MIN, -1_i128), (Duration::MAX, 1)] {
            for slope in [-2_i128, 2] {
                let ghost_origin = crate::link::duration_nanos(edge) + sign * 100;
                let client = Timeline {
                    tempo: Tempo::new(120.0),
                    time_origin: crate::link::duration_from_nanos(ghost_origin / slope),
                    ..Timeline::default()
                };
                let session = Timeline {
                    tempo: Tempo::new(120.0),
                    beat_origin: Beats::from_microbeats(if sign > 0 { -2 } else { 0 }),
                    ..Timeline::default()
                };
                let mapped = update_session_timeline_from_client(
                    session,
                    client,
                    Duration::zero(),
                    GhostXForm {
                        slope: slope as f64,
                        intercept: Duration::zero(),
                    },
                )
                .unwrap();
                assert_eq!(mapped.beat_origin.value, -sign as i64);
                assert_eq!(
                    crate::link::duration_nanos(mapped.time_origin),
                    ghost_origin - sign * 500
                );
            }
        }
    }

    #[test]
    fn wide_beat_zero_is_mapped_before_chrono_clamping() {
        for sign in [-1, 1] {
            for slope in [-2, 2] {
                let session = Timeline {
                    tempo: Tempo::new(60.0),
                    beat_origin: Beats::new(-f64::from(sign)),
                    time_origin: if sign < 0 {
                        Duration::MIN
                    } else {
                        Duration::MAX
                    },
                };
                let wide = crate::link::duration_nanos(session.time_origin)
                    + i128::from(sign) * 1_000_000_000;
                let expected = crate::link::duration_from_nanos(crate::link::round_ratio(
                    wide,
                    i128::from(slope),
                ));
                let client = Timeline {
                    tempo: session.tempo,
                    time_origin: expected,
                    ..Timeline::default()
                };
                let mapped = update_client_timeline_from_session(
                    client,
                    session,
                    Duration::zero(),
                    GhostXForm {
                        slope: f64::from(slope),
                        intercept: Duration::zero(),
                    },
                )
                .unwrap();
                assert_eq!(mapped, client);
            }
        }
    }

    #[test]
    fn wide_boundary_timeline_converts_before_narrowing() {
        let timeline = Timeline {
            tempo: Tempo::new(20.0),
            beat_origin: Beats::default(),
            time_origin: Duration::microseconds(i64::MIN),
        };
        assert_eq!(
            timeline.to_beats(Duration::microseconds(i64::MAX)).value,
            6_148_914_691_236_517_205
        );
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::from_microbeats(i64::MIN),
            time_origin: Duration::zero(),
        };
        assert_eq!(
            timeline.from_beats(Beats::from_microbeats(i64::MAX)),
            Duration::microseconds(i64::MAX) + Duration::nanoseconds(500)
        );
        let timeline = Timeline {
            time_origin: Duration::microseconds(-i64::MAX),
            ..timeline
        };
        assert_eq!(
            timeline.from_beats(Beats::from_microbeats(i64::MAX)),
            Duration::nanoseconds(500)
        );
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::from_microbeats(i64::MIN),
            time_origin: Duration::zero(),
        };
        assert_eq!(
            timeline.to_beats(Duration::microseconds(i64::MAX)).value,
            i64::MAX - 1
        );
        let reversed = Timeline {
            beat_origin: Beats::from_microbeats(i64::MAX),
            ..timeline
        };
        assert_eq!(
            reversed.from_beats(Beats::from_microbeats(i64::MIN)),
            -Duration::microseconds(i64::MAX) - Duration::nanoseconds(500)
        );
        assert_eq!(
            timeline.from_beats(Beats::from_microbeats(i64::MIN + 1)),
            Duration::nanoseconds(500)
        );
    }

    use super::*;

    #[test]
    fn timeline_encoding_saturates_both_wire_time_boundaries() {
        for limit in [i64::MIN, i64::MAX] {
            for delta in [-1, 0, 1] {
                let expected = limit.saturating_add(delta);
                let timeline = Timeline {
                    tempo: Tempo::new(120.0),
                    time_origin: Duration::microseconds(limit) + Duration::microseconds(delta),
                    ..Timeline::default()
                };
                let bytes = crate::encoding::encode_to_vec(&timeline).unwrap();
                assert_eq!(&bytes[bytes.len() - 8..], &expected.to_be_bytes());
                let (decoded, consumed) = Timeline::decode_from(&bytes).unwrap();
                assert_eq!(decoded.time_origin, Duration::microseconds(expected));
                assert_eq!(consumed, bytes.len());
                assert_eq!(decoded.tempo, timeline.tempo);
                assert_eq!(decoded.beat_origin, timeline.beat_origin);
            }
        }
    }

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
        let new_client =
            update_client_timeline_from_session(cur_client, session, t0, x_form).unwrap();
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
