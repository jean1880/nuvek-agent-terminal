//! A worker that put a request to the user shows as waiting until the last one is settled; a
//! stopped turn and an exited process take their requests with them, a failed turn does not.
use super::*;
use agent_core::event::{
    Decision, Envelope, Event, Question, QuestionSource, ResponseCapability, TurnState,
};

fn approval(request: &str) -> Envelope {
    Envelope::new(Event::ApprovalRequested {
        tool: "Bash".into(),
        title: None,
        input: Value::Null,
        reason: None,
        options: vec![Decision::Allow, Decision::Deny],
        response: ResponseCapability::Live,
        remembers: None,
    })
    .request(request)
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

fn resolved(request: &str) -> Envelope {
    Envelope::new(Event::ApprovalResolved {
        decision: Decision::Allow,
    })
    .request(request)
}

/// A worker with a run open, as if its task were running.
async fn running_worker(sup: &Rc<Supervisor>, root: &WorkerId, key: &str) -> WorkerId {
    let receipt = sup.spawn_worker(root, spawn_request(key)).await.unwrap();
    let worker = spawned(&receipt);
    pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
    {
        let mut state = sup.state.borrow_mut();
        let w = state.workers.get_mut(&worker).unwrap();
        w.run = Some(agent_core::delegation::RunId::parse("r_awareRun1").unwrap());
        w.row.state = "running".into();
    }
    worker
}

fn feed(sup: &Rc<Supervisor>, worker: &WorkerId, env: Envelope) {
    let scope = sup.scope(worker).expect("scope");
    sup.on_child_envelope(worker, scope, env, (1, None), false, None);
}

fn state_of(sup: &Rc<Supervisor>, worker: &WorkerId) -> String {
    sup.state.borrow().workers[worker].row.state.clone()
}

fn summary_of(sup: &Rc<Supervisor>, root: &WorkerId, worker: &WorkerId) -> WorkerSummary {
    let thread = sup.state.borrow().roots[root].row.root_thread_id.clone();
    sup.summaries(&thread)
        .into_iter()
        .find(|s| &s.worker_id == worker)
        .expect("summary")
}

#[test]
fn a_worker_waits_until_its_last_open_request_is_settled() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = running_worker(&sup, &root, "two-approvals").await;
        feed(&sup, &worker, approval("a"));
        feed(&sup, &worker, approval("b"));
        assert_eq!(state_of(&sup, &worker), "waiting");
        assert!(summary_of(&sup, &root, &worker).waiting_on_user);

        feed(&sup, &worker, resolved("a"));
        assert_eq!(state_of(&sup, &worker), "waiting", "one is still open");
        assert!(summary_of(&sup, &root, &worker).waiting_on_user);

        feed(&sup, &worker, resolved("b"));
        assert_eq!(state_of(&sup, &worker), "running");
        assert!(!summary_of(&sup, &root, &worker).waiting_on_user);
        stop(&sup).await;
    });
}

#[test]
fn an_expired_request_stops_the_wait() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = running_worker(&sup, &root, "expiry").await;
        feed(&sup, &worker, approval("a"));
        assert_eq!(state_of(&sup, &worker), "waiting");
        feed(
            &sup,
            &worker,
            Envelope::new(Event::ApprovalExpired).request("a"),
        );
        assert_eq!(state_of(&sup, &worker), "running");
        stop(&sup).await;
    });
}

#[test]
fn a_question_makes_the_worker_wait_and_its_answer_ends_it() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = running_worker(&sup, &root, "question").await;
        feed(&sup, &worker, question("q1"));
        assert_eq!(state_of(&sup, &worker), "waiting");
        assert!(summary_of(&sup, &root, &worker).waiting_on_user);
        feed(
            &sup,
            &worker,
            Envelope::new(Event::QuestionResolved { answered: true }).request("q1"),
        );
        assert_eq!(state_of(&sup, &worker), "running");
        stop(&sup).await;
    });
}

#[test]
fn an_interrupted_turn_clears_its_requests() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = running_worker(&sup, &root, "interrupt").await;
        feed(&sup, &worker, approval("a"));
        feed(&sup, &worker, question("q1"));
        feed(
            &sup,
            &worker,
            Envelope::turn_completed(TurnState::Interrupted, None),
        );
        assert!(!summary_of(&sup, &root, &worker).waiting_on_user);
        assert!(sup.state.borrow().workers[&worker].open_requests.is_empty());
        stop(&sup).await;
    });
}

#[test]
fn a_failed_turn_keeps_a_background_request() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = running_worker(&sup, &root, "failed").await;
        feed(&sup, &worker, approval("a"));
        feed(
            &sup,
            &worker,
            Envelope::turn_completed(TurnState::Failed, Some("boom")),
        );
        assert!(
            summary_of(&sup, &root, &worker).waiting_on_user,
            "a request a background worker made outlives a failed turn"
        );
        stop(&sup).await;
    });
}

#[test]
fn an_exited_process_takes_its_unanswered_question_with_it() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = running_worker(&sup, &root, "exit").await;
        feed(&sup, &worker, question("q1"));
        assert!(summary_of(&sup, &root, &worker).waiting_on_user);
        feed(
            &sup,
            &worker,
            Envelope::new(Event::SessionExited {
                code: Some(0),
                expected: true,
            }),
        );
        assert!(!summary_of(&sup, &root, &worker).waiting_on_user);
        stop(&sup).await;
    });
}

#[test]
fn a_request_of_an_earlier_process_does_not_make_a_new_one_wait() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let worker = running_worker(&sup, &root, "rotation").await;
        feed(&sup, &worker, question("q1"));
        // The process was replaced without its exit being seen.
        {
            let mut state = sup.state.borrow_mut();
            let w = state.workers.get_mut(&worker).unwrap();
            w.generation = ProcessGeneration(w.generation.0 + 1);
        }
        assert!(!summary_of(&sup, &root, &worker).waiting_on_user);
        stop(&sup).await;
    });
}
