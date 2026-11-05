use chrono::Duration;

use super::beats::Beats;
use super::timeline::Timeline;

fn narrow(beats: i128) -> Beats {
    Beats::from_microbeats(beats.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

fn phase_wide(beats: i128, quantum: i128) -> i128 {
    if quantum == 0 {
        0
    } else {
        beats.rem_euclid(quantum.abs())
    }
}

/// Return the nonnegative remainder modulo the quantum magnitude, or zero
/// for a zero quantum.
pub fn phase(beats: Beats, quantum: Beats) -> Beats {
    narrow(phase_wide(
        i128::from(beats.value),
        i128::from(quantum.value),
    ))
}

fn next_phase_match_wide(x: i128, target: i128, quantum: i128) -> i128 {
    let offset = phase_wide(
        phase_wide(target, quantum) - phase_wide(x, quantum),
        quantum,
    );
    x.saturating_add(offset)
}

fn closest_phase_match_wide(x: i128, target: i128, quantum: i128) -> i128 {
    let quantum = quantum.abs();
    if quantum == 0 {
        return x;
    }
    let offset = phase_wide(
        phase_wide(target, quantum) - phase_wide(x, quantum),
        quantum,
    );
    x.saturating_add(if offset * 2 >= quantum {
        offset - quantum
    } else {
        offset
    })
}

/// Return the least value at or above x matching the target phase.
/// A zero quantum returns x; an unrepresentable result saturates.
pub fn next_phase_match(x: Beats, target: Beats, quantum: Beats) -> Beats {
    narrow(next_phase_match_wide(
        x.value.into(),
        target.value.into(),
        quantum.value.into(),
    ))
}

/// Return the closest phase match, choosing the lower match at a tie.
pub fn closest_phase_match(x: Beats, target: Beats, quantum: Beats) -> Beats {
    narrow(closest_phase_match_wide(
        x.value.into(),
        target.value.into(),
        quantum.value.into(),
    ))
}

fn to_phase_encoded_microbeats(tl: &Timeline, time: Duration, quantum: Beats) -> i128 {
    let beat = tl.microbeats_at(time);
    closest_phase_match_wide(
        beat,
        phase_wide(beat, quantum.value.into())
            - phase_wide(tl.beat_origin.value.into(), quantum.value.into()),
        quantum.value.into(),
    )
}

/// Interpret the timeline origin as a quantum boundary and return the
/// corresponding phase-encoded beat value, saturating only the final result.
pub fn to_phase_encoded_beats(tl: &Timeline, time: Duration, quantum: Beats) -> Beats {
    narrow(to_phase_encoded_microbeats(tl, time, quantum))
}

/// Invert phase encoding, rounding up at a phase midpoint to undo the
/// forward conversion's downward tie break.
pub fn from_phase_encoded_beats(tl: &Timeline, beat: Beats, quantum: Beats) -> Duration {
    let beat = i128::from(beat.value);
    let origin = i128::from(tl.beat_origin.value);
    let quantum = i128::from(quantum.value).abs();
    let from_origin = beat - origin;
    let remainder = phase_wide(from_origin, quantum);
    let origin_offset = from_origin - remainder;
    let inverse_phase_offset = closest_phase_match_wide(
        quantum - remainder,
        quantum - phase_wide(beat, quantum),
        quantum,
    );
    tl.time_at_microbeats(origin + origin_offset + quantum - inverse_phase_offset)
}

/// Force beat at time implementation for timeline modification
pub fn force_beat_at_time_impl(
    timeline: &mut Timeline,
    beat: Beats,
    time: Duration,
    quantum: Beats,
) {
    // There are two components to the beat adjustment: a phase shift
    // and a beat magnitude adjustment.
    let cur_beat_at_time = to_phase_encoded_microbeats(timeline, time, quantum);
    let closest_in_phase =
        closest_phase_match_wide(cur_beat_at_time, beat.value.into(), quantum.value.into());
    let time_origin = shifted_time_origin_nanos(*timeline, closest_in_phase - cur_beat_at_time);
    // Narrow only after both magnitude adjustments have cancelled.
    let beat_origin = (i128::from(timeline.beat_origin.value) + i128::from(beat.value))
        .saturating_sub(closest_in_phase);
    let min_time = super::duration_nanos(Duration::MIN);
    let max_time = super::duration_nanos(Duration::MAX);
    let out_of_range = !(i64::MIN as i128..=i64::MAX as i128).contains(&beat_origin)
        || !(min_time..=max_time).contains(&time_origin);
    let adjusted = Timeline {
        beat_origin: narrow(beat_origin),
        time_origin: super::duration_from_nanos(time_origin),
        ..*timeline
    };
    if out_of_range
        || to_phase_encoded_microbeats(&adjusted, time, quantum) != i128::from(beat.value)
    {
        // The effective boundary is the closest quantum multiple to the stored
        // origin, and can lie just outside that origin's i64 range.
        let boundary = i128::from(beat.value) - phase_wide(beat.value.into(), quantum.value.into());
        // Prefer exact nanosecond anchors, retaining the old quantized fallback.
        for precise in [true, false] {
            if !precise && !out_of_range {
                break;
            }
            for boundary in [boundary, boundary + i128::from(quantum.value).abs()] {
                let origin = narrow(boundary);
                if closest_phase_match_wide(origin.value.into(), 0, quantum.value.into())
                    != boundary
                {
                    continue;
                }
                let offset = i128::from(beat.value) - boundary;
                let elapsed = if precise {
                    let period = timeline.tempo.period_micros();
                    offset
                        .saturating_mul(period / 1_000)
                        .saturating_add(super::round_ratio(offset * (period % 1_000), 1_000))
                } else {
                    timeline
                        .tempo
                        .micros_from_microbeats(offset)
                        .saturating_mul(1_000)
                };
                let anchored_time = super::duration_nanos(time).saturating_sub(elapsed);
                if precise || (min_time..=max_time).contains(&anchored_time) {
                    let anchored = Timeline {
                        beat_origin: origin,
                        time_origin: super::duration_from_nanos(anchored_time),
                        ..*timeline
                    };
                    if !precise
                        || to_phase_encoded_microbeats(&anchored, time, quantum)
                            == i128::from(beat.value)
                    {
                        *timeline = anchored;
                        return;
                    }
                }
            }
        }
    }
    *timeline = adjusted;
}

/// Shift client timeline by the given beat offset
pub fn shift_client_timeline(timeline: Timeline, shift: Beats) -> Timeline {
    shift_client_timeline_wide(timeline, shift.value.into())
}

fn shift_client_timeline_wide(mut timeline: Timeline, shift: i128) -> Timeline {
    timeline.time_origin = super::duration_from_nanos(shifted_time_origin_nanos(timeline, shift));
    timeline
}

fn shifted_time_origin_nanos(timeline: Timeline, shift: i128) -> i128 {
    let origin = i128::from(timeline.beat_origin.value);
    let period = timeline.tempo.period_micros();
    // Cancel the whole-period terms before multiplication, preserving each
    // original conversion's rounding even when their magnitudes exceed i128.
    let remainder = period % 1_000_000;
    let delta = shift.saturating_mul(period / 1_000_000).saturating_add(
        super::round_ratio((shift - origin) * remainder, 1_000_000)
            - super::round_ratio(-origin * remainder, 1_000_000),
    );
    super::duration_nanos(timeline.time_origin).saturating_sub(delta.saturating_mul(1_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::tempo::Tempo;

    #[test]
    fn force_microbeats_accepts_exact_clamped_nanosecond_anchors() {
        for (bpm, beat, quantum, time, anchor) in [
            (
                60.0,
                i64::MIN,
                i64::MAX,
                Duration::MAX - Duration::nanoseconds(999),
                Duration::MAX,
            ),
            (
                -60.0,
                i64::MAX,
                3,
                Duration::MAX - Duration::nanoseconds(999),
                Duration::MAX,
            ),
            (
                60_000_000.0 / 1999.0,
                i64::MAX,
                3,
                Duration::MIN + Duration::nanoseconds(1),
                Duration::MIN,
            ),
            (
                -60_000_000.0 / 1999.0,
                i64::MAX,
                3,
                Duration::MAX - Duration::nanoseconds(1),
                Duration::MAX,
            ),
        ] {
            let mut timeline = Timeline {
                tempo: Tempo::new(bpm),
                beat_origin: Beats::default(),
                time_origin: Duration::zero(),
            };
            let beat = Beats::from_microbeats(beat);
            let quantum = Beats::from_microbeats(quantum);
            force_beat_at_time_impl(&mut timeline, beat, time, quantum);
            assert_eq!(to_phase_encoded_beats(&timeline, time, quantum), beat);
            assert_eq!(timeline.time_origin, anchor);
        }
    }

    #[test]
    fn force_microbeats_preserves_nanosecond_anchors_at_duration_limits() {
        for (bpm, microbeats) in [(120.0, 1), (150.0, 2), (-120.0, -1), (-150.0, -2)] {
            for time in [
                Duration::MAX,
                Duration::MIN,
                Duration::MAX - Duration::nanoseconds(1),
            ] {
                for origin in [Duration::zero(), time] {
                    for quantum in [Beats::new(4.0), Beats::new(-4.0), Beats::default()] {
                        let mut timeline = Timeline {
                            tempo: Tempo::new(bpm),
                            beat_origin: Beats::default(),
                            time_origin: origin,
                        };
                        let beat = Beats::from_microbeats(microbeats);
                        force_beat_at_time_impl(&mut timeline, beat, time, quantum);
                        assert_eq!(
                            to_phase_encoded_beats(&timeline, time, quantum),
                            beat,
                            "bpm={bpm}, time={time}, origin={origin}, quantum={quantum:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn review_round_force_beat_keeps_effective_quantum_boundaries() {
        for (time, beat, quantum) in [
            (Duration::MIN, i64::MAX, i64::MIN),
            (Duration::MAX, i64::MIN, 4_000_000),
        ] {
            let mut timeline = Timeline {
                tempo: Tempo::new(60.0),
                beat_origin: Beats::default(),
                time_origin: Duration::zero(),
            };
            let beat = Beats::from_microbeats(beat);
            let quantum = Beats::from_microbeats(quantum);
            force_beat_at_time_impl(&mut timeline, beat, time, quantum);
            assert_eq!(to_phase_encoded_beats(&timeline, time, quantum), beat);
        }
    }

    #[test]
    fn review_round_force_beat_reanchors_large_times() {
        for time in [
            Duration::microseconds(i64::MAX),
            Duration::microseconds(i64::MIN),
            Duration::MAX,
            Duration::MIN,
        ] {
            for quantum in [0.0, 4.0, -4.0] {
                for beat in [-1.0, 0.0, 1.0] {
                    let mut timeline = Timeline {
                        tempo: Tempo::new(120.0),
                        beat_origin: Beats::default(),
                        time_origin: Duration::zero(),
                    };
                    let beat = Beats::new(beat);
                    let quantum = Beats::new(quantum);
                    force_beat_at_time_impl(&mut timeline, beat, time, quantum);
                    assert_eq!(to_phase_encoded_beats(&timeline, time, quantum), beat);
                }
            }
        }
    }

    #[test]
    fn force_beat_reanchors_zero_period_magnitude() {
        for micros in [-1, 1] {
            let mut timeline = Timeline {
                tempo: Tempo::new(1e20),
                beat_origin: Beats::default(),
                time_origin: Duration::zero(),
            };
            force_beat_at_time_impl(
                &mut timeline,
                Beats::default(),
                Duration::microseconds(micros),
                Beats::default(),
            );
            assert_eq!(timeline.beat_origin.value, 0);
            assert_eq!(timeline.time_origin, Duration::microseconds(micros));
            assert_eq!(
                to_phase_encoded_beats(&timeline, Duration::microseconds(micros), Beats::default()),
                Beats::default()
            );
        }
    }

    #[test]
    fn numeric_cleanup_phase_preserves_extreme_origin_roundtrips() {
        for (origin, time) in [
            (
                Duration::zero(),
                Duration::microseconds((1_i64 << 62) + 1_000_000),
            ),
            (
                Duration::microseconds(-(1_i64 << 62)),
                Duration::microseconds(500_000),
            ),
        ] {
            let timeline = Timeline {
                tempo: Tempo::new(120.0),
                beat_origin: Beats::from_microbeats(i64::MIN),
                time_origin: origin,
            };
            let encoded = to_phase_encoded_beats(&timeline, time, Beats::new(4.0));
            assert_eq!(
                from_phase_encoded_beats(&timeline, encoded, Beats::new(4.0)),
                time
            );
        }
        // A wide relative difference must be reduced before composing a small result.
        assert_eq!(
            closest_phase_match(
                Beats::from_microbeats(i64::MIN),
                Beats::default(),
                Beats::new(4.0)
            )
            .value,
            i64::MIN
        );
        assert_eq!(
            closest_phase_match(
                Beats::from_microbeats(i64::MAX),
                Beats::default(),
                Beats::new(4.0)
            )
            .value,
            i64::MAX
        );
    }

    #[test]
    fn numeric_cleanup_phase_composes_before_final_time_conversion() {
        for sign in [-1, 1] {
            let timeline = Timeline {
                tempo: Tempo::new(120.0),
                beat_origin: Beats::new(sign as f64),
                time_origin: Duration::zero(),
            };
            let time = Duration::microseconds(sign * ((1_i64 << 62) - 250_000));
            let encoded = to_phase_encoded_beats(&timeline, time, Beats::new(4.0));
            assert_ne!(encoded.value, if sign < 0 { i64::MIN } else { i64::MAX });
            assert_eq!(
                from_phase_encoded_beats(&timeline, encoded, Beats::new(4.0)),
                time
            );
        }
        for (origin, beat, expected) in [
            (i64::MIN, i64::MAX, Duration::MAX),
            (i64::MAX, i64::MIN, Duration::MIN),
        ] {
            let timeline = Timeline {
                tempo: Tempo::new(1e-20),
                beat_origin: Beats::from_microbeats(origin),
                time_origin: Duration::zero(),
            };
            assert_eq!(
                from_phase_encoded_beats(&timeline, Beats::from_microbeats(beat), Beats::new(4.0)),
                expected
            );
        }
        for origin in [i64::MIN, -3, 0, 3, i64::MAX] {
            let timeline = Timeline {
                tempo: Tempo::new(60.0),
                beat_origin: Beats::from_microbeats(origin),
                time_origin: Duration::microseconds(origin),
            };
            for quantum in [0, 1, 3, -3, 4_000_000, -4_000_000, i64::MIN] {
                let quantum = Beats::from_microbeats(quantum);
                let time = Duration::microseconds(500_000);
                let encoded = to_phase_encoded_beats(&timeline, time, quantum);
                assert_eq!(
                    from_phase_encoded_beats(&timeline, encoded, quantum),
                    time,
                    "origin={origin},quantum={quantum:?}"
                );
            }
        }
        assert_eq!(
            closest_phase_match(
                Beats::default(),
                Beats::from_microbeats(1),
                Beats::from_microbeats(3)
            )
            .value,
            1
        );
    }

    #[test]
    fn numeric_cleanup_shift_retains_whole_difference_and_origin() {
        let timeline = Timeline {
            tempo: Tempo::new(0.001),
            beat_origin: Beats::from_microbeats(-1),
            time_origin: Duration::zero(),
        };
        assert_eq!(
            shift_client_timeline(timeline, Beats::from_microbeats(i64::MIN)).time_origin,
            Duration::MAX
        );
        assert_eq!(
            shift_client_timeline(timeline, Beats::from_microbeats(i64::MAX)).time_origin,
            Duration::MIN
        );
        let timeline = Timeline {
            time_origin: Duration::MAX,
            ..timeline
        };
        let shift = 200_000_000_000_000_000_i64;
        assert_eq!(
            shift_client_timeline(timeline, Beats::from_microbeats(shift)).time_origin,
            super::super::duration_from_nanos(
                super::super::duration_nanos(Duration::MAX) - i128::from(shift) * 60_000 * 1_000
            )
        );
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            time_origin: Duration::zero(),
            ..timeline
        };
        assert_eq!(
            shift_client_timeline(timeline, Beats::from_microbeats(1)).time_origin,
            Duration::zero(),
            "preserve rounding of each beat-origin conversion"
        );
    }

    #[test]
    fn numeric_cleanup_force_beat_keeps_cancelling_magnitude_wide() {
        let mut high = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::from_microbeats(i64::MAX),
            time_origin: Duration::zero(),
        };
        force_beat_at_time_impl(
            &mut high,
            Beats::new(1.0),
            Duration::zero(),
            Beats::new(4.0),
        );
        assert_eq!(
            to_phase_encoded_beats(&high, Duration::zero(), Beats::new(4.0)),
            Beats::new(1.0)
        );
        let mut timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::from_microbeats(i64::MIN),
            time_origin: Duration::zero(),
        };
        let time = Duration::microseconds((1_i64 << 62) + 1_000_000);
        force_beat_at_time_impl(&mut timeline, Beats::new(1.0), time, Beats::new(4.0));
        assert_eq!(
            to_phase_encoded_beats(&timeline, time, Beats::new(4.0)),
            Beats::new(1.0)
        );
    }

    #[test]
    fn test_phase_positive() {
        let beats = Beats::new(5.5);
        let quantum = Beats::new(4.0);
        let result = phase(beats, quantum);
        assert!((result.floating() - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_phase_negative() {
        let beats = Beats::new(-2.5);
        let quantum = Beats::new(4.0);
        let result = phase(beats, quantum);
        assert!((result.floating() - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_phase_zero_quantum() {
        let beats = Beats::new(5.5);
        let quantum = Beats::new(0.0);
        let result = phase(beats, quantum);
        assert_eq!(result.floating(), 0.0);
    }

    #[test]
    fn test_phase_exact_quantum() {
        let beats = Beats::new(8.0);
        let quantum = Beats::new(4.0);
        let result = phase(beats, quantum);
        assert!((result.floating() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_next_phase_match() {
        let x = Beats::new(1.0);
        let target = Beats::new(2.5);
        let quantum = Beats::new(4.0);
        let result = next_phase_match(x, target, quantum);
        assert!((result.floating() - 2.5).abs() < 1e-10);
    }

    #[test]
    fn test_next_phase_match_wrap_around() {
        let x = Beats::new(3.0);
        let target = Beats::new(1.5);
        let quantum = Beats::new(4.0);
        let result = next_phase_match(x, target, quantum);
        assert!((result.floating() - 5.5).abs() < 1e-10);
    }

    #[test]
    fn test_closest_phase_match() {
        let x = Beats::new(2.8);
        let target = Beats::new(1.0);
        let quantum = Beats::new(4.0);
        let result = closest_phase_match(x, target, quantum);
        // Should pick the closest match, which is 1.0 (not 5.0)
        assert!((result.floating() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_to_phase_encoded_beats() {
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        let time = Duration::seconds(1);
        let quantum = Beats::new(4.0);

        let result = to_phase_encoded_beats(&timeline, time, quantum);
        // At 120 BPM, 1 second = 2 beats
        let expected_beats = timeline.to_beats(time);
        let phase_encoded = closest_phase_match(expected_beats, expected_beats, quantum);
        assert!((result.floating() - phase_encoded.floating()).abs() < 1e-10);
    }

    #[test]
    fn test_from_phase_encoded_beats() {
        let timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };
        let beat = Beats::new(2.0);
        let quantum = Beats::new(4.0);

        let result = from_phase_encoded_beats(&timeline, beat, quantum);
        // At 120 BPM, 2 beats = 1 second
        assert!(
            (result - Duration::seconds(1))
                .num_microseconds()
                .unwrap()
                .abs()
                < 1000
        );
    }

    #[test]
    fn test_phase_roundtrip() {
        let timeline = Timeline {
            tempo: Tempo::new(140.0),
            beat_origin: Beats::new(1.5),
            time_origin: Duration::milliseconds(500),
        };
        let original_time = Duration::seconds(2);
        let quantum = Beats::new(4.0);

        // Convert time to phase-encoded beats and back
        let beats = to_phase_encoded_beats(&timeline, original_time, quantum);
        let result_time = from_phase_encoded_beats(&timeline, beats, quantum);

        // Should be very close due to quantum alignment
        let diff = (result_time - original_time)
            .num_microseconds()
            .unwrap()
            .abs();
        assert!(diff < 10000); // Within 10ms tolerance due to phase encoding
    }

    #[test]
    fn test_force_beat_at_time_impl() {
        let mut timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        };

        let target_beat = Beats::new(4.0);
        let target_time = Duration::seconds(1);
        let quantum = Beats::new(4.0);

        force_beat_at_time_impl(&mut timeline, target_beat, target_time, quantum);

        // After forcing, the beat at target_time should be close to target_beat
        let result_beat = to_phase_encoded_beats(&timeline, target_time, quantum);
        assert!((result_beat.floating() - target_beat.floating()).abs() < 1e-6);
    }

    #[test]
    fn test_shift_client_timeline() {
        let original_timeline = Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(1.0),
            time_origin: Duration::seconds(1),
        };

        let shift = Beats::new(2.0);
        let shifted_timeline = shift_client_timeline(original_timeline, shift);

        // The tempo and beat origin should remain the same
        assert_eq!(shifted_timeline.tempo.bpm(), original_timeline.tempo.bpm());
        assert_eq!(shifted_timeline.beat_origin, original_timeline.beat_origin);

        // The time origin should be shifted by the corresponding time amount
        let expected_time_shift = original_timeline.tempo.beats_to_micros(shift);
        assert_eq!(
            shifted_timeline.time_origin,
            original_timeline.time_origin - expected_time_shift
        );
    }
}
