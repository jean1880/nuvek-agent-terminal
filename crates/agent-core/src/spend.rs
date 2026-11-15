//! Turning a running cost total into the cost of one turn.

/// The running total to measure the next turn from, once a turn reported `now`.
///
/// A reading that is not finite, or an exact zero after a paid turn (an interrupted or failed
/// turn reports no total of its own), leaves `prev` as it was.
pub fn next_base(prev: Option<f64>, now: f64) -> Option<f64> {
    let prev = prev.filter(|p| p.is_finite());
    if !now.is_finite() || (now == 0.0 && prev.is_some_and(|p| p > 0.0)) {
        return prev;
    }
    Some(now)
}

/// What one turn cost, from the agent's running total before it (`prev`, `None` at the first
/// report) and after it (`now`).
///
/// A total that falls to a lower non-zero value means a new process or a reset, so `now` is the
/// whole of that turn's cost. Float noise that leaves `now` a hair under `prev` is not a reset:
/// it counts as nothing. An exact zero after a paid turn is no reset either (see [`next_base`]):
/// it costs nothing. A `now` that is not finite costs nothing, and a `prev` that is not finite
/// is taken as none.
pub fn cumulative_to_delta(prev: Option<f64>, now: f64) -> f64 {
    if !now.is_finite() {
        return 0.0;
    }
    let Some(prev) = prev.filter(|p| p.is_finite()) else {
        return now.max(0.0);
    };
    if now >= prev {
        return now - prev;
    }
    if now == 0.0 || prev - now <= 1e-12 * prev.abs().max(1.0) {
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
        assert_eq!(next_base(Some(2.0), 0.5), Some(0.5));
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

    #[test]
    fn a_zero_total_after_a_paid_turn_costs_nothing_and_keeps_the_base() {
        assert_eq!(cumulative_to_delta(Some(0.5), 0.0), 0.0);
        assert_eq!(next_base(Some(0.5), 0.0), Some(0.5));
        assert_eq!(next_base(None, 0.0), Some(0.0));
    }

    #[test]
    fn non_finite_inputs_are_ignored() {
        assert_eq!(cumulative_to_delta(Some(1.0), f64::NAN), 0.0);
        assert_eq!(cumulative_to_delta(Some(1.0), f64::INFINITY), 0.0);
        assert_eq!(cumulative_to_delta(Some(f64::NAN), 0.25), 0.25);
        assert_eq!(cumulative_to_delta(Some(f64::INFINITY), 0.25), 0.25);
        assert_eq!(next_base(Some(1.0), f64::NAN), Some(1.0));
        assert_eq!(next_base(Some(f64::NAN), 0.25), Some(0.25));
    }
}
