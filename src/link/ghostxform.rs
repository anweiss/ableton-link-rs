use std::time::Duration;

#[derive(PartialEq, Debug, Clone, Copy, Default)]
pub struct GhostXForm {
    pub slope: f64,
    pub intercept: Duration,
}

impl GhostXForm {
    pub fn host_to_ghost(&self, host_time: Duration) -> Duration {
        Duration::from_micros(((host_time.mul_f64(self.slope)) + self.intercept).as_micros() as u64)
    }

    pub fn ghost_to_host(&self, ghost_time: Duration) -> Duration {
        Duration::from_micros(
            (ghost_time - self.intercept)
                .div_f64(self.slope)
                .as_micros() as u64,
        )
    }
}
