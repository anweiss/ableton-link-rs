use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;
use triple_buffer::TripleBuffer;

use ableton_link_rs::link::{
    BasicLink,
    host_time_filter::HostTimeFilter,
    linear_regression::linear_regression,
    median::median_from_iter,
    clock::Clock,
};

/// Integration test for the enhanced Ableton Link functionality
/// This test validates the real-time safety improvements and new features
#[tokio::test]
async fn test_enhanced_link_functionality() {
    let _ = tracing_subscriber::fmt::try_init();
    
    info!("Starting enhanced Link functionality test");

    // Test 1: Triple Buffer functionality
    test_triple_buffer_realtime_safety().await;
    
    // Test 2: Host Time Filter and Linear Regression
    test_clock_synchronization_components();
    
    // Test 3: Enhanced BasicLink with callbacks
    test_enhanced_basic_link().await;
    
    // Test 4: Real-time Session State
    test_realtime_session_state().await;
    
    info!("All enhanced functionality tests completed successfully");
}

async fn test_triple_buffer_realtime_safety() {
    info!("Testing TripleBuffer real-time safety");
    
    // Create the triple buffer and split into input/output handles
    let buffer = TripleBuffer::new(&0i32);
    let (input, output) = buffer.split();
    
    let input = Arc::new(std::sync::Mutex::new(input));
    let output = Arc::new(std::sync::Mutex::new(output));
    
    // Simulate real-time writer thread
    let writer_input = input.clone();
    let writer_handle = tokio::spawn(async move {
        for i in 1..=50 {
            {
                let mut input = writer_input.lock().unwrap();
                input.write(i);
            }
            // Simulate audio buffer processing time (e.g., 5ms at 48kHz)
            sleep(Duration::from_micros(100)).await;
        }
    });
    
    // Simulate real-time reader thread
    let reader_output = output.clone();
    let reader_handle = tokio::spawn(async move {
        let mut last_value = 0;
        let mut new_values_count = 0;
        
        for _ in 0..100 {
            {
                let mut output = reader_output.lock().unwrap();
                if output.update() {
                    let value = *output.read();
                    assert!(value > last_value, "Values should be monotonically increasing");
                    last_value = value;
                    new_values_count += 1;
                }
            }
            sleep(Duration::from_micros(50)).await;
        }
        
        info!("TripleBuffer: Read {} new values", new_values_count);
        assert!(new_values_count > 0, "Should have read some new values");
    });
    
    writer_handle.await.unwrap();
    reader_handle.await.unwrap();
    
    info!("TripleBuffer real-time safety test passed");
}

fn test_clock_synchronization_components() {
    info!("Testing clock synchronization components");
    
    // Test linear regression
    let points = vec![
        (0.0, 1.0),
        (1.0, 3.0), 
        (2.0, 5.0),
        (3.0, 7.0)
    ];
    let (slope, intercept) = linear_regression(points.into_iter());
    assert!((slope - 2.0).abs() < 1e-10, "Linear regression slope should be 2.0");
    assert!((intercept - 1.0).abs() < 1e-10, "Linear regression intercept should be 1.0");
    
    // Test median calculation
    let measurements = vec![100.0, 102.0, 101.0, 150.0, 103.0, 99.0, 101.5];
    let median_result = median_from_iter(measurements.into_iter()).unwrap();
    assert!(median_result > 99.0 && median_result < 105.0, "Median should filter out outlier");
    
    // Test host time filter
    let clock = Clock::default();
    let mut filter = HostTimeFilter::<10>::new(clock);
    
    // Add some measurements to the filter
    for i in 0..15 {
        let sample_time = i as f64 * 1000.0; // 1ms intervals
        let _host_time = filter.sample_time_to_host_time(sample_time);
    }
    
    assert!(filter.is_ready(), "Host time filter should be ready after enough samples");
    assert_eq!(filter.num_points(), 10, "Should be limited by filter capacity");
    
    info!("Clock synchronization components test passed");
}

async fn test_enhanced_basic_link() {
    info!("Testing enhanced BasicLink with callbacks");
    
    let mut link = BasicLink::new(120.0).await;
    
    // Test callback setup
    let callback_invoked = Arc::new(std::sync::Mutex::new(false));
    let callback_flag = callback_invoked.clone();
    
    link.set_tempo_callback(move |bpm| {
        info!("Tempo callback invoked with BPM: {}", bpm);
        *callback_flag.lock().unwrap() = true;
    });
    
    let peer_callback_invoked = Arc::new(std::sync::Mutex::new(false));
    let peer_callback_flag = peer_callback_invoked.clone();
    
    link.set_num_peers_callback(move |count| {
        info!("Peer count callback invoked with count: {}", count);
        *peer_callback_flag.lock().unwrap() = true;
    });
    
    let start_stop_callback_invoked = Arc::new(std::sync::Mutex::new(false));
    let start_stop_callback_flag = start_stop_callback_invoked.clone();
    
    link.set_start_stop_callback(move |is_playing| {
        info!("Start/stop callback invoked with playing: {}", is_playing);
        *start_stop_callback_flag.lock().unwrap() = true;
    });
    
    // Give callbacks a moment to be invoked
    sleep(Duration::from_millis(10)).await;
    
    // Verify initial callbacks were invoked
    assert!(*callback_invoked.lock().unwrap(), "Tempo callback should be invoked on setup");
    assert!(*peer_callback_invoked.lock().unwrap(), "Peer count callback should be invoked on setup");
    assert!(*start_stop_callback_invoked.lock().unwrap(), "Start/stop callback should be invoked on setup");
    
    // Test real-time session state access
    let session_state = link.capture_audio_session_state();
    assert_eq!(session_state.tempo(), 120.0, "Should capture correct tempo");
    assert!(!session_state.is_playing(), "Should initially not be playing");
    
    info!("Enhanced BasicLink test passed");
}

async fn test_realtime_session_state() {
    info!("Testing real-time session state handling");
    
    let mut link = BasicLink::new(140.0).await;
    
    // Capture initial state
    let mut session_state = link.capture_audio_session_state();
    assert_eq!(session_state.tempo(), 140.0);
    
    // Modify state
    let current_time = link.clock().micros();
    session_state.set_tempo(160.0, current_time);
    session_state.set_is_playing(true, current_time);
    
    // Commit changes
    link.commit_audio_session_state(session_state);
    
    // Verify changes
    let updated_state = link.capture_audio_session_state();
    assert_eq!(updated_state.tempo(), 160.0, "Tempo should be updated");
    assert!(updated_state.is_playing(), "Should be playing after commit");
    
    info!("Real-time session state test passed");
}

/// Performance test for real-time operations
#[tokio::test]
async fn test_realtime_performance() {
    let _ = tracing_subscriber::fmt::try_init();
    
    info!("Starting real-time performance test");
    
    let mut link = BasicLink::new(120.0).await;
    
    let start_time = std::time::Instant::now();
    let iterations = 10000;
    
    // Test audio thread performance
    for i in 0..iterations {
        let session_state = link.capture_audio_session_state();
        
        // Simulate some audio processing
        let current_time = link.clock().micros();
        let beat = session_state.beat_at_time(current_time, 4.0);
        let phase = session_state.phase_at_time(current_time, 4.0);
        
        // Occasionally modify state
        if i % 1000 == 0 {
            let mut modified_state = session_state;
            modified_state.set_tempo(120.0 + (i as f64 * 0.01), current_time);
            link.commit_audio_session_state(modified_state);
        }
        
        // Verify calculations are reasonable
        assert!(beat >= 0.0);
        assert!((0.0..4.0).contains(&phase));
    }
    
    let elapsed = start_time.elapsed();
    let operations_per_second = iterations as f64 / elapsed.as_secs_f64();
    
    info!(
        "Real-time performance: {} operations in {:?} ({:.0} ops/sec)",
        iterations, elapsed, operations_per_second
    );
    
    // Should be able to handle way more than audio rate operations
    assert!(operations_per_second > 100_000.0, "Should handle high-frequency real-time operations");
    
    info!("Real-time performance test passed");
}
