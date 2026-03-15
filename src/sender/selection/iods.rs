//! IoDS (In-order Delivery Scheduling) reordering prevention.
//!
//! Ensures packets are scheduled so that they arrive in order at the receiver,
//! reducing SRT retransmissions caused by out-of-order delivery.

/// IoDS scheduling state.
#[derive(Debug)]
pub struct IodsFilter {
    /// Last scheduled predicted arrival time.
    last_arrival: f64,
}

impl IodsFilter {
    pub fn new() -> Self {
        Self { last_arrival: 0.0 }
    }

    /// Record that a packet was scheduled with the given predicted arrival time.
    pub fn record_scheduled(&mut self, predicted_arrival: f64) {
        if predicted_arrival > self.last_arrival {
            self.last_arrival = predicted_arrival;
        }
    }

    /// Filter candidate indices to only those that maintain monotonic ordering.
    ///
    /// A candidate is valid if its predicted arrival time >= last_scheduled_arrival.
    pub fn filter_valid(
        &self,
        indices: &[usize],
        arrival_fn: impl Fn(usize) -> Option<f64>,
    ) -> Vec<usize> {
        indices
            .iter()
            .copied()
            .filter(|&idx| {
                if let Some(arrival) = arrival_fn(idx) {
                    arrival >= self.last_arrival
                } else {
                    false
                }
            })
            .collect()
    }

    /// Reset the ordering state (e.g., after a long gap).
    #[cfg(test)]
    pub fn reset(&mut self) {
        self.last_arrival = 0.0;
    }
}

impl Default for IodsFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monotonic_ordering() {
        let mut iods = IodsFilter::new();

        let arrivals = vec![0.1, 0.05, 0.2, 0.15];
        let indices: Vec<usize> = (0..4).collect();

        // Initially all pass (last_arrival = 0)
        let valid = iods.filter_valid(&indices, |i| Some(arrivals[i]));
        assert_eq!(valid, vec![0, 1, 2, 3]);

        // Schedule at t=0.15
        iods.record_scheduled(0.15);

        // Now only arrivals >= 0.15 should pass
        let valid = iods.filter_valid(&indices, |i| Some(arrivals[i]));
        assert_eq!(valid, vec![2, 3]); // 0.2 >= 0.15 and 0.15 >= 0.15
    }

    #[test]
    fn test_empty_candidates() {
        let iods = IodsFilter::new();
        let valid = iods.filter_valid(&[], |_: usize| Some(1.0));
        assert!(valid.is_empty());
    }

    #[test]
    fn test_reset() {
        let mut iods = IodsFilter::new();
        iods.record_scheduled(100.0);

        let valid = iods.filter_valid(&[0], |_| Some(1.0));
        assert!(valid.is_empty());

        iods.reset();
        let valid = iods.filter_valid(&[0], |_| Some(1.0));
        assert_eq!(valid, vec![0]);
    }

    #[test]
    fn test_none_arrival_filtered_out() {
        let iods = IodsFilter::new();
        let valid = iods.filter_valid(&[0, 1, 2], |i| if i == 1 { None } else { Some(1.0) });
        assert_eq!(valid, vec![0, 2]);
    }
}
