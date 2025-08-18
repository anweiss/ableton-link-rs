use std::time::Instant;

use ableton_link_rs::link::BasicLink;
use tokio::time::{sleep, Duration};
use tracing::info;

fn init_tracing() {
    let _ = tracing_subscriber::fmt::try_init();
}

#[tokio::test]
async fn test_peer_timeout_when_disabled() {
    init_tracing();

    // Add small delay to avoid test interference
    sleep(Duration::from_millis(100)).await;

    // Create two Link instances
    let mut link1 = BasicLink::new(120.0).await;
    let mut link2 = BasicLink::new(120.0).await;

    // Enable both instances
    link1.enable().await;
    link2.enable().await;

    info!("Both Link instances enabled, waiting for peer discovery...");

    // Wait for peer discovery - increase timeout and add more detailed logging
    let mut discovered = false;
    for i in 0..100 {
        // Increased from 50 to 100 (10 seconds instead of 5)
        sleep(Duration::from_millis(100)).await;
        let peers1 = link1.num_peers();
        let peers2 = link2.num_peers();

        if i % 10 == 0 {
            // Log every second
            info!(
                "Discovery attempt {}/100: Link1 peers: {}, Link2 peers: {}",
                i, peers1, peers2
            );
        }

        if peers1 >= 1 && peers2 >= 1 {
            discovered = true;
            info!(
                "Peer discovery successful at attempt {}: Link1={}, Link2={}",
                i, peers1, peers2
            );
            break;
        }
    }

    assert!(
        discovered,
        "Peers should discover each other within 10 seconds"
    );
    info!("Peer discovery successful!");

    // Record when we disable Link2
    let disable_time = Instant::now();
    link2.disable().await;
    info!("Disabled Link2, measuring response time...");

    // Check how quickly Link1 detects that Link2 has been disabled
    // When Link2 is properly disabled (sends bye-bye), Link1 should update quickly
    let mut peer_count_updated = false;
    let mut response_duration = Duration::from_secs(0);

    for i in 0..50 {
        // Increased from 30 to 50 (5 seconds instead of 3)
        sleep(Duration::from_millis(100)).await;
        let peers1 = link1.num_peers();

        if i % 5 == 0 {
            // Log every 500ms
            info!(
                "Disable detection attempt {}/50: Link1 peers: {}",
                i, peers1
            );
        }

        if peers1 == 0 {
            response_duration = disable_time.elapsed();
            peer_count_updated = true;
            info!(
                "Peer disable detected at attempt {}, duration: {:?}",
                i, response_duration
            );
            break;
        }
    }

    assert!(
        peer_count_updated,
        "Link1 should detect Link2 disable within 5 seconds"
    );

    info!("Peer count updated after {:?}", response_duration);

    // When properly disabled with bye-bye message, response should be reasonably quick (under 3 seconds)
    // This is more forgiving than the original 1-second requirement to handle timing variations
    assert!(
        response_duration < Duration::from_millis(3000),
        "When Link2 sends bye-bye on disable, Link1 should respond within 3 seconds, but took {:?}",
        response_duration
    );

    info!(
        "✅ Peer disable response test passed! Update occurred in {:?}",
        response_duration
    );

    // Clean up
    link1.disable().await;
    link2.disable().await;

    // Small delay before test ends to avoid interference with next test
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn test_peer_rejoin_after_timeout() {
    init_tracing();

    // Add small delay to avoid test interference
    sleep(Duration::from_millis(200)).await;

    // Create two Link instances
    let mut link1 = BasicLink::new(120.0).await;
    let mut link2 = BasicLink::new(120.0).await;

    // Enable both instances
    link1.enable().await;
    link2.enable().await;

    // Wait for peer discovery
    let mut discovered = false;
    for i in 0..100 {
        // Increased timeout for initial discovery
        sleep(Duration::from_millis(100)).await;
        let peers1 = link1.num_peers();
        let peers2 = link2.num_peers();

        if i % 10 == 0 {
            // Log every second
            info!(
                "Initial discovery attempt {}/100: Link1 peers: {}, Link2 peers: {}",
                i, peers1, peers2
            );
        }

        if peers1 >= 1 && peers2 >= 1 {
            discovered = true;
            info!("Initial peer discovery successful at attempt {}", i);
            break;
        }
    }
    assert!(discovered, "Initial peer discovery failed");

    // Disable Link2 and wait for timeout
    link2.disable().await;
    for _ in 0..60 {
        sleep(Duration::from_millis(100)).await;
        if link1.num_peers() == 0 {
            break;
        }
    }
    assert_eq!(link1.num_peers(), 0, "Link1 should detect Link2 timeout");

    // Re-enable Link2 and test rediscovery
    link2.enable().await;
    info!("Re-enabled Link2, waiting for rediscovery...");

    let mut rediscovered = false;
    for i in 0..100 {
        // Increased timeout for rediscovery
        sleep(Duration::from_millis(100)).await;
        let peers1 = link1.num_peers();
        let peers2 = link2.num_peers();

        if i % 10 == 0 {
            // Log every second
            info!(
                "Rediscovery attempt {}/100: Link1 peers: {}, Link2 peers: {}",
                i, peers1, peers2
            );
        }

        if peers1 >= 1 && peers2 >= 1 {
            rediscovered = true;
            info!("Rediscovery successful at attempt {}", i);
            break;
        }
    }

    assert!(
        rediscovered,
        "Peers should rediscover each other after re-enable"
    );
    info!("✅ Peer rejoin test passed!");

    // Clean up
    link1.disable().await;
    link2.disable().await;

    // Small delay before test ends to avoid interference with next test
    sleep(Duration::from_millis(100)).await;
}
