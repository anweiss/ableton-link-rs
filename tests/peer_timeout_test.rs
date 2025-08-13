use std::time::Instant;

use ableton_link_rs::link::BasicLink;
use tokio::time::{sleep, Duration};
use tracing::{debug, info};

fn init_tracing() {
    let _ = tracing_subscriber::fmt::try_init();
}

#[tokio::test]
async fn test_peer_timeout_when_disabled() {
    init_tracing();

    // Create two Link instances
    let mut link1 = BasicLink::new(120.0).await;
    let mut link2 = BasicLink::new(120.0).await;

    // Enable both instances
    link1.enable().await;
    link2.enable().await;

    info!("Both Link instances enabled, waiting for peer discovery...");

    // Wait for peer discovery
    let mut discovered = false;
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        let peers1 = link1.num_peers();
        let peers2 = link2.num_peers();
        
        debug!("Link1 peers: {}, Link2 peers: {}", peers1, peers2);
        
        if peers1 >= 1 && peers2 >= 1 {
            discovered = true;
            break;
        }
    }

    assert!(discovered, "Peers should discover each other within 5 seconds");
    info!("Peer discovery successful!");

    // Record when we disable Link2
    let disable_time = Instant::now();
    link2.disable().await;
    info!("Disabled Link2, measuring response time...");

    // Check how quickly Link1 detects that Link2 has been disabled
    // When Link2 is properly disabled (sends bye-bye), Link1 should update quickly
    let mut peer_count_updated = false;
    let mut response_duration = Duration::from_secs(0);
    
    for i in 0..30 { // Wait up to 3 seconds
        sleep(Duration::from_millis(100)).await;
        let peers1 = link1.num_peers();
        
        debug!("Iteration {}: Link1 peers: {}", i, peers1);
        
        if peers1 == 0 {
            response_duration = disable_time.elapsed();
            peer_count_updated = true;
            break;
        }
    }

    assert!(peer_count_updated, "Link1 should detect Link2 disable within 3 seconds");
    
    info!("Peer count updated after {:?}", response_duration);
    
    // When properly disabled with bye-bye message, response should be quick (under 1 second)
    assert!(
        response_duration < Duration::from_millis(1000),
        "When Link2 sends bye-bye on disable, Link1 should respond within 1 second, but took {:?}",
        response_duration
    );

    info!("✅ Peer disable response test passed! Update occurred in {:?}", response_duration);
}

#[tokio::test]
async fn test_peer_rejoin_after_timeout() {
    init_tracing();

    // Create two Link instances
    let mut link1 = BasicLink::new(120.0).await;
    let mut link2 = BasicLink::new(120.0).await;

    // Enable both instances
    link1.enable().await;
    link2.enable().await;

    // Wait for peer discovery
    let mut discovered = false;
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if link1.num_peers() >= 1 && link2.num_peers() >= 1 {
            discovered = true;
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
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if link1.num_peers() >= 1 && link2.num_peers() >= 1 {
            rediscovered = true;
            break;
        }
    }

    assert!(rediscovered, "Peers should rediscover each other after re-enable");
    info!("✅ Peer rejoin test passed!");
}
