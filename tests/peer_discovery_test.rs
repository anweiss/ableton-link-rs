use ableton_link_rs::link::BasicLink;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_single_node_shows_zero_peers() {
    let _ = tracing_subscriber::fmt::try_init();
    
    // Create a single Link instance
    let mut link = BasicLink::new(120.0).await;
    
    // Enable it
    link.enable().await;
    
    // Give it time to start up
    sleep(Duration::from_millis(1000)).await;
    
    // Should see 0 peers when alone
    let peers = link.num_peers();
    
    println!("Single link sees {} peers", peers);
    
    assert_eq!(peers, 0, "Single Link instance should see 0 peers when alone");
    
    // Disable
    link.disable().await;
    
    sleep(Duration::from_millis(100)).await;
    
    // Verify peer count remains 0
    assert_eq!(link.num_peers(), 0);
    
    println!("✓ Single node test completed");
}
