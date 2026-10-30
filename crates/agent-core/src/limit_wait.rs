//! When a thread its agent's usage limit stopped continues on its own.
//!
//! A [`LimitWait`] starts when the agent refuses work over its limit. With a reset time from the
//! agent it waits for that time (plus [`GRACE`]). Without one it asks for a prompt-less quota
//! reading on a backoff ([`Step::Probe`]). The windows exhausted in the reading held when the
//! wait starts are the ones that stopped the thread (a plan has others, for models the thread
//! may not use); it continues once a reading taken after the limit shows every one of them
//! free (or, if none was exhausted then, every window free). A window still shown exhausted a
//! grace past its own reset time counts as free: readings are merged, so that copy is usually
//! stale. A reading taken after the limit can show it so at any later tick, with no new probe.
//!
//! A stale copy and a window genuinely still full past its reset look the same, so a guess is
//! never ruled out for good (an abandoned window would then hold the thread forever). Instead a
//! refusal (the one that starts the wait, or one during it) backs off every copy already past
//! its reset: it is not taken as stale again until it has waited as long as it had already been
//! stale, at least [`BACKOFF_MIN`]. Each failed guess so at least doubles the next gap, so a
//! genuinely full window costs a few failed turns spread ever wider, never a loop, and an
//! abandoned one is always tried again. A drop in usage or a new reset time is a new copy. A
//! refusal also restarts the clock: only readings taken after it count, and a reset time it came
//! after is dropped. When every window still holding the thread names a reset time ahead, the
//! next reading is taken just after the latest, which the thread shows as when it continues;
//! only a reading decides. The caller supplies the clock and the readings; nothing here sends or
//! probes anything.

/// Seconds past a reset time before continuing: the provider's clock and ours may differ.
pub const GRACE: i64 = 60;

/// Seconds between quota readings while no reset time is known: 2, 5, 10, then every 15 min.
const POLL: [i64; 4] = [2 * 60, 5 * 60, 10 * 60, 15 * 60];

/// The shortest wait before a copy a refusal came after is taken as stale again.
pub const BACKOFF_MIN: i64 = 15 * 60;

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

    /// Shown exhausted a grace past its own reset time. A window the agent stops reporting is
    /// kept as last seen (`AccountStatus` merges readings), so such a copy is usually stale.
    fn past_reset(&self, now: i64) -> bool {
        self.exhausted()
            && self
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
    /// Copies a refusal came after (see [`BackedOff`]): not taken as stale until their gap ends.
    backed_off: Vec<BackedOff>,
}

/// A copy (key and reset time) that was already past its reset when the agent refused work.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BackedOff {
    key: String,
    reset: i64,
    /// When it may be taken as stale again: the refusal plus as long as the copy had already
    /// been stale then, at least [`BACKOFF_MIN`].
    until: i64,
}

/// The copies the windows `held` at a refusal at `now` back off.
fn backed_off_by(held: &[Window], now: i64) -> Vec<BackedOff> {
    held.iter()
        .filter(|w| w.past_reset(now))
        .filter_map(|w| {
            let reset = w.resets_at?;
            let gap = now.saturating_sub(reset).max(BACKOFF_MIN);
            Some(BackedOff {
                key: w.key.clone(),
                reset,
                until: now.saturating_add(gap),
            })
        })
        .collect()
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
            backed_off: backed_off_by(held, now),
        }
    }

    /// Free at `now`: not exhausted, or past its own reset (a stale copy) once any back-off a
    /// refusal put on that same copy has ended. If a stale guess is wrong, the next turn fails
    /// on the limit and that refusal backs the copy off for longer.
    fn free(&self, w: &Window, now: i64) -> bool {
        !w.exhausted()
            || (w.past_reset(now)
                && !self
                    .backed_off
                    .iter()
                    .any(|b| b.key == w.key && Some(b.reset) == w.resets_at && now < b.until))
    }

    /// Until when a window that is not free holds the thread, if known: its reset still ahead,
    /// else the end of the back-off a refusal put on that copy.
    fn holds_until(&self, w: &Window, now: i64) -> Option<i64> {
        w.resets_at.filter(|r| *r > now).or_else(|| {
            self.backed_off
                .iter()
                .find(|b| b.key == w.key && Some(b.reset) == w.resets_at && now < b.until)
                .map(|b| b.until)
        })
    }

    /// The agent refused work again at `now` (a retry during the wait), with `reset` if known and
    /// `held` its windows then. The refusal is evidence as at the start of a wait: readings from
    /// before it no longer count, a reset time it came after is disproved, the copies past their
    /// reset are backed off, and a later reset time than the one waited for replaces it.
    pub fn learn(&mut self, reset: Option<i64>, now: i64, held: &[Window]) {
        self.since = self.since.max(now);
        if self.reset.is_some_and(|known| now >= known) {
            self.reset = None;
        }
        for copy in backed_off_by(held, now) {
            self.backed_off
                .retain(|b| !(b.key == copy.key && b.reset == copy.reset));
            self.backed_off.push(copy);
        }
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
                windows.iter().all(|w| self.free(w, now))
            } else {
                // Each blocking window must be in the reading, and free.
                self.blocking
                    .iter()
                    .all(|key| windows.iter().any(|w| w.key == *key && self.free(w, now)))
            };
            if freed {
                return Step::Continue;
            }
            // When every window still holding the thread says when it stops (its reset ahead, or
            // the end of a back-off), the next reading waits for the latest of them; one that
            // does not keeps the backoff.
            let latest = windows
                .iter()
                .filter(|w| !self.free(w, now))
                .filter(|w| self.blocking.is_empty() || self.blocking.contains(&w.key))
                .map(|w| self.holds_until(w, now))
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
        wait.learn(Some(900), 100, &[]);
        wait.learn(Some(50), 100, &[]);
        assert_eq!(wait.reset(), Some(1_000));
        wait.learn(Some(2_000), 100, &[]);
        assert_eq!(wait.reset(), Some(2_000));
        // A refusal after the reset it named disproves it: no blind continue at that time.
        let mut wait = LimitWait::new(Some(1_000), 100, &[]);
        wait.learn(None, 1_100, &[]);
        assert_eq!(wait.reset(), None);
        assert_ne!(wait.step(1_200, None), Step::Continue);
    }

    #[test]
    fn a_refusal_during_a_wait_is_evidence_like_the_one_that_started_it() {
        // Started at 100 with a reading at 200; the user retried at 4 100, past "Weekly"'s
        // reset, and was refused again with that copy unchanged.
        let copy = [window("Weekly", 1.0, Some(4_000))];
        let mut wait = LimitWait::new(None, 100, &copy);
        assert_eq!(wait.step(300, Some((200, &copy))), Step::Wait);
        wait.learn(None, 4_100, &copy);
        // The reading from before the refusal no longer counts, and the copy is backed off
        // (stale for 100 s at the refusal: the 15 min minimum, so until 5 000).
        assert_ne!(wait.step(4_200, Some((200, &copy))), Step::Continue);
        assert_ne!(wait.step(4_300, Some((4_250, &copy))), Step::Continue);
        assert_eq!(wait.step(5_000, Some((4_250, &copy))), Step::Continue);
        // A copy that changed after the refusal is judged afresh at once.
        let mut wait = LimitWait::new(None, 100, &copy);
        wait.learn(None, 4_100, &copy);
        let freed = [window("Weekly", 0.2, Some(600_000))];
        assert_eq!(wait.step(4_400, Some((4_350, &freed))), Step::Continue);
    }

    #[test]
    fn a_backed_off_abandoned_window_never_holds_the_thread_for_good() {
        // The real blocker had not reached the snapshot when the agent refused at 5 000: only
        // the abandoned "Weekly" copy (stale for 1 000 s) was held, so it is backed off until
        // 6 000. "5-hour" then arrives free and "Weekly" never changes: it is tried again.
        let held = [window("Weekly", 1.0, Some(4_000))];
        let later = [
            window("5-hour", 0.0, Some(27_000)),
            window("Weekly", 1.0, Some(4_000)),
        ];
        let mut wait = LimitWait::new(None, 5_000, &held);
        assert_ne!(wait.step(5_200, Some((5_100, &later))), Step::Continue);
        assert_eq!(wait.step(6_000, Some((5_100, &later))), Step::Continue);
        // Both held copies past their reset, only one genuinely full: each is backed off by
        // how long it had been stale, and the abandoned one is tried again once "5-hour" frees.
        let all_stale = [
            window("5-hour", 1.0, Some(3_000)),
            window("Weekly", 1.0, Some(4_000)),
        ];
        let five_hour_free = [
            window("5-hour", 0.0, Some(30_000)),
            window("Weekly", 1.0, Some(4_000)),
        ];
        let mut wait = LimitWait::new(None, 5_000, &all_stale);
        assert_ne!(
            wait.step(5_500, Some((5_400, &five_hour_free))),
            Step::Continue
        );
        assert_eq!(
            wait.step(6_000, Some((5_400, &five_hour_free))),
            Step::Continue
        );
        // A genuinely full window beside it is still waited for, at its own reset.
        let mixed = [
            window("5-hour", 1.0, Some(9_000)),
            window("Weekly", 1.0, Some(4_000)),
        ];
        let mut wait = LimitWait::new(None, 5_000, &mixed);
        assert_eq!(wait.step(5_110, Some((5_100, &mixed))), Step::Wait);
        assert_eq!(wait.reset(), Some(9_000));
        let freed = [
            window("5-hour", 0.0, Some(27_000)),
            window("Weekly", 1.0, Some(4_000)),
        ];
        assert_eq!(wait.step(9_100, Some((9_070, &freed))), Step::Continue);
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

    #[test]
    fn an_earlier_reading_after_the_limit_frees_a_window_once_its_reset_passes() {
        // The reading taken at 200 is the latest one: no new probe is needed to judge it later.
        let at_limit = [window("Weekly", 1.0, Some(4_000))];
        let mut wait = LimitWait::new(None, 100, &at_limit);
        assert_eq!(wait.step(300, Some((200, &at_limit))), Step::Wait);
        assert_eq!(
            wait.step(4_000 + GRACE, Some((200, &at_limit))),
            Step::Continue
        );
    }

    #[test]
    fn a_wrong_stale_guess_backs_off_and_each_failure_doubles_the_gap() {
        // The guess was wrong: the continued turn failed on the limit at 5 000 while "Weekly"
        // still showed the reset it had passed 1 000 s earlier. The copy is backed off as long.
        let refused = [window("Weekly", 1.0, Some(4_000))];
        let mut wait = LimitWait::new(None, 5_000, &refused);
        assert_eq!(wait.step(5_110, Some((5_100, &refused))), Step::Wait);
        assert_ne!(wait.step(5_999, Some((5_100, &refused))), Step::Continue);
        assert_eq!(wait.step(6_000, Some((5_100, &refused))), Step::Continue);
        // Refused again at 6 000: stale for 2 000 s now, so backed off until 8 000. The gaps
        // grow (1 000, 2 000, 4 000, ...), so a genuinely full window never loops.
        let mut wait = LimitWait::new(None, 6_000, &refused);
        assert_ne!(wait.step(7_999, Some((6_050, &refused))), Step::Continue);
        assert_eq!(wait.step(8_000, Some((6_050, &refused))), Step::Continue);
        // A copy naming a new reset is a new copy: waited for while ahead, stale once past.
        let mut wait = LimitWait::new(None, 5_000, &refused);
        let renewed = [window("Weekly", 1.0, Some(20_000))];
        assert_eq!(wait.step(5_110, Some((5_100, &renewed))), Step::Wait);
        assert_eq!(wait.reset(), Some(20_000));
        assert_eq!(
            wait.step(20_000 + GRACE, Some((5_100, &renewed))),
            Step::Continue
        );
        // So does a drop in usage under the same reset.
        let mut wait = LimitWait::new(None, 5_000, &refused);
        let dropped = [window("Weekly", 0.3, Some(4_000))];
        assert_eq!(wait.step(5_200, Some((5_100, &dropped))), Step::Continue);
    }

    #[test]
    fn a_stale_blocking_window_beside_a_full_one_waits_for_the_full_one() {
        let at_limit = [
            window("5-hour", 1.0, Some(4_000)),
            window("Weekly", 1.0, Some(9_000)),
        ];
        let mut wait = LimitWait::new(None, 100, &at_limit);
        assert_eq!(wait.step(300, Some((200, &at_limit))), Step::Wait);
        // "5-hour" is past its reset, "Weekly" is not: the thread still waits, for "Weekly".
        assert_eq!(wait.step(4_000 + GRACE, Some((200, &at_limit))), Step::Wait);
        assert_eq!(wait.reset(), Some(9_000));
        assert_eq!(
            wait.step(9_000 + GRACE, Some((200, &at_limit))),
            Step::Continue
        );
    }
}
