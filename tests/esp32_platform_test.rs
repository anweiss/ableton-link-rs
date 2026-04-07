//! Integration tests for ESP32 platform support.
//! These tests run on all platforms to verify compilation,
//! with ESP-IDF-specific tests gated behind #[cfg(target_os = "espidf")].

use ableton_link_rs::platform::PlatformClock;

#[test]
fn test_platform_clock_available() {
    let clock = PlatformClock::new();
    let time = clock.micros();
    assert!(time.num_microseconds().is_some());
}

#[test]
fn test_platform_clock_monotonicity() {
    let clock = PlatformClock::new();
    let t1 = clock.micros();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t2 = clock.micros();
    assert!(t2 > t1);
}

#[test]
fn test_clock_trait_interface() {
    let clock = PlatformClock::new();
    // Verify the trait methods work
    let ticks = clock.ticks();
    let as_micros = clock.ticks_to_micros(ticks);
    assert!(as_micros.num_microseconds().is_some());
}

// ESP-IDF specific tests — only run on ESP32 hardware
#[cfg(target_os = "espidf")]
mod espidf_tests {
    use ableton_link_rs::platform::clock::ClockTrait;
    use ableton_link_rs::platform::EspClock;

    #[test]
    fn test_esp_clock_creation() {
        let clock = EspClock::new();
        let time = clock.micros();
        assert!(
            time.num_microseconds().unwrap() > 0,
            "ESP timer should return positive time"
        );
    }

    #[test]
    fn test_esp_clock_microsecond_resolution() {
        let clock = EspClock::new();
        let t1 = clock.micros();
        // Busy-wait briefly
        for _ in 0..1000 {
            core::hint::black_box(());
        }
        let t2 = clock.micros();
        assert!(t2 >= t1, "ESP clock must be monotonic");
    }
}
