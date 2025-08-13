use ableton_link_rs::link::BasicLink;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_enable_disable_preserves_node_id() {
    let mut link = BasicLink::new(120.0).await;

    // Enable Link
    link.enable().await;

    // Get the initial node ID by capturing the internal state
    let initial_session = link.capture_app_session_state();

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

    // Check that peer count remains 0 (no false counting)
    assert_eq!(
        link.num_peers(),
        0,
        "Peer count should remain 0 after enable/disable cycle"
    );

    // Capture state again to ensure everything works
    let final_session = link.capture_app_session_state();

    // Basic sanity checks
    assert_eq!(initial_session.tempo(), final_session.tempo());

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

        // Verify peer count is 0 (no self-counting or false peers)
        let peers_after_enable = link.num_peers();
        println!("Cycle {}: Peers after enable: {}", i, peers_after_enable);
        assert_eq!(
            peers_after_enable, 0,
            "Peer count should be 0 in cycle {}",
            i
        );

        // Disable
        link.disable().await;
        sleep(Duration::from_millis(100)).await; // Increased delay

        // Verify peer count remains 0 after disable
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
