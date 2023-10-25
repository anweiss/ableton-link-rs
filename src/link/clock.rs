use std::time::Instant;

use chrono::Duration;

#[derive(Copy, Clone, Debug)]
pub struct Clock {
    pub start_time: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
        }
    }

    pub fn reset(&mut self) {
        self.start_time = Instant::now();
    }

    pub fn elapsed(&self) -> Duration {
        Duration::from_std(self.start_time.elapsed()).unwrap()
    }

    pub fn micros(&self) -> Duration {
        self.elapsed()
    }
}
