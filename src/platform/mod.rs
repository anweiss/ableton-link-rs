// Platform-specific optimizations and implementations
// Based on the C++ implementation's platform abstractions
// Now using 100% safe Rust implementations!

pub mod clock;
pub mod network;
pub mod thread;

// Re-export the appropriate PlatformClock for the current target
#[cfg(target_os = "espidf")]
pub use clock::EspClock as PlatformClock;
#[cfg(not(target_os = "espidf"))]
pub use clock::OptimizedClock as PlatformClock;

// Also export the safe clock for direct usage
pub use clock::SafeClock;

// Re-export EspClock when targeting ESP-IDF
#[cfg(target_os = "espidf")]
pub use clock::EspClock;

// Thread factory
pub use thread::ThreadFactory;

// Network interface scanner
pub use network::scan_network_interfaces;
