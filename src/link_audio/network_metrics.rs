//! Round-trip time based network quality estimation.
//!
//! Ported from `ableton/link_audio/NetworkMetrics.hpp`. Each announcement
//! carries a ping; the matching pong yields a round-trip time. The resulting
//! quality score is used to pick the best gateway for a peer that is reachable
//! on more than one interface.

use chrono::Duration;

const MAX_SIZE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetworkMetrics {
    pub speed: f64,
    pub jitter: f64,
}

impl NetworkMetrics {
    pub fn quality(&self) -> f64 {
        self.speed / (1.0 + self.jitter)
    }
}

/// A sliding window of the last [`MAX_SIZE`] round-trip times.
#[derive(Debug, Clone)]
pub struct NetworkMetricsFilter {
    ping_pong_times: [Duration; MAX_SIZE],
    current_index: usize,
    count: usize,
}

impl Default for NetworkMetricsFilter {
    fn default() -> Self {
        Self {
            ping_pong_times: [Duration::zero(); MAX_SIZE],
            current_index: 0,
            count: 0,
        }
    }
}

impl NetworkMetricsFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a round-trip time measurement.
    pub fn push(&mut self, time: Duration) {
        self.ping_pong_times[self.current_index] = time;
        self.current_index = (self.current_index + 1) % MAX_SIZE;
        if self.count < MAX_SIZE {
            self.count += 1;
        }
    }

    pub fn metrics(&self) -> NetworkMetrics {
        if self.count == 0 {
            return NetworkMetrics {
                speed: 0.0,
                jitter: 0.0,
            };
        }

        let times = &self.ping_pong_times[..self.count];
        let sum: f64 = times
            .iter()
            .map(|t| t.num_microseconds().unwrap_or(0) as f64)
            .sum();
        let avg_rtt = sum / self.count as f64;

        let speed = if avg_rtt != 0.0 { 1e6 / avg_rtt } else { 0.0 };

        let variance = times
            .iter()
            .map(|t| {
                let diff = t.num_microseconds().unwrap_or(0) as f64 - avg_rtt;
                diff * diff
            })
            .sum::<f64>()
            / self.count as f64;

        // Until the window is full the estimate is penalized so that a peer
        // with few samples does not win against a well measured one.
        let jitter = (1e4 - 1e4 * self.count as f64 / MAX_SIZE as f64) + variance.sqrt();

        NetworkMetrics { speed, jitter }
    }

    pub fn quality(&self) -> f64 {
        self.metrics().quality()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_has_zero_quality() {
        let filter = NetworkMetricsFilter::new();
        assert_eq!(filter.quality(), 0.0);
    }

    #[test]
    fn faster_round_trips_score_higher() {
        let mut fast = NetworkMetricsFilter::new();
        let mut slow = NetworkMetricsFilter::new();
        for _ in 0..MAX_SIZE {
            fast.push(Duration::microseconds(500));
            slow.push(Duration::microseconds(5000));
        }
        assert!(fast.quality() > slow.quality());
    }

    #[test]
    fn partially_filled_window_is_penalized() {
        let mut few = NetworkMetricsFilter::new();
        few.push(Duration::microseconds(500));

        let mut many = NetworkMetricsFilter::new();
        for _ in 0..MAX_SIZE {
            many.push(Duration::microseconds(500));
        }

        assert!(many.quality() > few.quality());
    }

    #[test]
    fn window_wraps_after_max_size() {
        let mut filter = NetworkMetricsFilter::new();
        for _ in 0..MAX_SIZE * 3 {
            filter.push(Duration::microseconds(1000));
        }
        assert_eq!(filter.count, MAX_SIZE);
        let metrics = filter.metrics();
        assert!((metrics.speed - 1000.0).abs() < 1e-6);
        assert!(metrics.jitter.abs() < 1e-6);
    }
}
