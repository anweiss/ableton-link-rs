// Example demonstrating platform-specific optimizations
use ableton_link_rs::link::clock::Clock;
use ableton_link_rs::platform;
use std::time::Instant;

fn main() {
    println!("Ableton Link Rust - Platform Optimizations Demo");
    println!("=================================================");

    // Test the optimized clock
    println!("\n1. Clock Performance Test:");
    let clock = Clock::new();

    // Measure time using platform-optimized clock
    let start = Instant::now();
    let mut measurements = Vec::new();

    for _ in 0..10000 {
        measurements.push(clock.micros());
    }

    let elapsed = start.elapsed();
    println!("   Performed 10,000 clock reads in: {:?}", elapsed);
    println!("   Average per read: {:?}", elapsed / 10000);

    // Demonstrate raw ticks access
    let ticks = clock.ticks();
    let micros_from_ticks = clock.ticks_to_micros(ticks);
    println!("   Raw ticks: {}", ticks);
    println!("   Converted back to micros: {:?}", micros_from_ticks);

    // Test platform-specific network interface scanning
    println!("\n2. Network Interface Scanning:");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let interfaces = platform::network::scan_network_interfaces().await;
        println!("   Found {} usable network interfaces:", interfaces.len());
        for (i, addr) in interfaces.iter().enumerate() {
            println!("     {}: {}", i + 1, addr);
        }
    });

    // Test thread factory
    println!("\n3. Thread Factory Test:");
    let handle = platform::ThreadFactory::make_thread("demo_thread".to_string(), || {
        println!("   Hello from optimized thread!");
        42
    });

    let result = handle.join().unwrap();
    println!("   Thread returned: {}", result);

    // Show current platform
    println!("\n4. Platform Detection:");
    #[cfg(target_os = "macos")]
    println!("   Using macOS optimizations (mach_absolute_time, pthread_setname_np)");

    #[cfg(target_os = "linux")]
    println!("   Using Linux optimizations (CLOCK_MONOTONIC_RAW, pthread_setname_np)");

    #[cfg(target_os = "windows")]
    println!("   Using Windows optimizations (QueryPerformanceCounter, SetThreadDescription)");

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    println!("   Using generic fallback implementations");

    println!("\nPlatform optimizations are active and working!");
}
