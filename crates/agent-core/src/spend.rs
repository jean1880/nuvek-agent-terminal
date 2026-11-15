//! Turning a running cost total into the cost of one turn.

/// What one turn cost, from the agent's running total before it (`prev`, `None` at the first
/// report) and after it (`now`).
///
/// A total that falls means a new process or a reset, so `now` is the whole of that turn's cost.
/// Float noise that leaves `now` a hair under `prev` is not a reset: it counts as nothing.
pub fn cumulative_to_delta(prev: Option<f64>, now: f64) -> f64 {
    let Some(prev) = prev else {
        return now.max(0.0);
    };
    if now >= prev {
        return now - prev;
    }
    // A drop within rounding error is noise; a real one starts the count again.
    if prev - now <= 1e-12 * prev.abs().max(1.0) {
        0.0
    } else {
        now.max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_report_is_the_whole_total() {
        assert_eq!(cumulative_to_delta(None, 0.25), 0.25);
    }

    #[test]
    fn a_rising_total_gives_the_step() {
        let d = cumulative_to_delta(Some(0.25), 0.75);
        assert!((d - 0.5).abs() < 1e-12);
    }

    #[test]
    fn an_unchanged_total_is_free() {
        assert_eq!(cumulative_to_delta(Some(0.25), 0.25), 0.0);
    }

    #[test]
    fn a_fallen_total_is_a_reset() {
        assert_eq!(cumulative_to_delta(Some(2.0), 0.5), 0.5);
    }

    #[test]
    fn float_noise_below_the_previous_total_is_zero() {
        assert_eq!(cumulative_to_delta(Some(0.3), 0.3 - 1e-15), 0.0);
    }

    #[test]
    fn a_negative_total_is_never_reported() {
        assert_eq!(cumulative_to_delta(None, -1.0), 0.0);
        assert_eq!(cumulative_to_delta(Some(1.0), -1.0), 0.0);
    }
}
