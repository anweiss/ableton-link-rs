use chrono::Duration;
use tokio::time::Instant;

#[derive(Copy, Clone, Debug)]
pub struct Clock {
    pub start_time: Instant,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            start_time: Instant::now(),
        }
    }
}

impl Clock {
    pub fn micros(&self) -> Duration {
        Duration::from_std(self.start_time.elapsed()).unwrap()
    }
}
