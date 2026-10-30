//! Turns nobody typed around a usage limit and a switch: sub-agent news waits while the thread
//! waits for its limit, the continuation after the limit carries a pending handoff (as the app's
//! notice, never the user's message), and "Continue in" another agent continues there. Real
//! Claude adapters over scripted processes; nothing reaches a provider.

use super::super::scripted_tests::minimal_env;
use super::*;
use crate::chat::session::{ContinueOutcome, WakeOutcome};
use crate::delegation::seam::SessionOptions;
use agent_core::delegation::ProcessGeneration;

/// Claude's protocol under another agent's name: a switch's "other agent" that is still a
/// scripted Claude process.
struct Relabelled {
    inner: agent_core::claude::ClaudeAdapter,
    driver: Driver,
}

impl Adapter for Relabelled {
    fn driver(&self) -> Driver {
        self.driver
    }
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    fn argv(&self, session: &OpenSession) -> Vec<String> {
        self.inner.argv(session)
    }
    fn handshake(&mut self) -> Vec<String> {
        self.inner.handshake()
    }
    fn handshake_answered(&self) -> bool {
        self.inner.handshake_answered()
    }
    fn prompt_starts_turn(&self, text: &str) -> bool {
        self.inner.prompt_starts_turn(text)
    }
    fn discard_queued_prompts(&mut self) {
        self.inner.discard_queued_prompts();
    }
    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        self.inner.encode(command)
    }
    fn feed(&mut self, line: &str) -> Vec<Envelope> {
        self.inner.feed(line)
    }
    fn feed_stderr(&mut self, line: &str) -> Vec<Envelope> {
        self.inner.feed_stderr(line)
    }
    fn feed_side(&mut self, id: &str, stdout: &str, ok: bool) -> Vec<Envelope> {
        self.inner.feed_side(id, stdout, ok)
    }
    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
        self.inner.on_exit(code)
    }
    fn pending_request_ids(&self) -> Vec<String> {
        self.inner.pending_request_ids()
    }
}

struct Thread {
    _dir: tempfile::TempDir,
    /// Where the thread's first agent logs what it is sent.
    old: std::path::PathBuf,
    /// Where an agent a switch starts logs what it is sent.
    new: std::path::PathBuf,
    store: Rc<Store>,
    thread: ThreadId,
    session: Rc<ChatSession>,
    seen: Seen,
}

impl Thread {
    /// A root thread on an answering Claude, whose switches start another answering Claude
    /// under the asked agent's name, settled after its prompt-less first turn.
    fn new(ctx: &glib::MainContext) -> Self {
        let dir = tempfile::tempdir().expect("dir");
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        std::fs::create_dir(&old).expect("old");
        std::fs::create_dir(&new).expect("new");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &old.to_string_lossy());
        let (sink, seen) = make_sink();
        let session = ChatSession::with_options(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            super::answering_claude(&old),
            SessionStore::from(store.clone()),
            thread.clone(),
            sink,
            Err(None),
            minimal_env(&old),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: None,
                child: None,
            },
        );
        let launch = super::answering_claude(&new);
        let env = minimal_env(&new);
        session.set_adapter_factory(Rc::new(move |driver, _| {
            Ok(AgentLaunch {
                profile: None,
                adapter: Box::new(Relabelled {
                    inner: agent_core::claude::ClaudeAdapter::new(),
                    driver,
                }),
                program: launch.program.clone(),
                extra_args: launch.extra_args.clone(),
                default_model: None,
                default_effort: None,
                env: env.clone(),
                approval: Err(None),
            })
        }));
        let t = Self {
            _dir: dir,
            old,
            new,
            store,
            thread,
            session,
            seen,
        };
        assert!(pump_until(ctx, 10, || t.idle()
            && has(&t.seen, |e| matches!(e, Event::TurnCompleted { .. }))));
        t
    }

    fn idle(&self) -> bool {
        let status = self.session.status();
        status.alive && !status.running_turn
    }

    fn sent(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("prompts.log")).unwrap_or_default()
    }

    fn sent_old(&self) -> String {
        Self::sent(&self.old)
    }

    fn sent_new(&self) -> String {
        Self::sent(&self.new)
    }

    /// The user messages the thread stored.
    fn user_messages(&self) -> Vec<String> {
        self.store
            .transcript_messages(&self.thread)
            .expect("messages")
            .into_iter()
            .filter(|m| m.role == "user")
            .map(|m| m.text)
            .collect()
    }

    fn notices(&self, prefix: &str) -> usize {
        self.seen
            .borrow()
            .iter()
            .filter(|e| matches!(&e.event, Event::Notice { text } if text.starts_with(prefix)))
            .count()
    }
}

/// While the thread waits for its usage limit, sub-agent news does not wake it (the limit would
/// refuse that turn), yet the continuation the wait is for still starts; once that turn has
/// completed the agent works again, and the news goes through.
#[test]
fn a_limit_wait_holds_wakes_but_never_the_continuation() {
    in_loop(|ctx| {
        let t = Thread::new(ctx);
        t.session.send_prompt("first");
        assert!(pump_until(ctx, 10, || t.idle()
            && t.session.inner.has_conversation.get()));

        t.session.set_limit_wait(true);
        assert_eq!(
            t.session.wake_blocker(),
            Some("the thread waits for its usage limit")
        );
        assert_eq!(t.session.wake_with("news"), WakeOutcome::Busy);
        assert_eq!(t.session.continue_after_limit(), ContinueOutcome::Started);
        assert!(pump_until(ctx, 10, || t
            .sent_old()
            .contains("Your usage limit has reset")));
        assert!(!t.sent_old().contains("news"));
        assert!(pump_until(ctx, 10, || t.idle()));

        // The continuation's turn completed: the session no longer waits, whatever the window does.
        assert_eq!(t.session.wake_with("news"), WakeOutcome::Started);
        assert!(pump_until(ctx, 10, || t.sent_old().contains("news")));
        t.session.shutdown(false);
    });
}

/// A thread waiting for its usage limit that switches to another agent waits for nothing any
/// more: the next sub-agent news wakes the new agent.
#[test]
fn a_switch_to_another_agent_ends_the_limit_wait_for_wakes() {
    in_loop(|ctx| {
        let t = Thread::new(ctx);
        t.session.send_prompt("first");
        assert!(pump_until(ctx, 10, || t.idle()));
        t.session.set_limit_wait(true);
        assert_eq!(t.session.wake_with("held news"), WakeOutcome::Busy);

        t.session.switch(Driver::Codex, None, None);
        assert!(pump_until(ctx, 10, || t.session.status().driver
            == Driver::Codex
            && t.idle()
            && t.session.inner.rollback.borrow().is_none()));
        // The new agent's first prompt carries the handoff; the user sends it.
        t.session.send_prompt("go on");
        assert!(pump_until(ctx, 10, || t.idle()
            && t.session.inner.pending_handoff.borrow().is_none()));
        assert_eq!(t.session.wake_with("news after"), WakeOutcome::Started);
        assert!(pump_until(ctx, 10, || t.sent_new().contains("news after")));
        t.session.shutdown(false);
    });
}

/// The turn the limit refused carried a handoff (the thread had just switched agent): the
/// continuation carries it, through the same path as a user prompt (in flight, then spent once
/// its turn answers), and is shown as the app's notice, never stored as the user's message.
#[test]
fn the_continuation_after_a_limit_carries_and_spends_a_pending_handoff() {
    in_loop(|ctx| {
        let t = Thread::new(ctx);
        let fence = "0123456789abcdef0123456789abcdef";
        t.session.inner.arm_handoff(Handoff {
            summary: "EARLIER-CONTEXT from the other agent".into(),
            fence: fence.into(),
            carried: 1,
            total: 1,
        });
        assert_eq!(
            t.session.wake_blocker(),
            Some("a handoff waits for a prompt"),
            "news still waits for a prompt that carries it"
        );
        assert_eq!(t.session.continue_after_limit(), ContinueOutcome::Started);
        assert!(pump_until(ctx, 10, || t
            .sent_old()
            .contains("Your usage limit has reset")));
        let sent = t.sent_old();
        assert!(sent.contains("EARLIER-CONTEXT"), "{sent}");
        assert!(sent.contains(&format!("User message (HANDOFF-{fence})")));
        assert!(pump_until(ctx, 10, || t.idle()
            && t.session.inner.pending_handoff.borrow().is_none()));
        assert!(has(&t.seen, |e| matches!(
            e,
            Event::HandoffSeeded { carried: 0, .. }
        )));
        assert_eq!(t.notices("Usage limit reset"), 1);
        assert!(
            t.user_messages()
                .iter()
                .all(|m| !m.contains("usage limit has reset")),
            "the continuation is the app's, not the user's: {:?}",
            t.user_messages()
        );
        t.session.shutdown(false);
    });
}

/// "Continue in" another agent after the limit: the switch hands the thread over and the new
/// agent is asked to continue at once, with the handoff, though no turn ran when it was asked.
#[test]
fn continue_in_another_agent_starts_a_continuation_there() {
    in_loop(|ctx| {
        let t = Thread::new(ctx);
        t.session.send_prompt("REQUEST: port the parser");
        assert!(pump_until(ctx, 10, || t.idle()));

        assert!(t
            .session
            .continue_after_switch_to(Driver::Codex, t.session.work_epoch()));
        t.session.switch(Driver::Codex, None, None);
        assert!(pump_until(ctx, 10, || t
            .sent_new()
            .contains("Continue the unfinished task")));
        let sent = t.sent_new();
        assert!(
            sent.contains("REQUEST: port the parser"),
            "with the handoff: {sent}"
        );
        assert!(pump_until(ctx, 10, || t.idle()));
        assert_eq!(
            t.sent_new().matches("Continue the unfinished task").count(),
            1
        );

        // An ordinary idle switch afterwards continues nothing.
        t.session.switch(Driver::Claude, None, None);
        assert!(pump_until(ctx, 10, || t.session.status().driver
            == Driver::Claude
            && t.idle()));
        assert_eq!(
            t.sent_new().matches("Continue the unfinished task").count(),
            1
        );
        t.session.shutdown(false);
    });
}

/// A "Continue in" that was not taken never continues a later switch: its question was
/// cancelled, or the thread's own agent continued after its limit and completed, or the user
/// wrote first (a message sent, queued or steered); and taking one back never takes a later one.
#[test]
fn a_continue_in_not_taken_never_continues_a_later_switch() {
    in_loop(|ctx| {
        let t = Thread::new(ctx);
        t.session.send_prompt("first");
        assert!(pump_until(ctx, 10, || t.idle()));

        // Cancelled: taken back.
        assert!(t
            .session
            .continue_after_switch_to(Driver::Agy, t.session.work_epoch()));
        t.session.forget_continue_after_switch(None);
        assert_eq!(t.session.continue_after_switch_armed(), None);

        // Only the one named is taken back.
        assert!(t
            .session
            .continue_after_switch_to(Driver::Agy, t.session.work_epoch()));
        let earlier = t.session.continue_after_switch_armed();
        assert!(t
            .session
            .continue_after_switch_to(Driver::Agy, t.session.work_epoch()));
        t.session.forget_continue_after_switch(earlier);
        assert!(t.session.continue_after_switch_armed().is_some());

        // The user writes first, however the message goes.
        t.session.queue_prompt("queued");
        assert_eq!(t.session.continue_after_switch_armed(), None);
        assert!(t
            .session
            .continue_after_switch_to(Driver::Agy, t.session.work_epoch()));
        t.session.steer_prompt("steered");
        assert_eq!(t.session.continue_after_switch_armed(), None);
        // The queued message goes first, then this one.
        assert!(pump_until(ctx, 10, || t.idle()
            && t.sent_old().contains("queued")
            && t.sent_old().contains("steered")));

        // Left waiting, then the thread's own agent continues after its limit and completes.
        assert!(t
            .session
            .continue_after_switch_to(Driver::Agy, t.session.work_epoch()));
        t.session.set_limit_wait(true);
        assert_eq!(t.session.continue_after_limit(), ContinueOutcome::Started);
        assert!(pump_until(ctx, 10, || t.idle()
            && t.sent_old().contains("Your usage limit has reset")));

        // A later idle switch continues nothing.
        t.session.switch(Driver::Agy, None, None);
        assert!(pump_until(ctx, 10, || t.session.status().driver
            == Driver::Agy
            && t.session.status().alive
            && !t.session.inner.switch_reading.get()));
        // Long enough for a continuation to have gone, had one been owed.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(300) {
            ctx.iteration(false);
        }
        assert!(!t.sent_new().contains("Continue the unfinished task"));
        assert!(!t.sent_old().contains("Continue the unfinished task"));
        t.session.shutdown(false);
    });
}

/// "Continue in" asked while the thread was stopped by its limit, confirmed only after its
/// own agent continued after the limit and finished the work (the agy question stayed open
/// meanwhile): nothing is armed, and the switch then continues nothing.
#[test]
fn a_continue_in_confirmed_after_the_work_finished_continues_nothing() {
    in_loop(|ctx| {
        let t = Thread::new(ctx);
        t.session.send_prompt("first");
        assert!(pump_until(ctx, 10, || t.idle()));
        // The question opens: the work as it stands now is what would be continued.
        let asked = t.session.work_epoch();

        // Meanwhile the limit resets and the thread's own agent finishes the work.
        t.session.set_limit_wait(true);
        assert_eq!(t.session.continue_after_limit(), ContinueOutcome::Started);
        assert!(pump_until(ctx, 10, || t.idle()
            && t.sent_old().contains("Your usage limit has reset")));

        // "Continue Anyway": nothing is owed any more.
        assert!(!t.session.continue_after_switch_to(Driver::Agy, asked));
        assert_eq!(t.session.continue_after_switch_armed(), None);
        t.session.switch(Driver::Agy, None, None);
        assert!(pump_until(ctx, 10, || t.session.status().driver
            == Driver::Agy
            && t.session.status().alive
            && !t.session.inner.switch_reading.get()));
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(300) {
            ctx.iteration(false);
        }
        assert!(!t.sent_new().contains("Continue the unfinished task"));
        assert!(!t.sent_old().contains("Continue the unfinished task"));
        t.session.shutdown(false);
    });
}
