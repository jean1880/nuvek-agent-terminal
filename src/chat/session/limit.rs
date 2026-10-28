//! Continuing a thread once its agent's usage limit has reset.
//!
//! The window decides when (`agent_core::limit_wait`); the session sends the prompt. It is the
//! app's, posted as a notice like a sub-agent wake, and resumes the same native session: a
//! process that exited (or was suspended) meanwhile is started again for it. Only a root thread
//! continues, and never one the user stopped. A handoff still waiting for a prompt (the limit hit
//! the turn that carried it) rides on the continuation, and is spent as a user prompt's would be;
//! the continuation itself is never stored as the user's message.

use std::rc::Rc;

use agent_core::event::{Envelope, Event};
use agent_core::handoff_budget::provider_message_with_handoff;
use tracing::info;

use super::delegated::TurnPurpose;
use super::{ChatSession, Inner};

/// What the agent is sent once its usage limit has reset.
pub(crate) const CONTINUE_PROMPT: &str =
    "[agent-terminal] Your usage limit has reset. Continue where you left off.";

/// What the thread shows when it continues.
const CONTINUE_NOTICE: &str = "Usage limit reset: the agent was asked to continue.";

/// What became of [`ChatSession::continue_after_limit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinueOutcome {
    /// The prompt was sent, and the notice posted.
    Started,
    /// Not now (a turn, a prompt, an approval or a switch is in the way): ask again later.
    Busy,
    /// Never: a delegated worker, or a root the user stopped and has not spoken to since.
    Refused,
}

impl ChatSession {
    /// Sends [`CONTINUE_PROMPT`] if nothing is in the way, restarting the agent's process (the
    /// same native session) when it is gone.
    pub(crate) fn continue_after_limit(&self) -> ContinueOutcome {
        self.inner.continue_after_limit()
    }

    /// Whether this session is a delegated worker (never a thread that continues on its own).
    pub(crate) fn is_delegated_child(&self) -> bool {
        self.inner.child.is_some()
    }
}

impl Inner {
    fn continue_after_limit(self: &Rc<Self>) -> ContinueOutcome {
        if self.child.is_some() || self.wakes_held.get() {
            return ContinueOutcome::Refused;
        }
        // A delegation root may need a rebind first: the hook starts it and says no.
        if self.turn_blocker(TurnPurpose::LimitContinue).is_some()
            || !self.root_prompt_allowed()
            || !self.apply_retired()
        {
            return ContinueOutcome::Busy;
        }
        info!("continuing the thread: its agent's usage limit has reset");
        // The turn the limit refused may have carried a handoff: the continuation carries it now,
        // through the same path (in flight, spent when its turn answers) as a user prompt.
        let handoff = if self.handoff_in_flight.get() {
            None
        } else {
            self.pending_handoff.borrow().clone()
        };
        let sent = handoff.as_ref().map_or_else(
            || CONTINUE_PROMPT.to_owned(),
            |h| provider_message_with_handoff(&h.summary, &h.fence, CONTINUE_PROMPT),
        );
        let carried = handoff.is_some();
        if !self.dispatch_in_flight(&sent, handoff) {
            return ContinueOutcome::Busy;
        }
        // As a wake: started only once the line went to a process that can take it, or is held
        // by a start still waiting on its checks. Otherwise nothing ran: the handoff still waits.
        if !self.wrote_now() && self.starting.borrow().is_none() {
            self.prompt_in_flight.set(false);
            if carried {
                self.handoff_in_flight.set(false);
            }
            self.state.borrow_mut().running_turn = false;
            self.pre_init_prompts
                .borrow_mut()
                .retain(|kept| kept.text != sent);
            return ContinueOutcome::Busy;
        }
        // It continues where it is: a "Continue in" another agent no longer applies.
        self.continue_on_switch_to.set(None);
        self.work_epoch.set(self.work_epoch.get().wrapping_add(1));
        self.emit(Envelope::new(Event::Notice {
            text: CONTINUE_NOTICE.to_owned(),
        }));
        ContinueOutcome::Started
    }
}
