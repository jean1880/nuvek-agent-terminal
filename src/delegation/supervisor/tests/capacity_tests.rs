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
