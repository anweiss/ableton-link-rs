use triple_buffer::TripleBuffer;
use chrono::Duration;

use crate::link::{
    state::ClientStartStopState,
    timeline::Timeline,
};

/// Test function to understand the external triple_buffer API
fn test_external_triple_buffer() {
    // Test with a simple type first
    let buffer: TripleBuffer<i32> = TripleBuffer::default();
    let (mut input, mut output) = buffer.split();
    
    // Write some data
    *input.write() = 42;
    input.publish();
    
    // Read the data
    let latest = output.update();
    if latest {
        println!("Read value: {}", *output.read());
    }
    
    // Now test with our actual data types
    let timeline_buffer: TripleBuffer<(Duration, Timeline)> = TripleBuffer::default();
    let (_timeline_input, _timeline_output) = timeline_buffer.split();
    
    let start_stop_buffer: TripleBuffer<ClientStartStopState> = TripleBuffer::default();
    let (_start_stop_input, _start_stop_output) = start_stop_buffer.split();
    
    println!("External triple buffer API test complete!");
}

// Export for testing
pub fn run_triple_buffer_test() {
    test_external_triple_buffer();
}
