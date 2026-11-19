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
    app_handle, off_main, record_new_thread_as, store_job, write_job, NewThreadIds, NewThreadSpec,
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
    static IN_FLIGHT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Queued details for dispatching a due or manually triggered scheduled prompt onto a session.
#[derive(Debug, Clone)]
pub(super) struct PendingScheduled {
    pub schedule_id: String,
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
    glib::MainContext::default().spawn_local(async move {
        let _ = write_job("pause stopped schedule", move |store: &Store| {
            for id in &stopped_ids {
                let _ = store.set_schedule_paused(id, Some(PausedReason::StoppedRun), None);
            }
            Ok(())
        })
        .await;
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
            let _ = write_job("record missed schedule", move |store: &Store| {
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
    let target_state = match &record.target {
        Target::ExistingThread { thread_id } => {
            match inspect_existing_thread(window, thread_id).await {
                Ok(s) => s,
                Err(pause) => {
                    let id = record.id.clone();
                    let _ = write_job("pause stale schedule", move |store: &Store| {
                        store.set_schedule_paused(&id, Some(pause), None)
                    })
                    .await;
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
                let _ = write_job("record skipped schedule run", move |store: &Store| {
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
                return false;
            }
            ScheduleDecision::Pause(reason) => {
                let id = record.id.clone();
                let _ = write_job("pause schedule", move |store: &Store| {
                    store.set_schedule_paused(&id, Some(reason), None)
                })
                .await;
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
    let stored = write_job("create scheduled thread", move |store: &Store| {
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
            glib::MainContext::default().spawn_local(async move {
                let id_copy = sched_id.clone();
                let res = write_job("record scheduled run", move |store: &Store| {
                    store.record_schedule_run(&ScheduleRunUpdate {
                        id: id_copy,
                        next_due: pending.next_due,
                        last_due: Some(pending.due),
                        last_outcome: Some(LastOutcome::Ran),
                        increment_run_count: true,
                        increment_unattended: pending.count_unattended,
                        paused_reason: pending.new_paused,
                    })
                })
                .await;
                if matches!(res, Some(Ok(()))) {
                    clear_in_flight(&sched_id);
                } else {
                    tracing::warn!(
                        schedule = %sched_id,
                        "failed to record scheduled run; keeping in-flight guard to prevent duplicate firing"
                    );
                }
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

/// Marks a failed/refused scheduled attempt without incrementing run counters.
pub(super) fn mark_dispatch_failed(_window: &AgentTerminalWindow, pending: PendingScheduled) {
    let sched_id = pending.schedule_id.clone();
    glib::MainContext::default().spawn_local(async move {
        let id_copy = sched_id.clone();
        let _ = write_job("record failed scheduled run", move |store: &Store| {
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

impl super::AgentTerminalWindow {
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
            let filter = filter.clone();
            let self_refresh_w = Rc::downgrade(&refresh);
            move || {
                let Some(obj) = obj.upgrade() else {
                    return;
                };
                let filter_id = filter.borrow().clone();
                let list_w = list_w.clone();
                let stack_w = stack_w.clone();
                let obj_c = obj.clone();
                let dialog_c = dialog_w.clone();
                let refresh_w = self_refresh_w.clone();
                glib::MainContext::default().spawn_local(async move {
                    let schedules = store_job(|store| store.list_schedules().unwrap_or_default())
                        .await
                        .unwrap_or_default();
                    let (Some(list_c), Some(stack_c)) = (list_w.upgrade(), stack_w.upgrade())
                    else {
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
                                let _ = write_job("toggle schedule pause", move |store: &Store| {
                                    store.set_schedule_paused(&id, paused, next_due_val)
                                })
                                .await;
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
                        del_btn.connect_clicked(move |_| {
                            let id = id_d.clone();
                            let row_w = row_w.clone();
                            let list_w = list_w.clone();
                            let stack_w = stack_w.clone();
                            glib::MainContext::default().spawn_local(async move {
                                let _ = write_job("delete schedule", move |store: &Store| {
                                    let _ = store.delete_schedule(&id)?;
                                    Ok(())
                                })
                                .await;
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
        page.add(&target_group);

        let sync_target_fields = {
            let folder_w = folder_entry.downgrade();
            let driver_w = driver_row.downgrade();
            let mode_w = mode_row.downgrade();
            let worktree_w = worktree_row.downgrade();
            let target_w = target_row.downgrade();
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

        let ends_list = gtk4::StringList::new(&["Never", "After a number of runs"]);
        let ends_row = adw::ComboRow::builder()
            .title("Ends")
            .model(&ends_list)
            .selected(match seed.existing.as_ref().map(|e| e.ends) {
                Some(Ends::AfterRuns { .. }) => 1,
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

        let obj_w = self.obj().downgrade();
        let dlg_w = dialog.downgrade();
        let save_w = save_btn.downgrade();
        let clear_composer = seed.clear_composer;
        let on_saved = seed.on_saved.clone();
        save_btn.connect_clicked(move |btn| {
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

            let target = if is_new_thread {
                Target::NewThread {
                    folder: folder.clone(),
                    driver,
                    profile_id: None,
                    mode,
                    model: None,
                    worktree: use_wt,
                }
            } else if let Some(tid) = seed.thread_id.as_ref() {
                Target::ExistingThread {
                    thread_id: tid.clone(),
                }
            } else {
                error_label.set_text("Choose a target thread or folder.");
                error_label.set_visible(true);
                return;
            };

            let rule = match kind_row.selected() {
                0 => {
                    let Some((y, m, d)) = parse_ymd(&date_row.text()) else {
                        error_label.set_text("Enter a valid date (YYYY-MM-DD).");
                        error_label.set_visible(true);
                        return;
                    };
                    let Some((hh, mm)) = parse_hhmm(&time_row.text()) else {
                        error_label.set_text("Enter a valid time (HH:MM).");
                        error_label.set_visible(true);
                        return;
                    };
                    match GlibLocalClock.resolve_local(y, m, d, hh, mm) {
                        agent_core::schedule::ResolveLocal::Exact(at)
                        | agent_core::schedule::ResolveLocal::SkippedSpringForward(at)
                        | agent_core::schedule::ResolveLocal::AmbiguousFallBack {
                            first: at, ..
                        } => Rule::Once { at },
                        agent_core::schedule::ResolveLocal::Invalid => {
                            error_label.set_text("That date and time is invalid.");
                            error_label.set_visible(true);
                            return;
                        }
                    }
                }
                1 => {
                    let Some((hh, mm)) = parse_hhmm(&time_row.text()) else {
                        error_label.set_text("Enter a valid time (HH:MM).");
                        error_label.set_visible(true);
                        return;
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
            let ends = if kind_row.selected() != 0 && ends_row.selected() == 1 {
                Ends::AfterRuns {
                    max: max_runs_adj.value() as u32,
                }
            } else {
                Ends::Never
            };

            let err_lbl = error_label.clone();
            let clear_tid = clear_composer.then(|| seed.thread_id.clone()).flatten();
            let edit_id = seed.editing_id.clone();
            let had_images = seed.has_images;
            let on_saved = on_saved.clone();
            let save_w = save_w.clone();
            btn.set_sensitive(false);
            let folder_check = is_new_thread.then_some(folder);
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
                let validated = match validate_form(
                    &FormInput {
                        prompt,
                        has_images: had_images,
                        target,
                        in_git_repo,
                        rule,
                        missed_policy,
                        ends,
                        now,
                    },
                    &GlibLocalClock,
                ) {
                    Ok(v) => v,
                    Err(msg) => {
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
                        store.update_schedule(
                            &id,
                            &validated.prompt,
                            &validated.rule,
                            validated.missed_policy,
                            validated.ends,
                            validated.next_due,
                        )?;
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
                        if let Some(tid) = clear_tid {
                            window.imp().with_chat(&tid, |chat| {
                                if let Some(view) = &chat.view {
                                    view.clear_composer_draft();
                                }
                            });
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

        imp.open_schedules_for_thread(&thread);
        let ctx = glib::MainContext::default();
        assert!(pump_until(&ctx, 5, || window.visible_dialog().is_some()));
        if let Some(dlg) = window.visible_dialog() {
            dlg.force_close();
        }
        crate::testutil::settle(&ctx);

        store.delete_schedule(&sid).expect("cleanup schedule");
        store.delete_thread(&thread).expect("cleanup thread");
        imp.reload_summaries();
    }
}
