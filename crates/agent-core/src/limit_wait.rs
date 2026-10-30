//! When a thread its agent's usage limit stopped continues on its own.
//!
//! A [`LimitWait`] starts when the agent refuses work over its limit. With a reset time from the
//! agent it waits for that time (plus [`GRACE`]). Without one it asks for a prompt-less quota
//! reading on a backoff ([`Step::Probe`]). The windows exhausted in the reading held when the
//! wait starts are the ones that stopped the thread (a plan has others, for models the thread
//! may not use); it continues once a reading taken after the limit shows every one of them
//! free (or, if none was exhausted then, every window free). A window still shown exhausted a
//! grace past its own reset time counts as free: that copy is stale. When all of them name a reset time,
//! the next reading is taken just after the latest, which the thread shows as when it continues;
//! only the reading decides. The caller supplies the clock and the readings; nothing here sends
//! or probes anything.

/// Seconds past a reset time before continuing: the provider's clock and ours may differ.
pub const GRACE: i64 = 60;

/// Seconds between quota readings while no reset time is known: 2, 5, 10, then every 15 min.
const POLL: [i64; 4] = [2 * 60, 5 * 60, 10 * 60, 15 * 60];

/// One quota window as the wait reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Which window it is (its group and label): the same key across readings.
    pub key: String,
    /// Used share, 0.0–1.0.
    pub used: f64,
    /// When it resets (epoch seconds), when known.
    pub resets_at: Option<i64>,
}

impl Window {
    fn exhausted(&self) -> bool {
        self.used >= 1.0
    }

    /// Free at `now`: not exhausted, or exhausted only as of a reading that predates its own
    /// reset. A window the agent stops reporting is kept as last seen (`AccountStatus` merges
    /// readings), so a copy still at 100 % a grace past its reset time is stale, not a limit.
    /// If it was not, the next turn fails on the limit again and a new wait starts.
    fn free(&self, now: i64) -> bool {
        !self.exhausted()
            || self
                .resets_at
                .is_some_and(|r| now >= r.saturating_add(GRACE))
    }
}

/// What the caller does on this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing yet.
    Wait,
    /// Ask for a fresh quota reading (no prompt); its answer is read on a later tick.
    Probe,
    /// The limit is over: continue the thread.
    Continue,
}

/// A thread waiting for its agent's usage limit to reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitWait {
    /// When the agent said the limit resets: continued then, without a reading.
    reset: Option<i64>,
    /// When the blocking windows say they reset (the latest): read again then, and shown.
    expected: Option<i64>,
    since: i64,
    next_check: i64,
    checks: usize,
    /// The windows exhausted when the wait started: what stopped the thread.
    blocking: Vec<String>,
}

impl LimitWait {
    /// The limit was hit at `now`; `reset` is when the agent said it resets, if it did (a time
    /// already past says nothing). `held`: the agent's quota windows as known now, whose
    /// exhausted ones are taken as what stopped the thread.
    pub fn new(reset: Option<i64>, now: i64, held: &[Window]) -> Self {
        Self {
            reset: reset.filter(|r| *r > now),
            expected: None,
            since: now,
            next_check: now.saturating_add(POLL[0]),
            checks: 0,
            blocking: held
                .iter()
                .filter(|w| w.exhausted())
                .map(|w| w.key.clone())
                .collect(),
        }
    }

    /// The agent refused work again at `now`, with `reset` if known: a later reset time than
    /// the one waited for replaces it.
    pub fn learn(&mut self, reset: Option<i64>, now: i64) {
        if let Some(reset) = reset.filter(|r| *r > now) {
            self.reset = Some(self.reset.map_or(reset, |known| known.max(reset)));
        }
    }

    /// When the limit resets, if known: what the thread says it waits for.
    pub fn reset(&self) -> Option<i64> {
        self.reset.or(self.expected)
    }

    /// What to do at `now`. `reading`: the agent's latest quota windows and when they arrived.
    pub fn step(&mut self, now: i64, reading: Option<(i64, &[Window])>) -> Step {
        if let Some(reset) = self.reset {
            return if now >= reset.saturating_add(GRACE) {
                Step::Continue
            } else {
                Step::Wait
            };
        }
        // Only a reading taken after the limit was hit says whether it is over.
        if let Some((_, windows)) = reading.filter(|(at, w)| *at > self.since && !w.is_empty()) {
            let freed = if self.blocking.is_empty() {
                windows.iter().all(|w| w.free(now))
            } else {
                // Each blocking window must be in the reading, and free.
                self.blocking
                    .iter()
                    .all(|key| windows.iter().any(|w| w.key == *key && w.free(now)))
            };
            if freed {
                return Step::Continue;
            }
            // When every window still exhausted says when it resets, the next reading waits for
            // the latest of them; one that does not keeps the backoff.
            let latest = windows
                .iter()
                .filter(|w| w.exhausted())
                .filter(|w| self.blocking.is_empty() || self.blocking.contains(&w.key))
                .map(|w| w.resets_at.filter(|r| *r > now))
                .collect::<Option<Vec<i64>>>()
                .and_then(|resets| resets.into_iter().max());
            if let Some(latest) = latest {
                if self.expected != Some(latest) {
                    self.expected = Some(latest);
                    self.next_check = latest.saturating_add(GRACE);
                }
            }
        }
        if now < self.next_check {
            return Step::Wait;
        }
        self.checks = self.checks.saturating_add(1);
        let delay = POLL[self.checks.min(POLL.len() - 1)];
        self.next_check = now.saturating_add(delay);
        Step::Probe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(key: &str, used: f64, resets_at: Option<i64>) -> Window {
        Window {
            key: key.into(),
            used,
            resets_at,
        }
    }

    #[test]
    fn a_known_reset_continues_a_minute_after_it() {
        let mut wait = LimitWait::new(Some(1_000), 100, &[]);
        assert_eq!(wait.reset(), Some(1_000));
        assert_eq!(wait.step(1_000, None), Step::Wait);
        assert_eq!(wait.step(1_000 + GRACE - 1, None), Step::Wait);
        assert_eq!(wait.step(1_000 + GRACE, None), Step::Continue);
        // A later reset reported again replaces it; an earlier or past one does not.
        let mut wait = LimitWait::new(Some(1_000), 100, &[]);
        wait.learn(Some(900), 100);
        wait.learn(Some(50), 100);
        assert_eq!(wait.reset(), Some(1_000));
        wait.learn(Some(2_000), 100);
        assert_eq!(wait.reset(), Some(2_000));
    }

    #[test]
    fn with_no_reset_it_reads_the_quota_on_a_backoff() {
        // A reset already past is no reset.
        let mut wait = LimitWait::new(Some(50), 100, &[]);
        assert_eq!(wait.reset(), None);
        assert_eq!(wait.step(100, None), Step::Wait);
        assert_eq!(wait.step(100 + 119, None), Step::Wait);
        assert_eq!(wait.step(100 + 120, None), Step::Probe);
        // The next reading is due 5 min later, then 10, then every 15.
        assert_eq!(wait.step(220 + 299, None), Step::Wait);
        assert_eq!(wait.step(220 + 300, None), Step::Probe);
        assert_eq!(wait.step(520 + 600, None), Step::Probe);
        assert_eq!(wait.step(1_120 + 900, None), Step::Probe);
        assert_eq!(wait.step(2_020 + 900, None), Step::Probe);
    }

    #[test]
    fn every_window_exhausted_at_the_limit_must_be_free() {
        let at_limit = [
            window("5-hour", 1.0, None),
            window("Weekly", 1.0, None),
            window("Weekly (Fable)", 0.3, None),
        ];
        let five_hour_free = [
            window("5-hour", 0.0, None),
            window("Weekly", 1.0, None),
            window("Weekly (Fable)", 1.0, None),
        ];
        let both_free = [
            window("5-hour", 0.0, None),
            window("Weekly", 0.2, None),
            window("Weekly (Fable)", 1.0, None),
        ];
        let mut wait = LimitWait::new(None, 100, &at_limit);
        // A reading from before the limit (or the same moment) says nothing.
        assert_eq!(wait.step(110, Some((100, &both_free))), Step::Wait);
        // One of two free is not enough.
        assert_eq!(wait.step(300, Some((200, &five_hour_free))), Step::Probe);
        // Both free: continue, though a window that was free at the limit is exhausted now.
        assert_eq!(wait.step(310, Some((305, &both_free))), Step::Continue);
        // A reading without a blocking window does not free it.
        let mut wait = LimitWait::new(None, 100, &at_limit);
        let partial = [window("5-hour", 0.0, None)];
        assert_eq!(wait.step(300, Some((200, &partial))), Step::Probe);
        // No windows at all is no evidence.
        let mut wait = LimitWait::new(None, 100, &at_limit);
        assert_eq!(wait.step(110, Some((105, &[]))), Step::Wait);
    }

    #[test]
    fn a_window_that_freed_before_the_first_reading_was_not_what_blocked() {
        // Held at the limit: nothing exhausted (the reading predates it). A later reading with
        // an unrelated window exhausted does not become the blocker; every window must be free.
        let held = [
            window("5-hour", 0.9, None),
            window("Weekly (Fable)", 0.5, None),
        ];
        let unrelated = [
            window("5-hour", 0.1, None),
            window("Weekly (Fable)", 1.0, None),
        ];
        let free = [
            window("5-hour", 0.1, None),
            window("Weekly (Fable)", 0.5, None),
        ];
        let mut wait = LimitWait::new(None, 100, &held);
        assert_eq!(wait.step(300, Some((200, &unrelated))), Step::Probe);
        assert_eq!(wait.step(310, Some((305, &free))), Step::Continue);
    }

    #[test]
    fn blocking_resets_time_the_next_reading_and_only_a_reading_continues() {
        let at_limit = [
            window("5-hour", 1.0, Some(4_000)),
            window("Weekly", 1.0, Some(9_000)),
        ];
        let mut wait = LimitWait::new(None, 100, &at_limit);
        assert_eq!(wait.step(300, Some((200, &at_limit))), Step::Wait);
        // Read again only after the latest blocking reset; it is shown as when it continues.
        assert_eq!(wait.reset(), Some(9_000));
        assert_eq!(wait.step(4_000 + GRACE, None), Step::Wait);
        assert_eq!(wait.step(9_000 + GRACE, None), Step::Probe);
        let free = [
            window("5-hour", 0.0, Some(20_000)),
            window("Weekly", 0.0, Some(600_000)),
        ];
        assert_eq!(wait.step(9_100, Some((9_070, &free))), Step::Continue);
        // One blocking window with no reset time keeps the backoff.
        let open = [
            window("5-hour", 1.0, Some(4_000)),
            window("Weekly", 1.0, None),
        ];
        let mut wait = LimitWait::new(None, 100, &open);
        assert_eq!(wait.step(300, Some((200, &open))), Step::Probe);
        assert_eq!(wait.reset(), None);
    }

    #[test]
    fn a_blocking_window_still_full_past_its_own_reset_is_stale() {
        // The agent stopped reporting "Weekly": every later reading carries its last copy, at
        // 100 %, while "5-hour" keeps arriving fresh. Without this the wait never ends.
        let at_limit = [
            window("5-hour", 0.4, Some(20_000)),
            window("Weekly", 1.0, Some(4_000)),
        ];
        let mut wait = LimitWait::new(None, 100, &at_limit);
        // Before its reset (and within the grace after it) the copy still holds the thread.
        assert_eq!(wait.step(300, Some((200, &at_limit))), Step::Wait);
        assert_eq!(wait.reset(), Some(4_000));
        assert_eq!(
            wait.step(4_000 + GRACE - 1, Some((4_000, &at_limit))),
            Step::Wait
        );
        // A grace past its own reset it is stale: the thread continues.
        assert_eq!(
            wait.step(4_000 + GRACE, Some((4_000 + GRACE, &at_limit))),
            Step::Continue
        );
        // Still only on a reading taken after the limit.
        let mut wait = LimitWait::new(None, 5_000, &at_limit);
        assert_eq!(wait.step(5_000, Some((4_500, &at_limit))), Step::Wait);
        // A full window with no reset time is never judged stale: only a fresh reading frees it.
        let open = [window("Weekly", 1.0, None)];
        let mut wait = LimitWait::new(None, 100, &open);
        assert_eq!(wait.step(1_000_000, Some((999_000, &open))), Step::Probe);
        // And with no blocking window at the limit, every window must be free the same way.
        let mut wait = LimitWait::new(None, 100, &[]);
        assert_eq!(
            wait.step(4_000 + GRACE, Some((4_000 + GRACE, &at_limit))),
            Step::Continue
        );
    }
}
