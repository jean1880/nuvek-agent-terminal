//! Pure view-model logic, local-clock GLib adapter, and form validation for scheduled prompts (F6).

use agent_core::adapter::Mode;
use agent_core::redact::redact;
use agent_core::schedule::{
    next_due, validate_ends, validate_prompt, validate_rule, validate_target, CivilDateTime, Ends,
    LastOutcome, LocalClock, MissedPolicy, PausedReason, ResolveLocal, Rule, Target,
    MIN_INTERVAL_SECS,
};
use gtk4::glib;

/// Modes offered for scheduled new-thread targets (`Mode::Bypass` is never allowed).
pub const SAFE_MODES: [Mode; 3] = [Mode::Plan, Mode::Ask, Mode::AcceptEdits];

/// Text shared by a sidebar clock's tooltip and the row's screen-reader description.
pub fn schedule_badge_description(badge: super::sidebar_model::ScheduleBadge, now: i64) -> String {
    use super::sidebar_model::ScheduleBadge;
    match badge {
        ScheduleBadge::Next(due) => format!(
            "Scheduled prompt. Next: {}",
            format_due(due, now, &GlibLocalClock)
        ),
        ScheduleBadge::Paused(reason) => format!("Scheduled prompt. {}", reason.label()),
        ScheduleBadge::Unknown => "Schedule state unknown: the store could not be read".to_owned(),
    }
}

pub fn safe_mode_index(mode: Mode) -> u32 {
    match mode {
        Mode::Plan => 0,
        Mode::Ask => 1,
        Mode::AcceptEdits => 2,
        Mode::Bypass => 1,
    }
}

pub fn safe_mode_at(idx: u32) -> Mode {
    SAFE_MODES.get(idx as usize).copied().unwrap_or(Mode::Ask)
}

/// Converts between Unix epoch timestamps and the desktop's local timezone via [`glib::DateTime`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GlibLocalClock;

impl LocalClock for GlibLocalClock {
    fn civil_at(&self, epoch: i64) -> Option<CivilDateTime> {
        let dt = glib::DateTime::from_unix_local(epoch).ok()?;
        let wd = u8::try_from(dt.day_of_week().saturating_sub(1)).ok()?;
        Some(CivilDateTime {
            year: dt.year(),
            month: u8::try_from(dt.month()).ok()?,
            day: u8::try_from(dt.day_of_month()).ok()?,
            weekday_mon0: wd.min(6),
            hour: u8::try_from(dt.hour()).ok()?,
            minute: u8::try_from(dt.minute()).ok()?,
            second: u8::try_from(dt.second()).ok()?,
        })
    }

    fn resolve_local(&self, year: i32, month: u8, day: u8, hour: u8, minute: u8) -> ResolveLocal {
        let Ok(dt) = glib::DateTime::from_local(
            year,
            i32::from(month),
            i32::from(day),
            i32::from(hour),
            i32::from(minute),
            0.0,
        ) else {
            return ResolveLocal::Invalid;
        };
        let resolved_hour = dt.hour();
        let resolved_minute = dt.minute();
        let resolved_day = dt.day_of_month();
        let epoch = dt.to_unix();
        // Spring-forward gap: GLib shifts forward past the skipped wall clock time.
        if resolved_day != i32::from(day)
            || resolved_hour != i32::from(hour)
            || resolved_minute != i32::from(minute)
        {
            return ResolveLocal::SkippedSpringForward(epoch);
        }
        // Check if an hour later still resolves to the exact same wall time (fall-back fold).
        if let Ok(later) = glib::DateTime::from_unix_local(epoch.saturating_add(3600)) {
            if later.day_of_month() == i32::from(day)
                && later.hour() == i32::from(hour)
                && later.minute() == i32::from(minute)
            {
                return ResolveLocal::AmbiguousFallBack {
                    first: epoch,
                    second: epoch.saturating_add(3600),
                };
            }
        }
        ResolveLocal::Exact(epoch)
    }
}

/// Checks whether `prompt` contains an unresolved `{placeholder}` token that F5 saved prompts use.
pub fn has_unfilled_placeholder(prompt: &str) -> bool {
    agent_core::saved_prompts::parse(prompt)
        .into_iter()
        .any(|p| !matches!(p, agent_core::saved_prompts::Segment::Text(_)))
}

/// Parameters gathered from the schedule creation/edit form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormInput {
    pub prompt: String,
    pub has_images: bool,
    pub target: Target,
    pub in_git_repo: bool,
    pub rule: Rule,
    pub missed_policy: MissedPolicy,
    pub ends: Ends,
    pub now: i64,
}

/// Validated schedule fields ready to persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSchedule {
    pub prompt: String,
    pub target: Target,
    pub rule: Rule,
    pub missed_policy: MissedPolicy,
    pub ends: Ends,
    pub next_due: Option<i64>,
    pub has_secret_warning: bool,
}

/// Validates a schedule dialog submission before touching the store.
pub fn validate_form(
    input: &FormInput,
    clock: &dyn LocalClock,
) -> Result<ValidatedSchedule, &'static str> {
    if input.has_images {
        return Err("Remove attached images before scheduling a prompt.");
    }
    validate_prompt(&input.prompt)?;
    if has_unfilled_placeholder(&input.prompt) {
        return Err("Fill in every {placeholder} before scheduling.");
    }
    validate_target(&input.target)?;
    if let Target::NewThread {
        model: Some(model), ..
    } = &input.target
    {
        if model.chars().any(char::is_control) {
            return Err("Enter a model name without control characters.");
        }
    }
    if let Target::NewThread { worktree: true, .. } = &input.target {
        if !input.in_git_repo {
            return Err("Isolated schedules need a git repository for worktree isolation.");
        }
    }
    validate_rule(&input.rule)?;
    validate_ends(input.ends)?;
    let computed_next = match &input.rule {
        Rule::Once { at } => (*at > input.now).then_some(*at),
        Rule::Days { .. } | Rule::Interval { .. } => next_due(&input.rule, input.now, clock),
    };
    if computed_next.is_none() {
        return Err("Choose a future time for this schedule.");
    }
    if let Ends::OnDate { at } = input.ends {
        if at <= input.now || computed_next.is_some_and(|next| next > at) {
            return Err("Choose an end date on or after the first run.");
        }
    }
    let trimmed = input.prompt.trim().to_owned();
    let has_secret_warning = redact(&trimmed) != trimmed;
    Ok(ValidatedSchedule {
        prompt: trimmed,
        target: input.target.clone(),
        rule: input.rule.clone(),
        missed_policy: input.missed_policy,
        ends: input.ends,
        next_due: computed_next,
        has_secret_warning,
    })
}

/// Formats `epoch` cleanly in local time (`"Mon 08:00"` or `"YYYY-MM-DD HH:MM"`).
pub fn format_due(epoch: i64, now: i64, clock: &dyn LocalClock) -> String {
    let Some(c) = clock.civil_at(epoch) else {
        return epoch.to_string();
    };
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let day_name = DAYS.get(usize::from(c.weekday_mon0)).copied().unwrap_or("");
    if epoch.saturating_sub(now).abs() < 6 * 86_400 {
        format!("{day_name} {:02}:{:02}", c.hour, c.minute)
    } else {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            c.year, c.month, c.day, c.hour, c.minute
        )
    }
}

/// Single-line description of target + status shown in the Scheduled Prompts list.
pub fn schedule_row_subtitle(
    rule: &Rule,
    target_label: &str,
    next_due: Option<i64>,
    last_outcome: Option<LastOutcome>,
    paused: Option<PausedReason>,
    now: i64,
    clock: &dyn LocalClock,
) -> String {
    let mut parts = vec![target_label.to_owned(), rule.summary(clock)];
    if let Some(reason) = paused {
        parts.push(reason.label().to_owned());
    } else if let Some(due) = next_due {
        parts.push(format!("Next: {}", format_due(due, now, clock)));
    }
    if paused.is_none() && next_due.is_none() {
        parts.push("Next run unknown".to_owned());
    }
    if let Some(outcome) = last_outcome {
        parts.push(outcome.label().to_owned());
    }
    parts.join(" · ")
}

/// Converts a positive interval in hours to seconds clamped to [`MIN_INTERVAL_SECS`].
pub fn interval_secs_from_hours(hours: u32) -> i64 {
    i64::from(hours.max(1))
        .saturating_mul(3600)
        .max(MIN_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::adapter::Driver;

    #[test]
    fn schedule_badges_name_next_paused_and_unknown_for_screen_readers() {
        use super::super::sidebar_model::ScheduleBadge;
        assert!(
            schedule_badge_description(ScheduleBadge::Next(1_795_000_000), 1_795_000_000)
                .contains("Next:")
        );
        assert!(
            schedule_badge_description(ScheduleBadge::Paused(PausedReason::Budget), 0)
                .contains(PausedReason::Budget.label())
        );
        assert!(schedule_badge_description(ScheduleBadge::Unknown, 0).contains("unknown"));
    }

    #[test]
    fn safe_modes_cover_all_non_bypass_modes() {
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits, Mode::Bypass] {
            match mode {
                Mode::Plan | Mode::Ask | Mode::AcceptEdits => {
                    assert!(SAFE_MODES.contains(&mode), "missing {mode:?}");
                    let idx = safe_mode_index(mode);
                    assert_eq!(safe_mode_at(idx), mode);
                }
                Mode::Bypass => {
                    assert!(!SAFE_MODES.contains(&mode));
                }
            }
        }
    }

    #[test]
    fn form_validation_enforces_placeholders_images_and_future_due() {
        let clock = GlibLocalClock;
        let now = 1_795_000_000;
        let base = FormInput {
            prompt: "Run tests".into(),
            has_images: false,
            target: Target::ExistingThread {
                thread_id: "t1".into(),
            },
            in_git_repo: true,
            rule: Rule::Once { at: now + 600 },
            missed_policy: MissedPolicy::RunOnce,
            ends: Ends::Never,
            now,
        };
        assert!(validate_form(&base, &clock).is_ok());

        let with_images = FormInput {
            has_images: true,
            ..base.clone()
        };
        assert!(validate_form(&with_images, &clock).is_err());

        let with_placeholder = FormInput {
            prompt: "Inspect {branch} failures".into(),
            ..base.clone()
        };
        assert!(validate_form(&with_placeholder, &clock).is_err());

        let past_once = FormInput {
            rule: Rule::Once { at: now - 10 },
            ..base.clone()
        };
        assert!(validate_form(&past_once, &clock).is_err());

        assert!(validate_form(
            &FormInput {
                ends: Ends::OnDate { at: now + 300 },
                ..base.clone()
            },
            &clock
        )
        .is_err());
        assert!(validate_form(
            &FormInput {
                ends: Ends::OnDate { at: now + 600 },
                ..base.clone()
            },
            &clock
        )
        .is_ok());

        let agy_no_git = FormInput {
            in_git_repo: false,
            target: Target::NewThread {
                folder: "/tmp/nonrepo".into(),
                driver: Driver::Agy,
                profile_id: None,
                mode: Mode::Ask,
                model: None,
                worktree: true,
            },
            ..base
        };
        assert!(validate_form(&agy_no_git, &clock).is_err());
    }

    #[test]
    fn glib_local_clock_resolves_and_formats_timestamps() {
        let clock = GlibLocalClock;
        let now = 1_795_000_000;
        let civil = clock.civil_at(now).expect("civil");
        assert!(civil.year >= 2026);
        let text = format_due(now + 3600, now, &clock);
        assert!(!text.is_empty());
    }
}
