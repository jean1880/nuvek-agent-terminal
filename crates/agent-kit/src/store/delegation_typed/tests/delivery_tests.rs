//! Delivery and completion: a refused steer that waits for the next turn, work abandoned by a
//! restart, a revocation scoped to the subtree it names, one replay precedence, and a result
//! stored as all of its parts or none.
use super::*;

/// A message's stored delivery state and the run it was delivered into.
fn delivery(s: &Store, m: &MessageId) -> (String, Option<String>) {
    let row = s.delegation_message(m.as_str()).expect("q").expect("row");
    (row.delivery_state, row.recipient_run_id)
}

/// An instruction from `principal` to `child` under `policy`, accepted aimed at `target_run`.
fn send_with(
    s: &Store,
    principal: &WorkerId,
    child: &WorkerId,
    policy: &str,
    target_run: Option<RunId>,
    key: &str,
) -> AcceptedOp {
    new_op(accept(
        s,
        principal,
        req(
            ToolName::CrossAgentSend,
            json!({"target": child.as_str(), "text": "steer", "kind": "instruction",
                   "delivery": policy, "idempotency_key": key}),
        ),
        Resolution::default(),
        AcceptPlan::Send(SendPlan {
            initial: DeliveryState::Queued,
            target_run,
        }),
    ))
}

/// A run's terminal event, linked to its worker.
fn end_event(s: &Store, child: &WorkerId, run: &RunId) -> PublishEvent {
    let thread = s
        .delegation_worker(child.as_str())
        .expect("q")
        .expect("row")
        .backing_thread_id
        .expect("thread");
    PublishEvent {
        thread,
        provider_thread: None,
        envelope: Envelope::new(Event::Notice {
            text: "finished".into(),
        }),
        links: vec![LinkSpec {
            process_generation: 0,
            worker: child.clone(),
            run: Some(run.clone()),
            category: EventCategory::Result,
            invocation: None,
        }],
        worker_states: Vec::new(),
        run_states: Vec::new(),
        aliases: Vec::new(),
    }
}

fn completion(s: &Store, child: &WorkerId, run: &RunId, parts: Vec<String>) -> Completion {
    Completion {
        run: run.clone(),
        state: RunState::Completed,
        worker_state: WorkerState::Waiting,
        reason: None,
        usage_json: None,
        result: parts,
        dispatch_failure: None,
        event: end_event(s, child, run),
    }
}

/// The reports `root` holds from `child`, oldest first.
fn reports(s: &Store, child: &WorkerId) -> Vec<String> {
    s.ids_with(
        "SELECT body FROM delegation_messages WHERE sender_worker_id = ?1 AND kind = 'report'
         ORDER BY rowid",
        &[&child.as_str()],
    )
    .expect("bodies")
}

#[test]
fn a_refused_auto_steer_waits_for_the_next_turn_and_is_confirmed_in_the_run_it_reaches() {
    let f = fx();
    let op = spawn(&f.s, &f.root.worker, "k1");
    let (child, first) = (op.worker.expect("w"), op.run.expect("r"));
    f.s.advance_delegation_run(&first, RunState::Running, None)
        .expect("running");
    let auto = send_with(
        &f.s,
        &f.root.worker,
        &child,
        "auto",
        Some(first.clone()),
        "a1",
    );
    let message = auto.message.clone().expect("m");
    f.s.reserve_delegation_dispatch(&auto.operation)
        .expect("s")
        .expect("reserved");
    f.s.record_delegation_dispatch(&auto.operation, &DispatchFact::Submitted)
        .expect("submitted");
    assert_eq!(delivery(&f.s, &message).0, "submitted");
    // No ordinary delivery fact takes it back: an echo, a confirmation or an unknown outcome
    // all go through `record_delegation_delivery`, which refuses the step.
    assert!(f
        .s
        .record_delegation_delivery(&message, DeliveryState::QueuedNextTurn, None)
        .is_err());
    assert_eq!(delivery(&f.s, &message).0, "submitted");
    // The refused steer's own path does.
    assert_eq!(
        f.s.requeue_refused_delegation_steer(&message)
            .expect("requeued"),
        DeliveryState::QueuedNextTurn
    );
    assert_eq!(op_state(&f.s, &auto.operation), "submitted");
    assert_eq!(delivery(&f.s, &message), ("queued_next_turn".into(), None));
    // Only once: it is no longer submitted.
    assert!(f.s.requeue_refused_delegation_steer(&message).is_err());
    // The next turn: the run that refused it ended, a new one takes it.
    f.s.end_delegation_run_if_open(first.as_str(), "completed", None, 1)
        .expect("ended");
    let next = RunId::from_suffix(&"c".repeat(24)).expect("run");
    f.s.insert_delegation_run(&RunRow {
        id: next.to_string(),
        worker_id: child.to_string(),
        process_generation: 1,
        provider_thread_id: None,
        native_turn_id: None,
        authority_epoch: 0,
        state: "running".into(),
        terminal_reason: None,
        usage_json: None,
        created_at: 2,
        updated_at: 2,
    })
    .expect("next run");
    f.s.record_delegation_delivery(&message, DeliveryState::DispatchReserved, None)
        .expect("reserved again");
    f.s.record_delegation_delivery(&message, DeliveryState::ProviderConfirmed, Some(&next))
        .expect("an auto message is delivered in the run it reaches");
    f.s.record_delegation_dispatch(&auto.operation, &DispatchFact::Confirmed)
        .expect("confirmed");
    assert_eq!(op_state(&f.s, &auto.operation), "confirmed");
    assert_eq!(
        delivery(&f.s, &message),
        ("provider_confirmed".into(), Some(next.to_string()))
    );
}

#[test]
fn a_strict_steer_has_no_next_turn_to_wait_for() {
    let f = fx();
    let op = spawn(&f.s, &f.root.worker, "k1");
    let (child, first) = (op.worker.expect("w"), op.run.expect("r"));
    f.s.advance_delegation_run(&first, RunState::Running, None)
        .expect("running");
    let strict = send_with(
        &f.s,
        &f.root.worker,
        &child,
        "this_run",
        Some(first.clone()),
        "s1",
    );
    let message = strict.message.clone().expect("m");
    f.s.reserve_delegation_dispatch(&strict.operation)
        .expect("s")
        .expect("reserved");
    f.s.record_delegation_dispatch(&strict.operation, &DispatchFact::Submitted)
        .expect("submitted");
    assert!(f.s.requeue_refused_delegation_steer(&message).is_err());
    assert_eq!(delivery(&f.s, &message).0, "submitted");
    f.s.record_delegation_dispatch(
        &strict.operation,
        &DispatchFact::Failed(ToolError::new(ErrorCode::StaleRun, "refused")),
    )
    .expect("failed");
    assert_eq!(op_state(&f.s, &strict.operation), "failed");
    assert_eq!(delivery(&f.s, &message).0, "failed");
}

#[test]
fn a_restart_fails_undispatched_work_for_a_child_and_leaves_a_childs_mailbox_alone() {
    let f = fx();
    let Fx {
        _dir,
        path,
        s,
        root,
        ..
    } = f;
    let child = spawn(&s, &root.worker, "k1").worker.expect("w");
    let instruction = new_op(send(
        &s,
        &root.worker,
        child.as_str(),
        "instruction",
        "go",
        "i1",
    ));
    let report = new_op(send(&s, &child, "parent", "report", "done", "r1"));
    let report_message = report.message.clone().expect("m");
    s.read_and_present_delegation_page(&query(
        &root.worker,
        &root.worker,
        vec![root.worker.clone()],
        None,
    ))
    .expect("s")
    .expect("page");
    s.acknowledge_delegation_messages(&AckRequest {
        principal: root.worker.clone(),
        request: ack_req(&[&report_message], "ack1"),
    })
    .expect("s")
    .expect("ack");
    let before = s
        .delegation_message(report_message.as_str())
        .expect("q")
        .expect("row");
    drop(s); // the app dies with the instruction accepted and never reserved
    let s = open(&path);
    let rec = s.reconcile_delegation_after_restart().expect("reconcile");
    assert!(rec.operations.contains(&instruction.operation));
    assert!(!rec.operations.contains(&report.operation));
    assert_eq!(op_state(&s, &instruction.operation), "failed");
    let message = instruction.message.clone().expect("m");
    assert_eq!(delivery(&s, &message).0, "failed");
    assert!(rec.messages.contains(&message));
    // A retry of the instruction gets its failure, saying why; nothing is sent again.
    let again = accept(
        &s,
        &root.worker,
        req(
            ToolName::CrossAgentSend,
            json!({"target": child.as_str(), "text": "go", "kind": "instruction",
                   "delivery": "next_turn", "idempotency_key": "i1"}),
        ),
        Resolution::default(),
        AcceptPlan::Send(SendPlan {
            initial: DeliveryState::Queued,
            target_run: None,
        }),
    );
    let Ok(Accepted::Replay(ReplayOutcome::Error(e))) = again else {
        panic!("{again:?}")
    };
    assert_eq!(e.code, ErrorCode::RuntimeUnavailable);
    assert!(e.message.contains("restarted"), "{}", e.message);
    assert_eq!(
        e.details.as_ref().expect("details")["replayed"],
        json!(true)
    );
    // The child's report is mailbox data: read and acknowledged, exactly as it was.
    assert_eq!(op_state(&s, &report.operation), "accepted");
    let after = s
        .delegation_message(report_message.as_str())
        .expect("q")
        .expect("row");
    assert_eq!(after, before);
    let replayed = new_op_or_replay(accept(
        &s,
        &child,
        req(
            ToolName::CrossAgentSend,
            json!({"target": "parent", "text": "done", "kind": "report",
                   "delivery": "next_turn", "idempotency_key": "r1"}),
        ),
        Resolution::default(),
        AcceptPlan::Send(SendPlan {
            initial: DeliveryState::Queued,
            target_run: None,
        }),
    ));
    assert_eq!(replayed["replayed"], json!(true));
    assert_eq!(replayed["message_id"], json!(report_message.as_str()));
}

fn new_op_or_replay(v: Verdict<Accepted>) -> Value {
    match v {
        Ok(Accepted::Replay(ReplayOutcome::Receipt(r))) => r,
        other => panic!("not a replayed receipt: {other:?}"),
    }
}

#[test]
fn a_close_cancels_only_its_subtree_and_carries_the_siblings_work_to_the_new_epoch() {
    let f = fx();
    let closed = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
    let sibling = spawn(&f.s, &f.root.worker, "k2").worker.expect("w");
    let grandchild = spawn(&f.s, &closed, "k3").worker.expect("w");
    let to_closed = new_op(send(
        &f.s,
        &f.root.worker,
        closed.as_str(),
        "instruction",
        "a",
        "s1",
    ));
    let to_grandchild = new_op(send(
        &f.s,
        &closed,
        grandchild.as_str(),
        "instruction",
        "b",
        "s2",
    ));
    let to_sibling = new_op(send(
        &f.s,
        &f.root.worker,
        sibling.as_str(),
        "instruction",
        "c",
        "s3",
    ));
    let revoked =
        f.s.revoke_delegation(&f.root.worker, &RevocationEvent::Close(closed.clone()))
            .expect("revoke");
    assert_eq!(revoked.authority_epoch, 1);
    for op in [&to_closed, &to_grandchild] {
        assert!(revoked.invalidated.contains(&op.operation));
        assert_eq!(op_state(&f.s, &op.operation), "failed");
        let m = op.message.clone().expect("m");
        assert_eq!(delivery(&f.s, &m).0, "cancelled");
        let error =
            f.s.delegation_operation(op.operation.as_str())
                .expect("q")
                .expect("row")
                .error_json
                .expect("error");
        assert!(error.contains("its worker was closed"), "{error}");
    }
    // The sibling's instruction is still queued, carried to the new epoch, and dispatches.
    assert!(!revoked.invalidated.contains(&to_sibling.operation));
    let m = to_sibling.message.clone().expect("m");
    assert_eq!(delivery(&f.s, &m).0, "queued");
    f.s.reserve_delegation_dispatch(&to_sibling.operation)
        .expect("s")
        .expect("the sibling's work survives the close");
    assert_eq!(delivery(&f.s, &m).0, "dispatch_reserved");
    // A worker's exit is scoped the same way, and says so.
    let late = new_op(send(
        &f.s,
        &f.root.worker,
        sibling.as_str(),
        "instruction",
        "d",
        "s4",
    ));
    let other = spawn(&f.s, &f.root.worker, "k5").worker.expect("w");
    let survivor = new_op(send(
        &f.s,
        &f.root.worker,
        other.as_str(),
        "instruction",
        "e",
        "s5",
    ));
    f.s.revoke_delegation(
        &f.root.worker,
        &RevocationEvent::GenerationExited(sibling.clone()),
    )
    .expect("exit");
    let error =
        f.s.delegation_operation(late.operation.as_str())
            .expect("q")
            .expect("row")
            .error_json
            .expect("error");
    assert!(error.contains("its worker stopped"), "{error}");
    let m = survivor.message.clone().expect("m");
    assert_eq!(delivery(&f.s, &m).0, "queued");
    // A stop still cancels across the whole tree.
    f.s.revoke_delegation(&f.root.worker, &RevocationEvent::RootStop)
        .expect("stop");
    assert_eq!(op_state(&f.s, &survivor.operation), "failed");
    assert_eq!(delivery(&f.s, &m).0, "cancelled");
}

#[test]
fn a_mode_restriction_still_cancels_queued_work_across_the_tree() {
    let f = fx();
    let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
    let queued = new_op(send(
        &f.s,
        &f.root.worker,
        child.as_str(),
        "instruction",
        "go",
        "s1",
    ));
    f.s.revoke_delegation(
        &f.root.worker,
        &RevocationEvent::RootRestriction {
            new_mode: agent_core::adapter::Mode::Plan,
        },
    )
    .expect("restrict");
    assert_eq!(op_state(&f.s, &queued.operation), "failed");
    assert_eq!(
        delivery(&f.s, &queued.message.clone().expect("m")).0,
        "cancelled"
    );
}

#[test]
fn a_retry_gets_what_its_first_attempt_did_whatever_the_epoch() {
    let f = fx();
    let first = spawn(&f.s, &f.root.worker, "k1");
    f.s.reserve_delegation_dispatch(&first.operation)
        .expect("s")
        .expect("reserved");
    f.s.record_delegation_dispatch(&first.operation, &DispatchFact::Submitted)
        .expect("submitted");
    let invalidated = spawn(&f.s, &f.root.worker, "k2");
    f.s.revoke_delegation(&f.root.worker, &RevocationEvent::RootStop)
        .expect("bump");
    let with_epoch = |key: &str, task: &str, epoch: u64| {
        req(
            ToolName::CrossAgentSpawn,
            json!({"profile_id": "codex-default", "name": "reviewer", "task": task,
                   "task_mode": "plan", "workspace": {"kind": "shared_read_only"},
                   "expected_authority_epoch": epoch, "idempotency_key": key}),
        )
    };
    let plan = || AcceptPlan::Spawn(spawn_plan());
    // The identical request after the bump, its stale epoch and all: the stored receipt.
    for request in [
        spawn_req("k1", "review the parser"),
        with_epoch("k1", "review the parser", 0),
    ] {
        let again = accept(&f.s, &f.root.worker, request, Resolution::default(), plan());
        let Ok(Accepted::Replay(ReplayOutcome::Receipt(r))) = again else {
            panic!("{again:?}")
        };
        assert_eq!(r["operation_id"], json!(first.operation.as_str()));
        assert_eq!(r["replayed"], json!(true));
        assert_eq!(r["dispatch"], json!("submitted"));
    }
    // An operation the bump failed replays its failure; it never becomes a success.
    let failed = accept(
        &f.s,
        &f.root.worker,
        with_epoch("k2", "review the parser", 1),
        Resolution::default(),
        plan(),
    );
    let Ok(Accepted::Replay(ReplayOutcome::Error(e))) = failed else {
        panic!("{failed:?}")
    };
    assert_eq!(e.code, ErrorCode::StaleEpoch);
    assert_eq!(
        e.details.as_ref().expect("d")["operation_id"],
        json!(invalidated.operation.as_str())
    );
    // A changed body under a used key is a conflict, whatever the epoch.
    let changed = accept(
        &f.s,
        &f.root.worker,
        with_epoch("k1", "review the lexer", 0),
        Resolution::default(),
        plan(),
    );
    assert_eq!(
        changed.err().map(|e| e.code),
        Some(ErrorCode::IdempotencyConflict)
    );
    // An unused key with a stale epoch is refused.
    let stale = accept(
        &f.s,
        &f.root.worker,
        with_epoch("k9", "review the parser", 0),
        Resolution::default(),
        plan(),
    );
    assert_eq!(stale.err().map(|e| e.code), Some(ErrorCode::StaleEpoch));
    assert!(f
        .s
        .delegation_operation_by_key(f.root.worker.as_str(), "spawn", "k9")
        .expect("q")
        .is_none());
}

#[test]
fn a_long_result_is_stored_as_every_part_together_and_reads_back_in_full() {
    let f = fx();
    let op = spawn(&f.s, &f.root.worker, "k1");
    let (child, run) = (op.worker.expect("w"), op.run.expect("r"));
    let text: String = (0..(MAX_RESULT_BYTES * 3 + 500))
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let split = agent_core::delegation::mailbox::result_parts(&text, MAX_RESULT_BYTES, 0);
    assert_eq!(split.parts.len(), 4);
    let done =
        f.s.complete_delegation_run(&completion(&f.s, &child, &run, split.parts.clone()))
            .expect("complete");
    assert_eq!(done.parts.len(), 4);
    assert_eq!(done.notification, done.parts.first().cloned());
    assert!(done.parts.windows(2).all(|w| w[0].1 < w[1].1), "in order");
    let bodies = reports(&f.s, &child);
    assert_eq!(bodies, split.parts);
    assert!(bodies.iter().all(|b| b.len() <= MAX_RESULT_BYTES));
    // The parent pages through its mailbox and gets every part, in order, whole.
    let mut read = Vec::new();
    let mut after = None;
    for _ in 0..16 {
        let page =
            f.s.read_delegation_page(&query(
                &f.root.worker,
                &f.root.worker,
                vec![f.root.worker.clone()],
                after.clone(),
            ))
            .expect("s")
            .expect("page");
        for m in page.page["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            read.push(m["text"].as_str().expect("text").to_owned());
        }
        if page.page["next_cursor"].is_null() || page.last_seq.is_none() {
            break;
        }
        after = Some(next_cursor(&page.page));
        if read.len() == 4 {
            break;
        }
    }
    assert_eq!(read, split.parts, "every part read back, in order");
    let rebuilt: String = read
        .iter()
        .map(|p| p.split_once("]\n").map_or(p.as_str(), |(_, t)| t))
        .collect();
    assert_eq!(rebuilt, text);
}

#[test]
fn a_completion_that_fails_part_way_leaves_no_part_and_no_end() {
    let f = fx();
    let op = spawn(&f.s, &f.root.worker, "k1");
    let (child, run) = (op.worker.expect("w"), op.run.expect("r"));
    // The third report of this child cannot be stored.
    f.s.conn
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER third_part BEFORE INSERT ON delegation_messages
             WHEN NEW.sender_worker_id = '{child}' AND NEW.kind = 'report'
              AND (SELECT COUNT(*) FROM delegation_messages
                   WHERE sender_worker_id = '{child}' AND kind = 'report') >= 2
             BEGIN SELECT RAISE(ABORT, 'injected'); END;"
        ))
        .expect("trigger");
    let parts = vec![
        "[part 1/3]\none".into(),
        "[part 2/3]\ntwo".into(),
        "[part 3/3]\nthree".into(),
    ];
    let failed =
        f.s.complete_delegation_run(&completion(&f.s, &child, &run, parts));
    assert!(failed.is_err());
    assert!(reports(&f.s, &child).is_empty(), "no part was kept");
    assert_eq!(
        f.s.delegation_run(run.as_str())
            .expect("q")
            .expect("row")
            .state,
        "reserved",
        "the run did not end"
    );
}

#[test]
fn a_cancel_before_the_turn_opened_ends_the_run_before_its_dispatch_fails() {
    let f = fx();
    let op = spawn(&f.s, &f.root.worker, "k1");
    let (child, run) = (op.worker.expect("w"), op.run.expect("r"));
    f.s.reserve_delegation_dispatch(&op.operation)
        .expect("s")
        .expect("reserved");
    let mut c = completion(&f.s, &child, &run, vec!["cancelled".into()]);
    c.state = RunState::Interrupted;
    c.reason = Some("cancelled".into());
    c.dispatch_failure = Some((
        op.operation.clone(),
        ToolError::new(ErrorCode::StaleRun, "run cancelled before dispatch"),
    ));
    let done = f.s.complete_delegation_run(&c).expect("complete");
    let ended = f.s.delegation_run(run.as_str()).expect("q").expect("row");
    assert_eq!(ended.state, "interrupted", "the cancel stays a cancel");
    assert_eq!(ended.terminal_reason.as_deref(), Some("cancelled"));
    assert_eq!(op_state(&f.s, &op.operation), "failed");
    assert!(done.notification.is_some(), "the parent is told");
    let links = f.s.delegation_event_links(done.event.seq).expect("links");
    assert!(links
        .iter()
        .any(|l| l.worker_id == child.as_str() && l.run_id.as_deref() == Some(run.as_str())));
}
