/// Calculate the median of a collection of values
/// This is used in measurement processing for clock synchronization
/// to filter out outliers and get a robust estimate.
pub fn median(data: &mut [f64]) -> Option<f64> {
    if data.len() < 3 {
        return None;
    }

    let n = data.len();
    
    // Sort the data using partial_cmp which handles floats properly
    data.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    
    if n % 2 == 0 {
        // Even number of elements - return average of two middle elements
        let mid1_idx = n / 2 - 1;
        let mid2_idx = n / 2;
        Some((data[mid1_idx] + data[mid2_idx]) / 2.0)
    } else {
        // Odd number of elements - return middle element
        let mid_idx = n / 2;
        Some(data[mid_idx])
    }
}

/// Calculate median from an iterator (creates a temporary vector)
pub fn median_from_iter<I>(iter: I) -> Option<f64>
where
    I: Iterator<Item = f64>,
{
    let mut data: Vec<f64> = iter.collect();
    median(&mut data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_median_odd_length() {
        let mut data = [1.0, 3.0, 2.0, 5.0, 4.0];
        let result = median(&mut data);
        assert_eq!(result, Some(3.0));
    }

    #[test]
    fn test_median_even_length() {
        let mut data = [1.0, 4.0, 2.0, 3.0];
        let result = median(&mut data);
        assert_eq!(result, Some(2.5));
    }

    #[test]
    fn test_median_single_element() {
        let mut data = [42.0];
        let result = median(&mut data);
        assert_eq!(result, None); // Less than 3 elements
    }

    #[test]
    fn test_median_two_elements() {
        let mut data = [1.0, 2.0];
        let result = median(&mut data);
        assert_eq!(result, None); // Less than 3 elements
    }

    #[test]
    fn test_median_three_elements() {
        let mut data = [3.0, 1.0, 2.0];
        let result = median(&mut data);
        assert_eq!(result, Some(2.0));
    }

    #[test]
    fn test_median_with_duplicates() {
        let mut data = [1.0, 2.0, 2.0, 3.0, 4.0];
        let result = median(&mut data);
        assert_eq!(result, Some(2.0));
    }

    #[test]
    fn test_median_unsorted() {
        let mut data = [9.0, 1.0, 8.0, 2.0, 7.0, 3.0, 6.0];
        let result = median(&mut data);
        assert_eq!(result, Some(6.0));
    }

    #[test]
    fn test_median_from_iter() {
        let data = vec![5.0, 1.0, 3.0, 9.0, 2.0];
        let result = median_from_iter(data.into_iter());
        assert_eq!(result, Some(3.0));
    }

    #[test]
    fn test_median_integers() {
        let mut data = [10.0, 20.0, 30.0, 40.0];
        let result = median(&mut data);
        assert_eq!(result, Some(25.0));
    }

    #[test]
    fn test_median_measurement_scenario() {
        // Simulate measurement data with some outliers
        let mut measurements = [100.0, 102.0, 101.0, 150.0, 103.0, 99.0, 101.5];
        let result = median(&mut measurements);
        
        // Should filter out the outlier (150.0) and return a reasonable median
        assert!(result.is_some());
        let median_val = result.unwrap();
        assert!(median_val > 99.0 && median_val < 105.0);
    }
}
