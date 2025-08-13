use ableton_link_rs::link::controller::Controller;
use ableton_link_rs::link::tempo::Tempo;
use ableton_link_rs::link::clock::Clock;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_node_id_preservation_across_enable_disable() {
    let _ = tracing_subscriber::fmt::try_init();
    
    // Create a controller directly to access internal state
    let tempo = Tempo::new(120.0);
    let clock = Clock::default();
    let mut controller = Controller::new(tempo, clock).await;
    
    // Get the initial NodeId
    let initial_node_id = controller.peer_state.try_lock().unwrap().node_state.node_id;
    println!("Initial NodeId: {}", initial_node_id);
    
    // Enable Link
    controller.enable().await;
    sleep(Duration::from_millis(100)).await;
    
    let enabled_node_id = controller.peer_state.try_lock().unwrap().node_state.node_id;
    println!("NodeId after enable: {}", enabled_node_id);
    assert_eq!(initial_node_id, enabled_node_id, "NodeId should be preserved after enable");
    
    // Disable Link
    controller.disable().await;
    sleep(Duration::from_millis(100)).await;
    
    let disabled_node_id = controller.peer_state.try_lock().unwrap().node_state.node_id;
    println!("NodeId after disable: {}", disabled_node_id);
    assert_eq!(initial_node_id, disabled_node_id, "NodeId should be preserved after disable");
    
    // Re-enable Link
    controller.enable().await;
    sleep(Duration::from_millis(100)).await;
    
    let re_enabled_node_id = controller.peer_state.try_lock().unwrap().node_state.node_id;
    println!("NodeId after re-enable: {}", re_enabled_node_id);
    assert_eq!(initial_node_id, re_enabled_node_id, "NodeId should be preserved after re-enable");
    
    // Verify peer count remains 0 throughout
    assert_eq!(controller.num_peers(), 0, "Peer count should remain 0");
    
    println!("✓ NodeId preservation test passed");
}
