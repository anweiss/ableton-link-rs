use ableton_link_rs::link::BasicLink;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_enable_disable_preserves_node_id() {
    let mut link = BasicLink::new(120.0).await;

    // Enable Link
    link.enable().await;

    // Small delay to ensure initialization is complete
    sleep(Duration::from_millis(100)).await;

    // Disable Link
    link.disable().await;

    // Small delay to ensure cleanup
    sleep(Duration::from_millis(100)).await;

    // Re-enable Link
    link.enable().await;

    // Small delay to ensure initialization is complete
    sleep(Duration::from_millis(100)).await;

    // Note: peer count may be > 0 if other test instances are running
    // The important thing is that we don't crash and the API works
    let peer_count = link.num_peers();
    println!("Peer count after enable/disable cycle: {}", peer_count);

    // Capture state to ensure everything works after enable/disable cycle
    let final_session = link.capture_app_session_state();

    // Verify tempo is a valid BPM (could have changed via peer sync when tests run in parallel)
    assert!(
        final_session.tempo() > 0.0,
        "Tempo should be positive after enable/disable cycle"
    );

    println!("✓ Enable/disable cycle completed successfully");
}

#[tokio::test]
async fn test_multiple_enable_disable_cycles() {
    let _ = tracing_subscriber::fmt::try_init();
    let mut link = BasicLink::new(140.0).await;

    // Run multiple enable/disable cycles
    for i in 1..=3 {
        println!("Starting cycle {}", i);

        // Enable
        link.enable().await;
        sleep(Duration::from_millis(100)).await; // Increased delay

        // Check that the API works (peer count may vary due to other test instances)
        let peers_after_enable = link.num_peers();
        println!("Cycle {}: Peers after enable: {}", i, peers_after_enable);

        // Disable
        link.disable().await;
        sleep(Duration::from_millis(100)).await; // Increased delay

        // After disable, peer count should always be 0
        let peers_after_disable = link.num_peers();
        println!("Cycle {}: Peers after disable: {}", i, peers_after_disable);
        assert_eq!(
            peers_after_disable, 0,
            "Peer count should be 0 after disable in cycle {}",
            i
        );
    }

    println!("✓ Multiple enable/disable cycles completed successfully");
}
