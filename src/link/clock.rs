use std::time::Instant;

use chrono::Duration;

#[derive(Default, Copy, Clone, Debug)]
pub struct Clock {
    ticks_to_micros: f64,
}

impl Clock {
    pub fn new() -> Self {
        let time_info = Instant::now();
        let ticks_to_micros = 1_000_000.0 / time_info.elapsed().as_micros() as f64;
        Self { ticks_to_micros }
    }

    pub fn ticks_to_micros(&self, ticks: u64) -> Duration {
        Duration::microseconds((self.ticks_to_micros * ticks as f64).round() as i64)
    }

    pub fn micros_to_ticks(&self, micros: Duration) -> u64 {
        (micros.num_microseconds().unwrap() as f64 / self.ticks_to_micros).round() as u64
    }

    pub fn ticks(&self) -> u64 {
        let now = Instant::now();
        now.elapsed().as_micros() as u64 * self.ticks_to_micros as u64
    }

    pub fn micros(&self) -> Duration {
        self.ticks_to_micros(self.ticks())
    }
}
