//! A delegated worker's idle process is paused only for its supervisor: through the same checks
//! as a thread's idle suspension, with the supervisor's consent, silently, and resumed on the
//! same native session.

use super::*;
use crate::agent_proc::ExitWatch;
use crate::delegation::seam::{
    ApprovalGate, ChildPolicy, ChildPublish, DelegatedInput, DelegatedOutcome, DelegationHooks,
    GateVerdict, InputSource, LaunchBinding, SessionOptions, TurnBoundary,
};
use agent_core::delegation::mailbox::DeliveryPlan;
use agent_core::delegation::{AuthorityEpoch, OperationId, WorkerId};
use serde_json::Value;

/// A gate that leaves every call to the session's own policy.
struct Defer;

impl ApprovalGate for Defer {
    fn judge(&self, _tool: &str, _input: &Value, _cwd: Option<&str>) -> GateVerdict {
        GateVerdict::Defer
    }

    fn rule_may_apply(&self, _action: &crate::always_allow::Action, _cwd: Option<&str>) -> bool {
        false
    }
}

/// A publication boundary that passes everything through.
struct Pass;

impl ChildPublish for Pass {
    fn publish(&self, _generation: ProcessGeneration, env: Envelope) -> Option<Envelope> {
        Some(env)
    }

    fn persist(&self, _thread: &str, _provider: Option<&str>, _env: &Envelope) {}

    fn cancel_generation(&self, _generation: ProcessGeneration) {}
}

/// Hooks standing in for the supervisor: they consent to a pause when told to, and count how
/// often they were asked.
#[derive(Default)]
struct Consent {
    allow: Cell<bool>,
    asks: Cell<usize>,
}

impl DelegationHooks for Consent {
    fn binding(&self, _driver: Driver, _generation: ProcessGeneration) -> Option<LaunchBinding> {
        None
    }

    fn process_started(&self, _generation: ProcessGeneration, _pid: Option<i32>, _w: ExitWatch) {}

    fn process_stopping(&self, _generation: ProcessGeneration) {}

    fn suspending(&self, _generation: ProcessGeneration) -> bool {
        self.asks.set(self.asks.get() + 1);
        self.allow.get()
    }

    fn line(&self, _generation: ProcessGeneration, _line: &str, _stderr: bool) {}

    fn observed(&self, _generation: ProcessGeneration, _env: &Envelope) {}

    fn turn(&self, _generation: ProcessGeneration, _boundary: TurnBoundary<'_>) {}

    fn mode_changed(&self, _effective: Mode) {}
}

fn task(text: &str) -> DelegatedInput {
    DelegatedInput {
        operation: OperationId::from_suffix("operation1").expect("operation id"),
        epoch: AuthorityEpoch(1),
        source: InputSource::Task,
        from: WorkerId::from_suffix("parent001").expect("worker id"),
        from_label: "Claude (root)".into(),
        text: text.into(),
        plan: DeliveryPlan::StartTurn,
        tag: "op-tag".into(),
    }
}

/// Each way a worker's session can hold something only its process keeps, set and undone.
type Hold = fn(&Inner, bool);

const HOLDS: &[(&str, Hold)] = &[
    ("a turn is running", |i, on| {
        i.state.borrow_mut().running_turn = on
    }),
    ("Codex session grants", |i, on| {
        let mut grants = i.codex_grants.borrow_mut();
        if on {
            grants.push("Network access".into());
        } else {
            grants.clear();
        }
    }),
    ("active provider workers", |i, on| {
        i.active_workers.set(usize::from(on))
    }),
    ("a prompt in flight", |i, on| i.prompt_in_flight.set(on)),
    ("a handoff in flight", |i, on| i.handoff_in_flight.set(on)),
    ("rebinding", |i, on| i.rebinding.set(on)),
    ("a deferred mode", |i, on| {
        i.deferred_mode.set(on.then_some(Mode::Plan))
    }),
    ("an open approval", |i, on| {
        let mut open = i.open_approvals.borrow_mut();
        if on {
            open.push("request-1".into());
        } else {
            open.clear();
        }
    }),
    ("an open permission request", |i, on| {
        let mut held = i.held_permissions.borrow_mut();
        if on {
            held.insert("request-2".into());
        } else {
            held.clear();
        }
    }),
    ("a transcript read", |i, on| {
        i.agy_reading.set(on.then_some(1))
    }),
];

/// Nothing a worker's process alone holds is ever lost: each hold keeps it running (the
/// supervisor is not even asked), a thread's idle sweep never stops it, and with its
/// supervisor's consent an idle one pauses silently and its restart resumes the same session.
#[test]
fn a_worker_pauses_only_when_idle_with_its_supervisors_consent_and_resumes_its_session() {
    in_loop(|ctx| {
        let dir = tempfile::tempdir().expect("dir");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &dir.path().to_string_lossy());
        let (sink, seen) = make_sink();
        let hooks = Rc::new(Consent::default());
        let mut open = super::idle_claude(dir.path());
        open.mode = Mode::Plan;
        let session = ChatSession::with_options(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            open,
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            super::super::scripted_tests::minimal_env(dir.path()),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: Some(ChildPolicy {
                    gate: Rc::new(Defer),
                    publish: Rc::new(Pass),
                    mode: Mode::Plan,
                }),
            },
        );
        session.set_delegation_worker(WorkerId::from_suffix("child0001").expect("worker id"));
        session.reload_session();
        assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
            e,
            Event::TurnCompleted { .. }
        ))));
        assert_eq!(
            session.inner.suspend_blocker(true),
            Some("no conversation yet to resume"),
            "the prompt-less init turn names no transcript"
        );
        assert!(matches!(
            session.deliver_delegated(task("do the task")),
            DelegatedOutcome::TurnStarted
        ));
        assert!(pump_until(ctx, 10, || session.inner.has_conversation.get()
            && !session.status().running_turn));

        assert_eq!(session.inner.suspend_blocker(true), None, "idle now");
        assert_eq!(
            session.inner.suspend_blocker(false),
            Some("a delegated worker"),
            "a thread's idle sweep never stops a worker"
        );
        assert!(!session.suspend_if_idle());
        assert_eq!(hooks.asks.get(), 0);

        // A worker with no session id to resume is never stopped.
        let native = session.inner.state.borrow_mut().native_id.take();
        assert_eq!(
            session.inner.suspend_blocker(true),
            Some("no session id to resume")
        );
        assert!(!session.suspend_child_if_idle());
        session.inner.state.borrow_mut().native_id = native;
        let provisional = Some(("provider".to_owned(), "native".to_owned()));
        *session.inner.provisional_native.borrow_mut() = provisional;
        assert_eq!(
            session.inner.suspend_blocker(true),
            Some("a Codex thread not yet resumable")
        );
        assert!(!session.suspend_child_if_idle());
        *session.inner.provisional_native.borrow_mut() = None;
        for (why, hold) in HOLDS {
            hold(&session.inner, true);
            assert_eq!(session.inner.suspend_blocker(true), Some(*why));
            assert!(!session.suspend_child_if_idle(), "{why}");
            hold(&session.inner, false);
        }
        assert_eq!(
            hooks.asks.get(),
            0,
            "the supervisor is asked only once all is idle"
        );
        assert!(session.status().alive, "refusals stop nothing");

        assert!(!session.suspend_child_if_idle(), "the supervisor refused");
        assert_eq!(hooks.asks.get(), 1);
        assert!(session.status().alive);

        hooks.allow.set(true);
        let before = seen.borrow().len();
        assert!(
            session.suspend_child_if_idle(),
            "idle, and the supervisor agreed"
        );
        assert!(!session.status().alive);
        assert!(session.resumes_from_suspension());
        pump_until(ctx, 1, || false);
        assert_eq!(seen.borrow().len(), before, "nothing reached the view");

        // Its supervisor restarts it as a rebind does: the same native session resumes.
        session.rebind_start();
        let args = dir.path().join("args.log");
        assert!(pump_until(ctx, 10, || session.status().alive
            && std::fs::read_to_string(&args)
                .is_ok_and(|a| a.lines().count() == 2)));
        let launches = std::fs::read_to_string(&args).expect("args");
        let launches: Vec<&str> = launches.lines().collect();
        assert_eq!(launches.len(), 2, "{launches:?}");
        assert!(!launches[0].contains("--resume"), "{launches:?}");
        assert!(launches[1].contains("--resume=s1"), "{launches:?}");
    });
}

/// The supervisor's path pauses only its own workers, never a thread's agent.
#[test]
fn only_a_delegated_worker_is_paused_for_its_supervisor() {
    in_loop(|_| {
        let dir = tempfile::tempdir().expect("dir");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &dir.path().to_string_lossy());
        let (session, _seen) = harness(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            super::idle_claude(dir.path()),
            &store,
            &thread,
        );
        assert_eq!(
            session.inner.suspend_blocker(true),
            Some("not a delegated worker")
        );
        assert!(!session.suspend_child_if_idle());
    });
}
