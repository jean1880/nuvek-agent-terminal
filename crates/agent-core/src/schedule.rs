//! Pure scheduling math, validation and firing decisions for scheduled prompts (no GTK, no I/O).
//!
//! Covers one-shot, weekday-and-time, and interval rules across local-time DST transitions
//! through [`LocalClock`], missed-run coalescing at app launch, and the invariants for
//! unattended turns:
//! - never in dangerous permissive mode (`Mode::Bypass`),
//! - `agy` only in an isolated worktree,
//! - never answering an approval,
//! - capped at [`MAX_UNATTENDED_SCHEDULED_RUNS`] unattended runs between user messages.

use crate::adapter::{Driver, Mode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Maximum unattended runs for one schedule since the user last spoke, edited or resumed it.
pub const MAX_UNATTENDED_SCHEDULED_RUNS: u32 = 10;
/// Shortest allowed interval for repeating interval rules (60 minutes).
pub const MIN_INTERVAL_SECS: i64 = 60 * 60;
/// Grace window after `due` within which a busy/waiting thread defers instead of skipping (30 min).
pub const FIRE_GRACE_SECS: i64 = 30 * 60;
/// Most schedules started in a single scheduler tick across the whole app.
pub const MAX_FIRES_PER_TICK: usize = 3;
/// Key inside `ItemStarted.input` tagging a user message emitted by a scheduled run.
pub const SCHEDULED_MARKER_KEY: &str = "scheduled";

/// The three scheduling modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Once,
    Days,
    Interval,
}

impl RuleKind {
    /// Every rule kind, guarded by [`tests::rule_kind_all_is_exhaustive`].
    pub const ALL: [RuleKind; 3] = [RuleKind::Once, RuleKind::Days, RuleKind::Interval];

    pub fn label(self) -> &'static str {
        match self {
            RuleKind::Once => "Once",
            RuleKind::Days => "Days",
            RuleKind::Interval => "Every N hours",
        }
    }
}

/// When a schedule is due.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rule {
    /// Run once at `at` (Unix epoch seconds).
    Once { at: i64 },
    /// Run on selected weekdays (`[Mon, Tue, Wed, Thu, Fri, Sat, Sun]`) at local `hour:minute`.
    Days {
        weekdays: [bool; 7],
        hour: u8,
        minute: u8,
    },
    /// Run every `every_secs` seconds (>= [`MIN_INTERVAL_SECS`]) anchored at `anchor` (epoch secs).
    Interval { every_secs: i64, anchor: i64 },
}

impl Rule {
    pub fn kind(&self) -> RuleKind {
        match self {
            Rule::Once { .. } => RuleKind::Once,
            Rule::Days { .. } => RuleKind::Days,
            Rule::Interval { .. } => RuleKind::Interval,
        }
    }

    /// Short human summary for chips and list rows.
    pub fn summary(&self, clock: &dyn LocalClock) -> String {
        match self {
            Rule::Once { at } => match clock.civil_at(*at) {
                Some(c) => format!(
                    "once {:04}-{:02}-{:02} {:02}:{:02}",
                    c.year, c.month, c.day, c.hour, c.minute
                ),
                None => "once".to_owned(),
            },
            Rule::Days {
                weekdays,
                hour,
                minute,
            } => {
                let day_part = weekday_summary(weekdays);
                format!("{day_part} {hour:02}:{minute:02}")
            }
            Rule::Interval { every_secs, .. } => {
                let hours = (*every_secs / 3600).max(1);
                if hours == 1 {
                    "every hour".to_owned()
                } else {
                    format!("every {hours} hours")
                }
            }
        }
    }
}

fn weekday_summary(weekdays: &[bool; 7]) -> String {
    const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    if *weekdays == [true, true, true, true, true, false, false] {
        return "weekdays".to_owned();
    }
    if *weekdays == [false, false, false, false, false, true, true] {
        return "weekends".to_owned();
    }
    if *weekdays == [true; 7] {
        return "daily".to_owned();
    }
    let picked: Vec<&str> = weekdays
        .iter()
        .zip(NAMES)
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
    if picked.is_empty() {
        "days".to_owned()
    } else {
        picked.join("/")
    }
}

/// What happens when one or more occurrences passed while the app was closed or suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissedPolicy {
    Skip,
    RunOnce,
}

impl MissedPolicy {
    /// Every missed-run policy, guarded by [`tests::missed_policy_all_is_exhaustive`].
    pub const ALL: [MissedPolicy; 2] = [MissedPolicy::Skip, MissedPolicy::RunOnce];

    pub fn as_str(self) -> &'static str {
        match self {
            MissedPolicy::Skip => "skip",
            MissedPolicy::RunOnce => "run_once",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "skip" => Some(MissedPolicy::Skip),
            "run_once" => Some(MissedPolicy::RunOnce),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MissedPolicy::Skip => "Skip missed runs",
            MissedPolicy::RunOnce => "Run once when reopened",
        }
    }
}

/// Optional stopping rule for a repeating schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ends {
    #[default]
    Never,
    AfterRuns {
        max: u32,
    },
    OnDate {
        at: i64,
    },
}

/// Recorded outcome of the most recent scheduled check/fire (`schedules.last_outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastOutcome {
    Ran,
    Missed,
    SkippedBusy,
    SkippedApproval,
    SkippedLimit,
    SkippedMode,
    SkippedAgyWorktree,
    Failed,
}

impl LastOutcome {
    /// Every outcome stored in `schedules.last_outcome`, guarded by
    /// [`tests::last_outcome_all_is_exhaustive`].
    pub const ALL: [LastOutcome; 8] = [
        LastOutcome::Ran,
        LastOutcome::Missed,
        LastOutcome::SkippedBusy,
        LastOutcome::SkippedApproval,
        LastOutcome::SkippedLimit,
        LastOutcome::SkippedMode,
        LastOutcome::SkippedAgyWorktree,
        LastOutcome::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LastOutcome::Ran => "ran",
            LastOutcome::Missed => "missed",
            LastOutcome::SkippedBusy => "skipped_busy",
            LastOutcome::SkippedApproval => "skipped_approval",
            LastOutcome::SkippedLimit => "skipped_limit",
            LastOutcome::SkippedMode => "skipped_mode",
            LastOutcome::SkippedAgyWorktree => "skipped_agy_worktree",
            LastOutcome::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == raw)
    }

    pub fn label(self) -> &'static str {
        match self {
            LastOutcome::Ran => "Ran",
            LastOutcome::Missed => "Missed while closed",
            LastOutcome::SkippedBusy => "Skipped: thread busy",
            LastOutcome::SkippedApproval => "Skipped: waiting for your approval",
            LastOutcome::SkippedLimit => "Skipped: waiting for usage limit",
            LastOutcome::SkippedMode => "Skipped: Bypass mode not allowed",
            LastOutcome::SkippedAgyWorktree => "Skipped: agy requires a worktree",
            LastOutcome::Failed => "Failed",
        }
    }
}

/// Why a schedule is paused (`schedules.paused_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PausedReason {
    User,
    StoppedRun,
    Cap,
    Budget,
    Ended,
    TargetGone,
    SettingOff,
}

impl PausedReason {
    /// Every pause reason stored in `schedules.paused_reason`, guarded by
    /// [`tests::paused_reason_all_is_exhaustive`].
    pub const ALL: [PausedReason; 7] = [
        PausedReason::User,
        PausedReason::StoppedRun,
        PausedReason::Cap,
        PausedReason::Budget,
        PausedReason::Ended,
        PausedReason::TargetGone,
        PausedReason::SettingOff,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PausedReason::User => "user",
            PausedReason::StoppedRun => "stopped_run",
            PausedReason::Cap => "cap",
            PausedReason::Budget => "budget",
            PausedReason::Ended => "ended",
            PausedReason::TargetGone => "target_gone",
            PausedReason::SettingOff => "setting_off",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == raw)
    }

    pub fn label(self) -> &'static str {
        match self {
            PausedReason::User => "Paused by you",
            PausedReason::StoppedRun => "Paused: you stopped its run",
            PausedReason::Cap => "Paused: 10 unattended runs reached",
            PausedReason::Budget => "Paused: spending budget reached",
            PausedReason::Ended => "Ended",
            PausedReason::TargetGone => "Paused: target folder or thread missing",
            PausedReason::SettingOff => "Paused: scheduled prompts disabled in Settings",
        }
    }
}

/// Where a scheduled prompt runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    ExistingThread {
        thread_id: String,
    },
    NewThread {
        folder: String,
        driver: Driver,
        profile_id: Option<String>,
        mode: Mode,
        model: Option<String>,
        worktree: bool,
    },
}

/// Civil local date and time fields used by [`LocalClock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilDateTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    /// `0` = Monday .. `6` = Sunday.
    pub weekday_mon0: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Result of resolving a local wall-clock `(year, month, day, hour, minute)` to Unix epoch secs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveLocal {
    Exact(i64),
    /// Clocks sprang forward over this wall time; carries the first valid instant after the gap.
    SkippedSpringForward(i64),
    /// Clocks fell back so the wall time occurs twice; carries both instants in order.
    AmbiguousFallBack {
        first: i64,
        second: i64,
    },
    Invalid,
}

/// Pure clock interface for converting between Unix epoch seconds and civil local wall-clock time.
pub trait LocalClock {
    fn civil_at(&self, epoch: i64) -> Option<CivilDateTime>;
    fn resolve_local(&self, year: i32, month: u8, day: u8, hour: u8, minute: u8) -> ResolveLocal;
}

/// Validates a schedule prompt: must contain non-whitespace text.
pub fn validate_prompt(prompt: &str) -> Result<(), &'static str> {
    if prompt.trim().is_empty() {
        return Err("Enter a prompt to schedule.");
    }
    Ok(())
}

/// Validates a scheduling rule.
pub fn validate_rule(rule: &Rule) -> Result<(), &'static str> {
    match rule {
        Rule::Once { at } => {
            if *at <= 0 {
                return Err("Choose a valid run time.");
            }
        }
        Rule::Days {
            weekdays,
            hour,
            minute,
        } => {
            if !weekdays.iter().any(|on| *on) {
                return Err("Select at least one weekday.");
            }
            if *hour >= 24 || *minute >= 60 {
                return Err("Enter a valid time of day (HH:MM).");
            }
        }
        Rule::Interval { every_secs, anchor } => {
            if *every_secs < MIN_INTERVAL_SECS {
                return Err("Interval must be at least 1 hour.");
            }
            if *anchor <= 0 {
                return Err("Choose a valid start time.");
            }
        }
    }
    Ok(())
}

/// Validates the target of a schedule (refuses `Mode::Bypass` and non-worktree `Driver::Agy`).
pub fn validate_target(target: &Target) -> Result<(), &'static str> {
    match target {
        Target::ExistingThread { thread_id } => {
            if thread_id.trim().is_empty() {
                return Err("Missing target thread.");
            }
        }
        Target::NewThread {
            folder,
            driver,
            mode,
            worktree,
            ..
        } => {
            if folder.trim().is_empty() {
                return Err("Choose a folder for the scheduled thread.");
            }
            if *mode == Mode::Bypass {
                return Err("Scheduled prompts never run in Bypass mode.");
            }
            if *driver == Driver::Agy && !*worktree {
                return Err("Antigravity schedules must run in an isolated git worktree.");
            }
        }
    }
    Ok(())
}

/// Validates stopping conditions.
pub fn validate_ends(ends: Ends) -> Result<(), &'static str> {
    match ends {
        Ends::Never => Ok(()),
        Ends::AfterRuns { max } => {
            if max == 0 {
                Err("Run limit must be at least 1.")
            } else {
                Ok(())
            }
        }
        Ends::OnDate { at } => {
            if at <= 0 {
                Err("Choose a valid end date.")
            } else {
                Ok(())
            }
        }
    }
}

/// Computes the next Unix epoch timestamp strictly after `after` for `rule`, or `None` when none
/// remains (e.g. a past `Once`, or an invalid rule).
pub fn next_due(rule: &Rule, after: i64, clock: &dyn LocalClock) -> Option<i64> {
    validate_rule(rule).ok()?;
    match rule {
        Rule::Once { at } => (*at > after).then_some(*at),
        Rule::Days {
            weekdays,
            hour,
            minute,
        } => {
            let start = clock.civil_at(after)?;
            // Scan up to 8 days so a same-day time already passed wraps cleanly to next week.
            for days in (days_from_civil(start.year, start.month, start.day)..).take(8) {
                let (y, m, d) = civil_from_days(days);
                let wd = weekday_mon0_from_days(days) as usize;
                if weekdays[wd] {
                    let candidate = match clock.resolve_local(y, m, d, *hour, *minute) {
                        ResolveLocal::Exact(t) | ResolveLocal::SkippedSpringForward(t) => Some(t),
                        ResolveLocal::AmbiguousFallBack { first, .. } => Some(first),
                        ResolveLocal::Invalid => None,
                    };
                    if let Some(t) = candidate {
                        if t > after {
                            return Some(t);
                        }
                    }
                }
            }
            None
        }
        Rule::Interval { every_secs, anchor } => {
            if *anchor > after {
                return Some(*anchor);
            }
            let steps = (after - anchor) / every_secs + 1;
            anchor.checked_add(steps.checked_mul(*every_secs)?)
        }
    }
}

/// Outcome of checking whether a scheduled run missed its window while the app was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedResult {
    NotDue,
    DueNow {
        due: i64,
    },
    Missed {
        latest_due: i64,
        run_once: bool,
        next_due: Option<i64>,
    },
}

/// Checks `scheduled_due` against `now`, coalescing any missed occurrences into at most one run.
pub fn missed(
    rule: &Rule,
    scheduled_due: i64,
    now: i64,
    policy: MissedPolicy,
    clock: &dyn LocalClock,
) -> MissedResult {
    if now < scheduled_due {
        return MissedResult::NotDue;
    }
    if now <= scheduled_due.saturating_add(FIRE_GRACE_SECS) {
        return MissedResult::DueNow { due: scheduled_due };
    }
    let latest_due = match rule {
        Rule::Once { at } => *at,
        Rule::Interval { every_secs, anchor } => {
            if *every_secs >= MIN_INTERVAL_SECS && now >= *anchor {
                let steps = (now.saturating_sub(*anchor)) / every_secs;
                anchor.saturating_add(steps.saturating_mul(*every_secs))
            } else {
                scheduled_due
            }
        }
        Rule::Days {
            weekdays,
            hour,
            minute,
        } => {
            let mut latest = scheduled_due;
            if let Some(now_civil) = clock.civil_at(now) {
                let today_days = days_from_civil(now_civil.year, now_civil.month, now_civil.day);
                for offset in 0..8 {
                    let days = today_days - offset;
                    let (y, m, d) = civil_from_days(days);
                    let wd = weekday_mon0_from_days(days) as usize;
                    if weekdays[wd] {
                        let candidate = match clock.resolve_local(y, m, d, *hour, *minute) {
                            ResolveLocal::Exact(t) | ResolveLocal::SkippedSpringForward(t) => {
                                Some(t)
                            }
                            ResolveLocal::AmbiguousFallBack { first, .. } => Some(first),
                            ResolveLocal::Invalid => None,
                        };
                        if let Some(t) = candidate {
                            if t <= now && t >= scheduled_due {
                                latest = t;
                                break;
                            }
                        }
                    }
                }
            }
            latest
        }
    };
    let next = next_due(rule, now, clock);
    MissedResult::Missed {
        latest_due,
        run_once: policy == MissedPolicy::RunOnce,
        next_due: next,
    }
}

/// Live state checked right before starting a scheduled turn.
#[derive(Debug, Clone)]
pub struct ScheduleState<'a> {
    pub enabled: bool,
    pub paused: Option<PausedReason>,
    pub unattended_runs: u32,
    pub run_count: u32,
    pub ends: Ends,
    pub now: i64,
    pub due: i64,
    pub driver: Driver,
    pub in_worktree: bool,
    pub mode: Mode,
    pub child: bool,
    pub wakes_held: bool,
    pub limit_waiting: bool,
    pub has_open_approval: bool,
    pub busy_reason: Option<&'a str>,
}

/// Decision for a due schedule tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleDecision {
    Fire,
    Defer(String),
    Skip(LastOutcome),
    Pause(PausedReason),
}

/// Evaluates whether a due schedule may start a turn right now.
pub fn decide(state: &ScheduleState<'_>) -> ScheduleDecision {
    if !state.enabled {
        return ScheduleDecision::Pause(PausedReason::SettingOff);
    }
    if let Some(reason) = state.paused {
        return ScheduleDecision::Pause(reason);
    }
    match state.ends {
        Ends::Never => {}
        Ends::AfterRuns { max } => {
            if state.run_count >= max {
                return ScheduleDecision::Pause(PausedReason::Ended);
            }
        }
        Ends::OnDate { at } => {
            if state.now > at {
                return ScheduleDecision::Pause(PausedReason::Ended);
            }
        }
    }
    if state.unattended_runs >= MAX_UNATTENDED_SCHEDULED_RUNS {
        return ScheduleDecision::Pause(PausedReason::Cap);
    }
    if state.child {
        return ScheduleDecision::Skip(LastOutcome::Failed);
    }
    if state.driver == Driver::Agy && !state.in_worktree {
        return ScheduleDecision::Skip(LastOutcome::SkippedAgyWorktree);
    }
    if state.mode == Mode::Bypass {
        return ScheduleDecision::Skip(LastOutcome::SkippedMode);
    }
    let past_grace = state.now > state.due.saturating_add(FIRE_GRACE_SECS);
    if state.has_open_approval {
        return if past_grace {
            ScheduleDecision::Skip(LastOutcome::SkippedApproval)
        } else {
            ScheduleDecision::Defer("waiting for your approval".to_owned())
        };
    }
    if state.limit_waiting {
        return if past_grace {
            ScheduleDecision::Skip(LastOutcome::SkippedLimit)
        } else {
            ScheduleDecision::Defer("waiting for usage limit reset".to_owned())
        };
    }
    if state.wakes_held {
        return if past_grace {
            ScheduleDecision::Skip(LastOutcome::SkippedBusy)
        } else {
            ScheduleDecision::Defer("stopped until you send a message".to_owned())
        };
    }
    if let Some(why) = state.busy_reason {
        return if past_grace {
            ScheduleDecision::Skip(LastOutcome::SkippedBusy)
        } else {
            ScheduleDecision::Defer(why.to_owned())
        };
    }
    ScheduleDecision::Fire
}

/// Marker written into `ItemStarted.input` for a scheduled user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledMarker {
    pub schedule: String,
    pub due: i64,
}

/// Builds the `ItemStarted.input` JSON value for a scheduled user message.
pub fn scheduled_input(schedule: &str, due: i64) -> Value {
    json!({
        SCHEDULED_MARKER_KEY: {
            "schedule": schedule,
            "due": due,
        }
    })
}

/// Reads [`ScheduledMarker`] from `ItemStarted.input` when present and valid.
pub fn parse_scheduled_input(input: &Value) -> Option<ScheduledMarker> {
    let inner = input.get(SCHEDULED_MARKER_KEY)?;
    serde_json::from_value::<ScheduledMarker>(inner.clone())
        .ok()
        .filter(|m| !m.schedule.trim().is_empty())
}

// Howard Hinnant's proleptic Gregorian calendar conversions (days since 1970-01-01).
fn days_from_civil(y: i32, m: u8, d: u8) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let d = i64::from(d);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + i64::from(m <= 2);
    (y as i32, m as u8, d as u8)
}

/// Weekday `0 = Mon .. 6 = Sun` from Unix days (1970-01-01 was Thursday = 3).
fn weekday_mon0_from_days(days: i64) -> u8 {
    ((days + 3).rem_euclid(7)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedOffsetClock {
        offset_secs: i64,
    }

    impl FixedOffsetClock {
        fn utc() -> Self {
            Self { offset_secs: 0 }
        }
    }

    impl LocalClock for FixedOffsetClock {
        fn civil_at(&self, epoch: i64) -> Option<CivilDateTime> {
            let local = epoch + self.offset_secs;
            let days = local.div_euclid(86_400);
            let secs = local.rem_euclid(86_400);
            let (year, month, day) = civil_from_days(days);
            Some(CivilDateTime {
                year,
                month,
                day,
                weekday_mon0: weekday_mon0_from_days(days),
                hour: (secs / 3600) as u8,
                minute: ((secs % 3600) / 60) as u8,
                second: (secs % 60) as u8,
            })
        }

        fn resolve_local(
            &self,
            year: i32,
            month: u8,
            day: u8,
            hour: u8,
            minute: u8,
        ) -> ResolveLocal {
            let days = days_from_civil(year, month, day);
            let local_epoch = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60;
            ResolveLocal::Exact(local_epoch - self.offset_secs)
        }
    }

    /// Fake clock that models US-Eastern-like DST transitions on specific dates:
    /// - Spring forward on 2026-03-08: 02:00..02:59 skipped, jumps to 03:00 EDT (-4h).
    /// - Fall back on 2026-11-01: 01:00..01:59 happens twice (-4h first, -5h second).
    struct DstClock;

    impl LocalClock for DstClock {
        fn civil_at(&self, epoch: i64) -> Option<CivilDateTime> {
            // Spring transition instant: 2026-03-08 02:00 EST = 2026-03-08 07:00 UTC.
            let spring = days_from_civil(2026, 3, 8) * 86_400 + 7 * 3600;
            // Fall transition instant: 2026-11-01 02:00 EDT = 2026-11-01 06:00 UTC.
            let fall = days_from_civil(2026, 11, 1) * 86_400 + 6 * 3600;
            let offset = if epoch >= spring && epoch < fall {
                -4 * 3600
            } else {
                -5 * 3600
            };
            FixedOffsetClock {
                offset_secs: offset,
            }
            .civil_at(epoch)
        }

        fn resolve_local(
            &self,
            year: i32,
            month: u8,
            day: u8,
            hour: u8,
            minute: u8,
        ) -> ResolveLocal {
            let base = days_from_civil(year, month, day) * 86_400
                + i64::from(hour) * 3600
                + i64::from(minute) * 60;
            if (year, month, day) == (2026, 3, 8) && hour == 2 {
                let first_valid = days_from_civil(2026, 3, 8) * 86_400 + 7 * 3600;
                return ResolveLocal::SkippedSpringForward(first_valid);
            }
            if (year, month, day) == (2026, 11, 1) && hour == 1 {
                return ResolveLocal::AmbiguousFallBack {
                    first: base + 4 * 3600,
                    second: base + 5 * 3600,
                };
            }
            let spring = days_from_civil(2026, 3, 8) * 86_400 + 7 * 3600;
            let fall = days_from_civil(2026, 11, 1) * 86_400 + 6 * 3600;
            let edt_guess = base + 4 * 3600;
            if edt_guess >= spring && edt_guess < fall {
                ResolveLocal::Exact(edt_guess)
            } else {
                ResolveLocal::Exact(base + 5 * 3600)
            }
        }
    }

    fn base_state<'a>() -> ScheduleState<'a> {
        ScheduleState {
            enabled: true,
            paused: None,
            unattended_runs: 0,
            run_count: 0,
            ends: Ends::Never,
            now: 1_000_000,
            due: 1_000_000,
            driver: Driver::Claude,
            in_worktree: false,
            mode: Mode::Plan,
            child: false,
            wakes_held: false,
            limit_waiting: false,
            has_open_approval: false,
            busy_reason: None,
        }
    }

    #[test]
    fn rule_kind_all_is_exhaustive() {
        for kind in RuleKind::ALL {
            let _ = match kind {
                RuleKind::Once => 0,
                RuleKind::Days => 1,
                RuleKind::Interval => 2,
            };
        }
    }

    #[test]
    fn missed_policy_all_is_exhaustive() {
        for policy in MissedPolicy::ALL {
            let back = MissedPolicy::parse(policy.as_str()).expect("round-trip");
            assert_eq!(back, policy);
            let _ = match policy {
                MissedPolicy::Skip => 0,
                MissedPolicy::RunOnce => 1,
            };
        }
    }

    #[test]
    fn last_outcome_all_is_exhaustive() {
        for outcome in LastOutcome::ALL {
            assert_eq!(LastOutcome::parse(outcome.as_str()), Some(outcome));
            let _ = match outcome {
                LastOutcome::Ran => 0,
                LastOutcome::Missed => 1,
                LastOutcome::SkippedBusy => 2,
                LastOutcome::SkippedApproval => 3,
                LastOutcome::SkippedLimit => 4,
                LastOutcome::SkippedMode => 5,
                LastOutcome::SkippedAgyWorktree => 6,
                LastOutcome::Failed => 7,
            };
        }
    }

    #[test]
    fn paused_reason_all_is_exhaustive() {
        for reason in PausedReason::ALL {
            assert_eq!(PausedReason::parse(reason.as_str()), Some(reason));
            let _ = match reason {
                PausedReason::User => 0,
                PausedReason::StoppedRun => 1,
                PausedReason::Cap => 2,
                PausedReason::Budget => 3,
                PausedReason::Ended => 4,
                PausedReason::TargetGone => 5,
                PausedReason::SettingOff => 6,
            };
        }
    }

    #[test]
    fn next_due_weekdays_skips_weekend() {
        let clock = FixedOffsetClock::utc();
        let rule = Rule::Days {
            weekdays: [true, true, true, true, true, false, false],
            hour: 8,
            minute: 0,
        };
        // Friday 2026-11-13 09:00 UTC -> next weekday is Monday 2026-11-16 08:00 UTC.
        let fri = days_from_civil(2026, 11, 13) * 86_400 + 9 * 3600;
        let mon = days_from_civil(2026, 11, 16) * 86_400 + 8 * 3600;
        assert_eq!(next_due(&rule, fri, &clock), Some(mon));
    }

    #[test]
    fn next_due_spring_forward_moves_to_next_valid_minute() {
        let rule = Rule::Days {
            weekdays: [true; 7],
            hour: 2,
            minute: 30,
        };
        let sat = days_from_civil(2026, 3, 7) * 86_400 + 12 * 3600;
        let expected = days_from_civil(2026, 3, 8) * 86_400 + 7 * 3600;
        assert_eq!(next_due(&rule, sat, &DstClock), Some(expected));
    }

    #[test]
    fn next_due_fall_back_fires_once() {
        let rule = Rule::Days {
            weekdays: [true; 7],
            hour: 1,
            minute: 30,
        };
        // First 01:30 EDT = 05:30 UTC; asking after 05:30 UTC advances to next day (Nov 2).
        let first_occurrence = days_from_civil(2026, 11, 1) * 86_400 + 5 * 3600 + 30 * 60;
        let sat = days_from_civil(2026, 10, 31) * 86_400 + 12 * 3600;
        assert_eq!(next_due(&rule, sat, &DstClock), Some(first_occurrence));
        let mon = days_from_civil(2026, 11, 2) * 86_400 + 6 * 3600 + 30 * 60;
        assert_eq!(next_due(&rule, first_occurrence, &DstClock), Some(mon));
    }

    #[test]
    fn missed_runs_coalesce_to_one_and_skip_policy_never_runs_missed() {
        let clock = FixedOffsetClock::utc();
        let rule = Rule::Interval {
            every_secs: 3600,
            anchor: 1_000_000,
        };
        let now = 1_000_000 + 5 * 3600 + 100;
        assert_eq!(
            missed(&rule, 1_000_000, now, MissedPolicy::RunOnce, &clock),
            MissedResult::Missed {
                latest_due: 1_000_000 + 5 * 3600,
                run_once: true,
                next_due: Some(1_000_000 + 6 * 3600),
            }
        );
        assert_eq!(
            missed(&rule, 1_000_000, now, MissedPolicy::Skip, &clock),
            MissedResult::Missed {
                latest_due: 1_000_000 + 5 * 3600,
                run_once: false,
                next_due: Some(1_000_000 + 6 * 3600),
            }
        );
    }

    #[test]
    fn rule_below_min_interval_is_refused() {
        let rule = Rule::Interval {
            every_secs: MIN_INTERVAL_SECS - 1,
            anchor: 1000,
        };
        assert!(validate_rule(&rule).is_err());
        assert_eq!(next_due(&rule, 1000, &FixedOffsetClock::utc()), None);
    }

    #[test]
    fn target_validation_enforces_mode_and_agy_worktree() {
        assert!(validate_target(&Target::NewThread {
            folder: "/repo".into(),
            driver: Driver::Claude,
            profile_id: None,
            mode: Mode::Bypass,
            model: None,
            worktree: true,
        })
        .is_err());
        assert!(validate_target(&Target::NewThread {
            folder: "/repo".into(),
            driver: Driver::Agy,
            profile_id: None,
            mode: Mode::Plan,
            model: None,
            worktree: false,
        })
        .is_err());
        assert!(validate_target(&Target::NewThread {
            folder: "/repo".into(),
            driver: Driver::Agy,
            profile_id: None,
            mode: Mode::AcceptEdits,
            model: None,
            worktree: true,
        })
        .is_ok());
    }

    #[test]
    fn decide_pauses_after_max_unattended_runs() {
        let mut s = base_state();
        s.unattended_runs = MAX_UNATTENDED_SCHEDULED_RUNS;
        assert_eq!(decide(&s), ScheduleDecision::Pause(PausedReason::Cap));
    }

    #[test]
    fn decide_skips_bypass_and_forced() {
        let mut s = base_state();
        s.mode = Mode::Bypass;
        assert_eq!(decide(&s), ScheduleDecision::Skip(LastOutcome::SkippedMode));
        s.mode = Mode::Plan;
        s.driver = Driver::Agy;
        s.in_worktree = false;
        assert_eq!(
            decide(&s),
            ScheduleDecision::Skip(LastOutcome::SkippedAgyWorktree)
        );
        s.in_worktree = true;
        assert_eq!(decide(&s), ScheduleDecision::Fire);
    }

    #[test]
    fn decide_defers_then_skips_after_grace() {
        let mut s = base_state();
        s.busy_reason = Some("a turn is running");
        assert!(matches!(decide(&s), ScheduleDecision::Defer(_)));
        s.now = s.due + FIRE_GRACE_SECS + 1;
        assert_eq!(decide(&s), ScheduleDecision::Skip(LastOutcome::SkippedBusy));

        let mut approval = base_state();
        approval.has_open_approval = true;
        assert!(matches!(decide(&approval), ScheduleDecision::Defer(_)));
        approval.now = approval.due + FIRE_GRACE_SECS + 1;
        assert_eq!(
            decide(&approval),
            ScheduleDecision::Skip(LastOutcome::SkippedApproval)
        );
    }

    #[test]
    fn scheduled_input_round_trips() {
        let val = scheduled_input("sched-1", 1_795_000_000);
        assert_eq!(
            parse_scheduled_input(&val),
            Some(ScheduledMarker {
                schedule: "sched-1".into(),
                due: 1_795_000_000,
            })
        );
    }

    #[test]
    fn grace_window_at_max_timestamp_never_overflows() {
        let clock = FixedOffsetClock::utc();
        let rule = Rule::Once { at: i64::MAX };
        assert_eq!(
            missed(&rule, i64::MAX, i64::MAX, MissedPolicy::Skip, &clock),
            MissedResult::DueNow { due: i64::MAX }
        );
        let mut s = base_state();
        s.due = i64::MAX;
        s.now = i64::MAX;
        s.busy_reason = Some("busy");
        assert!(matches!(decide(&s), ScheduleDecision::Defer(_)));
    }
}
