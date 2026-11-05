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
        let nanos = super::duration_nanos(host_time);
        if nanos == 0 {
            return self.intercept;
        }
        let offset = if self.slope.is_finite() {
            let (mantissa, exponent, sign) =
                num_traits::float::FloatCore::integer_decode(self.slope);
            let magnitude = nanos.unsigned_abs();
            let mantissa = u128::from(mantissa);
            // Divide before multiplying so the exact binary product fits, while
            // retaining the fractional microsecond until the single rounding.
            let fraction = (magnitude % 1_000) * mantissa;
            let whole = (magnitude / 1_000) * mantissa + fraction / 1_000;
            let magnitude = if exponent < 0 {
                let shift = u32::from(exponent.unsigned_abs());
                if shift >= 128 {
                    0
                } else {
                    (whole >> shift)
                        + u128::from(whole & ((1_u128 << shift) - 1) >= (1_u128 << (shift - 1)))
                }
            } else {
                let shift = exponent as u32;
                let limit = i128::MAX as u128;
                if shift >= 127 || whole > (limit >> shift) {
                    limit
                } else {
                    (whole << shift)
                        .saturating_add(
                            (fraction % 1_000)
                                .saturating_mul(1_u128 << shift)
                                .saturating_add(500)
                                / 1_000,
                        )
                        .min(limit)
                }
            } as i128;
            if (nanos < 0) != (sign < 0) {
                -magnitude
            } else {
                magnitude
            }
        } else {
            (self.slope * (nanos as f64 / 1_000.0)).round() as i128
        };
        super::duration_from_nanos(
            offset
                .saturating_mul(1_000)
                .saturating_add(super::duration_nanos(self.intercept)),
        )
    }

    pub fn ghost_to_host(&self, ghost_time: Duration) -> Duration {
        let difference = super::duration_nanos(ghost_time) - super::duration_nanos(self.intercept);
        if self.slope == 0.0 || !self.slope.is_finite() {
            return Duration::microseconds(
                (difference as f64 / 1_000.0 / self.slope).round() as i64
            );
        }
        let (mantissa, exponent, sign) = num_traits::float::FloatCore::integer_decode(self.slope);
        let difference = if sign < 0 { -difference } else { difference };
        let denominator = i128::from(mantissa) * 1_000;
        let micros = if exponent >= 0 {
            let shift = exponent as u32;
            if shift >= 127 || denominator > (i128::MAX >> shift) {
                0
            } else {
                super::round_ratio(difference, denominator << shift)
            }
        } else {
            let shift = u32::from(exponent.unsigned_abs());
            if difference == 0 {
                0
            } else if shift >= 127 || difference.unsigned_abs() > ((i128::MAX as u128) >> shift) {
                // The denominator is below 2^63, so an overflowing numerator
                // necessarily produces host microseconds outside i64.
                if difference < 0 {
                    i128::MIN
                } else {
                    i128::MAX
                }
            } else {
                super::round_ratio(difference << shift, denominator)
            }
        };
        Duration::microseconds(micros.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn clock_precision_forward_preserves_cancelling_integer_and_fractional_parts() {
        let micros = (1_i64 << 62) + 511;
        for sign in [-1, 1] {
            for slope in [2.0, -2.0] {
                let direction = sign * if slope < 0.0 { -1 } else { 1 };
                let form = GhostXForm {
                    slope,
                    intercept: Duration::microseconds(-direction * i64::MAX),
                };
                assert_eq!(
                    form.host_to_ghost(Duration::microseconds(sign * micros)),
                    Duration::microseconds(direction * 1_023)
                );
            }
            let form = GhostXForm {
                slope: sign as f64 * f64::from_bits(1.0_f64.to_bits() + 1),
                intercept: Duration::microseconds(-sign * micros),
            };
            assert_eq!(
                form.host_to_ghost(Duration::microseconds(micros)),
                Duration::microseconds(sign * 1_024)
            );
        }
        for input in [Duration::MIN, Duration::MAX] {
            let form = GhostXForm {
                slope: 0.5,
                intercept: -(input / 2),
            };
            let expected = crate::link::duration_from_nanos(
                crate::link::round_ratio(crate::link::duration_nanos(input), 2_000) * 1_000
                    + crate::link::duration_nanos(form.intercept),
            );
            assert_eq!(form.host_to_ghost(input), expected);
        }
        for slope in [f64::MIN_POSITIVE, f64::from_bits(1)] {
            let form = GhostXForm {
                slope,
                intercept: Duration::nanoseconds(37),
            };
            for input in [Duration::MIN, Duration::MAX] {
                assert_eq!(form.host_to_ghost(input), form.intercept);
            }
        }
        for slope in [f64::MAX, -f64::MAX] {
            let form = GhostXForm {
                slope,
                intercept: Duration::nanoseconds(37),
            };
            assert_eq!(form.host_to_ghost(Duration::zero()), form.intercept);
            for nanos in [-1, 1] {
                assert_eq!(
                    form.host_to_ghost(Duration::nanoseconds(nanos)),
                    if (slope < 0.0) == (nanos < 0) {
                        Duration::MAX
                    } else {
                        Duration::MIN
                    }
                );
            }
        }
    }

    #[test]
    fn clock_precision_inverse_preserves_identity_and_binary_ratios() {
        let micros = (1_i64 << 53) - 21;
        for sign in [-1, 1] {
            let identity = GhostXForm {
                slope: 1.0,
                intercept: Duration::zero(),
            };
            let time = Duration::microseconds(sign * micros);
            assert_eq!(identity.ghost_to_host(time), time);
            for (numerator, denominator) in [
                (1, 1),
                (-1, 1),
                (2, 1),
                (-2, 1),
                (1, 2),
                (-1, 2),
                (3, 2),
                (-3, 2),
            ] {
                let host = sign * 2 * micros;
                let form = GhostXForm {
                    slope: numerator as f64 / denominator as f64,
                    intercept: Duration::nanoseconds(37),
                };
                let ghost = Duration::microseconds(host * numerator / denominator) + form.intercept;
                assert_eq!(form.ghost_to_host(ghost), Duration::microseconds(host));
            }
        }
        let identity = GhostXForm {
            slope: 1.0,
            intercept: Duration::zero(),
        };
        for (nanos, micros) in [(1_499, 1), (1_500, 2), (-1_499, -1), (-1_500, -2)] {
            assert_eq!(
                identity.ghost_to_host(Duration::nanoseconds(nanos)),
                Duration::microseconds(micros)
            );
        }
        for slope in [f64::MIN_POSITIVE, f64::from_bits(1)] {
            for sign in [-1, 1] {
                let form = GhostXForm {
                    slope: sign as f64 * slope,
                    intercept: Duration::nanoseconds(37),
                };
                assert_eq!(form.ghost_to_host(form.intercept), Duration::zero());
                assert_eq!(
                    form.ghost_to_host(form.intercept + Duration::nanoseconds(1)),
                    Duration::microseconds(if sign < 0 { i64::MIN } else { i64::MAX })
                );
            }
        }
        for slope in [f64::MAX, -f64::MAX] {
            let form = GhostXForm {
                slope,
                intercept: Duration::nanoseconds(37),
            };
            for input in [Duration::MIN, Duration::MAX] {
                assert_eq!(form.ghost_to_host(input), Duration::zero());
            }
        }
    }

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
            chrono::Duration::microseconds(-i64::MAX)
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

    #[test]
    fn wide_boundary_forward_scaling_and_intercept_cancellation() {
        let identity = GhostXForm {
            slope: 1.0,
            intercept: Duration::zero(),
        };
        for input in [
            Duration::MIN,
            Duration::MAX,
            Duration::microseconds(i64::MAX) + Duration::microseconds(1),
            Duration::microseconds(i64::MIN) - Duration::microseconds(1),
        ] {
            assert_eq!(identity.host_to_ghost(input), input);
        }
        for (input, expected) in [(1_499, 1), (1_500, 2), (-1_499, -1), (-1_500, -2)] {
            assert_eq!(
                identity.host_to_ghost(Duration::nanoseconds(input)),
                Duration::microseconds(expected)
            );
        }
        for sign in [-1, 1] {
            let input = Duration::microseconds(sign * i64::MAX) * 2;
            let form = GhostXForm {
                slope: 0.25,
                intercept: Duration::zero(),
            };
            let expected = Duration::microseconds(sign * (i64::MAX / 2));
            assert!(
                (form.host_to_ghost(input) - expected)
                    .num_microseconds()
                    .unwrap()
                    .abs()
                    <= 2
            );
            let form = GhostXForm {
                slope: 2.0,
                intercept: -Duration::microseconds(sign * i64::MAX),
            };
            let output = form.host_to_ghost(Duration::microseconds(sign * i64::MAX));
            assert!(
                (output - Duration::microseconds(sign * i64::MAX))
                    .num_microseconds()
                    .unwrap()
                    .abs()
                    <= 2
            );
        }
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
