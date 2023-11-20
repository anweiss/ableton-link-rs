use chrono::Duration;


#[derive(PartialEq, Debug, Clone, Copy)]
pub struct GhostXForm {
    pub slope: f64,
    pub intercept: Duration,
}

impl Default for GhostXForm {
    fn default() -> Self {
        Self {
            slope: 0.0,
            intercept: Duration::zero(),
        }
    }
}

impl GhostXForm {
    pub fn host_to_ghost(&self, host_time: Duration) -> Duration {
        Duration::microseconds(
            (self.slope * host_time.num_microseconds().unwrap() as f64).round() as i64,
        ) + self.intercept
    }

    pub fn ghost_to_host(&self, ghost_time: Duration) -> Duration {
        Duration::microseconds(
            ((ghost_time - self.intercept).num_microseconds().unwrap() as f64 / self.slope).round()
                as i64,
        )
    }
}
