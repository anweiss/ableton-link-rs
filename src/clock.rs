use std::time::{Duration, Instant};

#[derive(Default, Copy, Clone)]
pub struct Clock {
    ticks_to_micros: f64,
}

impl Clock {
    fn new() -> Self {
        let time_info = Instant::now();
        let ticks_to_micros = 1_000_000.0 / time_info.elapsed().as_micros() as f64;
        Self { ticks_to_micros }
    }

    fn ticks_to_micros(&self, ticks: u64) -> Duration {
        Duration::from_micros((self.ticks_to_micros * ticks as f64).round() as u64)
    }

    fn micros_to_ticks(&self, micros: Duration) -> u64 {
        (micros.as_micros() as f64 / self.ticks_to_micros).round() as u64
    }

    fn ticks(&self) -> u64 {
        let now = Instant::now();
        now.elapsed().as_micros() as u64 * self.ticks_to_micros as u64
    }

    fn micros(&self) -> Duration {
        self.ticks_to_micros(self.ticks())
    }
}
