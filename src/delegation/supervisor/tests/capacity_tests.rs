//! Idle workers give their process slots back: a start past the app's cap pauses the least
//! recently active idle worker (losslessly, nothing revoked), the idle sweep pauses one idle
//! long enough, and a paused worker resumes its own session on its next instruction. A worker
//! with anything under way, or a live sub-agent of its own, is never paused.
use super::*;
use crate::chat::ChatBackend;
use agent_core::event::{Envelope, Event, Question, QuestionSource};

/// A worker whose task has completed, its process up and idle, spawned by `caller`.
async fn idle_worker(sup: &Rc<Supervisor>, caller: &WorkerId, key: &str) -> WorkerId {
    let receipt = sup
        .spawn_worker(caller, spawn_request(key))
        .await
        .expect("spawn");
    let worker = spawned(&receipt);
    pump_until(|| {
        let state = sup.state.borrow();
        let w = &state.workers[&worker];
        assert_ne!(w.row.state, "failed", "{:?}", w.row.delegation_reason);
        w.row.state == "completed"
            && w.run.is_none()
            && w.pending.is_none()
            && !w.dispatching
            && !w.generations.is_empty()
            && w.session().is_some_and(|s| !s.status().running_turn)
    })
    .await;
    worker
}

/// Marks `worker` as last active `minutes` ago.
fn idle_for(sup: &Rc<Supervisor>, worker: &WorkerId, minutes: u64) {
    let at = Instant::now()
        .checked_sub(Duration::from_secs(minutes * 60))
        .expect("a past instant");
    sup.state
        .borrow_mut()
        .workers
        .get_mut(worker)
        .expect("worker")
        .last_active = at;
}

fn live(sup: &Rc<Supervisor>, worker: &WorkerId) -> bool {
    !sup.state.borrow().workers[worker].generations.is_empty()
}

fn question(request: &str) -> Envelope {
    Envelope::new(Event::QuestionRequested {
        questions: vec![Question {
            id: "q".into(),
            header: "h".into(),
            question: "Which?".into(),
            options: Vec::new(),
            multi_select: false,
        }],
        source: QuestionSource::Agent,
    })
    .request(request)
}

#[test]
fn a_spawn_past_the_cap_pauses_the_least_recently_active_idle_worker_and_it_resumes_later() {
    in_context(async {
        let (dir, sup, root) = harness();
        let cap = sup.limits.live_processes_app as usize;
        let mut workers = Vec::new();
        for i in 0..cap {
            workers.push(idle_worker(&sup, &root, &format!("fill-{i}")).await);
        }
        assert_eq!(sup.live_processes() as usize, cap);
        // Not the first spawned: the least recently active.
        let oldest = workers[3].clone();
        idle_for(&sup, &oldest, 30);
        let epoch = sup.epoch(&root).unwrap();

        let extra = idle_worker(&sup, &root, "one-more").await;
        assert!(live(&sup, &extra));
        assert!(
            !live(&sup, &oldest),
            "the least recently active worker was paused"
        );
        for w in workers.iter().filter(|w| **w != oldest) {
            assert!(live(&sup, w), "only one worker is paused for one start");
        }
        assert_eq!(sup.live_processes() as usize, cap);
        assert_eq!(sup.epoch(&root).unwrap(), epoch, "nothing was revoked");
        {
            let state = sup.state.borrow();
            let w = &state.workers[&oldest];
            assert_eq!(
                w.row.state, "completed",
                "a paused worker is not failed or closed"
            );
            assert!(w.session().is_some_and(|s| s.resumes_from_suspension()));
            for other in workers.iter().filter(|w| **w != oldest) {
                assert!(
                    state.token_current(&state.workers[other]),
                    "every other worker's credential is still current"
                );
            }
        }

        // Its next instruction resumes its own native session, making room again.
        idle_for(&sup, &workers[0], 20);
        let paused = sup.state.borrow().workers[&oldest].session().unwrap();
        let paused_generation = paused.process_generation();
        sup.send_worker(&root, instruction(&oldest, "RESUMED_WORK", "after-pause"))
            .await
            .expect("send");
        pump_until(|| {
            let frames = std::fs::read_to_string(dir.path().join("frames.ndjson")).unwrap();
            live(&sup, &oldest)
                && frames.contains("RESUMED_WORK")
                && sup.state.borrow().workers[&oldest].run.is_none()
        })
        .await;
        let session = sup.state.borrow().workers[&oldest].session().unwrap();
        // The same session, so its native one: a restart resumes it (`--resume`, proven by
        // the session's own tests), never a session built anew.
        assert!(Rc::ptr_eq(&session, &paused), "the worker kept its session");
        assert!(
            session.process_generation() > paused_generation,
            "in a new process"
        );
        assert!(!session.resumes_from_suspension(), "the resume has started");
        assert!(
            !live(&sup, &workers[0]),
            "its restart paused the next least recently active worker"
        );
        assert_eq!(sup.live_processes() as usize, cap);
        assert_eq!(sup.epoch(&root).unwrap(), epoch);
        stop(&sup).await;
    });
}

#[test]
fn a_busy_worker_is_never_paused_and_with_none_idle_the_spawn_says_why() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let cap = sup.limits.live_processes_app as usize;
        let mut workers = Vec::new();
        for i in 0..cap - 1 {
            workers.push(idle_worker(&sup, &root, &format!("busy-{i}")).await);
        }
        // The last slot is a sub-agent of the last worker: a live descendant keeps it up.
        let parent = workers[cap - 2].clone();
        let grandchild = idle_worker(&sup, &parent, "grandchild").await;
        let scope = sup.scope(&workers[0]).expect("scope");
        sup.on_child_envelope(&workers[0], scope, question("q1"), (1, None), false, None);
        {
            let mut state = sup.state.borrow_mut();
            state.workers.get_mut(&workers[1]).unwrap().dispatching = true;
            state.workers.get_mut(&workers[2]).unwrap().preparing = true;
            state.workers.get_mut(&workers[3]).unwrap().run =
                Some(agent_core::delegation::RunId::parse("r_capacityRun1").unwrap());
            state.workers.get_mut(&grandchild).unwrap().dispatching = true;
        }
        // The rest are kept by something the supervisor holds for them: input queued, a
        // pending start.
        for w in &workers[4..cap - 2] {
            let input = crate::delegation::seam::DelegatedInput {
                operation: OperationId::from_suffix("capacityop1").unwrap(),
                epoch: sup.epoch(&root).unwrap(),
                source: crate::delegation::seam::InputSource::Task,
                from: root.clone(),
                from_label: "root".into(),
                text: "later".into(),
                plan: agent_core::delegation::mailbox::DeliveryPlan::StartTurn,
                tag: "capacity".into(),
            };
            sup.state.borrow_mut().workers.get_mut(w).unwrap().pending = Some(input);
        }
        for w in workers.iter().chain([&grandchild]) {
            idle_for(&sup, w, 60);
        }
        assert_eq!(sup.live_processes() as usize, cap);

        let e = sup
            .spawn_worker(&root, spawn_request("no-room"))
            .await
            .expect_err("no worker can be paused");
        assert_eq!(e.code, ErrorCode::CapacityReached);
        assert!(e.message.contains("live_processes_app"), "{}", e.message);
        assert!(e.message.contains("cross_agent_close"), "{}", e.message);
        assert!(e.retryable, "a run in progress may free a slot");
        let details = e.details.clone().expect("details");
        assert_eq!(details["limit"], "live_processes_app");
        assert_eq!(details["live_processes"], cap);
        for w in workers.iter().chain([&grandchild]) {
            assert!(live(&sup, w), "a busy worker is never paused");
        }
        assert_eq!(sup.stop_idle_children(15), 0, "nor by the idle sweep");

        // With no run in progress, nothing will end by itself: not worth retrying.
        sup.state
            .borrow_mut()
            .workers
            .get_mut(&workers[3])
            .unwrap()
            .run = None;
        sup.state
            .borrow_mut()
            .workers
            .get_mut(&workers[3])
            .unwrap()
            .preparing = true;
        let e = sup
            .spawn_worker(&root, spawn_request("still-no-room"))
            .await
            .expect_err("still none");
        assert!(!e.retryable, "{e:?}");
        for w in workers.iter().chain([&grandchild]) {
            assert!(live(&sup, w));
        }
        stop(&sup).await;
    });
}

#[test]
fn the_idle_sweep_pauses_a_worker_idle_past_the_threshold_and_never_at_zero() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let long = idle_worker(&sup, &root, "idle-long").await;
        let short = idle_worker(&sup, &root, "idle-short").await;
        idle_for(&sup, &long, 20);
        idle_for(&sup, &short, 5);
        let epoch = sup.epoch(&root).unwrap();

        assert_eq!(sup.stop_idle_children(0), 0, "0 is never");
        assert!(live(&sup, &long) && live(&sup, &short));

        assert_eq!(sup.stop_idle_children(15), 1);
        pump_until(|| !live(&sup, &long)).await;
        assert!(live(&sup, &short), "idle under the threshold: kept");
        assert_eq!(sup.epoch(&root).unwrap(), epoch, "nothing was revoked");
        {
            let state = sup.state.borrow();
            assert!(state.token_current(&state.workers[&short]));
            assert_eq!(state.workers[&long].row.state, "completed");
        }
        assert_eq!(
            sup.stop_idle_children(15),
            0,
            "a paused worker has nothing to stop"
        );
        stop(&sup).await;
    });
}

#[test]
fn a_worker_with_a_live_sub_agent_is_never_paused() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let middle = idle_worker(&sup, &root, "middle").await;
        let leaf = idle_worker(&sup, &middle, "leaf").await;
        idle_for(&sup, &middle, 60);
        idle_for(&sup, &leaf, 60);
        sup.state
            .borrow_mut()
            .workers
            .get_mut(&leaf)
            .unwrap()
            .dispatching = true;
        assert_eq!(sup.stop_idle_children(15), 0);
        assert!(live(&sup, &middle), "its sub-agent is at work");

        // The sub-agent idle, its process up: still a live link through the middle worker.
        sup.state
            .borrow_mut()
            .workers
            .get_mut(&leaf)
            .unwrap()
            .dispatching = false;
        idle_for(&sup, &leaf, 60);
        assert_eq!(sup.stop_idle_children(15), 1, "only the sub-agent");
        pump_until(|| !live(&sup, &leaf)).await;
        assert!(live(&sup, &middle));
        stop(&sup).await;
    });
}

/// Runs `fut` on the main context beside the test; its result lands in the cell.
fn run_beside<T: 'static>(
    fut: impl std::future::Future<Output = T> + 'static,
) -> Rc<RefCell<Option<T>>> {
    let out: Rc<RefCell<Option<T>>> = Rc::default();
    let slot = out.clone();
    glib::spawn_future_local(async move {
        let value = fut.await;
        *slot.borrow_mut() = Some(value);
    });
    out
}

/// `cap` idle workers, the first `oldest` of them least recently active in that order.
async fn full_table(sup: &Rc<Supervisor>, root: &WorkerId, key: &str) -> Vec<WorkerId> {
    let cap = sup.limits.live_processes_app as usize;
    let mut workers = Vec::new();
    for i in 0..cap {
        workers.push(idle_worker(sup, root, &format!("{key}-{i}")).await);
    }
    for (i, w) in workers.iter().enumerate() {
        idle_for(sup, w, 100 - i as u64);
    }
    workers
}

fn frames(dir: &tempfile::TempDir) -> String {
    std::fs::read_to_string(dir.path().join("frames.ndjson")).unwrap_or_default()
}

/// An instruction whose acceptance is still in flight holds its recipient: a spawn needing room
/// pauses another worker, and the instruction is delivered, never failed.
#[test]
fn a_worker_an_instruction_is_on_its_way_to_is_never_paused_for_room() {
    in_context(async {
        let (dir, sup, root) = harness();
        let workers = full_table(&sup, &root, "held").await;
        let hold = Rc::new(std::cell::Cell::new(true));
        sup.state.borrow_mut().hold_send_accept = Some(hold.clone());
        let send = {
            let (sup, target) = (sup.clone(), workers[0].clone());
            let root = root.clone();
            run_beside(async move {
                sup.send_worker(&root, instruction(&target, "HELD_SEND", "held-send"))
                    .await
            })
        };
        pump_until(|| sup.state.borrow().delivery_holds.contains_key(&workers[0])).await;

        let extra = idle_worker(&sup, &root, "needs-room").await;
        assert!(live(&sup, &extra));
        assert!(live(&sup, &workers[0]), "the recipient was not paused");
        assert!(
            !live(&sup, &workers[1]),
            "the next least recently active one was"
        );

        hold.set(false);
        pump_until(|| send.borrow().is_some() && frames(&dir).contains("HELD_SEND")).await;
        assert!(send.borrow_mut().take().unwrap().is_ok());
        assert!(
            sup.state.borrow().delivery_holds.is_empty(),
            "the hold was let go"
        );
        stop(&sup).await;
    });
}

/// A recipient whose process went away while its instruction was being accepted is started
/// again and given it, never handed a write it cannot take.
#[test]
fn an_instruction_to_a_worker_paused_during_its_acceptance_resumes_it() {
    in_context(async {
        let (dir, sup, root) = harness();
        let target = idle_worker(&sup, &root, "paused-anyway").await;
        let hold = Rc::new(std::cell::Cell::new(true));
        sup.state.borrow_mut().hold_send_accept = Some(hold.clone());
        let send = {
            let (sup, target, root) = (sup.clone(), target.clone(), root.clone());
            run_beside(async move {
                sup.send_worker(&root, instruction(&target, "AFTER_PAUSE", "paused-send"))
                    .await
            })
        };
        pump_until(|| sup.state.borrow().delivery_holds.contains_key(&target)).await;
        // Paused anyway (as if the hold were not there).
        sup.state.borrow_mut().delivery_holds.clear();
        let session = sup.state.borrow().workers[&target].session().unwrap();
        assert!(session.suspend_child_if_idle());
        pump_until(|| !live(&sup, &target)).await;

        hold.set(false);
        pump_until(|| send.borrow().is_some() && frames(&dir).contains("AFTER_PAUSE")).await;
        assert!(send.borrow_mut().take().unwrap().is_ok());
        assert!(live(&sup, &target), "resumed for it");
        stop(&sup).await;
    });
}

/// A start admitted but still preparing its launch holds its slot: a second start finds the
/// table full and makes room, so the processes never pass the cap. A failed launch gives its
/// slot back.
#[test]
fn an_admitted_start_reserves_its_slot_until_its_process_exists() {
    in_context(async {
        let (dir, sup, root) = harness();
        let cap = sup.limits.live_processes_app as usize;
        let workers = full_table(&sup, &root, "reserve").await;
        // The last one paused: one slot free.
        let paused = workers[cap - 1].clone();
        idle_for(&sup, &paused, 1000);
        assert_eq!(sup.stop_idle_children(500), 1);
        pump_until(|| !live(&sup, &paused)).await;

        let hold = Rc::new(std::cell::Cell::new(true));
        sup.state.borrow_mut().hold_launch = Some(hold.clone());
        let spawn = {
            let (sup, root) = (sup.clone(), root.clone());
            run_beside(async move { sup.spawn_worker(&root, spawn_request("reserved")).await })
        };
        pump_until(|| sup.usage(&root).live_processes as usize == cap).await;
        assert_eq!(
            sup.live_processes() as usize,
            cap - 1,
            "reserved, not started"
        );
        sup.send_worker(
            &root,
            instruction(&paused, "RESERVED_RESUME", "resume-held"),
        )
        .await
        .expect("send");
        // The resume found no free slot: it paused the least recently active worker.
        pump_until(|| !live(&sup, &workers[0])).await;

        hold.set(false);
        let mut spawned_worker = None;
        pump_until(|| {
            assert!(
                sup.live_processes() as usize <= cap,
                "the cap was passed: {}",
                sup.live_processes()
            );
            if spawned_worker.is_none() {
                spawned_worker = spawn
                    .borrow()
                    .as_ref()
                    .map(|r| spawned(r.as_ref().expect("spawn")));
            }
            spawned_worker.as_ref().is_some_and(|w| live(&sup, w))
                && live(&sup, &paused)
                && frames(&dir).contains("RESERVED_RESUME")
        })
        .await;
        assert_eq!(sup.live_processes() as usize, cap);
        assert!(
            sup.state.borrow().launch_slots.is_empty(),
            "every slot was handed on"
        );

        // A launch that fails after its admission gives its slot back.
        sup.state.borrow_mut().hold_launch = None;
        idle_for(&sup, &workers[1], 1000);
        std::fs::write(dir.path().join("fail-resolve-after-first"), "").unwrap();
        let receipt = sup
            .spawn_worker(&root, spawn_request("fails-to-launch"))
            .await
            .expect("accepted");
        let failed = spawned(&receipt);
        pump_until(|| sup.state.borrow().workers[&failed].row.state == "failed").await;
        std::fs::remove_file(dir.path().join("fail-resolve-after-first")).unwrap();
        std::fs::remove_file(dir.path().join("unavailable")).unwrap();
        assert!(
            sup.state.borrow().launch_slots.is_empty(),
            "the failed launch let go"
        );
        assert_eq!(sup.usage(&root).live_processes, sup.live_processes());
        stop(&sup).await;
    });
}

/// A worker idle since its run ended is as recent as that end, however quiet the run was.
#[test]
fn a_run_ending_makes_its_worker_recently_active() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let mut quiet = spawn_request("quiet-run");
        quiet.task = "hold".into();
        let long = spawned(&sup.spawn_worker(&root, quiet).await.unwrap());
        pump_until(|| sup.state.borrow().workers[&long].row.state == "running").await;
        let other = idle_worker(&sup, &root, "finished-meanwhile").await;
        // The run stays quiet a while after the other worker finished.
        glib::timeout_future(Duration::from_millis(50)).await;
        let ended_at = Instant::now();
        let scope = sup.scope(&long).expect("scope");
        sup.on_child_envelope(
            &long,
            scope,
            Envelope::turn_completed(agent_core::event::TurnState::Completed, None),
            (1, None),
            false,
            None,
        );
        let (long_at, other_at) = {
            let state = sup.state.borrow();
            (
                state.workers[&long].last_active,
                state.workers[&other].last_active,
            )
        };
        assert!(long_at >= ended_at, "stamped by its run's end");
        assert!(long_at > other_at);
        let order: Vec<WorkerId> = sup.idle_children().into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            order,
            vec![other.clone(), long.clone()],
            "least recently active first"
        );
        stop(&sup).await;
    });
}

/// A start that finds every slot taken while another start's pause is still stopping waits for
/// that exit instead of failing, and then makes its own room.
#[test]
fn a_start_waits_for_a_pause_already_stopping() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let workers = full_table(&sup, &root, "overlap").await;
        // Only the first can be paused for now.
        for w in &workers[1..] {
            sup.state.borrow_mut().delivery_holds.insert(w.clone(), 1);
        }
        // Each start polled once: the first pauses the worker and waits for its exit, which the
        // main context has not reported yet when the second finds no worker to pause.
        let mut first = {
            let (sup, root) = (sup.clone(), root.clone());
            Box::pin(async move { sup.spawn_worker(&root, spawn_request("first-start")).await })
        };
        let mut second = Box::pin(sup.spawn_worker(&root, spawn_request("second-start")));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(first.as_mut(), cx).is_pending());
            assert!(
                sup.usage(&root).stopping_processes > 0,
                "the pause is stopping"
            );
            assert!(std::future::Future::poll(second.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let first = run_beside(first);
        // Another worker becomes pausable while it waits.
        sup.state.borrow_mut().delivery_holds.remove(&workers[1]);
        let second = second
            .await
            .expect("the second start waited, then made room");
        pump_until(|| first.borrow().is_some()).await;
        let first = first.borrow_mut().take().unwrap().expect("the first start");
        for receipt in [&first, &second] {
            let w = spawned(receipt);
            pump_until(|| live(&sup, &w)).await;
        }
        assert!(!live(&sup, &workers[0]) && !live(&sup, &workers[1]));
        assert!(sup.live_processes() <= sup.limits.live_processes_app);
        sup.state.borrow_mut().delivery_holds.clear();
        stop(&sup).await;
    });
}
