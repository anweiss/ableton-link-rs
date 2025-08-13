/// Linear regression implementation for clock synchronization
/// Used to calculate the best-fit line through a set of time measurements
pub fn linear_regression<I>(points: I) -> (f64, f64)
where
    I: Iterator<Item = (f64, f64)> + Clone,
{
    let points_vec: Vec<(f64, f64)> = points.collect();
    let num_points = points_vec.len() as f64;

    if num_points == 0.0 {
        return (0.0, 0.0);
    }

    let (sum_x, sum_xx, sum_xy, sum_y) = points_vec.iter().fold(
        (0.0, 0.0, 0.0, 0.0),
        |(sum_x, sum_xx, sum_xy, sum_y), &(x, y)| {
            (sum_x + x, sum_xx + x * x, sum_xy + x * y, sum_y + y)
        },
    );

    let denominator = num_points * sum_xx - sum_x * sum_x;
    let slope = if denominator == 0.0 {
        0.0
    } else {
        (num_points * sum_xy - sum_x * sum_y) / denominator
    };
    let intercept = (sum_y - slope * sum_x) / num_points;

    (slope, intercept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_regression_perfect_line() {
        // Perfect line: y = 2x + 1
        let points = vec![(0.0, 1.0), (1.0, 3.0), (2.0, 5.0), (3.0, 7.0)];
        let (slope, intercept) = linear_regression(points.into_iter());

        assert!((slope - 2.0).abs() < 1e-10);
        assert!((intercept - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_linear_regression_horizontal_line() {
        // Horizontal line: y = 5
        let points = vec![(0.0, 5.0), (1.0, 5.0), (2.0, 5.0), (3.0, 5.0)];
        let (slope, intercept) = linear_regression(points.into_iter());

        assert!((slope - 0.0).abs() < 1e-10);
        assert!((intercept - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_linear_regression_empty() {
        let points: Vec<(f64, f64)> = vec![];
        let (slope, intercept) = linear_regression(points.into_iter());

        assert_eq!(slope, 0.0);
        assert_eq!(intercept, 0.0);
    }

    #[test]
    fn test_linear_regression_noisy_data() {
        // Approximately y = x + some noise
        let points = vec![
            (0.0, 0.1),
            (1.0, 1.05),
            (2.0, 1.98),
            (3.0, 3.02),
            (4.0, 3.95),
        ];
        let (slope, intercept) = linear_regression(points.into_iter());

        // Should be close to slope=1, intercept=0
        assert!((slope - 1.0).abs() < 0.1);
        assert!(intercept.abs() < 0.1);
    }
}
