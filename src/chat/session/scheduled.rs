//! Running a scheduled or repeating prompt on an idle thread (`crates/agent-core/src/schedule.rs`).
//!
//! Scheduled runs never call `user_spoke()`, never queue behind user input, never run in
//! `Mode::Bypass` (including forced-permissive hookless `agy`), and allow `Driver::Agy` only in an
//! isolated worktree. Unlike a wake turn, a scheduled run restarts an idle or suspended process and
//! carries any pending cross-agent handoff, storing a `scheduled` user-message item marked via
//! [`agent_core::schedule::scheduled_input`].

use std::rc::Rc;

use agent_core::adapter::{Driver, Mode};
use agent_core::event::{Envelope, Event, ItemKind, StreamKind};
use agent_core::handoff_budget::provider_message_with_handoff;
use gtk4::glib;
use tracing::{info, warn};

use super::delegated::TurnPurpose;
use super::{ChatSession, Inner};

/// Outcome of attempting to start a scheduled prompt turn via [`ChatSession::send_scheduled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "wired by F6 scheduler slice"))]
pub(crate) enum ScheduledOutcome {
    /// The scheduled turn started and its marked user message was recorded.
    Started,
    /// Temporarily blocked (running turn, open approval, usage-limit wait, etc.): defer within grace.
    Busy(&'static str),
    /// Disallowed by invariant (empty prompt, delegated child, remote thread, Bypass, hookless/non-worktree agy, stopped thread).
    Refused(&'static str),
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired by F6 scheduler slice"))]
impl ChatSession {
    /// Dispatches a scheduled prompt if permitted and the session is idle, restarting a stopped or
    /// suspended local process when needed.
    pub(crate) fn send_scheduled(
        &self,
        schedule_id: &str,
        due: i64,
        prompt: &str,
        in_worktree: bool,
    ) -> ScheduledOutcome {
        self.inner
            .send_scheduled(schedule_id, due, prompt, in_worktree)
    }

    /// First blocker keeping a scheduled prompt from starting right now, if any.
    pub(crate) fn scheduled_busy_reason(&self) -> Option<&'static str> {
        self.inner.turn_blocker(TurnPurpose::Scheduled)
    }

    /// Whether the user stopped this thread and has not spoken since.
    pub(crate) fn is_wakes_held(&self) -> bool {
        self.inner.wakes_held.get()
    }

    /// Whether the thread is currently waiting for a usage-limit reset.
    pub(crate) fn is_limit_waiting(&self) -> bool {
        self.inner.limit_waiting.get()
    }

    /// Whether any approval or permission request is open on this session.
    pub(crate) fn has_open_approval(&self) -> bool {
        !self.inner.open_approvals.borrow().is_empty()
            || !self.inner.codex_permissions.borrow().is_empty()
            || !self.inner.held_permissions.borrow().is_empty()
    }

    /// Returns and clears every schedule id interrupted by Stop since the last poll.
    pub(crate) fn take_stopped_schedules(&self) -> Vec<String> {
        std::mem::take(&mut *self.inner.stopped_schedules.borrow_mut())
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired by F6 scheduler slice"))]
impl Inner {
    fn send_scheduled(
        self: &Rc<Self>,
        schedule_id: &str,
        due: i64,
        prompt: &str,
        in_worktree: bool,
    ) -> ScheduledOutcome {
        if prompt.trim().is_empty() {
            return ScheduledOutcome::Refused("the scheduled prompt is empty");
        }
        if self.child.is_some() {
            return ScheduledOutcome::Refused("a delegated worker cannot run a scheduled prompt");
        }
        if self.remote.is_some() {
            return ScheduledOutcome::Refused("a remote thread cannot run a scheduled prompt");
        }
        if self.wakes_held.get() {
            return ScheduledOutcome::Refused("the thread was stopped");
        }
        if self.state.borrow().mode == Mode::Bypass
            || self.wanted_mode.get() == Mode::Bypass
            || self.open.borrow().forced_permissive
            || self.ask_unavailable()
        {
            return ScheduledOutcome::Refused(
                "scheduled prompts do not run without permission checks",
            );
        }
        if self.adapter.borrow().driver() == Driver::Agy && !in_worktree {
            return ScheduledOutcome::Refused("agy scheduled prompts require an isolated worktree");
        }
        if let Some(why) = self.turn_blocker(TurnPurpose::Scheduled) {
            return ScheduledOutcome::Busy(why);
        }
        if !self.root_prompt_allowed() || !self.apply_retired() {
            return ScheduledOutcome::Busy("the session is busy");
        }

        info!(schedule = schedule_id, "starting scheduled prompt turn");
        let handoff = if Self::is_slash_command(prompt) || self.handoff_in_flight.get() {
            None
        } else {
            self.pending_handoff.borrow().clone()
        };
        let sent = handoff.as_ref().map_or_else(
            || prompt.to_owned(),
            |h| provider_message_with_handoff(&h.summary, &h.fence, prompt),
        );
        let carried = handoff.is_some();
        let kept_before = self.pre_init_prompts.borrow().len();
        if !self.dispatch_in_flight(&sent, handoff, &[]) {
            return ScheduledOutcome::Busy("the agent could not accept the prompt");
        }
        if !self.wrote_now() && self.starting.borrow().is_none() {
            self.prompt_in_flight.set(false);
            if carried {
                self.handoff_in_flight.set(false);
            }
            self.state.borrow_mut().running_turn = false;
            self.pre_init_prompts.borrow_mut().truncate(kept_before);
            return ScheduledOutcome::Busy("the agent process is not ready");
        }
        if let Some(pending) = self.starting.borrow_mut().as_mut() {
            pending.has_scheduled = true;
        }

        // Scheduled runs are autonomous: never call `user_spoke()` or reset `unattended_runs` or
        // `auto_wakes`. Cancel any deferred "Continue in" target now that this turn moved forward,
        // while preserving any unconsumed `stopped_schedules` records for the scheduler.
        self.continue_on_switch_to.set(None);
        self.work_epoch.set(self.work_epoch.get().wrapping_add(1));
        *self.active_schedule.borrow_mut() = Some(schedule_id.to_owned());

        let pt = self.provider_thread.borrow().clone();
        let provider = (!pt.is_empty()).then_some(pt.as_str());
        let item = match self.store.append_scheduled_user_input(
            &self.thread,
            provider,
            prompt,
            schedule_id,
            due,
        ) {
            Ok(item) => item,
            Err(e) => {
                warn!(error = %e, "could not persist the scheduled prompt");
                glib::uuid_string_random().to_string()
            }
        };
        let input = agent_core::schedule::scheduled_input(schedule_id, due);
        self.deliver(
            &Envelope::new(Event::ItemStarted {
                kind: ItemKind::UserMessage,
                title: String::new(),
                input: Some(input),
                parent: None,
            })
            .item(item.clone()),
        );
        self.deliver(
            &Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: agent_core::redact::redact(prompt),
            })
            .item(item),
        );
        ScheduledOutcome::Started
    }
}
