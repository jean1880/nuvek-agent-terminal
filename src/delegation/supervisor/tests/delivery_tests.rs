//! Delivery and completion through the supervisor: a refused steer delivered on the next turn,
//! a sibling's close or exit that leaves the rest of the tree's work in place (queued, starting
//! or in flight), retries answered from what their first attempt did, and a long answer that
//! reaches the parent whole.
use super::*;
use agent_core::delegation::mailbox::MAX_RESULT_PARTS;
use agent_core::delegation::MessageId;
use agent_core::event::{Envelope, Event};

/// An operation's stored dispatch state.
fn dispatch(sup: &Rc<Supervisor>, op: &str) -> String {
    sup.services
        .handle
        .reader()
        .delegation_operation(op)
        .unwrap()
        .unwrap()
        .dispatch_state
}

/// A message's stored row.
fn message(sup: &Rc<Supervisor>, id: &str) -> agent_kit::store::delegation::MessageRow {
    sup.services
        .handle
        .reader()
        .delegation_message(id)
        .unwrap()
        .unwrap()
}

/// A child whose task turn stays open: busy until its run is cancelled.
async fn spawn_holding(sup: &Rc<Supervisor>, root: &WorkerId, key: &str) -> WorkerId {
    let mut request = spawn_request(key);
    request.task = "hold".into();
    let worker = spawned(&sup.spawn_worker(root, request).await.unwrap());
    pump_until(|| sup.state.borrow().workers[&worker].row.state == "running").await;
    worker
}

/// A child that ran its task and is idle, its process up.
async fn spawn_idle(sup: &Rc<Supervisor>, root: &WorkerId, key: &str) -> WorkerId {
    let worker = spawned(&sup.spawn_worker(root, spawn_request(key)).await.unwrap());
    pump_until(|| {
        let state = sup.state.borrow();
        let w = &state.workers[&worker];
        w.row.state == "completed" && w.run.is_none()
    })
    .await;
    worker
}

async fn settled(sup: &Rc<Supervisor>, root: &WorkerId) {
    pump_until(|| !sup.state.borrow().roots[root].revoking).await;
}

fn close_request(worker: &WorkerId, key: &str) -> CloseRequest {
    CloseRequest {
        worker_id: worker.clone(),
        expected_authority_epoch: None,
        idempotency_key: IdempotencyKey::parse(key).unwrap(),
    }
}

/// How a sibling is lost.
#[derive(Clone, Copy, Debug)]
enum Loss {
    Close,
    FailureExit,
}

async fn lose(sup: &Rc<Supervisor>, root: &WorkerId, worker: &WorkerId, loss: Loss) {
    match loss {
        Loss::Close => {
            sup.close_worker_call(root, close_request(worker, "lose-sibling"))
                .await
                .unwrap();
        }
        Loss::FailureExit => sup.fail_worker(
            worker,
            error(ErrorCode::ProviderUnavailable, "the sibling's agent failed"),
        ),
    }
    settled(sup, root).await;
}

/// The refusal a Codex run gives a steer it cannot take, for the control tagged `tag`.
fn refusal(tag: &str) -> Envelope {
    let mut env = Envelope::new(Event::ControlResult {
        ok: None,
        error: Some("no active turn to steer".into()),
    });
    env.request = Some(tag.to_owned());
    env
}

/// A refused `auto` steer falls back to the next turn: its success receipt is honoured, the
/// message is delivered in the next run and confirmed there.
#[test]
fn a_refused_auto_steer_is_delivered_on_the_next_turn() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = spawn_holding(&sup, &root, "steer-busy").await;
        let first = sup.state.borrow().workers[&worker].run.clone().unwrap();
        let mut request = instruction(&worker, "after the refusal", "steer-auto");
        request.delivery = Delivery::Auto;
        let receipt = sup.send_worker(&root, request).await.unwrap();
        let op = receipt["operation_id"].as_str().unwrap().to_owned();
        let id = receipt["message_id"].as_str().unwrap().to_owned();
        assert!(
            sup.state.borrow().controls.contains_key(&op),
            "written live, awaiting its outcome"
        );
        // The run refuses it before anything else is heard of it.
        sup.delivery_control(&worker, &refusal(&op));
        pump_until(|| dispatch(&sup, &op) == "confirmed").await;
        let row = message(&sup, &id);
        assert_eq!(row.delivery_state, "provider_confirmed");
        let reached = row.recipient_run_id.expect("the run it was delivered in");
        assert_ne!(reached, first.as_str(), "delivered on the next turn");
        let run = sup
            .services
            .handle
            .reader()
            .delegation_run(&reached)
            .unwrap()
            .unwrap();
        assert_eq!(run.worker_id, worker.as_str());
        stop(&sup).await;
    });
}

/// A `this_run` steer the run refuses has no next turn: it fails, saying so.
#[test]
fn a_refused_strict_steer_still_fails() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = spawn_holding(&sup, &root, "strict-busy").await;
        let mut request = instruction(&worker, "now or never", "steer-strict");
        request.delivery = Delivery::ThisRun;
        let receipt = sup.send_worker(&root, request).await.unwrap();
        let op = receipt["operation_id"].as_str().unwrap().to_owned();
        let id = receipt["message_id"].as_str().unwrap().to_owned();
        sup.delivery_control(&worker, &refusal(&op));
        pump_until(|| dispatch(&sup, &op) == "failed").await;
        let stored = sup
            .services
            .handle
            .reader()
            .delegation_operation(&op)
            .unwrap()
            .unwrap();
        assert!(stored
            .error_json
            .unwrap()
            .contains("strict steer was refused"));
        assert_eq!(message(&sup, &id).delivery_state, "failed");
        assert!(sup
            .state
            .borrow()
            .deferred
            .get(&worker)
            .is_none_or(|q| q.is_empty()));
        stop(&sup).await;
    });
}

/// A binding staged for `worker` that never launched (a launch about to start).
fn plant_staged(sup: &Rc<Supervisor>, worker: &WorkerId) {
    let binding = PreparedBinding {
        worker: worker.clone(),
        generation: ProcessGeneration(99),
        token: CapabilityToken::from_random([5; 32]),
        launch: crate::delegation::seam::LaunchBinding {
            open: None,
            env: Vec::new(),
            argv_tail: Vec::new(),
        },
        overlay: None,
    };
    sup.state
        .borrow_mut()
        .workers
        .get_mut(worker)
        .unwrap()
        .staged = Some(binding);
}

/// A busy sibling's queued next-turn instruction and a starting sibling (its task waiting, a
/// launch staged) both survive another worker's close or failure exit, move to the new epoch,
/// and are delivered.
#[test]
fn queued_and_starting_work_of_siblings_survives_a_close_and_a_failure_exit() {
    for loss in [Loss::Close, Loss::FailureExit] {
        in_context(async {
            let (dir, sup, root) = harness();
            let busy = spawn_holding(&sup, &root, "busy").await;
            let queued = sup
                .send_worker(&root, instruction(&busy, "queued for later", "queued"))
                .await
                .unwrap();
            let queued_op = queued["operation_id"].as_str().unwrap().to_owned();
            let queued_id = queued["message_id"].as_str().unwrap().to_owned();
            assert_eq!(sup.state.borrow().deferred[&busy].len(), 1);
            let lost = spawn_idle(&sup, &root, "lost").await;
            // The next child is held at its startup check: its task waits to be delivered.
            std::fs::write(dir.path().join("hold-bootstrap"), "").unwrap();
            let starting = spawned(
                &sup.spawn_worker(&root, spawn_request("starting"))
                    .await
                    .unwrap(),
            );
            pump_until(|| {
                let state = sup.state.borrow();
                let w = &state.workers[&starting];
                w.session.is_some() && w.pending.is_some()
            })
            .await;
            plant_staged(&sup, &busy);
            let before = sup.epoch(&root).unwrap();
            lose(&sup, &root, &lost, loss).await;
            let now = sup.epoch(&root).unwrap();
            assert!(now > before, "{loss:?} bumps the epoch");
            {
                let state = sup.state.borrow();
                let entries = &state.deferred[&busy];
                assert_eq!(
                    entries.len(),
                    1,
                    "{loss:?}: the queued instruction survives"
                );
                assert_eq!(
                    entries[0].input.epoch, now,
                    "{loss:?}: moved to the new epoch"
                );
                assert!(
                    state.workers[&busy].staged.is_some(),
                    "{loss:?}: a sibling's staged launch survives"
                );
                assert_eq!(
                    state.workers[&starting].pending.as_ref().unwrap().epoch,
                    now,
                    "{loss:?}: the starting sibling's task moved to the new epoch"
                );
            }
            assert_eq!(message(&sup, &queued_id).delivery_state, "queued_next_turn");
            assert_eq!(dispatch(&sup, &queued_op), "accepted");
            sup.state
                .borrow_mut()
                .workers
                .get_mut(&busy)
                .unwrap()
                .staged = None;
            // The starting sibling gets its task and runs it.
            std::fs::remove_file(dir.path().join("hold-bootstrap")).unwrap();
            pump_until(|| {
                let state = sup.state.borrow();
                let w = &state.workers[&starting];
                assert_ne!(
                    w.row.state, "failed",
                    "{loss:?}: {:?}",
                    w.row.delegation_reason
                );
                w.row.state == "completed"
            })
            .await;
            // The busy sibling's turn ends: its queued instruction is delivered.
            let run = sup.state.borrow().workers[&busy].run.clone().unwrap();
            sup.cancel_bound_run(&busy, &run, "Delegated run cancelled");
            pump_until(|| dispatch(&sup, &queued_op) == "confirmed").await;
            assert_eq!(
                message(&sup, &queued_id).delivery_state,
                "provider_confirmed"
            );
            stop(&sup).await;
        });
    }
}

/// A compromised bridge's exit fences that worker: its token dies and its connection is
/// revoked, its queued work goes, while a sibling's queued instruction stays.
#[test]
fn a_compromise_exit_fences_its_worker_and_leaves_siblings_queued_work() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let busy = spawn_holding(&sup, &root, "compromise-busy").await;
        let queued = sup
            .send_worker(
                &root,
                instruction(&busy, "still wanted", "compromise-queued"),
            )
            .await
            .unwrap();
        let queued_id = queued["message_id"].as_str().unwrap().to_owned();
        let lost = spawn_idle(&sup, &root, "compromised").await;
        connect(&sup, &lost, ConnId(71)).await;
        let lost_epoch = sup.epoch(&root).unwrap();
        sup.begin_revoke(&root, RevocationEvent::GenerationExited(lost.clone()));
        settled(&sup, &root).await;
        {
            let state = sup.state.borrow();
            assert!(state.workers[&lost].token.is_none(), "its token is killed");
            assert!(
                state.fences[&lost] >= lost_epoch,
                "its in-flight work is fenced"
            );
            assert_eq!(state.deferred[&busy].len(), 1);
        }
        assert!(
            sup.epoch_for(&lost, lost_epoch).is_err(),
            "work it captured before is void"
        );
        assert!(
            sup.epoch_for(&busy, lost_epoch).is_ok(),
            "a sibling's moves on to the new epoch"
        );
        assert_eq!(message(&sup, &queued_id).delivery_state, "queued_next_turn");
        stop(&sup).await;
    });
}

/// A Stop and a mode restriction still cancel queued work across the whole tree.
#[test]
fn a_stop_and_a_mode_restriction_still_cancel_queued_work_tree_wide() {
    for restrict in [false, true] {
        in_context(async {
            let (_dir, sup, root) = harness();
            let busy = spawn_holding(&sup, &root, "tree-busy").await;
            let queued = sup
                .send_worker(
                    &root,
                    instruction(&busy, "cancelled with the tree", "tree-queued"),
                )
                .await
                .unwrap();
            let op = queued["operation_id"].as_str().unwrap().to_owned();
            let id = queued["message_id"].as_str().unwrap().to_owned();
            if restrict {
                sup.revoke(
                    &root,
                    RevocationEvent::RootRestriction {
                        new_mode: Mode::Plan,
                    },
                )
                .await
                .unwrap();
            } else {
                let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
                sup.root_stop(&thread);
                settled(&sup, &root).await;
            }
            assert!(
                sup.state
                    .borrow()
                    .deferred
                    .get(&busy)
                    .is_none_or(|q| q.is_empty()),
                "restrict={restrict}"
            );
            pump_until(|| message(&sup, &id).delivery_state == "cancelled").await;
            assert_eq!(dispatch(&sup, &op), "failed");
            stop(&sup).await;
        });
    }
}

/// A spawn whose binding is being prepared while a sibling is closed starts and runs its task.
#[test]
fn a_sibling_closed_during_binding_preparation_leaves_the_launch_going() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let sibling = spawn_idle(&sup, &root, "prep-sibling").await;
        let mut spawn = Box::pin(sup.spawn_worker(&root, spawn_request("prep-launch")));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(spawn.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let worker = sup
            .state
            .borrow()
            .workers
            .keys()
            .find(|id| *id != &root && *id != &sibling)
            .unwrap()
            .clone();
        assert!(
            sup.state.borrow().workers[&worker].session.is_none(),
            "still preparing"
        );
        lose(&sup, &root, &sibling, Loss::Close).await;
        spawn.await.unwrap();
        pump_until(|| {
            let state = sup.state.borrow();
            let w = &state.workers[&worker];
            assert_ne!(w.row.state, "failed", "{:?}", w.row.delegation_reason);
            w.row.state == "completed"
        })
        .await;
        stop(&sup).await;
    });
}

/// An agy launch whose proof is being checked while a sibling is closed is allowed, its token
/// moved to the new epoch; the same launch is refused when the close covers its own worker.
#[test]
fn a_sibling_closed_during_the_launch_proof_leaves_the_launch_allowed() {
    for own in [false, true] {
        in_context(async {
            let (dir, sup, root) = harness();
            configure_agy(&sup, dir.path());
            let sibling = spawn_idle(&sup, &root, "proof-sibling").await;
            let worker = spawn_idle(&sup, &root, "proof-launch").await;
            let epoch = sup.epoch(&root).unwrap();
            {
                let mut state = sup.state.borrow_mut();
                let w = state.workers.get_mut(&worker).unwrap();
                w.driver = Driver::Agy;
                w.token = Some(TokenSlot::new(
                    epoch,
                    ProcessGeneration(7),
                    CapabilityToken::from_random([6; 32]),
                    None,
                ));
            }
            let facts = crate::delegation::seam::DelegationLaunchFacts {
                generation: ProcessGeneration(7),
                argv: vec!["agy".into()],
                physical_cwd: dir.path().to_owned(),
                effective_home: Some(dir.path().to_owned()),
                config_redirect_vars: vec![],
                hook_proven: true,
            };
            let hooks = crate::delegation::attest::Hooks {
                supervisor: Rc::downgrade(&sup),
                worker: worker.clone(),
            };
            let mut proof = hooks.before_spawn(facts);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(proof.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            let closed = if own { &worker } else { &sibling };
            lose(&sup, &root, closed, Loss::Close).await;
            let verdict = proof.await;
            if own {
                assert!(verdict.is_err(), "its own close refuses the launch");
            } else {
                assert_eq!(verdict, Ok(()), "a sibling's close does not");
                let state = sup.state.borrow();
                assert_eq!(
                    state.workers[&worker].token.as_ref().unwrap().epoch,
                    state.roots[&root].row.epoch()
                );
            }
            stop(&sup).await;
        });
    }
}

/// An instruction already taken off the queue for dispatch (its worker being started again)
/// when a sibling is closed is still delivered.
#[test]
fn a_sibling_closed_during_dispatch_leaves_the_delivery_going() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let sibling = spawn_idle(&sup, &root, "dispatch-sibling").await;
        let worker = spawn_idle(&sup, &root, "dispatch-target").await;
        // Its process stops: the next instruction starts it again before it is delivered.
        let session = sup.child_session(&worker).unwrap();
        session.shutdown(true);
        pump_until(|| sup.state.borrow().workers[&worker].generations.is_empty()).await;
        settled(&sup, &root).await;
        let receipt = sup
            .send_worker(&root, instruction(&worker, "start again", "dispatch-send"))
            .await
            .unwrap();
        let op = receipt["operation_id"].as_str().unwrap().to_owned();
        assert!(
            sup.state
                .borrow()
                .deferred
                .get(&worker)
                .is_none_or(|q| q.is_empty()),
            "taken off the queue for dispatch"
        );
        assert!(sup.state.borrow().workers[&worker].dispatching);
        lose(&sup, &root, &sibling, Loss::Close).await;
        pump_until(|| {
            let state = dispatch(&sup, &op);
            assert_ne!(state, "failed", "the delivery was failed");
            state == "confirmed"
        })
        .await;
        stop(&sup).await;
    });
}

/// A retry after the epoch moved on gets what its first attempt did; a changed request under
/// the same key is a conflict; an unused key with a stale epoch is refused.
#[test]
fn a_retry_after_an_epoch_bump_replays_its_receipt_through_the_supervisor() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let first = sup
            .spawn_worker(&root, spawn_request("replayed"))
            .await
            .unwrap();
        let worker = spawned(&first);
        pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
        let old = sup.epoch(&root).unwrap();
        let sibling = spawn_idle(&sup, &root, "bump").await;
        lose(&sup, &root, &sibling, Loss::Close).await;
        assert!(sup.epoch(&root).unwrap() > old);
        let mut stale = spawn_request("replayed");
        stale.expected_authority_epoch = Some(old.0);
        for retry in [spawn_request("replayed"), stale] {
            let again = sup.spawn_worker(&root, retry).await.unwrap();
            assert_eq!(again["operation_id"], first["operation_id"]);
            assert_eq!(again["worker_id"], first["worker_id"]);
            assert_eq!(again["replayed"], true);
        }
        let mut changed = spawn_request("replayed");
        changed.task = "a different task".into();
        assert_eq!(
            sup.spawn_worker(&root, changed).await.unwrap_err().code,
            ErrorCode::IdempotencyConflict
        );
        let mut unused = spawn_request("never-used");
        unused.expected_authority_epoch = Some(old.0);
        assert_eq!(
            sup.spawn_worker(&root, unused).await.unwrap_err().code,
            ErrorCode::StaleEpoch
        );
        stop(&sup).await;
    });
}

/// The reports `worker` sent `root`, read back through the root's mailbox pages.
async fn read_reports(sup: &Rc<Supervisor>, conn: ConnId, worker: &WorkerId) -> Vec<String> {
    let mut out = Vec::new();
    let mut after = None;
    for id in 1..64 {
        let page = tool(
            sup,
            conn,
            id,
            ToolRequest::Read(ReadRequest {
                after: after.clone(),
                ..Default::default()
            }),
        )
        .await;
        let messages = page["messages"].as_array().cloned().unwrap_or_default();
        for m in &messages {
            if m["from"] == worker.as_str() && m["kind"] == "report" {
                out.push(m["text"].as_str().unwrap().to_owned());
            }
        }
        if messages.is_empty() && page["events"].as_array().is_none_or(|e| e.is_empty()) {
            break;
        }
        after = Some(
            agent_core::delegation::Cursor::parse(page["next_cursor"].as_str().unwrap()).unwrap(),
        );
    }
    out
}

/// The text a part carries without its header.
fn part_body(part: &str) -> &str {
    part.strip_prefix("[part ")
        .and_then(|rest| rest.split_once("]\n"))
        .map_or(part, |(_, text)| text)
}

/// A long answer reaches the parent whole, in parts it can page through, and its wake says
/// where the rest is.
#[test]
fn a_long_answer_is_read_back_in_full_from_its_parts() {
    in_context(async {
        let (dir, sup, root) = harness();
        connect(&sup, &root, ConnId(72)).await;
        let cap = sup.limits.max_message_bytes as usize;
        // Multi-byte characters throughout, so cuts fall inside them.
        let answer: String = "Résumé — 日本語 🦀 answer line.\n".repeat(cap * 3 / 40 + 20);
        assert!(answer.len() > cap * 3);
        std::fs::write(dir.path().join("task-result"), &answer).unwrap();
        let worker = spawn_idle(&sup, &root, "long-answer").await;
        // The wake waiting for the root (taken before a read presents the first part, which
        // settles it) carries that part and says where the rest is.
        pump_until(|| {
            sup.state
                .borrow()
                .wakes
                .get(&root)
                .is_some_and(|q| q.pending() > 0)
        })
        .await;
        let (items, _) = sup.state.borrow_mut().wakes.get_mut(&root).unwrap().take();
        let item = items.iter().find(|i| i.worker == worker).unwrap().clone();
        let parts = read_reports(&sup, ConnId(72), &worker).await;
        assert!(
            parts.len() > 1 && parts.len() <= MAX_RESULT_PARTS,
            "{} parts",
            parts.len()
        );
        for (i, part) in parts.iter().enumerate() {
            assert!(part.len() <= cap, "part {} is {} bytes", i + 1, part.len());
            assert!(part.starts_with(&format!("[part {}/{}]", i + 1, parts.len())));
        }
        let rebuilt: String = parts.iter().map(|p| part_body(p)).collect();
        assert_eq!(rebuilt.trim_end(), answer.trim_end(), "read back in full");
        assert!(
            item.text.starts_with(&format!(
                "[result in {} parts; read them with cross_agent_read]\n{}",
                parts.len(),
                parts[0]
            )),
            "{}",
            item.text.chars().take(120).collect::<String>()
        );
        stop(&sup).await;
    });
}

/// An answer past eight parts is cut, the last part saying so; every part is within the
/// message cap, and a wake with it and two other children's news stays within its budget,
/// still saying where the parts are. (One agent message past the stream sanitiser's bound is
/// withheld before it gets here, so the split is driven directly.)
#[test]
fn an_answer_past_eight_parts_is_cut_and_its_wake_fits_with_other_news() {
    use agent_core::delegation::wake::{frame, Item, Outcome, Shown, WAKE_BUDGET};
    in_context(async {
        let (_dir, sup, _root) = harness();
        let cap = sup.limits.max_message_bytes as usize;
        let answer = "é".repeat(cap * 5);
        let (parts, wake) = crate::delegation::publish::result_messages(&sup, &answer, 0);
        assert_eq!(parts.len(), MAX_RESULT_PARTS);
        for part in &parts {
            assert!(part.len() <= cap, "{} bytes", part.len());
        }
        assert!(parts[MAX_RESULT_PARTS - 1].contains("were cut"));
        let item = |c: char, text: String| Item {
            worker: WorkerId::from_suffix(&c.to_string().repeat(24)).unwrap(),
            name: format!("child {c}"),
            driver: Driver::Claude,
            model: None,
            outcome: Outcome::Completed,
            text,
            shown: Shown::Event(1),
        };
        let news = [
            item('a', wake),
            item('b', "b".repeat(cap)),
            item('c', "c".repeat(cap)),
        ];
        let framed = frame(&news, 0, WAKE_BUDGET);
        assert!(framed.len() <= WAKE_BUDGET, "{} bytes", framed.len());
        assert!(framed.contains(&format!(
            "[result in {MAX_RESULT_PARTS} parts; read them with cross_agent_read]"
        )));
        stop(&sup).await;
    });
}

/// Only messages to a child are requeued: a child's report to its parent is mailbox data.
#[test]
fn a_childs_report_is_never_requeued() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = spawn_idle(&sup, &root, "report-child").await;
        connect(&sup, &worker, ConnId(73)).await;
        let report = tool(
            &sup,
            ConnId(73),
            1,
            ToolRequest::Send(report_to_parent("for the parent", "report-key")),
        )
        .await;
        let id = MessageId::parse(report["message_id"].as_str().unwrap()).unwrap();
        let before = message(&sup, id.as_str());
        assert!(sup
            .services
            .handle
            .reader()
            .requeue_refused_delegation_steer(&id)
            .is_err());
        assert_eq!(message(&sup, id.as_str()), before);
        assert_eq!(before.kind, "report");
        stop(&sup).await;
    });
}

/// How many startup checks the fake child has been sent so far.
fn startup_checks(dir: &std::path::Path) -> usize {
    std::fs::read_to_string(dir.join("frames.ndjson"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("\"dir\": \"in\"") && l.contains("startup check"))
        .count()
}

/// The delivery's own target exits (its generation's revocation commits) while the delivery
/// prepares to start it again and its parent stays current: the delivery fails, and no new
/// process is started for it.
#[test]
fn a_targets_own_exit_during_preparation_fails_the_delivery() {
    in_context(async {
        let (dir, sup, root) = harness();
        let worker = spawn_idle(&sup, &root, "own-exit").await;
        let session = sup.child_session(&worker).unwrap();
        session.shutdown(true);
        pump_until(|| sup.state.borrow().workers[&worker].generations.is_empty()).await;
        settled(&sup, &root).await;
        let checks = startup_checks(dir.path());
        let receipt = sup
            .send_worker(&root, instruction(&worker, "start again", "own-exit-send"))
            .await
            .unwrap();
        let op = receipt["operation_id"].as_str().unwrap().to_owned();
        // Taken off the queue for its start; the target's own exit commits before it goes on.
        assert!(sup.state.borrow().workers[&worker].dispatching);
        sup.begin_revoke(&root, RevocationEvent::GenerationExited(worker.clone()));
        settled(&sup, &root).await;
        pump_until(|| dispatch(&sup, &op) == "failed").await;
        pump_until(|| !sup.state.borrow().workers[&worker].dispatching).await;
        glib::timeout_future(Duration::from_millis(300)).await;
        assert!(
            sup.state.borrow().workers[&worker].generations.is_empty(),
            "no process was started for a delivery its target's exit revoked"
        );
        assert_eq!(startup_checks(dir.path()), checks);
        stop(&sup).await;
    });
}

/// A cancel that finds the task still pending (its child at its startup check) ends the run
/// through the one completion, and the child is stopped only after that end committed: the
/// run is interrupted with its reason, the task's dispatch failed, the parent told, and the
/// worker not failed.
#[test]
fn a_cancel_with_the_task_still_pending_keeps_its_completion() {
    in_context(async {
        let (dir, sup, root) = harness();
        std::fs::write(dir.path().join("hold-bootstrap"), "").unwrap();
        let receipt = sup
            .spawn_worker(&root, spawn_request("pending-cancel"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        let run = RunId::parse(receipt["run_id"].as_str().unwrap()).unwrap();
        pump_until(|| startup_checks(dir.path()) > 0).await;
        assert!(sup.state.borrow().workers[&worker].pending.is_some());
        sup.cancel_bound_run(&worker, &run, "Delegated run cancelled");
        pump_until(|| {
            run_state(&sup, run.as_str()).is_some_and(|s| s != "reserved" && s != "starting")
        })
        .await;
        let ended = sup
            .services
            .handle
            .reader()
            .delegation_run(run.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(ended.state, "interrupted");
        assert_eq!(ended.terminal_reason.as_deref(), Some("cancelled"));
        let op = receipt["operation_id"].as_str().unwrap();
        pump_until(|| dispatch(&sup, op) == "failed").await;
        connect(&sup, &root, ConnId(74)).await;
        let page = tool(
            &sup,
            ConnId(74),
            1,
            ToolRequest::Read(ReadRequest {
                target: Some(worker.clone()),
                ..Default::default()
            }),
        )
        .await;
        assert!(
            page["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["kind"] == "report"
                    && m["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("Cancelled before its turn started"))),
            "the parent's report: {page:#}"
        );
        // The child never got its task: it is stopped, after the end.
        pump_until(|| sup.state.borrow().workers[&worker].generations.is_empty()).await;
        {
            let state = sup.state.borrow();
            let w = &state.workers[&worker];
            assert_ne!(w.row.state, "failed", "{:?}", w.row.delegation_reason);
            assert!(w.row.delegation_reason.is_none());
        }
        std::fs::remove_file(dir.path().join("hold-bootstrap")).unwrap();
        stop(&sup).await;
    });
}

/// A busy worker's steer, sent as a Codex run takes it: queued, reserved, submitted and
/// awaiting the run's answer under `tag`.
async fn steer_in_flight(
    sup: &Rc<Supervisor>,
    root: &WorkerId,
    policy: Delivery,
    key: &str,
) -> (WorkerId, RunId, String, String) {
    let worker = spawn_holding(sup, root, &format!("{key}-busy")).await;
    let run = sup.state.borrow().workers[&worker].run.clone().unwrap();
    // Stored as `auto` (what a refusal requeues) but queued rather than written live: a
    // worker that takes no live input queues it for the next turn.
    let mut request = instruction(&worker, "steer it", key);
    if policy == Delivery::Auto {
        request.delivery = Delivery::Auto;
    }
    let driver = std::mem::replace(
        &mut sup
            .state
            .borrow_mut()
            .workers
            .get_mut(&worker)
            .unwrap()
            .driver,
        Driver::Agy,
    );
    let receipt = sup.send_worker(root, request).await.unwrap();
    sup.state
        .borrow_mut()
        .workers
        .get_mut(&worker)
        .unwrap()
        .driver = driver;
    let op = receipt["operation_id"].as_str().unwrap().to_owned();
    let id = receipt["message_id"].as_str().unwrap().to_owned();
    let mut entry = sup
        .state
        .borrow_mut()
        .deferred
        .get_mut(&worker)
        .unwrap()
        .pop_front()
        .unwrap();
    let store = sup.services.handle.reader();
    store
        .reserve_delegation_dispatch(&entry.input.operation)
        .unwrap()
        .unwrap();
    store
        .record_delegation_dispatch(
            &entry.input.operation,
            &agent_kit::store::delegation_typed::DispatchFact::Submitted,
        )
        .unwrap();
    entry.input.plan = agent_core::delegation::mailbox::DeliveryPlan::SteerNow(run.clone());
    entry.policy = policy;
    entry.reserved = true;
    sup.state
        .borrow_mut()
        .controls
        .insert(format!("{key}-tag"), entry);
    (worker, run, op, id)
}

/// A steer refused only after its turn ended is still requeued and delivered next turn: its
/// correlation outlives the turn.
#[test]
fn a_refusal_after_the_turn_ended_still_requeues_the_steer() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let (worker, run, op, id) = steer_in_flight(&sup, &root, Delivery::Auto, "late").await;
        sup.cancel_bound_run(&worker, &run, "Delegated run cancelled");
        pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
        assert!(
            sup.state.borrow().controls.contains_key("late-tag"),
            "the steer's correlation outlived its turn"
        );
        sup.delivery_control(&worker, &refusal("late-tag"));
        pump_until(|| dispatch(&sup, &op) == "confirmed").await;
        let row = message(&sup, &id);
        assert_eq!(row.delivery_state, "provider_confirmed");
        assert_ne!(row.recipient_run_id.as_deref(), Some(run.as_str()));
        stop(&sup).await;
    });
}

/// A steer no answer settles is taken as refused once its deadline passes: `auto` goes to the
/// next turn, `this_run` fails.
#[test]
fn a_steer_with_no_answer_settles_by_its_deadline() {
    for policy in [Delivery::Auto, Delivery::ThisRun] {
        in_context(async {
            let (_dir, sup, root) = harness();
            sup.state.borrow_mut().steer_settle = Some(Duration::from_millis(200));
            let (worker, run, op, id) = steer_in_flight(&sup, &root, policy, "silent").await;
            // The deadline is armed when the steer is sent; this one was planted.
            sup.arm_steer_settle(&worker, "silent-tag".into());
            sup.cancel_bound_run(&worker, &run, "Delegated run cancelled");
            let expected = if policy == Delivery::Auto {
                "confirmed"
            } else {
                "failed"
            };
            pump_until(|| dispatch(&sup, &op) == expected).await;
            let state = message(&sup, &id).delivery_state;
            assert_eq!(
                state,
                if policy == Delivery::Auto {
                    "provider_confirmed"
                } else {
                    "failed"
                },
                "{policy:?}"
            );
            stop(&sup).await;
        });
    }
}

/// A run's result keeps each assistant item's text in order: a snapshot (a split secret's
/// heal) replaces only its own item.
#[test]
fn a_snapshot_replaces_only_its_own_items_text() {
    use crate::delegation::publish::{accumulate, take_result};
    let mut items = Vec::new();
    accumulate(&mut items, Some("a"), "first draft", true, 1024);
    accumulate(&mut items, Some("b"), "second ", false, 1024);
    accumulate(&mut items, Some("b"), "item", false, 1024);
    accumulate(&mut items, Some("a"), "first, healed", true, 1024);
    assert_eq!(
        take_result(&mut items),
        ("first, healed\n\nsecond item".into(), 0)
    );
    // Past the limit, what is cut is counted.
    accumulate(&mut items, None, &"x".repeat(30), false, 20);
    assert_eq!(take_result(&mut items), ("x".repeat(20), 10));
}

/// Each run's result starts at its turn: nothing of an earlier run is carried into it.
#[test]
fn a_runs_result_starts_with_its_turn() {
    use crate::delegation::seam::ChildPublish;
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = spawn_idle(&sup, &root, "reset").await;
        let publisher = crate::delegation::publish::Publisher::new(Rc::downgrade(&sup), worker);
        let mut snapshot = Envelope::new(Event::ContentSnapshot {
            stream: agent_core::event::StreamKind::Assistant,
            text: "left from a cancelled run".into(),
        });
        snapshot.item = Some("a".into());
        publisher.persist("t", None, &snapshot);
        assert_eq!(publisher.result_so_far(), "left from a cancelled run");
        publisher.persist(
            "t",
            None,
            &Envelope::new(Event::TurnStarted {
                model: Some("synthetic-model".into()),
            }),
        );
        assert_eq!(publisher.result_so_far(), "");
        stop(&sup).await;
    });
}

/// A secret straddling the result's bound is redacted whole before the cut: none of it shows.
#[test]
fn a_secret_at_the_results_bound_is_redacted_before_it_is_cut() {
    use crate::delegation::publish::{
        accumulate, result_bound, result_messages, take_result, RESULT_LOOKAHEAD,
    };
    in_context(async {
        let (_dir, sup, _root) = harness();
        let bound = result_bound(&sup);
        let secret = format!("{}_{}", "ghp", "Ab3D".repeat(9));
        let text = format!("{}{secret} trailing words", "x".repeat(bound - 8));
        let mut items = Vec::new();
        accumulate(&mut items, None, &text, false, bound + RESULT_LOOKAHEAD);
        let (whole, cut) = take_result(&mut items);
        let (parts, _) = result_messages(&sup, &whole, cut);
        assert!(
            parts.iter().all(|p| !p.contains("ghp_Ab3D")),
            "a piece of the secret survived the cut"
        );
        stop(&sup).await;
    });
}
