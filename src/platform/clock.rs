// Platform-specific high-resolution clock implementations
// Based on Ableton Link's clock optimizations
// Now using 100% safe Rust implementations!

use chrono::Duration;
use std::time::Instant;

pub trait ClockTrait {
    /// Get current time in microseconds since some reference point
    fn micros(&self) -> Duration;

    /// Get raw ticks (platform-specific)
    fn ticks(&self) -> u64;

    /// Convert ticks to microseconds
    fn ticks_to_micros(&self, ticks: u64) -> Duration;

    /// Convert microseconds to ticks
    fn micros_to_ticks(&self, micros: Duration) -> u64;
}

// Safe Rust implementation using std::time::Instant
// This is high-resolution and monotonic on all platforms
#[derive(Clone, Copy, Debug)]
pub struct SafeClock {
    start_instant: Instant,
}

impl SafeClock {
    pub fn new() -> Self {
        Self {
            start_instant: Instant::now(),
        }
    }
}

impl Default for SafeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockTrait for SafeClock {
    fn micros(&self) -> Duration {
        let elapsed = self.start_instant.elapsed();
        // Convert std::time::Duration to chrono::Duration
        // std::time::Duration has microsecond precision
        Duration::microseconds(elapsed.as_micros() as i64)
    }

    fn ticks(&self) -> u64 {
        // Use microseconds as our "tick" unit for consistent behavior
        self.micros().num_microseconds().unwrap_or(0) as u64
    }

    fn ticks_to_micros(&self, ticks: u64) -> Duration {
        // Since our ticks are already microseconds, this is a direct conversion
        Duration::microseconds(ticks as i64)
    }

    fn micros_to_ticks(&self, micros: Duration) -> u64 {
        // Since our ticks are microseconds, this is a direct conversion
        micros.num_microseconds().unwrap_or(0) as u64
    }
}

// ESP-IDF clock implementation using esp_timer_get_time() for microsecond precision.
// This mirrors the C++ Ableton Link reference implementation's ESP32 platform code.
#[cfg(target_os = "espidf")]
mod espidf_clock {
    use chrono::Duration;

    unsafe extern "C" {
        fn esp_timer_get_time() -> i64;
    }

    #[derive(Clone, Copy, Debug)]
    pub struct EspClock;

    impl EspClock {
        pub fn new() -> Self {
            Self
        }

        pub fn micros(&self) -> Duration {
            Duration::microseconds(unsafe { esp_timer_get_time() })
        }
    }

    impl Default for EspClock {
        fn default() -> Self {
            Self::new()
        }
    }

    impl super::ClockTrait for EspClock {
        fn micros(&self) -> Duration {
            self.micros()
        }

        fn ticks(&self) -> u64 {
            self.micros().num_microseconds().unwrap_or(0) as u64
        }

        fn ticks_to_micros(&self, ticks: u64) -> Duration {
            Duration::microseconds(ticks as i64)
        }

        fn micros_to_ticks(&self, micros: Duration) -> u64 {
            micros.num_microseconds().unwrap_or(0) as u64
        }
    }
}

#[cfg(target_os = "espidf")]
pub use espidf_clock::EspClock;

// High-level Clock type that uses the safe implementation
#[derive(Clone, Copy, Debug)]
pub struct OptimizedClock {
    inner: SafeClock,
}

impl OptimizedClock {
    pub fn new() -> Self {
        Self {
            inner: SafeClock::new(),
        }
    }

    pub fn micros(&self) -> Duration {
        self.inner.micros()
    }

    pub fn ticks(&self) -> u64 {
        self.inner.ticks()
    }

    pub fn ticks_to_micros(&self, ticks: u64) -> Duration {
        self.inner.ticks_to_micros(ticks)
    }

    pub fn micros_to_ticks(&self, micros: Duration) -> u64 {
        self.inner.micros_to_ticks(micros)
    }
}

impl Default for OptimizedClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_clock_creation() {
        let clock = SafeClock::new();
        let _micros = clock.micros();
        // Clock should work without panicking
    }

    #[test]
    fn test_safe_clock_monotonic() {
        let clock = SafeClock::new();
        let time1 = clock.micros();

        // Sleep for a small amount
        std::thread::sleep(std::time::Duration::from_millis(1));

        let time2 = clock.micros();
        assert!(time2 > time1, "Clock should be monotonic");
    }

    #[test]
    fn test_safe_clock_conversions() {
        let clock = SafeClock::new();
        let micros = Duration::microseconds(1000);
        let ticks = clock.micros_to_ticks(micros);
        let converted_back = clock.ticks_to_micros(ticks);

        assert_eq!(micros, converted_back);
    }

    #[test]
    fn test_optimized_clock() {
        let clock = OptimizedClock::new();
        let time1 = clock.micros();

        // Sleep for a small amount
        std::thread::sleep(std::time::Duration::from_millis(1));

        let time2 = clock.micros();
        assert!(time2 > time1, "OptimizedClock should be monotonic");
    }

    #[test]
    fn test_clock_trait_conversions_roundtrip() {
        // Verify the ClockTrait conversion contract: ticks_to_micros and micros_to_ticks
        // are inverses of each other. This validates the interface for all implementations.
        let clock = SafeClock::new();
        let test_values: Vec<u64> = vec![0, 1, 1000, 1_000_000, 60_000_000];
        for &val in &test_values {
            let micros = Duration::microseconds(val as i64);
            let ticks = clock.micros_to_ticks(micros);
            let back = clock.ticks_to_micros(ticks);
            assert_eq!(
                micros, back,
                "Round-trip conversion failed for {val} microseconds"
            );
        }
    }

    #[test]
    fn test_espclock_trait_contract() {
        // Validate the ClockTrait interface contract using SafeClock.
        // Since EspClock can't run in CI, we verify the shared contract
        // (inverse conversions, zero handling, large values) on SafeClock,
        // which EspClock mirrors.
        let clock = SafeClock::new();

        // micros_to_ticks and ticks_to_micros are inverse operations
        let test_values: Vec<u64> = vec![0, 1, 500, 1_000, 1_000_000, 3_600_000_000];
        for &val in &test_values {
            let micros = Duration::microseconds(val as i64);
            let ticks = clock.micros_to_ticks(micros);
            let back = clock.ticks_to_micros(ticks);
            assert_eq!(micros, back, "Round-trip failed for {val} microseconds");
        }

        // ticks_to_micros(0) returns zero duration
        let zero = clock.ticks_to_micros(0);
        assert_eq!(
            zero,
            Duration::microseconds(0),
            "ticks_to_micros(0) must be zero"
        );

        // Large value (1 hour) round-trips correctly
        let one_hour_us: u64 = 3_600_000_000;
        let one_hour = Duration::microseconds(one_hour_us as i64);
        let ticks = clock.micros_to_ticks(one_hour);
        assert_eq!(ticks, one_hour_us);
        assert_eq!(clock.ticks_to_micros(ticks), one_hour);
    }

    #[test]
    fn test_platform_clock_type_exists() {
        // Verify the cfg-gate PlatformClock alias compiles and works
        use super::super::PlatformClock;
        let clock = PlatformClock::new();
        let dur = clock.micros();
        assert!(
            dur.num_microseconds().is_some(),
            "PlatformClock::micros() should return a valid duration"
        );
    }

    #[test]
    fn test_optimized_clock_precision() {
        let clock = OptimizedClock::new();
        let start = clock.micros();

        // Busy wait for a short time to test precision
        let mut iterations = 0;
        while clock.micros() - start < Duration::microseconds(100) {
            iterations += 1;
            if iterations > 100000 {
                break; // Safety valve
            }
        }

        let elapsed = clock.micros() - start;
        assert!(
            elapsed.num_microseconds().unwrap() >= 0,
            "Should measure positive elapsed time"
        );
    }
}
