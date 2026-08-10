//! Mapping between local Link beat time and the session-global beat time used
//! on the LinkAudio wire.
//!
//! Ported from `ableton/link_audio/BeatTimeMapping.hpp`. Buffers carry beat
//! times that are independent of each peer's quantum phase alignment, so the
//! phase-encoded offset of the timeline origin is removed before sending and
//! re-applied after receiving.

use crate::link::{beats::Beats, phase::to_phase_encoded_beats, timeline::Timeline};

/// Converts a local beat time to the session-global beat time.
pub fn global_beat_at_beat(timeline: &Timeline, target_beat: Beats, quantum: Beats) -> Beats {
    let beats_diff = to_phase_encoded_beats(timeline, timeline.time_origin, quantum);
    target_beat - beats_diff
}

/// Converts a session-global beat time to local beat time.
pub fn beat_at_global_beat(timeline: &Timeline, global_beat: Beats, quantum: Beats) -> Beats {
    let beats_diff = to_phase_encoded_beats(timeline, timeline.time_origin, quantum);
    global_beat + beats_diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::tempo::Tempo;
    use chrono::Duration;

    fn timeline() -> Timeline {
        Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(3.0),
            time_origin: Duration::microseconds(1_000_000),
        }
    }

    #[test]
    fn global_and_local_beats_roundtrip() {
        let tl = timeline();
        let quantum = Beats::new(4.0);
        let local = Beats::new(7.25);

        let global = global_beat_at_beat(&tl, local, quantum);
        assert_eq!(beat_at_global_beat(&tl, global, quantum), local);
    }

    #[test]
    fn mapping_is_offset_by_timeline_phase() {
        let tl = timeline();
        let quantum = Beats::new(4.0);
        let offset = to_phase_encoded_beats(&tl, tl.time_origin, quantum);

        assert_eq!(
            global_beat_at_beat(&tl, Beats::new(0.0), quantum),
            Beats::new(0.0) - offset
        );
    }
}
