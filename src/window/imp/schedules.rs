//! UI, dialogs, and background 60-second tick scheduler for scheduled & repeating prompts (F6).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::adapter::{Driver, Mode};
use agent_core::redact::redact;
use agent_core::schedule::{
    decide, missed, next_due, Ends, LastOutcome, LocalClock, MissedPolicy, MissedResult,
    PausedReason, Rule, RuleKind, ScheduleDecision, ScheduleState, Target,
    MAX_UNATTENDED_SCHEDULED_RUNS,
};
use agent_kit::store::{NewSchedule, ScheduleRecord, ScheduleRunUpdate, Store};
use gtk4::glib;
use tracing::{info, warn};

use super::super::AgentTerminalWindow;
use super::present_message_over;
#[cfg(test)]
use super::threads::app_store;
use super::threads::{
    app_handle, off_main, record_new_thread_as, store_job, store_writer, write_job, NewThreadIds,
    NewThreadSpec,
};
use crate::chat::session::{ChatSession, ScheduledOutcome};
use crate::window::schedule_model::{
    format_due, interval_secs_from_hours, safe_mode_at, safe_mode_index, schedule_row_subtitle,
    validate_form, FormInput, GlibLocalClock, SAFE_MODES,
};

const TICK_INTERVAL_SECS: u32 = 60;
const STARTUP_CATCHUP_SECS: u64 = 2;
const MAX_FIRES_PER_TICK: usize = 3;
const PROMPT_PREVIEW_CHARS: usize = 80;

thread_local! {
    static SCHEDULER_STARTED: Cell<bool> = const { Cell::new(false) };
    static STATUS_GENERATION: crate::window::Generation = const { crate::window::Generation::new() };
    static IN_FLIGHT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Queued details for dispatching a due or manually triggered scheduled prompt onto a session.
#[derive(Debug, Clone)]
pub(super) struct PendingScheduled {
    pub schedule_id: String,
    /// Revision checked again after asynchronous target preparation, before automatic delivery.
    pub revision: i64,
    pub due: i64,
    pub prompt: String,
    pub in_worktree: bool,
    pub next_due: Option<i64>,
    pub new_paused: Option<PausedReason>,
    pub count_unattended: bool,
    pub notify: bool,
    pub manual: bool,
}

/// Starts the application-wide scheduler (2 s catch-up check, then every 60 s).
pub(in crate::window) fn start_scheduler(app: &adw::Application) {
    let started = SCHEDULER_STARTED.with(|flag| flag.replace(true));
    if started {
        return;
    }
    let weak_catchup = app.downgrade();
    glib::timeout_add_local_once(Duration::from_secs(STARTUP_CATCHUP_SECS), move || {
        if let Some(app) = weak_catchup.upgrade() {
            tick(&app);
        }
    });
    let weak_tick = app.downgrade();
    glib::timeout_add_seconds_local(TICK_INTERVAL_SECS, move || {
        let Some(app) = weak_tick.upgrade() else {
            return glib::ControlFlow::Break;
        };
        tick(&app);
        glib::ControlFlow::Continue
    });
}

fn now_unix() -> i64 {
    glib::DateTime::now_local()
        .or_else(|_| glib::DateTime::now_utc())
        .map(|dt| dt.to_unix())
        .unwrap_or(0)
}

fn first_window(app: &adw::Application) -> Option<AgentTerminalWindow> {
    app.active_window()
        .and_then(|w| w.downcast::<AgentTerminalWindow>().ok())
        .or_else(|| {
            app.windows()
                .into_iter()
                .find_map(|w| w.downcast::<AgentTerminalWindow>().ok())
        })
}

fn prompt_preview(prompt: &str) -> String {
    let clean = redact(prompt.trim()).replace('\n', " ");
    let clipped: String = clean.chars().take(PROMPT_PREVIEW_CHARS).collect();
    if clean.chars().count() > PROMPT_PREVIEW_CHARS {
        format!("{clipped}…")
    } else {
        clipped
    }
}

fn send_scheduled_notification(window: &AgentTerminalWindow, schedule_id: &str, prompt: &str) {
    let Some(app) = window.application() else {
        return;
    };
    let notification = gtk4::gio::Notification::new("Scheduled prompt started");
    notification.set_body(Some(&prompt_preview(prompt)));
    let id = format!("agent-terminal-schedule-{schedule_id}");
    app.send_notification(Some(&id), &notification);
}

fn mark_in_flight(id: &str) -> bool {
    IN_FLIGHT.with(|set| set.borrow_mut().insert(id.to_owned()))
}

fn clear_in_flight(id: &str) {
    IN_FLIGHT.with(|set| {
        set.borrow_mut().remove(id);
    });
}

/// Harvests any scheduled runs interrupted via Stop and marks their schedules as stopped.
fn harvest_stopped_schedules(app: &adw::Application) {
    let mut stopped_ids = Vec::new();
    for win in app.windows() {
        let Ok(win) = win.downcast::<AgentTerminalWindow>() else {
            continue;
        };
        let imp = win.imp();
        let tabs = imp.tabs.borrow();
        for tab in tabs.iter() {
            if let Some(session) = tab.chat.as_ref().and_then(|c| c.slot.get()) {
                stopped_ids.extend(session.take_stopped_schedules());
            }
        }
    }
    if stopped_ids.is_empty() {
        return;
    }
    stopped_ids.sort();
    stopped_ids.dedup();
    let window = first_window(app);
    glib::MainContext::default().spawn_local(async move {
        let result = write_job("pause stopped schedule", move |store: &Store| {
            let mut paused = Vec::new();
            for id in &stopped_ids {
                store.set_schedule_paused(id, Some(PausedReason::StoppedRun), None)?;
                if let Some(record) = store.get_schedule(id)? {
                    paused.push(record);
                }
            }
            Ok(paused)
        })
        .await;
        if let (Some(window), Some(Ok(records))) = (window, result) {
            for record in records {
                schedule_feedback(&window, &record, PausedReason::StoppedRun.label()).await;
            }
        }
    });
}

fn tick(app: &adw::Application) {
    harvest_stopped_schedules(app);
    let Some(window) = first_window(app) else {
        return;
    };
    let (enabled, notify) = {
        let cfg = window.imp().config.borrow();
        (cfg.schedules_enabled, cfg.notify_scheduled)
    };
    if !enabled {
        return;
    }
    let app = app.clone();
    let weak_win = window.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let now = now_unix();
        if !check_schedule_budget(&window).await {
            window.imp().refresh_schedule_status();
            return;
        }
        window.imp().refresh_schedule_status();
        let due = store_job(move |store| store.due_schedules(now))
            .await
            .and_then(Result::ok)
            .unwrap_or_default();
        if due.is_empty() {
            return;
        }
        let mut fired = 0usize;
        for record in due {
            let Some(window) = weak_win.upgrade() else {
                return;
            };
            if fired >= MAX_FIRES_PER_TICK {
                break;
            }
            if evaluate_and_maybe_fire(&app, &window, record, now, notify).await {
                fired += 1;
            }
        }
    });
}

/// Re-check the current warning state rather than the notification latch. A restarted app or
/// an explicitly resumed schedule must not silently bypass a still-reached spending warning.
async fn check_schedule_budget(window: &AgentTerminalWindow) -> bool {
    let budget = window.imp().config.borrow().spend_budget();
    let result = write_job("check scheduled prompt spending", move |store: &Store| {
        store.ensure_spend_index()?;
        let reached = store.spend_budget_reached(
            now_unix().saturating_mul(1000),
            &agent_kit::store::local_day,
            &budget,
        )?;
        let paused = if reached {
            store.pause_schedules_for_budget()?
        } else {
            Vec::new()
        };
        Ok((reached, paused))
    })
    .await;
    match result {
        Some(Ok((reached, paused))) => {
            budget_paused_feedback(window, paused).await;
            !reached
        }
        _ => {
            warn!("scheduled prompt spending state unavailable; holding automatic runs");
            false
        }
    }
}

async fn budget_allows_resume(window: &AgentTerminalWindow) -> bool {
    let budget = window.imp().config.borrow().spend_budget();
    let result = write_job(
        "check spending before resuming schedule",
        move |store: &Store| {
            store.ensure_spend_index()?;
            store.spend_budget_reached(
                now_unix().saturating_mul(1000),
                &agent_kit::store::local_day,
                &budget,
            )
        },
    )
    .await;
    match result {
        Some(Ok(false)) => true,
        Some(Ok(true)) => {
            window.imp().show_toast(
                "Cannot resume while a spending warning is reached. Change the warning amount or wait for the period to reset. Run Now starts one manual run.",
            );
            false
        }
        _ => {
            window
                .imp()
                .show_toast("Cannot resume: spending state could not be checked");
            false
        }
    }
}

/// Automatic outcomes are app notices, never user messages. Unopened targets retain the
/// notice in the store; live sessions deliver and persist it through their ordinary sink.
async fn schedule_feedback(window: &AgentTerminalWindow, record: &ScheduleRecord, text: &str) {
    schedule_feedback_inner(window, record, text, false).await;
}

async fn schedule_feedback_inner(
    window: &AgentTerminalWindow,
    record: &ScheduleRecord,
    text: &str,
    stored: bool,
) {
    let text = redact(&format!("Scheduled prompt: {text}"));
    if let Target::ExistingThread { thread_id } = &record.target {
        let owner = super::threads::thread_owner(thread_id).unwrap_or_else(|| window.clone());
        let mut delivered = false;
        let target = owner
            .imp()
            .with_chat(thread_id, |chat| (chat.slot.get(), chat.view.clone()));
        if let Some((session, view)) = target {
            if stored {
                if let Some(view) = &view {
                    view.sink()(&agent_core::event::Envelope::new(
                        agent_core::event::Event::Notice { text: text.clone() },
                    ));
                    delivered = true;
                }
            } else if let Some(session) = session {
                session.note(text.clone());
                delivered = true;
            }
        }
        if !delivered && !stored {
            let tid = thread_id.clone();
            let notice = agent_core::event::Envelope::new(agent_core::event::Event::Notice {
                text: text.clone(),
            });
            let _ = write_job("record schedule notice", move |store: &Store| {
                store.append_event(&tid, None, &notice).map(|_| ())
            })
            .await;
        }
    }
    window.imp().show_toast(&text);
    if record.paused_reason != Some(PausedReason::User)
        && window.imp().config.borrow().notify_scheduled
    {
        if let Some(app) = window.application() {
            let note = gtk4::gio::Notification::new("Scheduled prompt");
            note.set_body(Some(&text));
            app.send_notification(Some(&format!("schedule-state-{}", record.id)), &note);
        }
    }
    window.imp().refresh_schedule_status();
}

pub(super) async fn budget_paused_feedback(
    window: &AgentTerminalWindow,
    records: Vec<ScheduleRecord>,
) {
    for record in records {
        schedule_feedback_inner(window, &record, PausedReason::Budget.label(), true).await;
    }
}

fn after_fire_schedule(
    record: &ScheduleRecord,
    reference_now: i64,
) -> (Option<i64>, Option<PausedReason>) {
    match record.rule {
        Rule::Once { .. } => return (None, Some(PausedReason::Ended)),
        Rule::Days { .. } | Rule::Interval { .. } => {}
    }
    let next_count = record.run_count.saturating_add(1);
    match record.ends {
        Ends::AfterRuns { max } if next_count >= max => return (None, Some(PausedReason::Ended)),
        _ => {}
    }
    let candidate = next_due(&record.rule, reference_now, &GlibLocalClock);
    match (candidate, record.ends) {
        (Some(next), Ends::OnDate { at }) if next > at => (None, Some(PausedReason::Ended)),
        (None, _) => (None, Some(PausedReason::Ended)),
        (next, _) => (next, None),
    }
}

/// Inspects one due schedule, advancing skipped occurrences or dispatching a due turn.
/// Returns `true` if a dispatch was initiated (counts toward [`MAX_FIRES_PER_TICK`]).
async fn evaluate_and_maybe_fire(
    _app: &adw::Application,
    window: &AgentTerminalWindow,
    record: ScheduleRecord,
    now: i64,
    notify: bool,
) -> bool {
    if !mark_in_flight(&record.id) {
        return false;
    }
    let scheduled_due = record.next_due.unwrap_or(now);
    let clock = GlibLocalClock;
    let due = match missed(
        &record.rule,
        scheduled_due,
        now,
        record.missed_policy,
        &clock,
    ) {
        MissedResult::NotDue => {
            clear_in_flight(&record.id);
            return false;
        }
        MissedResult::Missed {
            latest_due,
            run_once: false,
            next_due,
        } => {
            let id = record.id.clone();
            let result = write_job("record missed schedule", move |store: &Store| {
                store.record_schedule_run(&ScheduleRunUpdate {
                    id,
                    next_due,
                    last_due: Some(latest_due),
                    last_outcome: Some(LastOutcome::Missed),
                    increment_run_count: false,
                    increment_unattended: false,
                    paused_reason: None,
                })
            })
            .await;
            if matches!(result, Some(Ok(()))) {
                schedule_feedback(window, &record, LastOutcome::Missed.label()).await;
            }
            clear_in_flight(&record.id);
            return false;
        }
        MissedResult::DueNow { due } => due,
        MissedResult::Missed {
            latest_due,
            run_once: true,
            ..
        } => latest_due,
    };

    let dispatched = dispatch_checked_due(window, record.clone(), due, now, notify, false).await;
    if !dispatched {
        clear_in_flight(&record.id);
    }
    dispatched
}

struct LiveTargetState {
    driver: Driver,
    in_worktree: bool,
    mode: Mode,
    child: bool,
    wakes_held: bool,
    limit_waiting: bool,
    has_open_approval: bool,
    busy_reason: Option<String>,
}

async fn inspect_existing_thread(
    window: &AgentTerminalWindow,
    thread_id: &str,
) -> Result<LiveTargetState, PausedReason> {
    let imp = window.imp();
    if let Some(owner) = super::threads::thread_owner(thread_id) {
        let owner_imp = owner.imp();
        let tab_info = owner_imp.page_of_thread(thread_id).and_then(|page| {
            owner_imp
                .tabs
                .borrow()
                .iter()
                .find(|t| t.page == page)
                .and_then(|t| {
                    let chat = t.chat.as_ref()?;
                    let session = chat.slot.get();
                    let in_worktree = t.worktree.is_some();
                    let mode = session
                        .as_ref()
                        .map(|s| s.wanted_mode())
                        .or_else(|| chat.view.as_ref().and_then(|v| v.replayed_mode()))
                        .unwrap_or(Mode::Ask);
                    Some((chat.driver, in_worktree, mode, session))
                })
        });
        if let Some((driver, in_worktree, mode, session)) = tab_info {
            if let Some(session) = session {
                return Ok(LiveTargetState {
                    driver,
                    in_worktree,
                    mode,
                    child: session.is_delegated_child(),
                    wakes_held: session.is_wakes_held(),
                    limit_waiting: session.is_limit_waiting(),
                    has_open_approval: session.has_open_approval(),
                    busy_reason: session.scheduled_busy_reason().map(str::to_owned),
                });
            }
            return Ok(LiveTargetState {
                driver,
                in_worktree,
                mode,
                child: false,
                wakes_held: false,
                limit_waiting: false,
                has_open_approval: false,
                busy_reason: None,
            });
        }
    }

    let tid = thread_id.to_owned();
    let stored = store_job(move |store| {
        let summary = store.thread_summary(&tid).ok().flatten();
        let attrs = store.thread_attributes(&tid).ok();
        let driver = store
            .provider_threads(&tid)
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .and_then(|p| Driver::from_key(&p.driver));
        (summary, attrs, driver)
    })
    .await;
    let Some((Some(summary), attrs, provider_driver)) = stored else {
        return Err(PausedReason::TargetGone);
    };
    if summary.archived {
        return Err(PausedReason::TargetGone);
    }
    let driver = provider_driver
        .or_else(|| summary.driver.as_deref().and_then(Driver::from_key))
        .or_else(|| imp.default_agent())
        .unwrap_or(Driver::Claude);
    let in_worktree = attrs.is_some_and(|a| a.worktree.is_some());
    Ok(LiveTargetState {
        driver,
        in_worktree,
        mode: Mode::Ask,
        child: false,
        wakes_held: false,
        limit_waiting: false,
        has_open_approval: false,
        busy_reason: None,
    })
}

async fn dispatch_checked_due(
    window: &AgentTerminalWindow,
    record: ScheduleRecord,
    due: i64,
    now: i64,
    notify: bool,
    manual: bool,
) -> bool {
    let record = if !manual {
        if !check_schedule_budget(window).await {
            return false;
        }
        let id = record.id.clone();
        match store_job(move |s| s.get_schedule(&id)).await {
            Some(Ok(Some(fresh)))
                if fresh.paused_reason.is_none() && fresh.next_due == record.next_due =>
            {
                fresh
            }
            _ => return false,
        }
    } else {
        record
    };
    let target_state = match &record.target {
        Target::ExistingThread { thread_id } => {
            match inspect_existing_thread(window, thread_id).await {
                Ok(s) => s,
                Err(pause) => {
                    let id = record.id.clone();
                    let result = write_job("pause stale schedule", move |store: &Store| {
                        store.set_schedule_paused(&id, Some(pause), None)
                    })
                    .await;
                    if matches!(result, Some(Ok(()))) {
                        schedule_feedback(window, &record, pause.label()).await;
                    }
                    if manual {
                        window
                            .imp()
                            .show_toast("Could not run: the target thread is archived or missing");
                    }
                    return false;
                }
            }
        }
        Target::NewThread {
            driver,
            mode,
            worktree,
            ..
        } => LiveTargetState {
            driver: *driver,
            in_worktree: *worktree,
            mode: *mode,
            child: false,
            wakes_held: false,
            limit_waiting: false,
            has_open_approval: false,
            busy_reason: None,
        },
    };

    if !manual {
        let state = ScheduleState {
            enabled: window.imp().config.borrow().schedules_enabled,
            paused: record.paused_reason,
            unattended_runs: record.unattended_runs,
            run_count: record.run_count,
            ends: record.ends,
            now,
            due,
            driver: target_state.driver,
            in_worktree: target_state.in_worktree,
            mode: target_state.mode,
            child: target_state.child,
            wakes_held: target_state.wakes_held,
            limit_waiting: target_state.limit_waiting,
            has_open_approval: target_state.has_open_approval,
            busy_reason: target_state.busy_reason.as_deref(),
        };
        match decide(&state) {
            ScheduleDecision::Fire => {}
            ScheduleDecision::Defer(_) => return false,
            ScheduleDecision::Skip(outcome) => {
                let (next_due, paused_reason) = after_skip_schedule(&record, now.max(due));
                let id = record.id.clone();
                let result = write_job("record skipped schedule run", move |store: &Store| {
                    store.record_schedule_run(&ScheduleRunUpdate {
                        id,
                        next_due,
                        last_due: Some(due),
                        last_outcome: Some(outcome),
                        increment_run_count: false,
                        increment_unattended: false,
                        paused_reason,
                    })
                })
                .await;
                if matches!(result, Some(Ok(()))) {
                    schedule_feedback(window, &record, outcome.label()).await;
                }
                return false;
            }
            ScheduleDecision::Pause(reason) => {
                let id = record.id.clone();
                let result = write_job("pause schedule", move |store: &Store| {
                    store.set_schedule_paused(&id, Some(reason), None)
                })
                .await;
                if matches!(result, Some(Ok(()))) {
                    schedule_feedback(window, &record, reason.label()).await;
                }
                return false;
            }
        }
    }

    let (next_due, new_paused) = if manual {
        (record.next_due, record.paused_reason)
    } else {
        after_fire_schedule(&record, now.max(due))
    };
    let pending = PendingScheduled {
        schedule_id: record.id.clone(),
        revision: record.updated_at,
        due,
        prompt: record.prompt.clone(),
        in_worktree: target_state.in_worktree,
        next_due,
        new_paused,
        count_unattended: !manual,
        notify: notify && !manual,
        manual,
    };

    match record.target {
        Target::ExistingThread { thread_id } => window
            .imp()
            .dispatch_scheduled_on_thread(&thread_id, pending),
        Target::NewThread {
            folder,
            driver,
            profile_id,
            mode,
            model,
            worktree,
        } => {
            spawn_new_scheduled_thread(
                window,
                NewScheduledTarget {
                    folder,
                    driver,
                    profile_id,
                    mode,
                    model,
                    worktree,
                },
                pending,
            )
            .await
        }
    }
}

fn after_skip_schedule(
    record: &ScheduleRecord,
    reference_now: i64,
) -> (Option<i64>, Option<PausedReason>) {
    match record.rule {
        Rule::Once { .. } => (None, Some(PausedReason::Ended)),
        Rule::Days { .. } | Rule::Interval { .. } => {
            let candidate = next_due(&record.rule, reference_now, &GlibLocalClock);
            match (candidate, record.ends) {
                (Some(next), Ends::OnDate { at }) if next > at => (None, Some(PausedReason::Ended)),
                (None, _) => (None, Some(PausedReason::Ended)),
                (next, _) => (next, None),
            }
        }
    }
}

struct NewScheduledTarget {
    folder: String,
    driver: Driver,
    profile_id: Option<String>,
    mode: Mode,
    model: Option<String>,
    worktree: bool,
}

async fn spawn_new_scheduled_thread(
    window: &AgentTerminalWindow,
    target: NewScheduledTarget,
    pending: PendingScheduled,
) -> bool {
    let root_raw = window.imp().config.borrow().worktree_root.clone();
    let root_trimmed = root_raw.trim();
    let worktree_root =
        (!root_trimmed.is_empty()).then(|| agent_kit::paths::expand_tilde(root_trimmed));
    let folder = target.folder.clone();
    let sched_id = pending.schedule_id.clone();
    let make_wt = target.worktree;
    let prep = off_main(
        "preparing scheduled thread folder",
        move || -> Result<(String, Option<crate::worktree::WorktreeInfo>), String> {
            let dir_path = PathBuf::from(&folder);
            if !dir_path.is_dir() {
                return Err("the scheduled folder does not exist".to_owned());
            }
            if !make_wt {
                return Ok((folder, None));
            }
            let main = crate::worktree::main_toplevel(&dir_path)?;
            let slug: String = sched_id
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(8)
                .collect();
            let nonce = now_unix();
            let branch = format!("sched-{slug}-{nonce}");
            crate::worktree::check_new_branch(&main, &branch)?;
            let base = crate::worktree::head_branch(&main);
            let base_name = match base {
                crate::worktree::HeadBranch::Branch(ref b) => b.as_str(),
                _ => "HEAD",
            };
            let commit = crate::worktree::resolve_base(&main, base_name)?;
            let wt_path = crate::worktree::default_path(
                &main,
                &branch,
                worktree_root.as_deref().map(std::path::Path::new),
            );
            crate::worktree::add(&main, &branch, &wt_path, &commit)?;
            let cwd = wt_path.to_string_lossy().into_owned();
            Ok((
                cwd,
                Some(crate::worktree::WorktreeInfo {
                    path: wt_path,
                    branch,
                    main_toplevel: main,
                    base: match base {
                        crate::worktree::HeadBranch::Branch(b) => Some(b),
                        _ => None,
                    },
                }),
            ))
        },
    )
    .await;

    let Ok((cwd, worktree_info)) = prep else {
        let msg = prep
            .err()
            .unwrap_or_else(|| "could not prepare scheduled folder".to_owned());
        warn!(schedule = %pending.schedule_id, error = %msg, "scheduled new thread setup failed");
        mark_dispatch_failed(window, pending);
        return false;
    };

    let store = app_handle();
    let Ok(thread_id) = store.new_id() else {
        mark_dispatch_failed(window, pending);
        return false;
    };
    let Ok(provider_id) = store.new_id() else {
        mark_dispatch_failed(window, pending);
        return false;
    };
    let safe_mode = Mode::session_safe(target.mode);
    let title = Some(format!("Scheduled: {}", prompt_preview(&pending.prompt)));
    let spec = NewThreadSpec {
        cwd: cwd.clone(),
        title,
        driver: target.driver,
        model: target.model.clone(),
        mode: Some(safe_mode),
        handoff: None,
        attributes: agent_kit::store::ThreadAttributes {
            profile_id: target.profile_id.clone(),
            worktree: worktree_info.clone(),
            fork: None,
        },
    };
    let ids = NewThreadIds {
        thread: thread_id.clone(),
        provider: provider_id,
    };
    let stored = schedule_write_receipt("create scheduled thread", move |store: &Store| {
        record_new_thread_as(store, &ids, &spec)
    })
    .await;
    match stored {
        Some(Ok(())) => {
            window.imp().open_scheduled_created(
                &thread_id,
                target.driver,
                target.profile_id,
                worktree_info,
                safe_mode,
                pending,
            );
            true
        }
        _ => {
            mark_dispatch_failed(window, pending);
            false
        }
    }
}

/// Dispatches a pending scheduled prompt onto an active [`ChatSession`] and persists its outcome.
pub(super) fn dispatch_on_session(
    window: &AgentTerminalWindow,
    session: &Rc<ChatSession>,
    pending: PendingScheduled,
) {
    let weak = window.downgrade();
    let session = session.clone();
    glib::spawn_future_local(async move {
        let Some(window) = weak.upgrade() else {
            clear_in_flight(&pending.schedule_id);
            return;
        };
        // This is the common boundary for ready sessions and pending prompts drained after
        // asynchronous folder/worktree/session startup. Never rely on the earlier tick check.
        if !pending.manual && !automatic_delivery_allowed(&window, &pending).await {
            clear_in_flight(&pending.schedule_id);
            window.imp().refresh_schedule_status();
            return;
        }
        if window.imp().is_closing() {
            clear_in_flight(&pending.schedule_id);
            return;
        }
        deliver_on_session(&window, &session, pending);
    });
}

async fn automatic_delivery_allowed(
    window: &AgentTerminalWindow,
    pending: &PendingScheduled,
) -> bool {
    if !window.imp().config.borrow().schedules_enabled {
        return false;
    }
    let budget = window.imp().config.borrow().spend_budget();
    let id = pending.schedule_id.clone();
    let result =
        schedule_write_receipt("revalidate scheduled prompt delivery", move |s: &Store| {
            s.atomically(|s| {
                s.ensure_spend_index()?;
                let reached = s.spend_budget_reached(
                    now_unix().saturating_mul(1000),
                    &agent_kit::store::local_day,
                    &budget,
                )?;
                let paused = if reached {
                    s.pause_schedules_for_budget()?
                } else {
                    Vec::new()
                };
                Ok((reached, s.get_schedule(&id)?, paused))
            })
        })
        .await;
    match result {
        Some(Ok((false, Some(record), _))) => {
            // No await or callback between this final check and send_scheduled. A pause,
            // deletion or edit made while the target was starting cancels the old delivery.
            window.imp().config.borrow().schedules_enabled
                && record.paused_reason.is_none()
                && record.prompt == pending.prompt
                && record.updated_at == pending.revision
        }
        Some(Ok((_, _, paused))) => {
            budget_paused_feedback(window, paused).await;
            false
        }
        _ => {
            warn!("could not revalidate scheduled prompt delivery; holding run");
            false
        }
    }
}

/// Waits for the actual committed writer receipt, without a UI timeout dropping the receiver.
/// The wait is blocking only on a GIO worker; the in-flight guard remains until it answers.
async fn schedule_write_receipt<T: Send + 'static>(
    what: &'static str,
    job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static,
) -> Option<agent_kit::store::Result<T>> {
    let Some(writer) = store_writer() else {
        return store_job(move |s| s.atomically(job)).await;
    };
    await_schedule_receipt(writer.call_result(what, job)).await
}

async fn await_schedule_receipt<T: Send + 'static>(
    receipt: std::sync::mpsc::Receiver<agent_kit::store::Result<T>>,
) -> Option<agent_kit::store::Result<T>> {
    gtk4::gio::spawn_blocking(move || receipt.recv().ok())
        .await
        .ok()
        .flatten()
}

fn deliver_on_session(
    window: &AgentTerminalWindow,
    session: &Rc<ChatSession>,
    pending: PendingScheduled,
) {
    let outcome = session.send_scheduled(
        &pending.schedule_id,
        pending.due,
        &pending.prompt,
        pending.in_worktree,
    );
    let sched_id = pending.schedule_id.clone();
    match outcome {
        ScheduledOutcome::Started => {
            info!(schedule = %sched_id, "scheduled prompt turn started");
            if pending.notify {
                send_scheduled_notification(window, &sched_id, &pending.prompt);
            }
            if pending.manual {
                window.imp().show_toast("Scheduled prompt started");
            }
            let weak = window.downgrade();
            glib::MainContext::default().spawn_local(async move {
                let id_copy = sched_id.clone();
                let receipt =
                    schedule_write_receipt("record scheduled run", move |store: &Store| {
                        store.record_schedule_run(&ScheduleRunUpdate {
                            id: id_copy,
                            next_due: pending.next_due,
                            last_due: Some(pending.due),
                            last_outcome: Some(LastOutcome::Ran),
                            increment_run_count: true,
                            increment_unattended: pending.count_unattended,
                            paused_reason: pending.new_paused,
                        })
                    });
                finish_run(weak, pending, receipt).await;
            });
        }
        ScheduledOutcome::Busy(why) => {
            if pending.manual {
                window.imp().show_toast(&format!("Cannot run now: {why}"));
            }
            clear_in_flight(&sched_id);
        }
        ScheduledOutcome::Refused(why) => {
            if pending.manual {
                window
                    .imp()
                    .show_toast(&format!("Scheduled prompt refused: {why}"));
            }
            mark_dispatch_failed(window, pending);
        }
    }
}

async fn finish_run(
    weak: glib::WeakRef<AgentTerminalWindow>,
    pending: PendingScheduled,
    receipt: impl std::future::Future<Output = Option<agent_kit::store::Result<()>>>,
) {
    let sched_id = pending.schedule_id.clone();
    let res = receipt.await;
    if matches!(res, Some(Ok(()))) {
        clear_in_flight(&sched_id);
        if let Some(window) = weak.upgrade() {
            window.imp().refresh_schedule_status();
            if let Some(reason) = pending.new_paused.filter(|_| !pending.manual) {
                let id = sched_id.clone();
                if let Some(Ok(Some(record))) = store_job(move |s| s.get_schedule(&id)).await {
                    schedule_feedback(&window, &record, reason.label()).await;
                }
            }
        }
    } else {
        tracing::warn!(
            schedule = %sched_id,
            "failed to record scheduled run; keeping in-flight guard to prevent duplicate firing"
        );
    }
}

/// Marks a failed/refused scheduled attempt without incrementing run counters.
pub(super) fn mark_dispatch_failed(_window: &AgentTerminalWindow, pending: PendingScheduled) {
    let sched_id = pending.schedule_id.clone();
    let weak = _window.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let id_copy = sched_id.clone();
        let result = schedule_write_receipt("record failed scheduled run", move |store: &Store| {
            store.record_schedule_run(&ScheduleRunUpdate {
                id: id_copy,
                next_due: pending.next_due,
                last_due: Some(pending.due),
                last_outcome: Some(LastOutcome::Failed),
                increment_run_count: false,
                increment_unattended: false,
                paused_reason: pending.new_paused,
            })
        })
        .await;
        if matches!(result, Some(Ok(()))) {
            let id = sched_id.clone();
            let record = store_job(move |s| s.get_schedule(&id)).await;
            if let (Some(window), Some(Ok(Some(record)))) = (weak.upgrade(), record) {
                schedule_feedback(&window, &record, LastOutcome::Failed.label()).await;
            }
        }
        clear_in_flight(&sched_id);
    });
}

fn default_once_inputs() -> (String, String) {
    let now = now_unix().saturating_add(3600);
    if let Ok(dt) = glib::DateTime::from_unix_local(now) {
        let minute = (dt.minute() / 5) * 5;
        return (
            format!(
                "{:04}-{:02}-{:02}",
                dt.year(),
                dt.month(),
                dt.day_of_month()
            ),
            format!("{:02}:{:02}", dt.hour(), minute),
        );
    }
    ("2026-01-01".to_owned(), "09:00".to_owned())
}

fn parse_hhmm(raw: &str) -> Option<(u8, u8)> {
    let (h, m) = raw.trim().split_once(':')?;
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = m.parse().ok()?;
    if hour < 24 && minute < 60 {
        Some((hour, minute))
    } else {
        None
    }
}

fn parse_ymd(raw: &str) -> Option<(i32, u8, u8)> {
    let mut parts = raw.trim().split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

fn target_summary(target: &Target) -> String {
    match target {
        Target::ExistingThread { thread_id } => {
            format!("Thread {}", &thread_id[..thread_id.len().min(8)])
        }
        Target::NewThread {
            driver, worktree, ..
        } => {
            if *worktree {
                format!("New {} worktree", driver.info().label)
            } else {
                format!("New {} thread", driver.info().label)
            }
        }
    }
}

fn preferred_schedule<'a>(
    records: impl Iterator<Item = &'a ScheduleRecord>,
) -> Option<&'a ScheduleRecord> {
    records.min_by_key(|r| {
        let priority = match r.paused_reason {
            Some(PausedReason::Ended) => 2,
            Some(_) => 0,
            None => 1,
        };
        (priority, r.next_due.unwrap_or(i64::MAX))
    })
}

impl super::AgentTerminalWindow {
    /// Reload header state through the store barrier. Errors remain visibly unknown rather
    /// than clearing a previously shown clock. Also called when a thread is first built.
    pub(super) fn refresh_schedule_status(&self) {
        let generation = STATUS_GENERATION.with(crate::window::Generation::next);
        let weak = self.obj().downgrade();
        glib::spawn_future_local(async move {
            let records = store_job(|s| s.list_schedules()).await;
            if !STATUS_GENERATION.with(|g| g.is_current(generation)) {
                return;
            }
            let Some(window) = weak.upgrade() else {
                return;
            };
            let groups = match &records {
                Some(Ok(records)) => {
                    let mut groups: std::collections::HashMap<&str, Vec<&ScheduleRecord>> =
                        std::collections::HashMap::new();
                    for record in records {
                        if let Target::ExistingThread { thread_id } = &record.target {
                            groups.entry(thread_id.as_str()).or_default().push(record);
                        }
                    }
                    Some(groups)
                }
                _ => None,
            };
            let windows = window
                .application()
                .map(|app| app.windows())
                .unwrap_or_else(|| vec![window.upcast::<gtk4::Window>()]);
            for window in windows {
                if !STATUS_GENERATION.with(|g| g.is_current(generation)) {
                    return;
                }
                let Ok(window) = window.downcast::<AgentTerminalWindow>() else {
                    continue;
                };
                let imp = window.imp();
                let enabled = imp.config.borrow().schedules_enabled;
                use crate::window::sidebar_model::ScheduleBadge;
                let badge_for = |r: &ScheduleRecord| {
                    if !enabled {
                        ScheduleBadge::Paused(PausedReason::SettingOff)
                    } else if let Some(reason) = r.paused_reason {
                        ScheduleBadge::Paused(reason)
                    } else {
                        r.next_due
                            .map(ScheduleBadge::Next)
                            .unwrap_or(ScheduleBadge::Unknown)
                    }
                };
                *imp.schedule_badges.borrow_mut() = groups.as_ref().map(|groups| {
                    groups
                        .iter()
                        .filter_map(|(thread, records)| {
                            preferred_schedule(records.iter().copied())
                                .map(|r| ((*thread).to_owned(), badge_for(r)))
                        })
                        .collect()
                });
                // Clone the views and thread ids, then release the registry before any GTK
                // setter or action can call back into the window and borrow its tabs again.
                let views = imp
                    .tabs
                    .borrow()
                    .iter()
                    .filter_map(|tab| {
                        let chat = tab.chat.as_ref()?;
                        Some((
                            chat.thread.clone(),
                            chat.view.clone()?,
                            tab.remote.is_some(),
                        ))
                    })
                    .collect::<Vec<_>>();
                for (thread, view, remote) in views {
                    if !STATUS_GENERATION.with(|g| g.is_current(generation)) {
                        return;
                    }
                    let record = groups
                        .as_ref()
                        .and_then(|groups| groups.get(thread.as_str()))
                        .and_then(|records| preferred_schedule(records.iter().copied()));
                    let status = if remote {
                        None
                    } else {
                        match (&groups, record) {
                            (None, _) => Some(ScheduleBadge::Unknown),
                            (_, Some(r)) => Some(badge_for(r)),
                            _ => None,
                        }
                    };
                    let text = status.map(|badge| {
                        crate::window::schedule_model::schedule_badge_description(badge, now_unix())
                    });
                    let paused = matches!(status, Some(ScheduleBadge::Paused(_)));
                    let id = record.map(|r| r.id.clone());
                    let weak = window.downgrade();
                    view.set_schedule_status(
                        text.as_deref(),
                        paused,
                        Rc::new(move |action| {
                            let Some(window) = weak.upgrade() else {
                                return;
                            };
                            if let Some(id) = &id {
                                window.imp().schedule_action(id, action);
                            } else {
                                window.imp().open_schedules_for_thread(&thread);
                            }
                        }),
                    );
                }
                if !STATUS_GENERATION.with(|g| g.is_current(generation)) {
                    return;
                }
                if imp.summaries.borrow().is_some() {
                    imp.refresh_sidebar();
                }
            }
        });
    }

    fn schedule_action(&self, id: &str, action: &str) {
        if action == "Schedules…" {
            self.show_schedules_manager();
            return;
        }
        let id = id.to_owned();
        let action = action.to_owned();
        let weak = self.obj().downgrade();
        glib::spawn_future_local(async move {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let query_id = id.clone();
            let record = store_job(move |s| s.get_schedule(&query_id)).await;
            let Some(Ok(Some(record))) = record else {
                window
                    .imp()
                    .show_toast("Could not read this schedule; refresh and try again");
                window.imp().refresh_schedule_status();
                return;
            };
            if action == "Run Now" {
                if !mark_in_flight(&id) {
                    window.imp().show_toast("Already running this schedule");
                    return;
                }
                let now = now_unix();
                if !dispatch_checked_due(&window, record, now, now, false, true).await {
                    clear_in_flight(&id);
                }
                return;
            }
            let result = if action == "Delete" {
                write_job("delete schedule", move |s: &Store| {
                    s.delete_schedule(&id).map(|_| ())
                })
                .await
            } else {
                if !budget_allows_resume(&window).await {
                    return;
                }
                let due = next_due(&record.rule, now_unix(), &GlibLocalClock);
                if due.is_none()
                    || matches!(record.ends, Ends::OnDate { at } if due.is_some_and(|d| d > at))
                    || matches!(record.ends, Ends::AfterRuns { max } if record.run_count >= max)
                {
                    window.imp().show_toast(
                        "This schedule has ended. Edit its time or end condition to resume.",
                    );
                    return;
                }
                write_job("resume schedule", move |s: &Store| {
                    s.set_schedule_paused(&id, None, due)
                })
                .await
            };
            window.imp().show_toast(match result {
                Some(Ok(())) if action == "Delete" => "Schedule deleted",
                Some(Ok(())) => "Schedule resumed",
                _ => "Could not update the schedule; refresh and try again",
            });
            window.imp().refresh_schedule_status();
        });
    }

    pub(super) fn active_chat_thread(&self) -> Option<(String, Option<String>, bool, bool)> {
        let tv = self.tab_view.borrow().clone()?;
        let page = tv.selected_page()?;
        let tabs = self.tabs.borrow();
        let tab = tabs.iter().find(|t| t.page == page)?;
        let chat = tab.chat.as_ref()?;
        let (draft, has_images) = chat
            .view
            .as_ref()
            .map(|v| v.composer_schedule_draft())
            .unwrap_or((None, false));
        let remote = tab.remote.is_some();
        Some((chat.thread.clone(), draft, has_images, remote))
    }

    pub(super) fn open_active_schedule_dialog(&self) {
        if let Some((thread, draft, has_images, remote)) = self.active_chat_thread() {
            if remote {
                self.show_toast("Threads on another machine cannot run scheduled prompts");
                return;
            }
            let clear_composer = draft.is_some() || has_images;
            self.open_schedule_dialog_for(
                Some(&thread),
                draft.as_deref(),
                has_images,
                clear_composer,
            );
        } else {
            self.open_schedule_dialog_for(None, None, false, false);
        }
    }

    pub(super) fn open_schedules_for_thread(&self, thread: &str) {
        self.show_schedules_filtered(Some(thread.to_owned()));
    }

    pub(super) fn show_schedules_manager(&self) {
        self.show_schedules_filtered(None);
    }

    fn show_schedules_filtered(&self, filter_thread: Option<String>) {
        let dialog = adw::Dialog::builder()
            .title("Scheduled Prompts")
            .content_width(680)
            .content_height(520)
            .build();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let new_btn = gtk4::Button::builder()
            .icon_name("at-list-add-symbolic")
            .tooltip_text("New Schedule…")
            .build();
        new_btn.update_property(&[gtk4::accessible::Property::Label("New schedule")]);
        header.pack_start(&new_btn);
        toolbar.add_top_bar(&header);

        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(16);
        body.set_margin_end(16);

        let empty = adw::StatusPage::builder()
            .icon_name("at-document-open-recent-symbolic")
            .title("No Scheduled Prompts")
            .description(format!(
                "Schedule one-shot or repeating prompts in any thread with /schedule, the send menu, or Ctrl+Shift+L. Repeating runs pause after {MAX_UNATTENDED_SCHEDULED_RUNS} unattended turns until you reply."
            ))
            .build();
        let create_first = gtk4::Button::builder()
            .label("Schedule a Prompt…")
            .css_classes(["suggested-action", "pill"])
            .halign(gtk4::Align::Center)
            .build();
        empty.set_child(Some(&create_first));

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let scroller = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&list)
            .build();

        let stack = gtk4::Stack::new();
        stack.set_vexpand(true);
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scroller, Some("list"));
        body.append(&stack);

        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));

        type RefreshSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
        let filter = Rc::new(RefCell::new(filter_thread.clone()));
        let refresh: RefreshSlot = Rc::new(RefCell::new(None));
        let refresh_fn: Rc<dyn Fn()> = Rc::new({
            let obj = self.obj().downgrade();
            let dialog_w = dialog.downgrade();
            let list_w = list.downgrade();
            let stack_w = stack.downgrade();
            let empty_w = empty.downgrade();
            let filter = filter.clone();
            let self_refresh_w = Rc::downgrade(&refresh);
            move || {
                let Some(obj) = obj.upgrade() else {
                    return;
                };
                let filter_id = filter.borrow().clone();
                let list_w = list_w.clone();
                let stack_w = stack_w.clone();
                let empty_w = empty_w.clone();
                let obj_c = obj.clone();
                let dialog_c = dialog_w.clone();
                let refresh_w = self_refresh_w.clone();
                glib::MainContext::default().spawn_local(async move {
                    let schedules = store_job(|store| store.list_schedules()).await;
                    let (Some(list_c), Some(stack_c)) = (list_w.upgrade(), stack_w.upgrade())
                    else {
                        return;
                    };
                    let Some(Ok(schedules)) = schedules else {
                        if let Some(empty) = empty_w.upgrade() {
                            empty.set_title("Schedule State Unknown");
                            empty.set_description(Some("The thread store could not be read. Close and reopen this list to try again."));
                        }
                        stack_c.set_visible_child_name("empty");
                        return;
                    };
                    let filtered: Vec<ScheduleRecord> = schedules
                        .into_iter()
                        .filter(|s| match (&filter_id, &s.target) {
                            (Some(tid), Target::ExistingThread { thread_id }) => tid == thread_id,
                            (Some(_), Target::NewThread { .. }) => false,
                            (None, _) => true,
                        })
                        .collect();
                    while let Some(child) = list_c.first_child() {
                        list_c.remove(&child);
                    }
                    if filtered.is_empty() {
                        stack_c.set_visible_child_name("empty");
                        return;
                    }
                    stack_c.set_visible_child_name("list");
                    let now = now_unix();
                    let clock = GlibLocalClock;
                    let imp = obj_c.imp();
                    for record in filtered {
                        let target_label = match &record.target {
                            Target::ExistingThread { thread_id } => imp
                                .summary_of(thread_id)
                                .map(|s| {
                                    if s.title.is_empty() {
                                        "Untitled thread".to_owned()
                                    } else {
                                        s.title
                                    }
                                })
                                .unwrap_or_else(|| target_summary(&record.target)),
                            Target::NewThread { .. } => target_summary(&record.target),
                        };
                        let subtitle = schedule_row_subtitle(
                            &record.rule,
                            &target_label,
                            record.next_due,
                            record.last_outcome,
                            record.paused_reason,
                            now,
                            &clock,
                        );
                        let row = adw::ActionRow::builder()
                            .title(glib::markup_escape_text(&prompt_preview(&record.prompt)))
                            .subtitle(glib::markup_escape_text(&subtitle))
                            .build();

                        let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
                        controls.set_valign(gtk4::Align::Center);

                        let run_btn = gtk4::Button::builder()
                            .label("Run Now")
                            .css_classes(["flat"])
                            .build();
                        let id_run = record.id.clone();
                        let obj_r = obj_c.downgrade();
                        run_btn.connect_clicked(move |_| {
                            let Some(w) = obj_r.upgrade() else {
                                return;
                            };
                            if !mark_in_flight(&id_run) {
                                w.imp().show_toast("Already running this schedule");
                                return;
                            }
                            let id_fetch = id_run.clone();
                            let w_c = w.clone();
                            glib::MainContext::default().spawn_local(async move {
                                let id_q = id_fetch.clone();
                                let fresh =
                                    store_job(move |s| s.get_schedule(&id_q).ok().flatten()).await;
                                let Some(rec) = fresh.flatten() else {
                                    clear_in_flight(&id_fetch);
                                    w_c.imp().show_toast("This schedule no longer exists");
                                    return;
                                };
                                let now = now_unix();
                                let ok =
                                    dispatch_checked_due(&w_c, rec.clone(), now, now, false, true)
                                        .await;
                                if !ok {
                                    clear_in_flight(&rec.id);
                                }
                            });
                        });
                        controls.append(&run_btn);

                        let is_paused = record.paused_reason.is_some();
                        let pause_btn = gtk4::Button::builder()
                            .label(if is_paused { "Resume" } else { "Pause" })
                            .css_classes(["flat"])
                            .build();
                        let id_p = record.id.clone();
                        let rule_p = record.rule.clone();
                        let ends_p = record.ends;
                        let run_count_p = record.run_count;
                        let obj_p = obj_c.downgrade();
                        let dlg_p = dialog_c.clone();
                        let refresh_p = refresh_w.clone();
                        pause_btn.connect_clicked(move |_| {
                            let Some(w) = obj_p.upgrade() else {
                                return;
                            };
                            let id = id_p.clone();
                            let rule = rule_p.clone();
                            let dlg = dlg_p.clone();
                            let refresh_p = refresh_p.clone();
                            glib::MainContext::default().spawn_local(async move {
                                let now = now_unix();
                                let (paused, next_due_val) = if is_paused {
                                    (None, next_due(&rule, now, &GlibLocalClock))
                                } else {
                                    (Some(PausedReason::User), None)
                                };
                                if is_paused && !budget_allows_resume(&w).await { return; }
                                if is_paused && (next_due_val.is_none()
                                    || matches!(ends_p, Ends::OnDate { at } if next_due_val.is_some_and(|due| due > at))
                                    || matches!(ends_p, Ends::AfterRuns { max } if run_count_p >= max)) {
                                    w.imp().show_toast(
                                        "This schedule has ended. Edit its time to resume.",
                                    );
                                    return;
                                }
                                let result =
                                    write_job("toggle schedule pause", move |store: &Store| {
                                        store.set_schedule_paused(&id, paused, next_due_val)
                                    })
                                    .await;
                                if !matches!(result, Some(Ok(()))) {
                                    w.imp().show_toast(
                                        "Could not update the schedule; refresh and try again",
                                    );
                                    return;
                                }
                                w.imp().refresh_schedule_status();
                                if dlg.upgrade().is_some() {
                                    w.imp().show_toast(if paused.is_some() {
                                        "Schedule paused"
                                    } else {
                                        "Schedule resumed"
                                    });
                                    if let Some(cb) =
                                        refresh_p.upgrade().and_then(|cell| cell.borrow().clone())
                                    {
                                        cb();
                                    }
                                }
                            });
                        });
                        controls.append(&pause_btn);

                        let edit_btn = gtk4::Button::builder()
                            .icon_name("at-document-edit-symbolic")
                            .tooltip_text("Edit")
                            .css_classes(["flat"])
                            .build();
                        edit_btn
                            .update_property(&[gtk4::accessible::Property::Label("Edit schedule")]);
                        let rec_e = record.clone();
                        let obj_e = obj_c.downgrade();
                        let refresh_e = refresh_w.clone();
                        edit_btn.connect_clicked(move |_| {
                            if let Some(w) = obj_e.upgrade() {
                                let on_saved =
                                    refresh_e.upgrade().and_then(|cell| cell.borrow().clone());
                                w.imp().open_edit_schedule_dialog(rec_e.clone(), on_saved);
                            }
                        });
                        controls.append(&edit_btn);

                        let del_btn = gtk4::Button::builder()
                            .icon_name("at-user-trash-symbolic")
                            .tooltip_text("Delete")
                            .css_classes(["flat"])
                            .build();
                        del_btn.update_property(&[gtk4::accessible::Property::Label(
                            "Delete schedule",
                        )]);
                        let id_d = record.id.clone();
                        let row_w = row.downgrade();
                        let list_w = list_c.downgrade();
                        let stack_w = stack_c.downgrade();
                        let obj_d = obj_c.downgrade();
                        del_btn.connect_clicked(move |_| {
                            let id = id_d.clone();
                            let row_w = row_w.clone();
                            let list_w = list_w.clone();
                            let stack_w = stack_w.clone();
                            let obj_d = obj_d.clone();
                            glib::MainContext::default().spawn_local(async move {
                                let result = write_job("delete schedule", move |store: &Store| {
                                    let _ = store.delete_schedule(&id)?;
                                    Ok(())
                                })
                                .await;
                                if let Some(window) = obj_d.upgrade() {
                                    if !matches!(result, Some(Ok(()))) {
                                        window.imp().show_toast(
                                            "Could not delete the schedule; refresh and try again",
                                        );
                                        return;
                                    }
                                    window.imp().show_toast("Schedule deleted");
                                    window.imp().refresh_schedule_status();
                                }
                                if let (Some(r), Some(l)) = (row_w.upgrade(), list_w.upgrade()) {
                                    l.remove(&r);
                                    if l.first_child().is_none() {
                                        if let Some(s) = stack_w.upgrade() {
                                            s.set_visible_child_name("empty");
                                        }
                                    }
                                }
                            });
                        });
                        controls.append(&del_btn);

                        row.add_suffix(&controls);
                        list_c.append(&row);
                    }
                });
            }
        });

        *refresh.borrow_mut() = Some(refresh_fn.clone());

        let open_new = {
            let obj = self.obj().downgrade();
            let filter = filter.clone();
            let refresh_w = Rc::downgrade(&refresh);
            move || {
                if let Some(w) = obj.upgrade() {
                    let tid = filter.borrow().clone();
                    let on_saved = refresh_w.upgrade().and_then(|c| c.borrow().clone());
                    w.imp().open_schedule_editor(ScheduleEditorSeed {
                        editing_id: None,
                        thread_id: tid,
                        initial_prompt: String::new(),
                        has_images: false,
                        clear_composer: false,
                        on_saved,
                        existing: None,
                    });
                }
            }
        };
        let on_new_1 = open_new.clone();
        new_btn.connect_clicked(move |_| on_new_1());
        create_first.connect_clicked(move |_| open_new());

        let refresh_drop = refresh.clone();
        dialog.connect_closed(move |_| {
            *refresh_drop.borrow_mut() = None;
        });

        refresh_fn();
        dialog.present(Some(&*self.obj()));
    }

    pub(super) fn open_schedule_dialog_for(
        &self,
        thread_id: Option<&str>,
        initial_prompt: Option<&str>,
        has_images: bool,
        clear_composer: bool,
    ) {
        self.open_schedule_editor(ScheduleEditorSeed {
            editing_id: None,
            thread_id: thread_id.map(str::to_owned),
            initial_prompt: initial_prompt.unwrap_or_default().to_owned(),
            has_images,
            clear_composer,
            on_saved: None,
            existing: None,
        });
    }

    fn open_edit_schedule_dialog(&self, record: ScheduleRecord, on_saved: Option<Rc<dyn Fn()>>) {
        let tid = match &record.target {
            Target::ExistingThread { thread_id } => Some(thread_id.clone()),
            Target::NewThread { .. } => None,
        };
        self.open_schedule_editor(ScheduleEditorSeed {
            editing_id: Some(record.id.clone()),
            thread_id: tid,
            initial_prompt: record.prompt.clone(),
            has_images: false,
            clear_composer: false,
            on_saved,
            existing: Some(record),
        });
    }

    fn open_schedule_editor(&self, seed: ScheduleEditorSeed) {
        let dialog = adw::Dialog::builder()
            .title(if seed.editing_id.is_some() {
                "Edit Scheduled Prompt"
            } else {
                "Schedule Prompt"
            })
            .content_width(540)
            .content_height(600)
            .build();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let cancel_btn = gtk4::Button::builder().label("Cancel").build();
        let save_btn = gtk4::Button::builder()
            .label(if seed.editing_id.is_some() {
                "Save"
            } else {
                "Schedule"
            })
            .css_classes(["suggested-action"])
            .build();
        header.pack_start(&cancel_btn);
        header.pack_end(&save_btn);
        toolbar.add_top_bar(&header);

        let page = adw::PreferencesPage::new();

        // Prompt group
        let prompt_group = adw::PreferencesGroup::builder().title("Prompt").build();
        let buffer = gtk4::TextBuffer::new(None);
        buffer.set_text(&seed.initial_prompt);
        let text_view = gtk4::TextView::builder()
            .buffer(&buffer)
            .wrap_mode(gtk4::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .build();
        text_view.update_property(&[gtk4::accessible::Property::Label("Scheduled prompt text")]);
        let prompt_scroller = gtk4::ScrolledWindow::builder()
            .min_content_height(96)
            .max_content_height(180)
            .child(&text_view)
            .build();
        let prompt_frame = gtk4::Frame::builder().child(&prompt_scroller).build();
        prompt_group.add(&prompt_frame);

        let secret_warn = gtk4::Label::builder()
            .label("Looks like this prompt contains a secret or token; remove it before saving.")
            .css_classes(["warning", "caption"])
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .build();
        prompt_group.add(&secret_warn);
        let error_label = gtk4::Label::builder()
            .css_classes(["error", "caption"])
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .build();
        prompt_group.add(&error_label);
        page.add(&prompt_group);

        // Target group
        let target_group = adw::PreferencesGroup::builder()
            .title("Target")
            .description(
                "Runs with existing always-allow and sandbox rules; Bypass mode is never used.",
            )
            .build();
        let thread_context = seed
            .thread_id
            .as_deref()
            .and_then(|id| self.summary_of(id))
            .map(|s| (s.id, s.cwd, s.driver.and_then(|k| Driver::from_key(&k))));
        let default_folder = thread_context
            .as_ref()
            .map(|(_, cwd, _)| cwd.clone())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "/".to_owned());

        let has_existing_thread = seed.thread_id.is_some();
        let target_options = if has_existing_thread {
            gtk4::StringList::new(&["This thread", "New thread in folder"])
        } else {
            gtk4::StringList::new(&["New thread in folder"])
        };
        let target_row = adw::ComboRow::builder()
            .title("Run in")
            .model(&target_options)
            .build();
        let initial_new_thread = match seed.existing.as_ref().map(|e| &e.target) {
            Some(Target::NewThread { .. }) => true,
            Some(Target::ExistingThread { .. }) => false,
            None => !has_existing_thread,
        };
        if has_existing_thread {
            target_row.set_selected(u32::from(initial_new_thread));
        } else {
            target_row.set_sensitive(false);
        }
        if seed.editing_id.is_some() {
            target_row.set_sensitive(false);
        }
        target_group.add(&target_row);

        let folder_entry = adw::EntryRow::builder()
            .title("Folder")
            .text(match seed.existing.as_ref().map(|e| &e.target) {
                Some(Target::NewThread { folder, .. }) => folder.as_str(),
                _ => default_folder.as_str(),
            })
            .build();
        target_group.add(&folder_entry);

        let driver_list = gtk4::StringList::new(
            &Driver::ALL
                .iter()
                .map(|d| d.info().label)
                .collect::<Vec<_>>(),
        );
        let driver_row = adw::ComboRow::builder()
            .title("Agent")
            .model(&driver_list)
            .build();
        let initial_driver = match seed.existing.as_ref().map(|e| &e.target) {
            Some(Target::NewThread { driver, .. }) => *driver,
            _ => thread_context
                .as_ref()
                .and_then(|(_, _, d)| *d)
                .or_else(|| self.default_agent())
                .unwrap_or(Driver::Claude),
        };
        if let Some(pos) = Driver::ALL.iter().position(|d| *d == initial_driver) {
            driver_row.set_selected(pos as u32);
        }
        target_group.add(&driver_row);

        let profiles = self
            .config
            .borrow()
            .profiles
            .iter()
            .filter(|p| !p.disabled && crate::config::profile_driver(p).is_some())
            .cloned()
            .collect::<Vec<_>>();
        let profile_names = std::iter::once("Agent default")
            .chain(profiles.iter().map(|p| p.name.as_str()))
            .collect::<Vec<_>>();
        let profile_row = adw::ComboRow::builder()
            .title("Profile")
            .model(&gtk4::StringList::new(&profile_names))
            .build();
        if let Some(Target::NewThread {
            profile_id: Some(id),
            ..
        }) = seed.existing.as_ref().map(|r| &r.target)
        {
            if let Some(index) = profiles.iter().position(|p| &p.id == id) {
                profile_row.set_selected(index as u32 + 1);
            }
        }
        target_group.add(&profile_row);
        let model_row = adw::EntryRow::builder()
            .title("Model (blank uses profile default)")
            .text(match seed.existing.as_ref().map(|r| &r.target) {
                Some(Target::NewThread {
                    model: Some(model), ..
                }) => model,
                _ => "",
            })
            .build();
        target_group.add(&model_row);

        let mode_list = gtk4::StringList::new(&["Plan", "Ask", "Accept Edits"]);
        let mode_row = adw::ComboRow::builder()
            .title("Mode")
            .model(&mode_list)
            .build();
        let initial_mode = match seed.existing.as_ref().map(|e| &e.target) {
            Some(Target::NewThread { mode, .. }) => *mode,
            _ => Mode::Ask,
        };
        mode_row.set_selected(safe_mode_index(initial_mode));
        target_group.add(&mode_row);

        let worktree_row = adw::SwitchRow::builder()
            .title("Run in an isolated git worktree")
            .subtitle("Recommended for repeating tasks; required for Antigravity")
            .active(match seed.existing.as_ref().map(|e| &e.target) {
                Some(Target::NewThread { worktree, .. }) => *worktree,
                _ => initial_driver == Driver::Agy,
            })
            .build();
        target_group.add(&worktree_row);
        let agy_ack = adw::SwitchRow::builder()
            .title("Allow Antigravity unattended in isolation")
            .subtitle("Antigravity acts without asking. Its worktree isolates files, but it retains session grants and network access.")
            .build();
        target_group.add(&agy_ack);
        page.add(&target_group);

        let sync_target_fields = {
            let folder_w = folder_entry.downgrade();
            let driver_w = driver_row.downgrade();
            let mode_w = mode_row.downgrade();
            let worktree_w = worktree_row.downgrade();
            let target_w = target_row.downgrade();
            let profile_w = profile_row.downgrade();
            let model_w = model_row.downgrade();
            let agy_w = agy_ack.downgrade();
            move || {
                let (
                    Some(folder_entry),
                    Some(driver_row),
                    Some(mode_row),
                    Some(worktree_row),
                    Some(target_row),
                ) = (
                    folder_w.upgrade(),
                    driver_w.upgrade(),
                    mode_w.upgrade(),
                    worktree_w.upgrade(),
                    target_w.upgrade(),
                )
                else {
                    return;
                };
                let is_new = !has_existing_thread || target_row.selected() == 1;
                folder_entry.set_visible(is_new);
                driver_row.set_visible(is_new);
                mode_row.set_visible(is_new);
                worktree_row.set_visible(is_new);
                if let Some(row) = profile_w.upgrade() {
                    row.set_visible(is_new);
                }
                if let Some(row) = model_w.upgrade() {
                    row.set_visible(is_new);
                }
                if let Some(row) = agy_w.upgrade() {
                    row.set_visible(is_new);
                }
            }
        };
        sync_target_fields();
        let sync_t = sync_target_fields.clone();
        target_row.connect_selected_notify(move |_| sync_t());

        // When group
        let when_group = adw::PreferencesGroup::builder().title("Schedule").build();
        let kind_list = gtk4::StringList::new(&[
            RuleKind::Once.label(),
            RuleKind::Days.label(),
            RuleKind::Interval.label(),
        ]);
        let kind_row = adw::ComboRow::builder()
            .title("Repeat")
            .model(&kind_list)
            .build();
        let (def_date, def_time) = default_once_inputs();
        let (init_kind, init_date, init_time, init_days, init_hours) =
            match seed.existing.as_ref().map(|e| &e.rule) {
                Some(Rule::Once { at }) => {
                    let (d, t) = GlibLocalClock
                        .civil_at(*at)
                        .map(|c| {
                            (
                                format!("{:04}-{:02}-{:02}", c.year, c.month, c.day),
                                format!("{:02}:{:02}", c.hour, c.minute),
                            )
                        })
                        .unwrap_or((def_date, def_time));
                    (
                        0u32,
                        d,
                        t,
                        [true, true, true, true, true, false, false],
                        4.0,
                    )
                }
                Some(Rule::Days {
                    weekdays,
                    hour,
                    minute,
                }) => (
                    1u32,
                    def_date,
                    format!("{hour:02}:{minute:02}"),
                    *weekdays,
                    4.0,
                ),
                Some(Rule::Interval { every_secs, .. }) => (
                    2u32,
                    def_date,
                    def_time,
                    [true, true, true, true, true, false, false],
                    (*every_secs as f64 / 3600.0).max(1.0),
                ),
                None => (
                    0u32,
                    def_date,
                    def_time,
                    [true, true, true, true, true, false, false],
                    4.0,
                ),
            };
        kind_row.set_selected(init_kind);
        when_group.add(&kind_row);

        let date_row = adw::EntryRow::builder()
            .title("Date (YYYY-MM-DD)")
            .text(&init_date)
            .build();
        when_group.add(&date_row);

        let time_row = adw::EntryRow::builder()
            .title("Time (HH:MM)")
            .text(&init_time)
            .build();
        when_group.add(&time_row);

        let days_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        days_box.set_margin_top(6);
        days_box.set_margin_bottom(6);
        days_box.set_halign(gtk4::Align::Center);
        const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let mut day_buttons = Vec::with_capacity(7);
        for (idx, name) in DAY_NAMES.iter().enumerate() {
            let btn = gtk4::ToggleButton::builder()
                .label(*name)
                .active(init_days[idx])
                .build();
            days_box.append(&btn);
            day_buttons.push(btn);
        }
        let days_action_row = adw::ActionRow::builder().title("Weekdays").build();
        days_action_row.add_suffix(&days_box);
        when_group.add(&days_action_row);

        let hours_adj = gtk4::Adjustment::new(init_hours, 1.0, 168.0, 1.0, 6.0, 0.0);
        let hours_row = adw::SpinRow::builder()
            .title("Every (hours)")
            .adjustment(&hours_adj)
            .digits(0)
            .build();
        when_group.add(&hours_row);

        let missed_list =
            gtk4::StringList::new(&[MissedPolicy::RunOnce.label(), MissedPolicy::Skip.label()]);
        let missed_row = adw::ComboRow::builder()
            .title("If app was closed")
            .model(&missed_list)
            .selected(match seed.existing.as_ref().map(|e| e.missed_policy) {
                Some(MissedPolicy::Skip) => 1,
                _ => 0,
            })
            .build();
        when_group.add(&missed_row);

        let ends_list = gtk4::StringList::new(&["Never", "After a number of runs", "On a date"]);
        let ends_row = adw::ComboRow::builder()
            .title("Ends")
            .model(&ends_list)
            .selected(match seed.existing.as_ref().map(|e| e.ends) {
                Some(Ends::AfterRuns { .. }) => 1,
                Some(Ends::OnDate { .. }) => 2,
                _ => 0,
            })
            .build();
        when_group.add(&ends_row);

        let max_runs_adj = gtk4::Adjustment::new(
            match seed.existing.as_ref().map(|e| e.ends) {
                Some(Ends::AfterRuns { max }) => f64::from(max),
                _ => 5.0,
            },
            1.0,
            500.0,
            1.0,
            5.0,
            0.0,
        );
        let max_runs_row = adw::SpinRow::builder()
            .title("Maximum runs")
            .adjustment(&max_runs_adj)
            .digits(0)
            .build();
        when_group.add(&max_runs_row);
        let end_date = match seed.existing.as_ref().map(|r| r.ends) {
            Some(Ends::OnDate { at }) => GlibLocalClock
                .civil_at(at)
                .map(|c| format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)),
            _ => None,
        }
        .unwrap_or_else(|| default_once_inputs().0);
        let end_date_row = adw::EntryRow::builder()
            .title("End date (YYYY-MM-DD)")
            .text(end_date)
            .build();
        when_group.add(&end_date_row);
        page.add(&when_group);

        let sync_when_fields = {
            let kind_w = kind_row.downgrade();
            let date_w = date_row.downgrade();
            let time_w = time_row.downgrade();
            let days_w = days_action_row.downgrade();
            let hours_w = hours_row.downgrade();
            let missed_w = missed_row.downgrade();
            let ends_w = ends_row.downgrade();
            let max_runs_w = max_runs_row.downgrade();
            let end_date_w = end_date_row.downgrade();
            move || {
                let (
                    Some(kind_row),
                    Some(date_row),
                    Some(time_row),
                    Some(days_action_row),
                    Some(hours_row),
                    Some(missed_row),
                    Some(ends_row),
                    Some(max_runs_row),
                ) = (
                    kind_w.upgrade(),
                    date_w.upgrade(),
                    time_w.upgrade(),
                    days_w.upgrade(),
                    hours_w.upgrade(),
                    missed_w.upgrade(),
                    ends_w.upgrade(),
                    max_runs_w.upgrade(),
                )
                else {
                    return;
                };
                let k = kind_row.selected();
                date_row.set_visible(k == 0);
                time_row.set_visible(k == 0 || k == 1);
                days_action_row.set_visible(k == 1);
                hours_row.set_visible(k == 2);
                missed_row.set_visible(k != 0);
                ends_row.set_visible(k != 0);
                max_runs_row.set_visible(k != 0 && ends_row.selected() == 1);
                if let Some(row) = end_date_w.upgrade() {
                    row.set_visible(k != 0 && ends_row.selected() == 2);
                }
            }
        };
        sync_when_fields();
        let s1 = sync_when_fields.clone();
        kind_row.connect_selected_notify(move |_| s1());
        let s2 = sync_when_fields.clone();
        ends_row.connect_selected_notify(move |_| s2());

        toolbar.set_content(Some(&page));
        dialog.set_child(Some(&toolbar));

        // Live secret preview notice
        let sw = secret_warn.clone();
        buffer.connect_changed(move |buf| {
            let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
            let has_secret = redact(&text) != text.as_str();
            sw.set_visible(has_secret);
        });

        let dlg_cancel = dialog.downgrade();
        cancel_btn.connect_clicked(move |_| {
            if let Some(d) = dlg_cancel.upgrade() {
                d.close();
            }
        });

        let gather: Rc<dyn Fn() -> Result<FormInput, &'static str>> = Rc::new({
            let buffer = buffer.clone();
            let target_row = target_row.clone();
            let folder_entry = folder_entry.clone();
            let driver_row = driver_row.clone();
            let profile_row = profile_row.clone();
            let model_row = model_row.clone();
            let mode_row = mode_row.clone();
            let worktree_row = worktree_row.clone();
            let kind_row = kind_row.clone();
            let date_row = date_row.clone();
            let time_row = time_row.clone();
            let day_buttons = day_buttons.clone();
            let hours_adj = hours_adj.clone();
            let missed_row = missed_row.clone();
            let ends_row = ends_row.clone();
            let max_runs_adj = max_runs_adj.clone();
            let end_date_row = end_date_row.clone();
            let thread_id = seed.thread_id.clone();
            let has_images = seed.has_images;
            let agy_ack = agy_ack.clone();
            move || {
                let now = now_unix();
                let prompt = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();

                let is_new_thread = !has_existing_thread || target_row.selected() == 1;
                let folder = folder_entry.text().trim().to_owned();
                let driver = Driver::ALL
                    .get(driver_row.selected() as usize)
                    .copied()
                    .unwrap_or(Driver::Claude);
                let mode = safe_mode_at(mode_row.selected());
                let use_wt = worktree_row.is_active();
                if is_new_thread && driver == Driver::Agy && !agy_ack.is_active() {
                    return Err(
                        "Acknowledge Antigravity’s unattended authority before scheduling.",
                    );
                }

                let target = if is_new_thread {
                    Target::NewThread {
                        folder: folder.clone(),
                        driver,
                        profile_id: profile_row
                            .selected()
                            .checked_sub(1)
                            .and_then(|i| profiles.get(i as usize))
                            .map(|p| p.id.clone()),
                        mode,
                        model: (!model_row.text().trim().is_empty())
                            .then(|| model_row.text().trim().to_owned()),
                        worktree: use_wt,
                    }
                } else if let Some(tid) = thread_id.as_ref() {
                    Target::ExistingThread {
                        thread_id: tid.clone(),
                    }
                } else {
                    return Err("Choose a target thread or folder.");
                };

                let rule = match kind_row.selected() {
                    0 => {
                        let Some((y, m, d)) = parse_ymd(&date_row.text()) else {
                            return Err("Enter a valid date (YYYY-MM-DD).");
                        };
                        let Some((hh, mm)) = parse_hhmm(&time_row.text()) else {
                            return Err("Enter a valid time (HH:MM).");
                        };
                        match GlibLocalClock.resolve_local(y, m, d, hh, mm) {
                            agent_core::schedule::ResolveLocal::Exact(at)
                            | agent_core::schedule::ResolveLocal::SkippedSpringForward(at)
                            | agent_core::schedule::ResolveLocal::AmbiguousFallBack {
                                first: at,
                                ..
                            } => Rule::Once { at },
                            agent_core::schedule::ResolveLocal::Invalid => {
                                return Err("That date and time is invalid.");
                            }
                        }
                    }
                    1 => {
                        let Some((hh, mm)) = parse_hhmm(&time_row.text()) else {
                            return Err("Enter a valid time (HH:MM).");
                        };
                        let mut weekdays = [false; 7];
                        for (i, b) in day_buttons.iter().enumerate() {
                            weekdays[i] = b.is_active();
                        }
                        Rule::Days {
                            weekdays,
                            hour: hh,
                            minute: mm,
                        }
                    }
                    _ => {
                        let secs = interval_secs_from_hours(hours_adj.value() as u32);
                        Rule::Interval {
                            every_secs: secs,
                            anchor: now,
                        }
                    }
                };

                let missed_policy = if missed_row.selected() == 1 {
                    MissedPolicy::Skip
                } else {
                    MissedPolicy::RunOnce
                };
                let ends = match (kind_row.selected(), ends_row.selected()) {
                    (0, _) | (_, 0) => Ends::Never,
                    (_, 1) => Ends::AfterRuns {
                        max: max_runs_adj.value() as u32,
                    },
                    _ => {
                        let (y, m, d) = parse_ymd(&end_date_row.text())
                            .ok_or("Enter a valid end date (YYYY-MM-DD).")?;
                        let at = match GlibLocalClock.resolve_local(y, m, d, 23, 59) {
                            agent_core::schedule::ResolveLocal::Exact(at)
                            | agent_core::schedule::ResolveLocal::SkippedSpringForward(at)
                            | agent_core::schedule::ResolveLocal::AmbiguousFallBack {
                                first: at,
                                ..
                            } => at,
                            _ => return Err("That end date is invalid."),
                        };
                        Ends::OnDate { at }
                    }
                };
                if let Some(profile) = profile_row
                    .selected()
                    .checked_sub(1)
                    .and_then(|i| profiles.get(i as usize))
                {
                    if is_new_thread && crate::config::profile_driver(profile) != Some(driver) {
                        return Err("Choose a profile for the selected agent.");
                    }
                }
                Ok(FormInput {
                    prompt,
                    has_images,
                    target,
                    in_git_repo: true,
                    rule,
                    missed_policy,
                    ends,
                    now,
                })
            }
        });
        let saving = Rc::new(Cell::new(false));
        let validation_generation = Rc::new(Cell::new(0u64));
        let validate_live: Rc<dyn Fn()> = Rc::new({
            let gather = gather.clone();
            let save = save_btn.downgrade();
            let error = error_label.downgrade();
            let saving = saving.clone();
            let generation = validation_generation.clone();
            move || {
                if saving.get() {
                    return;
                }
                let current = generation.get().wrapping_add(1);
                generation.set(current);
                let result = gather().and_then(|form| validate_form(&form, &GlibLocalClock));
                if let Some(save) = save.upgrade() {
                    save.set_sensitive(result.is_ok());
                }
                if let Some(error) = error.upgrade() {
                    error.set_visible(result.is_err());
                    if let Err(message) = result {
                        error.set_text(message);
                    }
                }
                if let Ok(mut form) = gather() {
                    if let Target::NewThread {
                        folder, worktree, ..
                    } = &form.target
                    {
                        let folder = folder.clone();
                        let worktree = *worktree;
                        if let Some(save) = save.upgrade() {
                            save.set_sensitive(false);
                        }
                        let save = save.clone();
                        let error = error.clone();
                        let generation = generation.clone();
                        let saving = saving.clone();
                        glib::spawn_future_local(async move {
                            let valid_folder = off_main("checking schedule folder", move || {
                                let path = PathBuf::from(folder);
                                Ok((
                                    path.is_dir(),
                                    !worktree || crate::worktree::main_toplevel(&path).is_ok(),
                                ))
                            })
                            .await;
                            if generation.get() != current || saving.get() {
                                return;
                            }
                            let result = match valid_folder {
                                Ok((true, git)) => {
                                    form.in_git_repo = git;
                                    validate_form(&form, &GlibLocalClock).map(|_| ())
                                }
                                _ => Err("Choose an existing folder for this schedule."),
                            };
                            if let Some(save) = save.upgrade() {
                                save.set_sensitive(result.is_ok());
                            }
                            if let Some(error) = error.upgrade() {
                                error.set_visible(result.is_err());
                                if let Err(message) = result {
                                    error.set_text(message);
                                }
                            }
                        });
                    }
                }
            }
        });
        for row in [
            &folder_entry,
            &model_row,
            &date_row,
            &time_row,
            &end_date_row,
        ] {
            let weak = Rc::downgrade(&validate_live);
            row.connect_changed(move |_| {
                if let Some(check) = weak.upgrade() {
                    check();
                }
            });
        }
        for row in [
            &target_row,
            &driver_row,
            &profile_row,
            &mode_row,
            &kind_row,
            &missed_row,
            &ends_row,
        ] {
            let weak = Rc::downgrade(&validate_live);
            row.connect_selected_notify(move |_| {
                if let Some(check) = weak.upgrade() {
                    check();
                }
            });
        }
        for button in &day_buttons {
            let weak = Rc::downgrade(&validate_live);
            button.connect_toggled(move |_| {
                if let Some(check) = weak.upgrade() {
                    check();
                }
            });
        }
        for adj in [&hours_adj, &max_runs_adj] {
            let weak = Rc::downgrade(&validate_live);
            adj.connect_value_changed(move |_| {
                if let Some(check) = weak.upgrade() {
                    check();
                }
            });
        }
        let weak = Rc::downgrade(&validate_live);
        buffer.connect_changed(move |_| {
            if let Some(check) = weak.upgrade() {
                check();
            }
        });
        let weak = Rc::downgrade(&validate_live);
        worktree_row.connect_active_notify(move |_| {
            if let Some(check) = weak.upgrade() {
                check();
            }
        });
        let weak = Rc::downgrade(&validate_live);
        agy_ack.connect_active_notify(move |_| {
            if let Some(check) = weak.upgrade() {
                check();
            }
        });
        validate_live();

        let obj_w = self.obj().downgrade();
        let dlg_w = dialog.downgrade();
        let save_w = save_btn.downgrade();
        let clear_composer = seed.clear_composer;
        let on_saved = seed.on_saved.clone();
        save_btn.connect_clicked(move |btn| {
            let _keep_validation_alive = &validate_live;
            if !btn.is_sensitive() {
                return;
            }
            let Some(window) = obj_w.upgrade() else {
                return;
            };
            let Some(dlg) = dlg_w.upgrade() else {
                return;
            };
            let now = now_unix();
            let mut form = match gather() {
                Ok(form) => form,
                Err(message) => { error_label.set_text(message); error_label.set_visible(true); return; }
            };
            let folder_check = match &form.target { Target::NewThread { folder, .. } => Some(folder.clone()), _ => None };
            let err_lbl = error_label.clone();
            let clear_tid = clear_composer.then(|| seed.thread_id.clone()).flatten();
            let edit_id = seed.editing_id.clone();
            let on_saved = on_saved.clone();
            let save_w = save_w.clone();
            btn.set_sensitive(false);
            saving.set(true);
            let saving = saving.clone();
            glib::MainContext::default().spawn_local(async move {
                let in_git_repo = match folder_check {
                    Some(dir) if !dir.is_empty() => {
                        off_main("checking repository for scheduled worktree", move || {
                            let p = PathBuf::from(&dir);
                            Ok(p.is_dir() && crate::worktree::main_toplevel(&p).is_ok())
                        })
                        .await
                        .unwrap_or(false)
                    }
                    _ => true,
                };
                form.in_git_repo = in_git_repo;
                let validated = match validate_form(&form, &GlibLocalClock) {
                    Ok(v) => v,
                    Err(msg) => {
                        saving.set(false);
                        err_lbl.set_text(msg);
                        err_lbl.set_visible(true);
                        if let Some(b) = save_w.upgrade() {
                            b.set_sensitive(true);
                        }
                        return;
                    }
                };
                if validated.has_secret_warning {
                    err_lbl.set_text(
                        "Secrets in prompts are redacted when saved; replace them if unintended.",
                    );
                    err_lbl.set_visible(true);
                }

                let target_due = validated.next_due;
                let res = write_job("save schedule", move |store: &Store| {
                    if let Some(id) = edit_id {
                        store.atomically(|store| {
                            store.update_schedule_target(&id, &validated.target)?;
                            store.update_schedule(
                            &id,
                            &validated.prompt,
                            &validated.rule,
                            validated.missed_policy,
                            validated.ends,
                            validated.next_due,
                            )
                        })?;
                    } else {
                        let _ = store.create_schedule(&NewSchedule {
                            target: validated.target,
                            prompt: validated.prompt,
                            rule: validated.rule,
                            missed_policy: validated.missed_policy,
                            ends: validated.ends,
                            next_due: validated.next_due,
                        })?;
                    }
                    Ok(())
                })
                .await;
                match res {
                    Some(Ok(())) => {
                        window.imp().refresh_schedule_status();
                        if let Some(tid) = clear_tid {
                            let view = window.imp().with_chat(&tid, |chat| chat.view.clone()).flatten();
                            if let Some(view) = view { view.clear_composer_draft(); }
                        }
                        if let Some(cb) = on_saved {
                            cb();
                        }
                        let due_str = target_due
                            .map(|d| format_due(d, now, &GlibLocalClock))
                            .unwrap_or_else(|| "soon".to_owned());
                        window.imp().show_toast(&format!("Scheduled ({due_str})"));
                        dlg.close();
                    }
                    Some(Err(e)) => {
                        saving.set(false);
                        if let Some(b) = save_w.upgrade() {
                            b.set_sensitive(true);
                        }
                        present_message_over(&dlg, "Could Not Save Schedule", &e.to_string());
                    }
                    None => {
                        // Keep Save disabled: `write_job` may still be queued and commit later,
                        // so allowing an immediate retry could insert a duplicate schedule.
                        present_message_over(
                            &dlg,
                            "Store Timed Out",
                            "The thread store did not reply in time. Close this dialog and check the Scheduled Prompts list before trying again.",
                        );
                    }
                }
            });
        });

        let _ = SAFE_MODES;
        dialog.present(Some(&*self.obj()));
    }
}

struct ScheduleEditorSeed {
    editing_id: Option<String>,
    thread_id: Option<String>,
    initial_prompt: String,
    has_images: bool,
    clear_composer: bool,
    on_saved: Option<Rc<dyn Fn()>>,
    existing: Option<ScheduleRecord>,
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::testutil::pump_until;

    fn pending_for_test(id: &str, now: i64, revision: i64) -> PendingScheduled {
        PendingScheduled {
            schedule_id: id.to_owned(),
            revision,
            due: now,
            prompt: "Check tests".into(),
            in_worktree: false,
            next_due: Some(now + 3600),
            new_paused: None,
            count_unattended: true,
            notify: false,
            manual: false,
        }
    }

    #[test]
    fn scheduled_finish_keeps_guard_until_late_committed_writer_receipt() {
        crate::testutil::in_loop(|ctx| {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("threads.db");
            let store = Store::open(&path).expect("store");
            let thread = store.create_thread("/tmp", None).expect("thread");
            let id = store
                .create_schedule(&NewSchedule {
                    target: Target::ExistingThread { thread_id: thread },
                    prompt: "Check tests".into(),
                    rule: Rule::Interval {
                        every_secs: 3600,
                        anchor: 1000,
                    },
                    missed_policy: MissedPolicy::Skip,
                    ends: Ends::Never,
                    next_due: Some(1000),
                })
                .expect("schedule");
            let writer = agent_kit::store_writer::StoreWriter::spawn(&path).expect("writer");
            let (release, held) = std::sync::mpsc::channel();
            let (started, waiting) = std::sync::mpsc::channel();
            let _hold_receipt = writer.call_result("hold scheduled receipt", move |_| {
                let _ = started.send(());
                held.recv()
                    .map_err(|e| agent_kit::store::StoreError::Failed(e.to_string()))?;
                Ok(())
            });
            waiting
                .recv_timeout(Duration::from_secs(5))
                .expect("writer held");
            let run_id = id.clone();
            let receipt = writer.call_result("record delayed scheduled run", move |s| {
                s.record_schedule_run(&ScheduleRunUpdate {
                    id: run_id,
                    next_due: Some(4600),
                    last_due: Some(1000),
                    last_outcome: Some(LastOutcome::Ran),
                    increment_run_count: true,
                    increment_unattended: true,
                    paused_reason: None,
                })
            });
            assert!(mark_in_flight(&id));
            let finished = Rc::new(Cell::new(false));
            let done = finished.clone();
            let pending = pending_for_test(&id, 1000, 0);
            ctx.spawn_local(async move {
                finish_run(
                    glib::WeakRef::new(),
                    pending,
                    await_schedule_receipt(receipt),
                )
                .await;
                done.set(true);
            });
            let until = std::time::Instant::now() + Duration::from_millis(50);
            assert!(pump_until(ctx, 2, || std::time::Instant::now() >= until));
            assert!(!finished.get());
            assert!(!mark_in_flight(&id), "pending receipt still owns its guard");
            release.send(()).expect("release writer");
            assert!(pump_until(ctx, 5, || finished.get()));
            assert_eq!(
                store
                    .get_schedule(&id)
                    .expect("read")
                    .expect("record")
                    .run_count,
                1
            );
            assert!(mark_in_flight(&id), "actual late commit released the guard");
            clear_in_flight(&id);
            assert_eq!(
                writer.shutdown(Duration::from_secs(5)),
                agent_kit::store_writer::Shutdown::Drained
            );
        });
    }

    #[test]
    #[ignore = "needs a display; run through make test-one"]
    fn scheduled_delivery_rechecks_budget_and_pause_after_startup() {
        use crate::chat::ChatBackend;
        use agent_core::event::{Envelope, Event, ItemKind, TurnState};
        crate::testutil::gtk_init().expect("GTK");
        let app = super::super::test_support::test_app("org.test.ScheduleDelivery", true);
        let ctx = glib::MainContext::default();
        let window = super::super::test_support::test_window(&app, &ctx);
        window.imp().config.borrow_mut().notify_scheduled = false;
        let dir = tempfile::tempdir().expect("fixture folder");
        let store = app_store();
        let thread = store
            .create_thread(&dir.path().to_string_lossy(), Some("Delivery"))
            .expect("thread");
        let seen = Rc::new(RefCell::new(Vec::<Envelope>::new()));
        let sink = seen.clone();
        let session = ChatSession::new(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            crate::testutil::open_session(
                "/usr/bin/python3",
                vec![
                    "-I".into(),
                    format!(
                        "{}/tests/support/scripted_claude.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    dir.path().to_string_lossy().into_owned(),
                ],
                dir.path().to_string_lossy(),
            ),
            store.clone(),
            thread.clone(),
            Rc::new(move |env| sink.borrow_mut().push(env.clone())),
            None,
        );
        session.send_prompt("startup check");
        assert!(pump_until(&ctx, 10, || seen
            .borrow()
            .iter()
            .any(|e| matches!(e.event, Event::TurnCompleted { .. }))
            && !session.status().running_turn));
        let now = now_unix();
        let id = store
            .create_schedule(&NewSchedule {
                target: Target::ExistingThread {
                    thread_id: thread.clone(),
                },
                prompt: "Check tests".into(),
                rule: Rule::Interval {
                    every_secs: 3600,
                    anchor: now,
                },
                missed_policy: MissedPolicy::Skip,
                ends: Ends::Never,
                next_due: Some(now),
            })
            .expect("schedule");
        let mut pending = pending_for_test(
            &id,
            now,
            store
                .get_schedule(&id)
                .expect("read")
                .expect("record")
                .updated_at,
        );
        assert!(
            ctx.block_on(automatic_delivery_allowed(&window, &pending)),
            "initial tick allowed it"
        );
        // Startup happened between the initial tick and this pending prompt being drained.
        // Another turn now crosses the warning threshold before actual delivery.
        let provider = store.provider_threads(&thread).expect("providers")[0]
            .id
            .clone();
        store
            .append_event(
                &thread,
                Some(&provider),
                &Envelope::new(Event::TurnCompleted {
                    state: TurnState::Completed,
                    usage: None,
                    cost_usd: Some(0.8),
                    error: None,
                    model: None,
                    billing: Some(agent_core::spend::Billing::Metered),
                }),
            )
            .expect("other turn cost");
        {
            let mut cfg = window.imp().config.borrow_mut();
            cfg.spend_warn_enabled = true;
            cfg.spend_warn_daily = Some(1.0);
        }
        let marked = || {
            store.events(&thread, None, 100).expect("events").into_iter().filter(|(_, env)| matches!(&env.event, Event::ItemStarted { kind: ItemKind::UserMessage, input: Some(input), .. } if agent_core::schedule::parse_scheduled_input(input).is_some())).count()
        };
        assert!(mark_in_flight(&id));
        dispatch_on_session(&window, &session, pending.clone());
        assert!(pump_until(&ctx, 10, || IN_FLIGHT.with(|s| !s.borrow().contains(&id))));
        assert_eq!(
            marked(),
            0,
            "pending automatic prompt never reached the session"
        );
        assert_eq!(
            store
                .get_schedule(&id)
                .expect("read")
                .expect("record")
                .paused_reason,
            Some(PausedReason::Budget)
        );
        let mut manual = pending.clone();
        manual.manual = true;
        manual.count_unattended = false;
        manual.new_paused = Some(PausedReason::Budget);
        assert!(mark_in_flight(&id));
        dispatch_on_session(&window, &session, manual);
        assert!(pump_until(&ctx, 10, || marked() == 1
            && IN_FLIGHT.with(|s| !s.borrow().contains(&id))));
        assert_eq!(
            store
                .get_schedule(&id)
                .expect("read")
                .expect("record")
                .paused_reason,
            Some(PausedReason::Budget),
            "manual delivery retains future pause"
        );
        assert!(pump_until(&ctx, 10, || !session.status().running_turn));
        window.imp().config.borrow_mut().spend_warn_enabled = false;
        store
            .set_schedule_paused(&id, None, Some(now))
            .expect("resume for next case");
        pending.revision = store
            .get_schedule(&id)
            .expect("read")
            .expect("record")
            .updated_at;
        assert!(ctx.block_on(automatic_delivery_allowed(&window, &pending)));
        store
            .set_schedule_paused(&id, Some(PausedReason::User), None)
            .expect("pause during startup");
        assert!(mark_in_flight(&id));
        dispatch_on_session(&window, &session, pending.clone());
        assert!(pump_until(&ctx, 10, || IN_FLIGHT.with(|s| !s.borrow().contains(&id))));
        assert_eq!(
            marked(),
            1,
            "a pause during startup cancels the pending automatic delivery"
        );
        store
            .set_schedule_paused(&id, None, Some(now))
            .expect("resume for edit case");
        pending.revision = store
            .get_schedule(&id)
            .expect("read")
            .expect("record")
            .updated_at;
        assert!(ctx.block_on(automatic_delivery_allowed(&window, &pending)));
        let other = store
            .create_thread("/tmp", Some("Changed target"))
            .expect("other target");
        store
            .update_schedule_target(
                &id,
                &Target::ExistingThread {
                    thread_id: other.clone(),
                },
            )
            .expect("edit target without changing prompt");
        assert!(mark_in_flight(&id));
        dispatch_on_session(&window, &session, pending);
        assert!(pump_until(&ctx, 10, || IN_FLIGHT.with(|s| !s.borrow().contains(&id))));
        assert_eq!(
            marked(),
            1,
            "a same-prompt target edit cancels the stale delivery"
        );
        session.shutdown(false);
        assert!(pump_until(&ctx, 10, || !session.status().alive));
        window.destroy();
        store.delete_thread(&thread).expect("cleanup");
        store.delete_thread(&other).expect("cleanup changed target");
    }

    #[test]
    fn schedule_status_generation_rejects_older_responses() {
        let old = STATUS_GENERATION.with(crate::window::Generation::next);
        let current = STATUS_GENERATION.with(crate::window::Generation::next);
        assert!(!STATUS_GENERATION.with(|g| g.is_current(old)));
        assert!(STATUS_GENERATION.with(|g| g.is_current(current)));
    }

    #[test]
    #[ignore = "needs a display; run through make test-one"]
    fn schedule_status_actions_preserve_budget_pause_and_update_background_badges() {
        use crate::window::sidebar_model::ScheduleBadge;
        use agent_core::event::{Envelope, Event, TurnState};
        crate::testutil::gtk_init().expect("GTK");
        let app = super::super::test_support::test_app("org.test.ScheduleStatus", true);
        let ctx = glib::MainContext::default();
        let window = super::super::test_support::test_window(&app, &ctx);
        let imp = window.imp();
        for profile in &mut imp.config.borrow_mut().profiles {
            profile.disabled = true;
            profile.command = "/nonexistent/schedule-test-agent".into();
        }
        imp.finish_shell(None);
        imp.reveal_shell();
        imp.config.borrow_mut().notify_scheduled = false;
        let store = app_store();
        let now = now_unix();
        let mut threads = Vec::new();
        let mut ids = Vec::new();
        let mut views = Vec::new();
        let mut provider = String::new();
        for name in ["Selected schedule", "Background schedule"] {
            let thread = store.create_thread("/tmp", Some(name)).expect("thread");
            provider = store
                .add_provider_thread(&thread, "claude", "default")
                .expect("provider");
            store
                .set_active_provider_thread(&thread, &provider)
                .expect("active provider");
            let id = store
                .create_schedule(&NewSchedule {
                    target: Target::ExistingThread {
                        thread_id: thread.clone(),
                    },
                    prompt: "Check tests".into(),
                    rule: Rule::Once { at: now + 3600 },
                    missed_policy: MissedPolicy::Skip,
                    ends: Ends::Never,
                    next_due: Some(now + 3600),
                })
                .expect("schedule");
            imp.reload_summaries();
            let page = imp.open_thread(&thread, views.is_empty()).expect("page");
            imp.ensure_built(&page);
            assert!(pump_until(&ctx, 10, || imp
                .with_chat(&thread, |c| c.view.is_some())
                .unwrap_or(false)));
            views.push(
                imp.with_chat(&thread, |c| c.view.clone())
                    .flatten()
                    .expect("view"),
            );
            threads.push(thread);
            ids.push(id);
        }
        window.present();
        imp.refresh_schedule_status();
        assert!(pump_until(&ctx, 10, || imp
            .schedule_badges
            .borrow()
            .as_ref()
            .is_some_and(|b| threads
                .iter()
                .all(|t| matches!(b.get(t), Some(ScheduleBadge::Next(_)))))));
        for view in &views {
            assert!(
                pump_until(&ctx, 10, || crate::testutil::label_texts(view.upcast_ref())
                    .iter()
                    .any(|t| t.contains("Next:"))),
                "both selected and background headers show their next run"
            );
        }
        {
            let mut config = imp.config.borrow_mut();
            config.spend_warn_enabled = true;
            config.spend_warn_daily = Some(1.0);
            config.spend_warn_monthly = None;
        }
        let cost = |value| {
            store
                .append_event(
                    &threads[1],
                    Some(&provider),
                    &Envelope::new(Event::TurnCompleted {
                        state: TurnState::Completed,
                        usage: None,
                        cost_usd: Some(value),
                        error: None,
                        model: None,
                        billing: Some(agent_core::spend::Billing::Metered),
                    }),
                )
                .expect("metered turn");
        };
        cost(0.79);
        assert!(
            ctx.block_on(check_schedule_budget(&window)),
            "below 80% remains active"
        );
        cost(0.01);
        assert!(
            !ctx.block_on(check_schedule_budget(&window)),
            "80% pauses schedules"
        );
        assert!(pump_until(&ctx, 10, || imp
            .schedule_badges
            .borrow()
            .as_ref()
            .is_some_and(|b| threads.iter().all(
                |t| b.get(t) == Some(&ScheduleBadge::Paused(PausedReason::Budget))
            ))));
        for view in &views {
            assert!(pump_until(&ctx, 10, || crate::testutil::label_texts(
                view.upcast_ref()
            )
            .iter()
            .any(|t| t.contains(PausedReason::Budget.label()))));
        }
        for title in ["Resume", "Run Now", "Delete"] {
            let control = crate::testutil::find_all::<gtk4::Button>(views[0].upcast_ref())
                .into_iter()
                .find(|b| b.label().as_deref() == Some(title))
                .expect("status control");
            assert!(control.grab_focus());
            imp.refresh_schedule_status();
            crate::testutil::settle(&ctx);
            let current = crate::testutil::find_all::<gtk4::Button>(views[0].upcast_ref())
                .into_iter()
                .find(|b| b.label().as_deref() == Some(title))
                .expect("refreshed control");
            assert_eq!(
                current, control,
                "{title} is reused on an unchanged refresh"
            );
            assert!(control.has_focus(), "{title} retains keyboard focus");
        }
        let resume = crate::testutil::find_all::<gtk4::Button>(views[0].upcast_ref())
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Resume"))
            .expect("Resume action");
        resume.emit_clicked();
        assert!(pump_until(&ctx, 10, || imp
            .test_toasts
            .borrow()
            .iter()
            .any(|t| t
                .title()
                .is_some_and(|t| t.contains("Cannot resume while")))));
        assert_eq!(
            store
                .get_schedule(&ids[0])
                .expect("schedule")
                .expect("present")
                .paused_reason,
            Some(PausedReason::Budget)
        );
        // The manual dispatcher bypasses the budget gate but retains the pause. A disabled
        // agent may refuse the attempt; it must never be rejected by the budget gate itself.
        let record = store.get_schedule(&ids[0]).expect("read").expect("record");
        assert_eq!(record.paused_reason, Some(PausedReason::Budget));
        let run_now = crate::testutil::find_all::<gtk4::Button>(views[0].upcast_ref())
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Run Now"))
            .expect("Run Now action");
        let before = imp.test_toasts.borrow().len();
        run_now.emit_clicked();
        assert!(pump_until(&ctx, 10, || imp.test_toasts.borrow().len() > before));
        assert!(!imp.test_toasts.borrow()[before..]
            .iter()
            .any(|t| t.title().is_some_and(|t| t.contains("Cannot resume while"))));
        assert_eq!(
            store
                .get_schedule(&ids[0])
                .expect("read")
                .expect("record")
                .paused_reason,
            Some(PausedReason::Budget)
        );
        let delete = crate::testutil::find_all::<gtk4::Button>(views[0].upcast_ref())
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Delete"))
            .expect("Delete action");
        delete.emit_clicked();
        assert!(pump_until(&ctx, 10, || store
            .get_schedule(&ids[0])
            .expect("read")
            .is_none()));
        assert!(pump_until(&ctx, 10, || imp
            .schedule_badges
            .borrow()
            .as_ref()
            .is_some_and(|b| !b.contains_key(&threads[0]))));
        // Unknown and known-empty are distinct cache states in the drawn sidebar model.
        *imp.schedule_badges.borrow_mut() = None;
        assert!(imp.sidebar_rows().iter().filter(|r| matches!(&r.key, crate::window::sidebar_model::RowKey::Thread(t) if threads.contains(t))).all(|r| r.schedule == Some(ScheduleBadge::Unknown)));
        window.destroy();
        for thread in threads {
            store.delete_thread(&thread).expect("cleanup");
        }
    }

    #[test]
    #[ignore = "needs a display; run through make test-one"]
    fn schedule_editor_validates_before_save_and_offers_profile_model_and_end_date() {
        crate::testutil::gtk_init().expect("GTK");
        let app = adw::Application::builder()
            .application_id("org.test.ScheduleEditor")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>)
            .expect("register");
        let window = AgentTerminalWindow::new(&app);
        window.present();
        window.imp().open_schedule_editor(ScheduleEditorSeed {
            editing_id: None,
            thread_id: None,
            initial_prompt: "Check tests".into(),
            has_images: false,
            clear_composer: false,
            on_saved: None,
            existing: None,
        });
        let ctx = glib::MainContext::default();
        assert!(pump_until(&ctx, 5, || window.visible_dialog().is_some()));
        let dialog = window.visible_dialog().expect("editor");
        assert!(crate::testutil::pump_until_dialog_open(&ctx, &dialog, 10));
        let root = dialog.upcast_ref::<gtk4::Widget>();
        let save = crate::testutil::find_all::<gtk4::Button>(root)
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Schedule"))
            .expect("Schedule button");
        let entries = crate::testutil::find_all::<adw::EntryRow>(root);
        let date = entries
            .iter()
            .find(|e| e.title() == "Date (YYYY-MM-DD)")
            .expect("date");
        let model = entries
            .iter()
            .find(|e| e.title() == "Model (blank uses profile default)")
            .expect("model");
        assert!(model.is_visible());
        assert!(crate::testutil::find_all::<adw::ComboRow>(root)
            .iter()
            .any(|r| r.title() == "Profile"));
        assert!(pump_until(&ctx, 10, || save.is_sensitive()));
        date.set_text("invalid");
        assert!(
            !save.is_sensitive(),
            "invalid date disables Save immediately"
        );
        date.set_text(default_once_inputs().0.as_str());
        assert!(pump_until(&ctx, 10, || save.is_sensitive()));
        let text = crate::testutil::find_first::<gtk4::TextView>(root).expect("prompt");
        text.buffer().set_text("Inspect {branch}");
        assert!(!save.is_sensitive(), "unfilled placeholders disable Save");
        text.buffer().set_text("Check tests");
        let combos = crate::testutil::find_all::<adw::ComboRow>(root);
        combos
            .iter()
            .find(|r| r.title() == "Repeat")
            .expect("repeat")
            .set_selected(2);
        combos
            .iter()
            .find(|r| r.title() == "Ends")
            .expect("ends")
            .set_selected(2);
        let end_date = entries
            .iter()
            .find(|e| e.title() == "End date (YYYY-MM-DD)")
            .expect("end date");
        assert!(end_date.is_visible());
        end_date.set_text("bad");
        assert!(!save.is_sensitive());
        end_date.set_text("2099-12-31");
        assert!(pump_until(&ctx, 10, || save.is_sensitive()));
        crate::testutil::settle(&ctx);
        crate::testutil::capture_window(window.upcast_ref(), "schedule-editor");
        dialog.force_close();
        window.imp().config.borrow_mut().notify_scheduled = false;
        let store = app_store();
        let thread = store
            .create_thread("/tmp", Some("Missed schedule"))
            .expect("thread");
        let now = now_unix();
        let id = store
            .create_schedule(&NewSchedule {
                target: Target::ExistingThread {
                    thread_id: thread.clone(),
                },
                prompt: "Check tests".into(),
                rule: Rule::Interval {
                    every_secs: 3600,
                    anchor: now - 7200,
                },
                missed_policy: MissedPolicy::Skip,
                ends: Ends::Never,
                next_due: Some(now - 7200),
            })
            .expect("schedule");
        let record = store.get_schedule(&id).expect("read").expect("record");
        assert!(!ctx.block_on(evaluate_and_maybe_fire(&app, &window, record, now, false)));
        assert_eq!(
            store
                .get_schedule(&id)
                .expect("read")
                .expect("record")
                .last_outcome,
            Some(LastOutcome::Missed)
        );
        let events = store.events(&thread, None, 100).expect("events");
        assert!(events.iter().any(|(_, e)| matches!(&e.event, agent_core::event::Event::Notice { text } if text.contains(LastOutcome::Missed.label()))), "a missed run leaves an app notice");
        assert!(
            !events.iter().any(|(_, e)| matches!(
                &e.event,
                agent_core::event::Event::ItemStarted {
                    kind: agent_core::event::ItemKind::UserMessage,
                    ..
                }
            )),
            "skipping never creates a user message"
        );
        store.delete_thread(&thread).expect("cleanup");
        window.destroy();
    }

    pub(in super::super) fn gtk_checks(window: &AgentTerminalWindow) {
        let imp = window.imp();
        let store = app_store();
        let thread = store
            .create_thread("/tmp", Some("sched-test"))
            .expect("thread");
        let now = now_unix();
        let validated = validate_form(
            &FormInput {
                prompt: "Check CI status".to_owned(),
                has_images: false,
                target: Target::ExistingThread {
                    thread_id: thread.clone(),
                },
                in_git_repo: true,
                rule: Rule::Once {
                    at: now.saturating_add(3600),
                },
                missed_policy: MissedPolicy::RunOnce,
                ends: Ends::Never,
                now,
            },
            &GlibLocalClock,
        )
        .expect("valid form");

        let sid = store
            .create_schedule(&NewSchedule {
                target: validated.target,
                prompt: validated.prompt,
                rule: validated.rule,
                missed_policy: validated.missed_policy,
                ends: validated.ends,
                next_due: validated.next_due,
            })
            .expect("create schedule");

        let ctx = glib::MainContext::default();
        window.present();
        assert!(pump_until(&ctx, 5, || window.is_mapped()));
        imp.open_schedules_for_thread(&thread);
        assert!(pump_until(&ctx, 5, || window.visible_dialog().is_some()));
        if let Some(dlg) = window.visible_dialog() {
            assert!(crate::testutil::pump_until_dialog_open(&ctx, &dlg, 10));
            dlg.force_close();
            assert!(pump_until(&ctx, 10, || window.visible_dialog().is_none()));
        }
        crate::testutil::settle(&ctx);

        store.delete_schedule(&sid).expect("cleanup schedule");
        store.delete_thread(&thread).expect("cleanup thread");
        imp.reload_summaries();
    }
}
