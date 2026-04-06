use ableton_link_rs::link::{
    beats::Beats, clock::Clock, ghostxform::GhostXForm, linear_regression::linear_regression,
    tempo::Tempo, timeline::Timeline,
};
use chrono::Duration;

/// Verify that linear regression produces the correct GhostXForm parameters
/// when given a perfect set of host→ghost time measurements.
#[test]
fn linear_regression_produces_correct_xform() {
    // Simulate measurements where ghost = 1.0001 * host + 500
    let slope = 1.0001_f64;
    let intercept = 500.0_f64;

    let points: Vec<(f64, f64)> = (0..20)
        .map(|i| {
            let host = i as f64 * 100_000.0;
            let ghost = slope * host + intercept;
            (host, ghost)
        })
        .collect();

    let (computed_slope, computed_intercept) = linear_regression(points.into_iter());

    assert!(
        (computed_slope - slope).abs() < 1e-8,
        "slope should match: got {}",
        computed_slope
    );
    assert!(
        (computed_intercept - intercept).abs() < 1e-4,
        "intercept should match: got {}",
        computed_intercept
    );

    // Build a GhostXForm from the regression result
    let xf = GhostXForm {
        slope: computed_slope,
        intercept: Duration::microseconds(computed_intercept.round() as i64),
    };

    // Verify round-trip
    let host = Duration::microseconds(1_000_000);
    let ghost = xf.host_to_ghost(host);
    let recovered = xf.ghost_to_host(ghost);
    assert!(
        (recovered - host).num_microseconds().unwrap().abs() <= 1,
        "round-trip should be within 1µs"
    );
}

/// Verify that noisy measurements still produce a close-enough transform
#[test]
fn noisy_measurements_produce_usable_xform() {
    let true_slope = 0.99995_f64;
    let true_intercept = -1000.0_f64;

    // Add small noise to ghost times
    let noise = [0.5, -0.3, 0.1, -0.7, 0.2, 0.4, -0.1, 0.6, -0.4, 0.3];
    let points: Vec<(f64, f64)> = noise
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let host = (i as f64 + 1.0) * 1_000_000.0;
            let ghost = true_slope * host + true_intercept + n;
            (host, ghost)
        })
        .collect();

    let (computed_slope, computed_intercept) = linear_regression(points.into_iter());

    assert!(
        (computed_slope - true_slope).abs() < 0.001,
        "slope should be close"
    );
    assert!(
        (computed_intercept - true_intercept).abs() < 100.0,
        "intercept should be close"
    );
}

/// Single-point regression should still produce a result (intercept = y, slope = 0)
#[test]
fn single_point_regression() {
    let points = vec![(100.0, 200.0)];
    let (slope, intercept) = linear_regression(points.into_iter());
    // With a single point, denominator is 0 so slope is 0
    assert_eq!(slope, 0.0);
    assert_eq!(intercept, 200.0);
}

/// Two-point regression should be exact
#[test]
fn two_point_regression_exact() {
    let points = vec![(0.0, 100.0), (1000.0, 1100.0)];
    let (slope, intercept) = linear_regression(points.into_iter());
    assert!((slope - 1.0).abs() < 1e-10);
    assert!((intercept - 100.0).abs() < 1e-10);
}

/// Verify negative slope works
#[test]
fn negative_slope_regression() {
    let points = vec![(0.0, 100.0), (100.0, 50.0), (200.0, 0.0)];
    let (slope, intercept) = linear_regression(points.into_iter());
    assert!((slope - (-0.5)).abs() < 1e-10);
    assert!((intercept - 100.0).abs() < 1e-10);
}

/// Verify GhostXForm works with the Timeline's beat/time conversions
#[test]
fn xform_with_timeline_integration() {
    let xf = GhostXForm {
        slope: 1.0,
        intercept: Duration::microseconds(1_000_000), // 1 second offset
    };

    let session_timeline = Timeline {
        tempo: Tempo::new(120.0),
        beat_origin: Beats::new(0.0),
        time_origin: Duration::microseconds(1_000_000), // Ghost time origin at 1s
    };

    // In ghost time, beat 2 is at ghost_time = 1_000_000 + 1_000_000 = 2_000_000
    let ghost_time_for_beat2 = session_timeline.from_beats(Beats::new(2.0));
    assert_eq!(
        ghost_time_for_beat2,
        Duration::microseconds(2_000_000),
        "at 120 BPM, beat 2 from origin 1s should be at 2s ghost time"
    );

    // Convert ghost time back to host time
    let host_time = xf.ghost_to_host(ghost_time_for_beat2);
    // host = (ghost - intercept) / slope = (2_000_000 - 1_000_000) / 1.0 = 1_000_000
    assert_eq!(host_time, Duration::microseconds(1_000_000));
}

/// Clock provides monotonically increasing timestamps
#[test]
fn clock_monotonicity() {
    let clock = Clock::default();
    let t1 = clock.micros();
    // Busy-wait briefly
    for _ in 0..1000 {
        std::hint::black_box(0);
    }
    let t2 = clock.micros();
    assert!(t2 >= t1, "clock should be monotonically increasing");
}
