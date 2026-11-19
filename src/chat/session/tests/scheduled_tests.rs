//! Session-layer tests for scheduled and repeating prompts (`src/chat/session/scheduled.rs`).

use super::*;
use agent_core::schedule::{parse_scheduled_input, Ends, MissedPolicy, Rule, Target};
use agent_kit::store::{NewSchedule, ScheduleRunUpdate};

fn schedule_for_thread(store: &Store, thread_id: &str) -> String {
    store
        .create_schedule(&NewSchedule {
            target: Target::ExistingThread {
                thread_id: thread_id.to_owned(),
            },
            prompt: "Run checks".into(),
            rule: Rule::Interval {
                every_secs: 3600,
                anchor: 1_000_000,
            },
            missed_policy: MissedPolicy::Skip,
            ends: Ends::Never,
            next_due: Some(1_000_000),
        })
        .expect("create schedule")
}

fn open_idle(ctx: &glib::MainContext, dir: &Path) -> (Rc<Store>, ThreadId, Rc<ChatSession>, Seen) {
    let store = Rc::new(Store::open_in_memory().expect("store"));
    let thread = fresh(&store, &dir.to_string_lossy());
    let (session, seen) = harness(
        Box::new(agent_core::claude::ClaudeAdapter::new()),
        answering_claude(dir),
        &store,
        &thread,
    );
    assert!(pump_until(ctx, 10, || {
        let st = session.status();
        st.alive && !st.running_turn && has(&seen, |e| matches!(e, Event::TurnCompleted { .. }))
    }));
    (store, thread, session, seen)
}

#[test]
fn scheduled_prompt_dispatches_records_marker_and_does_not_call_user_spoke() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let (store, thread, session, seen) = open_idle(ctx, tmp.path());
        let sid = schedule_for_thread(&store, &thread);
        store
            .record_schedule_run(&ScheduleRunUpdate {
                id: sid.clone(),
                next_due: Some(1_003_600),
                last_due: Some(1_000_000),
                last_outcome: None,
                increment_run_count: true,
                increment_unattended: true,
                paused_reason: None,
            })
            .expect("record prior run");
        session.inner.auto_wakes.set(3);
        let turns_before = seen
            .borrow()
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count();

        let outcome = session.send_scheduled(&sid, 1_003_600, "Check CI status", false);
        assert_eq!(outcome, ScheduledOutcome::Started);
        assert!(pump_until(ctx, 10, || {
            seen.borrow()
                .iter()
                .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
                .count()
                > turns_before
        }));

        // Scheduled marker emitted and stored on UserMessage ItemStarted.
        let marked_in_seen = seen.borrow().iter().any(|e| match &e.event {
            Event::ItemStarted {
                kind: ItemKind::UserMessage,
                input: Some(val),
                ..
            } => {
                parse_scheduled_input(val).is_some_and(|m| m.schedule == sid && m.due == 1_003_600)
            }
            _ => false,
        });
        assert!(marked_in_seen, "scheduled marker missing from live stream");

        let stored = store.events(&thread, None, 100).expect("events");
        let marked_in_store = stored.iter().any(|(_, e)| match &e.event {
            Event::ItemStarted {
                kind: ItemKind::UserMessage,
                input: Some(val),
                ..
            } => {
                parse_scheduled_input(val).is_some_and(|m| m.schedule == sid && m.due == 1_003_600)
            }
            _ => false,
        });
        assert!(marked_in_store, "scheduled marker missing from store");

        // Scheduled prompt never resets `auto_wakes` or `unattended_runs`.
        assert_eq!(session.inner.auto_wakes.get(), 3);
        assert_eq!(
            store
                .get_schedule(&sid)
                .expect("get")
                .expect("present")
                .unattended_runs,
            1
        );

        // A real user prompt DOES reset both `auto_wakes` and `unattended_runs`.
        session.send_prompt("Hello human");
        assert_eq!(session.inner.auto_wakes.get(), 0);
        assert_eq!(
            store
                .get_schedule(&sid)
                .expect("get")
                .expect("present")
                .unattended_runs,
            0
        );
    });
}

#[test]
fn scheduled_prompt_refuses_bypass_stopped_and_non_worktree_agy() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let (_store, _thread, session, _seen) = open_idle(ctx, tmp.path());

        assert!(matches!(
            session.send_scheduled("s1", 100, "   ", false),
            ScheduledOutcome::Refused(_)
        ));

        session.inner.state.borrow_mut().mode = Mode::Bypass;
        assert!(matches!(
            session.send_scheduled("s1", 100, "Run", false),
            ScheduledOutcome::Refused(_)
        ));
        session.inner.state.borrow_mut().mode = Mode::AcceptEdits;

        session.inner.wakes_held.set(true);
        assert!(matches!(
            session.send_scheduled("s1", 100, "Run", false),
            ScheduledOutcome::Refused(_)
        ));
        session.inner.wakes_held.set(false);

        // Hookless (forced-permissive) Agy refuses even inside a worktree.
        *session.inner.adapter.borrow_mut() = Box::new(AgyAdapter::new("agy"));
        assert!(matches!(
            session.send_scheduled("s1", 100, "Run", true),
            ScheduledOutcome::Refused(_)
        ));
        session.inner.open.borrow_mut().forced_permissive = false;

        // Hooked Agy refuses without worktree, allows with worktree = true.
        let handle = ApprovalHandle::bind(
            &tmp.path().join("rt"),
            "t1",
            tmp.path(),
            Mode::Ask,
            Duration::from_secs(30),
        )
        .expect("bind");
        *session.inner.approval.borrow_mut() = Some(handle);
        assert!(matches!(
            session.send_scheduled("s1", 100, "Run", false),
            ScheduledOutcome::Refused(_)
        ));
        assert_eq!(
            session.send_scheduled("s1", 100, "Run", true),
            ScheduledOutcome::Started
        );
    });
}

#[test]
fn async_hook_check_failure_cancels_pending_scheduled_prompt() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let handle = ApprovalHandle::bind(
            &tmp.path().join("rt"),
            "t1",
            tmp.path(),
            Mode::AcceptEdits,
            Duration::from_secs(30),
        )
        .expect("bind");
        let hook_ok = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hook_flag = hook_ok.clone();
        let (sink, seen) = make_sink();
        let open = scripted(
            tmp.path(),
            "echo '{\"event\":\"init\",\"conversation_id\":\"c1\",\"init\":{\"model\":\"m\"}}'\n\
             cat >> \"$D/agy_stdin.log\"\n",
        );
        let session = ChatSession::new(
            Box::new(AgyAdapter::new("agy")),
            open,
            store.clone(),
            thread,
            sink,
            Some(handle),
        );
        *session.inner.hook_check.borrow_mut() = std::sync::Arc::new(move |_| {
            if hook_flag.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(())
            } else {
                Err("hook file missing".to_owned())
            }
        });
        // Wait for initial process spawn with passing hook check.
        assert!(pump_until(ctx, 10, || {
            session.status().alive && session.inner.starting.borrow().is_none()
        }));

        // Exit process cleanly, then fail hook check on the restart triggered by send_scheduled.
        session.inner.finish_process(Some(0), true);
        let starts_before = seen
            .borrow()
            .iter()
            .filter(|e| matches!(e.event, Event::SessionStarted { .. }))
            .count();
        hook_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            session.send_scheduled("sched-agy", 100, "DO NOT EXECUTE HOOKLESS", true),
            ScheduledOutcome::Started
        );
        assert!(pump_until(ctx, 10, || {
            session.inner.starting.borrow().is_none()
                && seen
                    .borrow()
                    .iter()
                    .filter(|e| matches!(e.event, Event::SessionStarted { .. }))
                    .count()
                    > starts_before
                && has(
                    &seen,
                    |e| matches!(e, Event::Notice { text } if text.contains("Scheduled prompt cancelled")),
                )
        }));
        // The replacement hookless process emitted `init`: the thread must stay idle and accept
        // a normal user prompt immediately (never stuck in a phantom turn).
        assert!(!session.status().running_turn);
        assert!(session.inner.active_schedule.borrow().is_none());
        let stdin_log =
            std::fs::read_to_string(tmp.path().join("agy_stdin.log")).unwrap_or_default();
        assert!(
            !stdin_log.contains("DO NOT EXECUTE HOOKLESS"),
            "{stdin_log}"
        );
        session.send_prompt("after cancel");
        assert!(pump_until(ctx, 10, || {
            std::fs::read_to_string(tmp.path().join("agy_stdin.log"))
                .unwrap_or_default()
                .contains("after cancel")
        }));
        session.shutdown(false);
    });
}

#[test]
fn reentrant_permissions_forced_sink_does_not_have_new_prompt_cleared_by_old_cancel() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let handle = ApprovalHandle::bind(
            &tmp.path().join("rt"),
            "t1",
            tmp.path(),
            Mode::AcceptEdits,
            Duration::from_secs(30),
        )
        .expect("bind");
        let hook_ok = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hook_flag = hook_ok.clone();
        let seen: Seen = Rc::default();
        let slot: Rc<RefCell<Option<Rc<ChatSession>>>> = Rc::default();
        let triggered = Rc::new(Cell::new(false));
        let (sink_seen, sink_slot, sink_triggered) =
            (seen.clone(), slot.clone(), triggered.clone());
        let sink: EnvelopeSink = Rc::new(move |e: &Envelope| {
            sink_seen.borrow_mut().push(e.clone());
            if matches!(&e.event, Event::PermissionsForced { reason: Some(_) })
                && !sink_triggered.replace(true)
            {
                if let Some(s) = sink_slot.borrow().as_ref() {
                    s.inner.start_process();
                    s.send_prompt("reentrant follow-up");
                }
            }
        });
        let open = scripted(
            tmp.path(),
            "echo '{\"event\":\"init\",\"conversation_id\":\"c1\",\"init\":{\"model\":\"m\"}}'\n\
             cat >> \"$D/agy_stdin.log\"\n",
        );
        let session = ChatSession::new(
            Box::new(AgyAdapter::new("agy")),
            open,
            store.clone(),
            thread,
            sink,
            Some(handle),
        );
        *slot.borrow_mut() = Some(session.clone());
        *session.inner.hook_check.borrow_mut() = std::sync::Arc::new(move |_| {
            if hook_flag.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(())
            } else {
                Err("hook missing".to_owned())
            }
        });
        assert!(pump_until(ctx, 10, || {
            session.status().alive && session.inner.starting.borrow().is_none()
        }));

        session.inner.finish_process(Some(0), true);
        hook_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            session.send_scheduled("sched-reenter", 100, "DO NOT RUN", true),
            ScheduledOutcome::Started
        );
        assert!(pump_until(ctx, 10, || {
            triggered.get()
                && std::fs::read_to_string(tmp.path().join("agy_stdin.log"))
                    .unwrap_or_default()
                    .contains("reentrant follow-up")
        }));
        let log = std::fs::read_to_string(tmp.path().join("agy_stdin.log")).unwrap_or_default();
        assert!(!log.contains("DO NOT RUN"), "{log}");
        assert!(
            session.status().running_turn || has(&seen, |e| matches!(e, Event::TurnStarted { .. }))
        );
        session.shutdown(false);
    });
}

#[test]
fn scheduled_prompt_reports_busy_during_running_turn_or_limit_wait() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let open = scripted(
            tmp.path(),
            "IFS= read -r handshake\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
             while IFS= read -r line; do case \"$line\" in *'\"type\":\"user\"'*) \
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'; \
             sleep 30;; esac; done",
        );
        let (session, seen) = harness(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            open,
            &store,
            &thread,
        );
        assert!(pump_until(ctx, 10, || {
            let st = session.status();
            st.alive && !st.running_turn && has(&seen, |e| matches!(e, Event::TurnCompleted { .. }))
        }));

        session.set_limit_wait(true);
        assert!(matches!(
            session.send_scheduled("sched-1", 100, "Check", false),
            ScheduledOutcome::Busy(_)
        ));
        assert!(session.is_limit_waiting());
        session.set_limit_wait(false);

        assert!(!session.has_open_approval());
        assert_eq!(
            session.send_scheduled("sched-1", 100, "Check", false),
            ScheduledOutcome::Started
        );
        assert!(matches!(
            session.send_scheduled("sched-2", 200, "Second", false),
            ScheduledOutcome::Busy(_)
        ));
        assert!(session.scheduled_busy_reason().is_some());

        // Clicking Stop during the scheduled turn records stopped_schedules and sets wakes_held.
        session.interrupt();
        assert_eq!(session.take_stopped_schedules(), vec!["sched-1".to_owned()]);
        assert!(session.take_stopped_schedules().is_empty());
        assert!(session.is_wakes_held());
        assert!(matches!(
            session.send_scheduled("sched-1", 300, "After stop", false),
            ScheduledOutcome::Refused(_)
        ));

        // Multiple Stops stay queued even if another schedule starts before polling.
        session.send_prompt("resume");
        assert!(pump_until(ctx, 10, || session.status().alive));
        session.inner.state.borrow_mut().running_turn = false;
        session.inner.prompt_in_flight.set(false);
        assert_eq!(
            session.send_scheduled("sched-a", 400, "Run A", false),
            ScheduledOutcome::Started
        );
        session.interrupt();
        session.send_prompt("resume 2");
        session.inner.state.borrow_mut().running_turn = false;
        session.inner.prompt_in_flight.set(false);
        assert_eq!(
            session.send_scheduled("sched-b", 500, "Run B", false),
            ScheduledOutcome::Started
        );
        assert_eq!(session.take_stopped_schedules(), vec!["sched-a".to_owned()]);
    });
}

#[test]
fn scheduled_prompt_restarts_suspended_process_and_carries_pending_handoff() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let open = scripted(
            tmp.path(),
            "IFS= read -r handshake\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
             while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$D/prompts.log\"; \
             case \"$line\" in *'\"type\":\"user\"'*) \
             n=$(cat \"$D/n\" 2>/dev/null || echo 0); n=$((n+1)); printf '%s' \"$n\" > \"$D/n\"; \
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'; \
             echo '{\"type\":\"assistant\",\"uuid\":\"u'$n'\",\"parent_tool_use_id\":null,\"message\":{\"id\":\"m'$n'\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}'; \
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}';; esac; done",
        );
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let (session, seen) = harness(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            open,
            &store,
            &thread,
        );
        assert!(pump_until(ctx, 10, || {
            let st = session.status();
            st.alive && !st.running_turn && has(&seen, |e| matches!(e, Event::TurnCompleted { .. }))
        }));
        let warmup_turns = seen
            .borrow()
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count();
        session.send_prompt("warmup");
        assert!(pump_until(ctx, 10, || {
            seen.borrow()
                .iter()
                .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
                .count()
                > warmup_turns
                && !session.status().running_turn
        }));
        session.restore_handoff(Handoff {
            summary: "Carried context".into(),
            fence: "0123456789abcdef0123456789abcdef".into(),
            carried: 1,
            total: 1,
        });
        assert!(
            session.inner.suspend(false),
            "{:?}",
            session.inner.suspend_blocker(false)
        );
        assert!(pump_until(ctx, 10, || !session.status().alive));

        let turns_before = seen
            .borrow()
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count();
        assert_eq!(
            session.send_scheduled("sched-resume", 500, "Continue check", false),
            ScheduledOutcome::Started
        );
        // Wait for the resumed turn to become idle and spend the carried handoff.
        assert!(
            pump_until(ctx, 10, || {
                let st = session.status();
                st.alive
                    && !st.running_turn
                    && seen
                        .borrow()
                        .iter()
                        .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
                        .count()
                        > turns_before
                    && session.inner.pending_handoff.borrow().is_none()
            }),
            "turns_before={turns_before} now={} status={:?} handoff={:?} in_flight={} turn={:?} prompts={}",
            seen.borrow()
                .iter()
                .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
                .count(),
            (session.status().alive, session.status().running_turn),
            session.inner.pending_handoff.borrow().is_some(),
            session.inner.handoff_in_flight.get(),
            session.inner.handoff_turn.get(),
            std::fs::read_to_string(tmp.path().join("prompts.log")).unwrap_or_default(),
        );

        let sent = std::fs::read_to_string(tmp.path().join("prompts.log")).unwrap_or_default();
        assert!(sent.contains("Carried context"), "{sent}");
        assert!(sent.contains("Continue check"), "{sent}");
    });
}

#[test]
fn scheduled_prompt_refuses_delegated_child_and_remote_thread() {
    use super::super::scripted_tests::{minimal_env, scripted_codex};
    use crate::delegation::seam::{
        ApprovalGate, ChildPolicy, ChildPublish, GateVerdict, SessionOptions,
    };
    use crate::testutil::open_session;
    use agent_core::delegation::ProcessGeneration;
    use agent_core::remote::{RemoteFolder, RemoteProbe};

    struct NoopGate;
    impl ApprovalGate for NoopGate {
        fn judge(&self, _: &str, _: &serde_json::Value, _: Option<&str>) -> GateVerdict {
            GateVerdict::Defer
        }
        fn rule_may_apply(&self, _: &crate::always_allow::Action, _: Option<&str>) -> bool {
            false
        }
    }
    struct NoopPublish;
    impl ChildPublish for NoopPublish {
        fn publish(&self, _: ProcessGeneration, env: Envelope) -> Option<Envelope> {
            Some(env)
        }
        fn persist(&self, _: &str, _: Option<&str>, _: &Envelope) {}
        fn cancel_generation(&self, _: ProcessGeneration) {}
    }

    in_loop(|_ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let child_thread = fresh(&store, &tmp.path().to_string_lossy());
        let (sink, _seen) = make_sink();
        let child = ChatSession::with_options(
            Box::new(agent_core::codex::CodexAdapter::new()),
            scripted_codex(tmp.path(), Mode::AcceptEdits, None),
            SessionStore::from(store.clone()),
            child_thread,
            sink,
            Err(None),
            minimal_env(tmp.path()),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: None,
                child: Some(ChildPolicy {
                    gate: Rc::new(NoopGate),
                    publish: Rc::new(NoopPublish),
                    mode: Mode::AcceptEdits,
                }),
            },
        );
        assert!(matches!(
            child.send_scheduled("sched-child", 100, "Run", false),
            ScheduledOutcome::Refused(_)
        ));
        child.shutdown(false);

        let folder = RemoteFolder::new("box", "/srv/app").expect("folder");
        let remote_thread = fresh(&store, &folder.cwd());
        let (rsink, _rseen) = make_sink();
        let remote = ChatSession::remote(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            open_session("claude", Vec::new(), ""),
            store,
            remote_thread,
            rsink,
            LaunchEnv::default(),
            RemoteLaunch {
                folder,
                probe: RemoteProbe {
                    shell: "/bin/sh".into(),
                    path: "/usr/bin".into(),
                    cli: "/usr/bin/claude".into(),
                },
            },
            Rc::new(|_| Err("offline".into())),
        );
        assert!(matches!(
            remote.send_scheduled("sched-remote", 100, "Run", false),
            ScheduledOutcome::Refused(_)
        ));
        remote.shutdown(false);
    });
}
