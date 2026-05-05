use std::sync::{Arc, Mutex};
use std::time::Duration;

use ableton_link_rs::link::BasicLink;
use tokio::time::sleep;

#[tokio::test]
async fn test_enable_disable_preserves_tempo() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(128.0).await.unwrap();

    // Capture initial tempo
    let state_before = link.capture_app_session_state();
    assert_eq!(state_before.tempo(), 128.0);

    // Enable
    link.enable().await;
    sleep(Duration::from_millis(100)).await;

    // Tempo should be unchanged
    let state_enabled = link.capture_app_session_state();
    assert_eq!(state_enabled.tempo(), 128.0);

    // Disable
    link.disable().await;
    sleep(Duration::from_millis(100)).await;

    // Tempo should still be unchanged
    let state_disabled = link.capture_app_session_state();
    assert_eq!(state_disabled.tempo(), 128.0);
}

#[tokio::test]
async fn test_enable_disable_resets_peer_count() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(120.0).await.unwrap();
    assert_eq!(link.num_peers(), 0);

    link.enable().await;
    sleep(Duration::from_millis(100)).await;
    // Without other peers, count should still be 0
    assert_eq!(link.num_peers(), 0);

    link.disable().await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(link.num_peers(), 0);
    assert!(!link.is_enabled());
}

#[tokio::test]
async fn test_set_tempo_via_commit() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(120.0).await.unwrap();

    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();
    state.set_tempo(175.0, time);
    link.commit_app_session_state(state).await;

    let captured = link.capture_app_session_state();
    assert_eq!(captured.tempo(), 175.0);
}

#[tokio::test]
async fn test_start_stop_via_commit() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(120.0).await.unwrap();

    // Initially not playing
    let state = link.capture_app_session_state();
    assert!(!state.is_playing());

    // Start playing
    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();
    state.set_is_playing(true, time);
    link.commit_app_session_state(state).await;

    let captured = link.capture_app_session_state();
    assert!(captured.is_playing());

    // Stop playing
    let mut state = link.capture_app_session_state();
    let time2 = link.clock().micros();
    state.set_is_playing(false, time2);
    link.commit_app_session_state(state).await;

    let captured2 = link.capture_app_session_state();
    assert!(!captured2.is_playing());
}

#[tokio::test]
async fn test_multiple_tempo_changes() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(100.0).await.unwrap();

    for bpm in [110.0, 120.0, 130.0, 140.0, 150.0] {
        let mut state = link.capture_app_session_state();
        let time = link.clock().micros();
        state.set_tempo(bpm, time);
        link.commit_app_session_state(state).await;

        let captured = link.capture_app_session_state();
        assert_eq!(
            captured.tempo(),
            bpm,
            "tempo should be {} after commit",
            bpm
        );
    }
}

#[tokio::test]
async fn test_tempo_callback_fires_on_each_commit() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(120.0).await.unwrap();

    let call_count = Arc::new(Mutex::new(0u32));
    let call_count_clone = call_count.clone();
    link.set_tempo_callback(move |_bpm| {
        *call_count_clone.lock().unwrap() += 1;
    });

    // The initial callback from set_tempo_callback fires once
    let initial_count = *call_count.lock().unwrap();
    assert!(initial_count >= 1, "initial callback should fire");

    // Commit a tempo change
    let mut state = link.capture_app_session_state();
    let time = link.clock().micros();
    state.set_tempo(160.0, time);
    link.commit_app_session_state(state).await;

    let final_count = *call_count.lock().unwrap();
    assert!(
        final_count > initial_count,
        "callback should fire again after tempo commit"
    );
}

#[tokio::test]
async fn test_audio_session_state_roundtrip() {
    let _ = tracing_subscriber::fmt::try_init();

    let link = BasicLink::new(120.0).await.unwrap();

    // Capture, modify, commit via audio path
    let mut state = link.capture_audio_session_state();
    let time = link.clock().micros();
    state.set_tempo(144.0, time);
    link.commit_audio_session_state(state);

    // Re-capture via audio path
    let state2 = link.capture_audio_session_state();
    assert_eq!(state2.tempo(), 144.0);
}

#[tokio::test]
async fn test_start_stop_sync_toggle() {
    let _ = tracing_subscriber::fmt::try_init();

    let mut link = BasicLink::new(120.0).await.unwrap();
    assert!(!link.is_start_stop_sync_enabled());

    link.enable_start_stop_sync(true);
    assert!(link.is_start_stop_sync_enabled());

    link.enable_start_stop_sync(false);
    assert!(!link.is_start_stop_sync_enabled());
}

#[tokio::test]
async fn test_clock_monotonicity() {
    let _ = tracing_subscriber::fmt::try_init();

    let link = BasicLink::new(120.0).await.unwrap();
    let clock = link.clock();

    let t1 = clock.micros();
    sleep(Duration::from_millis(10)).await;
    let t2 = clock.micros();
    assert!(t2 > t1, "clock should be monotonically increasing");
}
