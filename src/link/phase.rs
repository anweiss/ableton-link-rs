use chrono::Duration;

use super::beats::Beats;
use super::timeline::Timeline;

/// Returns a value in the range [0,quantum) corresponding to beats %
/// quantum except that negative beat values are handled correctly.
/// If the given quantum is zero, returns zero.
pub fn phase(beats: Beats, quantum: Beats) -> Beats {
    if quantum.value == 0 {
        Beats { value: 0 }
    } else {
        // Handle negative beat values by doing the computation relative to an
        // origin that is on the nearest quantum boundary less than -(abs(x))
        let quantum_micros = quantum.micro_beats();
        let quantum_bins = (beats.micro_beats().abs() + quantum_micros) / quantum_micros;
        let quantum_beats = quantum_bins * quantum_micros;
        (beats + Beats::from_microbeats(quantum_beats)).r#mod(quantum)
    }
}

/// Return the least value greater than x that matches the phase of
/// target with respect to the given quantum. If the given quantum
/// is 0, x is returned.
pub fn next_phase_match(x: Beats, target: Beats, quantum: Beats) -> Beats {
    let desired_phase = phase(target, quantum);
    let x_phase = phase(x, quantum);
    let phase_diff = (desired_phase - x_phase + quantum).r#mod(quantum);
    x + phase_diff
}

/// Return the closest value to x that matches the phase of the target
/// with respect to the given quantum. The result deviates from x by at
/// most quantum/2, but may be less than x.
pub fn closest_phase_match(x: Beats, target: Beats, quantum: Beats) -> Beats {
    next_phase_match(x - Beats::new(0.5 * quantum.floating()), target, quantum)
}

/// Interprets the given timeline as encoding a quantum boundary at its
/// origin. Given such a timeline, returns a phase-encoded beat value
/// relative to the given quantum that corresponds to the given
/// time. The phase of the resulting beat value can be calculated with
/// phase(beats, quantum). The result will deviate by up to +-
/// (quantum/2) beats compared to the result of tl.to_beats(time).
pub fn to_phase_encoded_beats(tl: &Timeline, time: Duration, quantum: Beats) -> Beats {
    let beat = tl.to_beats(time);
    closest_phase_match(beat, beat - tl.beat_origin, quantum)
}

/// The inverse of to_phase_encoded_beats. Given a phase encoded beat
/// value from the given timeline and quantum, find the time value that
/// it maps to.
pub fn from_phase_encoded_beats(tl: &Timeline, beat: Beats, quantum: Beats) -> Duration {
    let from_origin = beat - tl.beat_origin;
    let origin_offset = from_origin - phase(from_origin, quantum);
    // invert the phase calculation so that it always rounds up in the
    // middle instead of down like closest_phase_match. Otherwise we'll
    // end up rounding down twice when a value is at phase quantum/2.
    let inverse_phase_offset = closest_phase_match(
        quantum - phase(from_origin, quantum),
        quantum - phase(beat, quantum),
        quantum,
    );
    tl.from_beats(tl.beat_origin + origin_offset + quantum - inverse_phase_offset)
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
    let cur_beat_at_time = to_phase_encoded_beats(timeline, time, quantum);
    let closest_in_phase = closest_phase_match(cur_beat_at_time, beat, quantum);
    *timeline = shift_client_timeline(*timeline, closest_in_phase - cur_beat_at_time);
    // Now adjust the magnitude
    timeline.beat_origin = timeline.beat_origin + beat - closest_in_phase;
}

/// Shift client timeline by the given beat offset
pub fn shift_client_timeline(mut timeline: Timeline, shift: Beats) -> Timeline {
    let time_delta = timeline.from_beats(shift) - timeline.from_beats(Beats { value: 0 });
    timeline.time_origin -= time_delta;
    timeline
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::tempo::Tempo;

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
