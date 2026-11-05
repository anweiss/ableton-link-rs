use chrono::Duration;
#[cfg(not(feature = "std"))]
use num_traits::float::FloatCore;

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
        let offset =
            Duration::microseconds((self.slope * super::micros(host_time) as f64).round() as i64);
        offset
            .checked_add(&self.intercept)
            .unwrap_or(if offset < Duration::zero() {
                Duration::MIN
            } else {
                Duration::MAX
            })
    }

    pub fn ghost_to_host(&self, ghost_time: Duration) -> Duration {
        // Preserve a wide (and potentially cancelling) difference through the
        // division. Only the final host microseconds are saturated by the cast.
        let nanos = |time: Duration| {
            i128::from(time.num_seconds()) * 1_000_000_000 + i128::from(time.subsec_nanos())
        };
        let difference = nanos(ghost_time) - nanos(self.intercept);
        Duration::microseconds((difference as f64 / 1_000.0 / self.slope).round() as i64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn admission_inverse_divides_wide_difference_before_saturating() {
        let form = super::GhostXForm {
            slope: 2.0,
            intercept: chrono::Duration::microseconds(-i64::MAX),
        };
        assert_eq!(
            form.ghost_to_host(chrono::Duration::microseconds(i64::MAX)),
            chrono::Duration::microseconds(i64::MAX)
        );
        let form = super::GhostXForm {
            slope: 2.0,
            intercept: chrono::Duration::microseconds(i64::MAX),
        };
        assert_eq!(
            form.ghost_to_host(chrono::Duration::microseconds(-i64::MAX)),
            chrono::Duration::microseconds(i64::MIN)
        );
        let large = chrono::Duration::MAX - chrono::Duration::seconds(1);
        let form = super::GhostXForm {
            slope: 2.0,
            intercept: large,
        };
        assert_eq!(
            form.ghost_to_host(large + chrono::Duration::microseconds(4)),
            chrono::Duration::microseconds(2)
        );
        let form = super::GhostXForm {
            slope: 1.0,
            intercept: chrono::Duration::MIN,
        };
        assert_eq!(
            form.ghost_to_host(chrono::Duration::MAX),
            chrono::Duration::microseconds(i64::MAX)
        );
    }

    use super::*;

    #[test]
    fn default_has_zero_slope_and_intercept() {
        let xf = GhostXForm::default();
        assert_eq!(xf.slope, 0.0);
        assert_eq!(xf.intercept, Duration::zero());
    }

    #[test]
    fn identity_transform_roundtrip() {
        let xf = GhostXForm {
            slope: 1.0,
            intercept: Duration::zero(),
        };
        let host = Duration::microseconds(5_000_000);
        assert_eq!(xf.host_to_ghost(host), host);
        assert_eq!(xf.ghost_to_host(host), host);
    }

    #[test]
    fn non_trivial_slope_host_to_ghost() {
        let xf = GhostXForm {
            slope: 2.0,
            intercept: Duration::microseconds(100),
        };
        let host = Duration::microseconds(1_000_000);
        // ghost = slope * host + intercept = 2_000_000 + 100 = 2_000_100
        assert_eq!(xf.host_to_ghost(host), Duration::microseconds(2_000_100));
    }

    #[test]
    fn non_trivial_slope_ghost_to_host() {
        let xf = GhostXForm {
            slope: 2.0,
            intercept: Duration::microseconds(100),
        };
        let ghost = Duration::microseconds(2_000_100);
        // host = (ghost - intercept) / slope = (2_000_100 - 100) / 2 = 1_000_000
        assert_eq!(xf.ghost_to_host(ghost), Duration::microseconds(1_000_000));
    }

    #[test]
    fn roundtrip_with_fractional_slope() {
        let xf = GhostXForm {
            slope: 1.0001,
            intercept: Duration::microseconds(-500_000),
        };
        let host = Duration::microseconds(10_000_000);
        let ghost = xf.host_to_ghost(host);
        let recovered = xf.ghost_to_host(ghost);
        // Allow 1µs tolerance due to rounding
        assert!(
            (recovered - host).num_microseconds().unwrap().abs() <= 1,
            "roundtrip error too large: host={}, recovered={}",
            host.num_microseconds().unwrap(),
            recovered.num_microseconds().unwrap()
        );
    }

    #[test]
    fn roundtrip_many_values() {
        let xf = GhostXForm {
            slope: 0.99987,
            intercept: Duration::microseconds(123_456),
        };
        for micros in [0i64, 1, 1_000, 1_000_000, 100_000_000] {
            let host = Duration::microseconds(micros);
            let ghost = xf.host_to_ghost(host);
            let recovered = xf.ghost_to_host(ghost);
            assert!(
                (recovered - host).num_microseconds().unwrap().abs() <= 1,
                "roundtrip failed for host={}µs",
                micros
            );
        }
    }

    #[test]
    fn composition_chaining() {
        // Applying xf1 then xf2 should give the same result as a single composed transform
        let xf1 = GhostXForm {
            slope: 1.5,
            intercept: Duration::microseconds(1000),
        };
        let xf2 = GhostXForm {
            slope: 0.8,
            intercept: Duration::microseconds(-200),
        };
        let host = Duration::microseconds(2_000_000);
        let intermediate = xf1.host_to_ghost(host);
        let final_ghost = xf2.host_to_ghost(intermediate);

        // Manually compose: slope_c = s2 * s1, intercept_c = s2 * i1 + i2
        let composed = GhostXForm {
            slope: xf2.slope * xf1.slope,
            intercept: Duration::microseconds(
                (xf2.slope * xf1.intercept.num_microseconds().unwrap() as f64).round() as i64
                    + xf2.intercept.num_microseconds().unwrap(),
            ),
        };
        let composed_ghost = composed.host_to_ghost(host);
        assert!(
            (final_ghost - composed_ghost)
                .num_microseconds()
                .unwrap()
                .abs()
                <= 1,
            "composition mismatch"
        );
    }

    #[test]
    fn negative_intercept() {
        let xf = GhostXForm {
            slope: 1.0,
            intercept: Duration::microseconds(-1_000_000),
        };
        let host = Duration::microseconds(5_000_000);
        assert_eq!(xf.host_to_ghost(host), Duration::microseconds(4_000_000));
        assert_eq!(
            xf.ghost_to_host(Duration::microseconds(4_000_000)),
            Duration::microseconds(5_000_000)
        );
    }

    #[test]
    fn slope_less_than_one() {
        let xf = GhostXForm {
            slope: 0.5,
            intercept: Duration::zero(),
        };
        let host = Duration::microseconds(4_000_000);
        assert_eq!(xf.host_to_ghost(host), Duration::microseconds(2_000_000));
        assert_eq!(
            xf.ghost_to_host(Duration::microseconds(2_000_000)),
            Duration::microseconds(4_000_000)
        );
    }

    #[test]
    fn zero_host_time() {
        let xf = GhostXForm {
            slope: 1.5,
            intercept: Duration::microseconds(999),
        };
        assert_eq!(
            xf.host_to_ghost(Duration::zero()),
            Duration::microseconds(999)
        );
    }
}
