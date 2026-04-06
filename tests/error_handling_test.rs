use ableton_link_rs::link::{
    beats::Beats,
    tempo::Tempo,
    timeline::{clamp_tempo, Timeline},
    BasicLink,
};
use chrono::Duration;

/// Tempo is clamped to valid range even when set through SessionState API
#[tokio::test]
async fn test_invalid_tempo_zero() {
    let _ = tracing_subscriber::fmt::try_init();

    let link = BasicLink::new(120.0).await;
    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();

    // Set tempo to 0 — should be clamped to minimum (20 BPM)
    state.set_tempo(0.0, time);
    assert_eq!(state.tempo(), 20.0, "zero tempo should be clamped to 20");
}

#[tokio::test]
async fn test_invalid_tempo_negative() {
    let _ = tracing_subscriber::fmt::try_init();

    let link = BasicLink::new(120.0).await;
    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();

    // Set tempo to negative — should be clamped to minimum (20 BPM)
    state.set_tempo(-50.0, time);
    assert_eq!(
        state.tempo(),
        20.0,
        "negative tempo should be clamped to 20"
    );
}

#[tokio::test]
async fn test_invalid_tempo_extremely_large() {
    let _ = tracing_subscriber::fmt::try_init();

    let link = BasicLink::new(120.0).await;
    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();

    state.set_tempo(100_000.0, time);
    assert_eq!(
        state.tempo(),
        999.0,
        "extremely large tempo should be clamped to 999"
    );
}

#[test]
fn clamp_tempo_boundary_values() {
    // At exact minimum
    let at_min = clamp_tempo(Timeline {
        tempo: Tempo::new(20.0),
        beat_origin: Beats::new(0.0),
        time_origin: Duration::zero(),
    });
    assert_eq!(at_min.tempo.bpm(), 20.0);

    // At exact maximum
    let at_max = clamp_tempo(Timeline {
        tempo: Tempo::new(999.0),
        beat_origin: Beats::new(0.0),
        time_origin: Duration::zero(),
    });
    assert_eq!(at_max.tempo.bpm(), 999.0);

    // Just below minimum
    let below_min = clamp_tempo(Timeline {
        tempo: Tempo::new(19.9),
        beat_origin: Beats::new(0.0),
        time_origin: Duration::zero(),
    });
    assert_eq!(below_min.tempo.bpm(), 20.0);

    // Just above maximum
    let above_max = clamp_tempo(Timeline {
        tempo: Tempo::new(999.1),
        beat_origin: Beats::new(0.0),
        time_origin: Duration::zero(),
    });
    assert_eq!(above_max.tempo.bpm(), 999.0);
}

/// Multiple rapid tempo changes should all be reflected correctly
#[tokio::test]
async fn test_rapid_tempo_changes() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(120.0).await;

    for bpm in (20..=200).step_by(10) {
        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_tempo(bpm as f64, time);
        link.commit_app_session_state(state).await;

        let captured = link.capture_app_session_state();
        assert_eq!(
            captured.tempo(),
            bpm as f64,
            "tempo should be {} after rapid change",
            bpm
        );
    }
}

/// Start/stop state transitions should be consistent with a small delay
#[tokio::test]
async fn test_start_stop_transitions() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(120.0).await;

    // Verify initial state
    let state = link.capture_app_session_state();
    assert!(!state.is_playing());

    // Transition: stopped → playing
    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();
    state.set_is_playing(true, time);
    link.commit_app_session_state(state).await;

    let captured = link.capture_app_session_state();
    assert!(captured.is_playing(), "should be playing after start");

    // Transition: playing → stopped
    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();
    state.set_is_playing(false, time);
    link.commit_app_session_state(state).await;

    let captured = link.capture_app_session_state();
    assert!(!captured.is_playing(), "should be stopped after stop");
}

/// Beat calculations at time should produce finite results across various quantum values
#[tokio::test]
async fn test_beat_and_phase_various_quantums() {
    let _ = tracing_subscriber::fmt::try_init();

    let link = BasicLink::new(120.0).await;
    let state = link.capture_app_session_state();
    let time = link.clock().micros();

    for quantum in [1.0, 2.0, 3.0, 4.0, 7.0, 8.0, 16.0] {
        let beat = state.beat_at_time(time, quantum);
        let phase_val = state.phase_at_time(time, quantum);
        assert!(
            beat.is_finite(),
            "beat should be finite for quantum={}",
            quantum
        );
        assert!(
            (0.0..quantum).contains(&phase_val),
            "phase {} should be in [0, {}) for quantum={}",
            phase_val,
            quantum,
            quantum
        );
    }
}

/// time_at_beat and beat_at_time should be approximate inverses
#[tokio::test]
async fn test_beat_time_inverse() {
    let _ = tracing_subscriber::fmt::try_init();

    let link = BasicLink::new(120.0).await;
    let state = link.capture_app_session_state();
    let time = link.clock().micros();
    let quantum = 4.0;

    let beat = state.beat_at_time(time, quantum);
    let recovered_time = state.time_at_beat(beat, quantum);
    let recovered_beat = state.beat_at_time(recovered_time, quantum);

    // Due to phase encoding, the beat should round-trip closely
    assert!(
        (recovered_beat - beat).abs() < 0.01,
        "beat round-trip error too large: {} vs {}",
        beat,
        recovered_beat
    );
}
