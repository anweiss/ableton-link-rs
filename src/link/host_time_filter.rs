use crate::link::{clock::Clock, linear_regression::linear_regression};
use chrono::Duration;
use std::collections::VecDeque;

/// Host time filter for clock drift compensation
/// This uses linear regression to map sample time to host time,
/// compensating for clock drift between different devices.
pub struct HostTimeFilter<const NUM_POINTS: usize = 512> {
    points: VecDeque<(f64, f64)>,
    clock: Clock,
}

impl<const NUM_POINTS: usize> HostTimeFilter<NUM_POINTS> {
    pub fn new(clock: Clock) -> Self {
        Self {
            points: VecDeque::with_capacity(NUM_POINTS),
            clock,
        }
    }

    /// Reset the filter, clearing all accumulated points
    pub fn reset(&mut self) {
        self.points.clear();
    }

    /// Convert sample time to host time using linear regression
    /// This method accumulates sample time points and uses linear regression
    /// to determine the best mapping to host time, compensating for clock drift.
    pub fn sample_time_to_host_time(&mut self, sample_time: f64) -> Duration {
        let host_micros = self.clock.micros().num_microseconds().unwrap() as f64;
        let point = (sample_time, host_micros);

        // Add the new point
        if self.points.len() >= NUM_POINTS {
            self.points.pop_front();
        }
        self.points.push_back(point);

        // Calculate linear regression if we have enough points
        if self.points.len() >= 2 {
            let (slope, intercept) = linear_regression(self.points.iter().copied());
            let host_time = slope * sample_time + intercept;
            Duration::microseconds(host_time.round() as i64)
        } else {
            // Not enough points for regression, return current host time
            self.clock.micros()
        }
    }

    /// Get the number of accumulated points
    pub fn num_points(&self) -> usize {
        self.points.len()
    }

    /// Check if the filter has enough points for reliable regression
    pub fn is_ready(&self) -> bool {
        self.points.len() >= 10 // Need at least 10 points for reasonable accuracy
    }
}

impl<const NUM_POINTS: usize> Default for HostTimeFilter<NUM_POINTS> {
    fn default() -> Self {
        Self::new(Clock::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration as StdDuration;

    #[test]
    fn test_host_time_filter_basic() {
        let mut filter = HostTimeFilter::<10>::new(Clock::default());

        // Add some sample points
        for i in 0..5 {
            let sample_time = i as f64 * 1000.0; // Sample every 1ms
            let _host_time = filter.sample_time_to_host_time(sample_time);
        }

        assert_eq!(filter.num_points(), 5);
    }

    #[test]
    fn test_host_time_filter_capacity() {
        let mut filter = HostTimeFilter::<3>::new(Clock::default());

        // Add more points than capacity
        for i in 0..5 {
            let sample_time = i as f64 * 1000.0;
            let _host_time = filter.sample_time_to_host_time(sample_time);
        }

        // Should be limited by capacity
        assert_eq!(filter.num_points(), 3);
    }

    #[test]
    fn test_host_time_filter_reset() {
        let mut filter = HostTimeFilter::<10>::new(Clock::default());

        // Add some points
        for i in 0..3 {
            let sample_time = i as f64 * 1000.0;
            let _host_time = filter.sample_time_to_host_time(sample_time);
        }

        assert_eq!(filter.num_points(), 3);

        filter.reset();
        assert_eq!(filter.num_points(), 0);
        assert!(!filter.is_ready());
    }

    #[test]
    fn test_host_time_filter_readiness() {
        let mut filter = HostTimeFilter::<20>::new(Clock::default());

        // Not ready initially
        assert!(!filter.is_ready());

        // Add enough points to be ready
        for i in 0..15 {
            let sample_time = i as f64 * 1000.0;
            let _host_time = filter.sample_time_to_host_time(sample_time);
            thread::sleep(StdDuration::from_micros(100));
        }

        assert!(filter.is_ready());
    }

    #[test]
    fn test_host_time_filter_drift_compensation() {
        let mut filter = HostTimeFilter::<50>::new(Clock::default());

        // Simulate a steady sample rate with slight drift
        let _base_time = std::time::Instant::now();

        for i in 0..20 {
            // Simulate 1ms sample intervals with slight drift
            let sample_time = i as f64 * 1000.0 + (i as f64 * 0.1); // 0.1μs drift per sample
            let _host_time = filter.sample_time_to_host_time(sample_time);
            thread::sleep(StdDuration::from_micros(500)); // Simulate time passing
        }

        // The filter should now be ready and providing compensated times
        assert!(filter.is_ready());

        // Test that consecutive calls provide reasonable values
        let time1 = filter.sample_time_to_host_time(20000.0);
        let time2 = filter.sample_time_to_host_time(21000.0);

        // Times should be ordered correctly
        assert!(time2 > time1);
    }
}
