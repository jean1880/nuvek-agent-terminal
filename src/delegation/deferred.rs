//! Supervisor-owned delivery queues. Every write follows a durable reservation and a fresh authority check.
use super::seam::{DelegatedInput, DelegatedOutcome, InputSource};
use super::supervisor::{error, Supervisor};
use agent_core::delegation::authority::delivery_allowed;
use agent_core::delegation::mailbox::{
    plan_delivery, DeliveryPlan, DeliveryState, Direction, TargetRuntime,
};
use agent_core::delegation::wire::Delivery as DeliveryPolicy;
use agent_core::delegation::wire::*;
use agent_core::delegation::{ErrorCode, RunId, ToolError, WorkerId};
use agent_core::event::{Envelope, Event};
use agent_kit::store::delegation_typed::*;
use serde_json::Value;
use std::rc::Rc;
use tracing::{info, warn};

/// The parent's report of a run cancelled before its task or message reached the agent.
pub(super) const CANCELLED_BEFORE_ITS_TURN: &str =
    "Cancelled before its turn started: the agent was never given this work.";

/// How long a steer may wait for the run's answer before its outcome is recorded unknown
/// (see `arm_steer_settle`).
pub(super) const STEER_SETTLE: std::time::Duration = std::time::Duration::from_secs(30);

/// The most overdue steers whose late answer is still awaited.
pub(super) const LATE_STEERS: usize = 64;

/// How many times the settle wait an overdue steer's late answer is still awaited (10 minutes
/// by default). Past it the correlation is dropped and the outcome stays unknown, so a worker
/// that never hears back can be paused again.
const LATE_STEER_WINDOW: u32 = 20;

/// How long a task handed to a child may wait for its turn to open (see `arm_task_start`).
pub(super) const TASK_START_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// The run a delivery starts (`None` for a live write or steer, which start none), whether the
/// delivery reserved it itself, and the process generation it was meant for.
struct Dispatch {
    run: Option<RunId>,
    fresh: bool,
    generation: agent_core::delegation::ProcessGeneration,
}

#[derive(Clone)]
pub(super) struct Delivery {
    pub worker: WorkerId,
    pub input: DelegatedInput,
    pub policy: DeliveryPolicy,
    pub reserved: bool,
}
impl Supervisor {
    fn target_runtime(&self, worker: &WorkerId) -> Result<TargetRuntime, ToolError> {
        let state = self.state.borrow();
        let w = state
            .workers
            .get(worker)
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
        Ok(TargetRuntime {
            driver: w.driver,
            live_input: w.driver != agent_core::adapter::Driver::Agy,
            active_run: (w.readback.ready && w.pending.is_none())
                .then(|| w.run.clone())
                .flatten(),
            process_alive: !w.generations.is_empty(),
            closed: w.row.state == "closed",
            awaiting_result: w.run.is_some(),
        })
    }
    pub(super) async fn send_worker(
        self: &Rc<Self>,
        caller: &WorkerId,
        r: SendRequest,
    ) -> Result<Value, ToolError> {
        if let Some(v) = self.existing(caller, &ToolRequest::Send(r.clone())).await? {
            return Ok(v);
        }
        let epoch = self.epoch(caller)?;
        let queued = self
            .state
            .borrow()
            .deferred
            .values()
            .map(|q| q.len())
            .sum::<usize>();
        if queued >= self.limits.pending_starts_app as usize {
            return Err(error(
                ErrorCode::CapacityReached,
                "deferred delivery queue is full",
            ));
        }
        let (target, direction) = {
            let state = self.state.borrow();
            let from = &state.workers[caller];
            let target = match &r.target {
                Target::Parent => from
                    .row
                    .parent()
                    .ok_or_else(|| error(ErrorCode::NotFound, "root has no parent"))?,
                Target::Worker(id) => id.clone(),
            };
            let to = state
                .workers
                .get(&target)
                .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
            if to.root != from.root {
                return Err(error(ErrorCode::NotFound, "unknown worker"));
            }
            let dir = if to.row.parent_id.as_deref() == Some(caller.as_str()) {
                Direction::ParentToChild
            } else if from.row.parent_id.as_deref() == Some(target.as_str()) {
                Direction::ChildToParent
            } else {
                return Err(error(
                    ErrorCode::NotFound,
                    "messages only go between a worker and its parent",
                ));
            };
            delivery_allowed(
                dir,
                r.kind,
                self.principal_mode(caller),
                to.row.task_mode_kind(),
            )
            .map_err(|c| error(c, "delivery exceeds authority"))?;
            (target, dir)
        };
        let plan = if direction == Direction::ChildToParent {
            DeliveryPlan::QueueNextTurn
        } else {
            plan_delivery(
                &self.target_runtime(&target)?,
                r.delivery,
                r.expected_run_id.as_ref(),
            )
            .map_err(|c| error(c, "delivery cannot target this run"))?
        };
        if direction == Direction::ParentToChild && self.state.borrow().workers[&target].dispatching
        {
            return Err(error(
                ErrorCode::CapacityReached,
                "recipient is dispatching another entry",
            ));
        }
        // From here to the delivery, the recipient is never paused to make room: the readiness
        // read now must still hold once the acceptance is in. Dropped on every return.
        let _hold = (direction == Direction::ParentToChild).then(|| self.hold_delivery(&target));
        let needs_rebind = |sup: &Self| {
            let state = sup.state.borrow();
            state.workers.get(&target).is_some_and(|w| {
                w.row.parent_id.is_some()
                    && (w.session.is_none()
                        || w.generations.is_empty()
                        || !w.readback.ready
                        || w.row.delegation_status == "rebind_required"
                        || !state.token_current(w))
            })
        };
        let needs_queue = matches!(plan, DeliveryPlan::QueueNextTurn) || needs_rebind(self);
        let live = match &plan {
            DeliveryPlan::SteerNow(r) => Some(r.clone()),
            DeliveryPlan::WriteLive { target } => target.clone(),
            _ => None,
        };
        let root = self.state.borrow().workers[caller].root.clone();
        let a = self.accept_request(
            caller,
            &root,
            ToolRequest::Send(r.clone()),
            agent_core::delegation::operation::Resolution {
                target: Some(target.clone()),
                ..Default::default()
            },
            AcceptPlan::Send(SendPlan {
                initial: if needs_queue {
                    DeliveryState::QueuedNextTurn
                } else {
                    DeliveryState::Queued
                },
                target_run: live,
            }),
            Vec::new(),
        );
        let (accepted, _) = self
            .accept(a, !needs_queue && direction == Direction::ParentToChild)
            .await?;
        let a = match accepted {
            Accepted::Replay(r) => return super::ops::replay_value(r),
            Accepted::New(a) => a,
        };
        if direction == Direction::ChildToParent {
            // Queued for the parent's mailbox; a root also hears it without asking.
            if let Some(id) = a.message.clone() {
                self.queue_wake(
                    caller,
                    agent_core::delegation::wake::Outcome::Message,
                    &r.text,
                    agent_core::delegation::wake::Shown::Message(id),
                );
            }
            return Ok(a.receipt);
        }
        let id = a
            .message
            .clone()
            .ok_or_else(|| error(ErrorCode::InternalError, "send accepted without a message"))?;
        let label = self.state.borrow().workers[caller].row.name.clone();
        let entry = Delivery {
            worker: target.clone(),
            input: DelegatedInput {
                operation: a.operation.clone(),
                epoch,
                source: InputSource::Message { id, kind: r.kind },
                from: caller.clone(),
                from_label: label,
                text: agent_core::redact::redact(&r.text),
                plan,
                tag: a.operation.to_string(),
            },
            policy: r.delivery,
            reserved: !needs_queue,
        };
        // A recipient whose process went away during the acceptance anyway (it exited, or was
        // stopped) is started again through the queue, never handed a write it cannot take.
        if needs_queue || needs_rebind(self) {
            self.state
                .borrow_mut()
                .deferred
                .entry(target.clone())
                .or_default()
                .push_back(entry);
            self.drain_deferred(&target);
        } else {
            self.deliver(entry).await;
        }
        Ok(a.receipt)
    }
    pub(super) fn dispatch_pending(self: &Rc<Self>, worker: &WorkerId) {
        let input = {
            let mut state = self.state.borrow_mut();
            state
                .workers
                .get_mut(worker)
                .filter(|w| w.readback.ready)
                .and_then(|w| w.pending.take())
        };
        if let Some(input) = input {
            let sup = self.clone();
            let worker = worker.clone();
            gtk4::glib::spawn_future_local(async move {
                sup.deliver(Delivery {
                    worker,
                    input,
                    policy: DeliveryPolicy::NextTurn,
                    reserved: true,
                })
                .await;
            });
        }
    }
    async fn reserve_run(&self, entry: &Delivery) -> Result<Option<RunId>, ToolError> {
        if !matches!(entry.input.plan, DeliveryPlan::StartTurn) {
            return Ok(None);
        }
        let root = self
            .state
            .borrow()
            .workers
            .get(&entry.worker)
            .map(|w| w.root.clone())
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
        if self.state.borrow().workers[&entry.worker].run.is_some() {
            return Ok(self.state.borrow().workers[&entry.worker].run.clone());
        }
        let usage = self.usage(&root);
        agent_core::delegation::limits::admit_spawn(&self.limits, &usage, false)?;
        let worker = entry.worker.clone();
        let epoch = entry.input.epoch;
        let generation = self.state.borrow().workers[&worker].generation.0;
        let run = self
            .services
            .durable("reserve delegated follow-up run", move |s| {
                s.atomically(|s| {
                    if let Some(run) = s.active_delegation_run(worker.as_str())? {
                        return RunId::parse(&run.id)
                            .map_err(|e| agent_kit::store::StoreError::Failed(e.to_string()));
                    }
                    let root = s
                        .delegation_root_of_worker(&worker)?
                        .ok_or(agent_kit::store::StoreError::WriterGone)?;
                    if root.authority_epoch != epoch.0 as i64 {
                        return Err(agent_kit::store::StoreError::Failed(
                            "stale authority while reserving follow-up".into(),
                        ));
                    }
                    let suffix = s.new_id()?.replace('-', "");
                    let run = RunId::from_suffix(&suffix)
                        .map_err(|e| agent_kit::store::StoreError::Failed(e.to_string()))?;
                    s.insert_delegation_run(&agent_kit::store::delegation::RunRow {
                        id: run.to_string(),
                        worker_id: worker.to_string(),
                        process_generation: generation as i64,
                        provider_thread_id: None,
                        native_turn_id: None,
                        authority_epoch: epoch.0 as i64,
                        state: "reserved".into(),
                        terminal_reason: None,
                        usage_json: None,
                        created_at: super::supervisor::now(),
                        updated_at: super::supervisor::now(),
                    })?;
                    Ok(run)
                })
            })
            .await?;
        if let Some(w) = self.state.borrow_mut().workers.get_mut(&entry.worker) {
            w.run = Some(run.clone());
        }
        Ok(Some(run))
    }
    async fn deliver(self: &Rc<Self>, mut entry: Delivery) {
        if let Some(w) = self.state.borrow_mut().workers.get_mut(&entry.worker) {
            w.dispatching = true;
        }
        if !entry.reserved {
            // A requeued refused steer is accepted again too: one reservation, with every check.
            let op = entry.input.operation.clone();
            match self
                .services
                .durable("reserve deferred delivery", move |s| {
                    Ok(s.reserve_delegation_dispatch(&op)?.map(|_| ()))
                })
                .await
            {
                Ok(Ok(_)) => entry.reserved = true,
                Ok(Err(e)) | Err(e) => {
                    if let Some(w) = self.state.borrow_mut().workers.get_mut(&entry.worker) {
                        w.end_dispatch();
                    }
                    self.record_fact(entry.input.operation, DispatchFact::Failed(e));
                    return;
                }
            }
        }
        let Some((prior, generation)) = self
            .state
            .borrow()
            .workers
            .get(&entry.worker)
            .map(|w| (w.run.clone(), w.generation))
        else {
            self.record_fact(
                entry.input.operation,
                DispatchFact::Failed(error(ErrorCode::NotFound, "unknown worker")),
            );
            return;
        };
        // A sibling's close or exit while this waited moved the epoch on, and this with it.
        let reserved_run = match self
            .settled_delivery_epoch(&entry.input.from, &entry.worker, entry.input.epoch)
            .await
        {
            Ok(epoch) => {
                entry.input.epoch = epoch;
                self.reserve_run(&entry).await
            }
            Err(e) => Err(e),
        };
        // The run this delivery starts, and whether it reserved that run itself.
        let used = match reserved_run {
            Ok(run) => Dispatch {
                fresh: run.is_some() && run != prior,
                run,
                generation,
            },
            Err(e) => {
                if let Some(w) = self.state.borrow_mut().workers.get_mut(&entry.worker) {
                    w.end_dispatch();
                }
                self.record_fact(entry.input.operation, DispatchFact::Failed(e));
                return;
            }
        };
        match self
            .settled_delivery_epoch(&entry.input.from, &entry.worker, entry.input.epoch)
            .await
        {
            Ok(epoch) => entry.input.epoch = epoch,
            Err(e) => {
                if let Some(w) = self.state.borrow_mut().workers.get_mut(&entry.worker) {
                    w.end_dispatch();
                }
                self.fail_dispatch(&entry, &used, e);
                return;
            }
        }
        let expected = match &entry.input.plan {
            DeliveryPlan::SteerNow(r) => Some(r),
            DeliveryPlan::WriteLive { target } => target.as_ref(),
            _ => None,
        };
        // No await from this check to the actual session write.
        if let Some(w) = self.state.borrow_mut().workers.get_mut(&entry.worker) {
            w.end_dispatch();
        }
        if let Err(e) = self.dispatch_check(
            &entry.input.from,
            &entry.worker,
            entry.input.epoch,
            expected,
        ) {
            self.fail_dispatch(&entry, &used, e);
            return;
        }
        let session = self.child_session(&entry.worker);
        let Some(session) = session else {
            self.fail_dispatch(
                &entry,
                &used,
                error(ErrorCode::ProviderUnavailable, "child session unavailable"),
            );
            return;
        };
        let outcome = session.deliver_delegated(entry.input.clone());
        self.state
            .borrow_mut()
            .dispatching
            .remove(&entry.input.operation);
        let turn_started = matches!(outcome, DelegatedOutcome::TurnStarted);
        match outcome {
            DelegatedOutcome::Refused(c) => {
                self.fail_dispatch(&entry, &used, error(c, "session refused delegated input"));
                return;
            }
            DelegatedOutcome::SteerSent { tag } => {
                self.record_fact(entry.input.operation.clone(), DispatchFact::Submitted);
                self.state
                    .borrow_mut()
                    .controls
                    .insert(tag.clone(), entry.clone());
                self.arm_steer_settle(&entry.worker, tag);
            }
            DelegatedOutcome::Written | DelegatedOutcome::TurnStarted => {
                self.record_fact(entry.input.operation.clone(), DispatchFact::Submitted);
                if self
                    .state
                    .borrow()
                    .workers
                    .get(&entry.worker)
                    .is_some_and(|w| w.driver != agent_core::adapter::Driver::Agy)
                {
                    self.state
                        .borrow_mut()
                        .controls
                        .insert(entry.input.tag.clone(), entry.clone());
                }
            }
        }
        let run = self
            .state
            .borrow()
            .workers
            .get(&entry.worker)
            .and_then(|w| w.run.clone());
        if let (true, Some(run)) = (turn_started, run.as_ref()) {
            info!(worker = %entry.worker, %run, "delegated task handed to the agent");
            self.arm_task_start(&entry.worker, run);
        }
        if let Some(run) = run {
            let sup = self.weak.clone();
            let worker = entry.worker.clone();
            let timeout = self.limits.run_deadline_default_ms;
            gtk4::glib::spawn_future_local(async move {
                gtk4::glib::timeout_future(std::time::Duration::from_millis(timeout)).await;
                if let Some(s) = sup.upgrade() {
                    let session = {
                        let state = s.state.borrow();
                        state
                            .workers
                            .get(&worker)
                            .filter(|w| w.run.as_ref() == Some(&run))
                            .and_then(|w| w.session())
                    };
                    if let Some(session) = session {
                        s.cancel_bound_run(&worker, &run, "Delegated run deadline elapsed");
                        session.note("Delegated run deadline elapsed.");
                    }
                }
            });
        }
    }
    pub(super) fn drain_deferred(self: &Rc<Self>, worker: &WorkerId) {
        let entry = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state.workers.get(worker) else {
                return;
            };
            if w.run.is_some()
                || w.dispatching
                || w.row.state == "closed"
                || w.row.delegation_status == "restricted"
            {
                return;
            }
            let entry = state.deferred.get_mut(worker).and_then(|q| q.pop_front());
            if entry.is_some() {
                if let Some(w) = state.workers.get_mut(worker) {
                    w.dispatching = true;
                }
            }
            entry
        };
        let Some(mut entry) = entry else {
            return;
        };
        let sup = self.clone();
        let worker = worker.clone();
        gtk4::glib::spawn_future_local(async move {
            if let Some(w) = sup.state.borrow_mut().workers.get_mut(&worker) {
                w.dispatching = true;
            }
            let ready = sup.ensure_child(&worker, &mut entry.input).await;
            if let Some(w) = sup.state.borrow_mut().workers.get_mut(&worker) {
                w.end_dispatch();
            }
            if let Err(e) = ready {
                sup.record_fact(entry.input.operation, DispatchFact::Failed(e));
                return;
            }
            entry.input.plan = DeliveryPlan::StartTurn;
            sup.deliver(entry).await;
        });
    }
    pub(super) fn delivery_control(self: &Rc<Self>, worker: &WorkerId, env: &Envelope) {
        let Some(tag) = env.request.as_ref() else {
            return;
        };
        let entry = take_scoped_control(&mut self.state.borrow_mut().controls, worker, tag);
        if let Some(entry) = entry {
            match &env.event {
                Event::ControlResult { ok: Some(_), .. } => self.confirm_delivery(entry),
                Event::ControlResult { error: Some(_), .. } => self.steer_refused(worker, entry),
                _ => {}
            }
            return;
        }
        // An answer for a steer already recorded unknown: it settles it now.
        let late = {
            let mut state = self.state.borrow_mut();
            let at = state
                .late_steers
                .iter()
                .position(|(t, e)| t == tag && &e.worker == worker);
            at.and_then(|at| state.late_steers.remove(at))
                .map(|(_, entry)| entry)
        };
        let Some(entry) = late else {
            return;
        };
        match &env.event {
            Event::ControlResult { ok: Some(_), .. } => self.confirm_late_steer(entry),
            // Refused: never delivered, proven now. `auto` goes to the next turn; `this_run`
            // stays unknown, as recorded.
            Event::ControlResult { error: Some(_), .. } if entry.policy == DeliveryPolicy::Auto => {
                self.steer_refused(worker, entry)
            }
            _ => {
                info!(%worker, operation = %entry.input.operation,
                    "a late answer for a strict steer; its outcome stays unknown");
            }
        }
    }
    /// A steer the run refused: `auto` falls back to the next turn (`mailbox::on_steer_refused`):
    /// the message waits for it again and its operation is accepted again, durably, before it is
    /// queued, so the next delivery reserves it with every check and confirms it in the run it
    /// reaches. That delivery is a new attempt with a correlation of its own: a late answer for
    /// this one can never settle it. `this_run` has no next turn: it fails.
    fn steer_refused(self: &Rc<Self>, worker: &WorkerId, mut entry: Delivery) {
        if entry.policy != DeliveryPolicy::Auto {
            self.record_fact(
                entry.input.operation,
                DispatchFact::Failed(error(ErrorCode::StaleRun, "strict steer was refused")),
            );
            return;
        }
        let InputSource::Message { id: message, .. } = entry.input.source.clone() else {
            return;
        };
        entry.reserved = false;
        entry.input.plan = DeliveryPlan::StartTurn;
        entry.input.tag = fresh_tag(&entry.input.operation);
        let operation = entry.input.operation.clone();
        let sup = self.clone();
        let worker = worker.clone();
        gtk4::glib::spawn_future_local(async move {
            let requeued = sup
                .services
                .durable("requeue refused steer", move |s| {
                    s.requeue_refused_delegation_steer(&message)
                })
                .await;
            if let Err(e) = requeued {
                warn!(%worker, %operation, "a refused steer could not be requeued");
                sup.record_fact(operation, DispatchFact::Failed(e));
                return;
            }
            info!(%worker, %operation, "refused steer requeued for the next turn");
            sup.state
                .borrow_mut()
                .deferred
                .entry(worker.clone())
                .or_default()
                .push_back(entry);
            // The run that refused it may already be over: nothing else would drain it.
            sup.drain_deferred(&worker);
        });
    }
    /// A steer's answer may come after its turn ended (the correlation outlives the turn). One
    /// with no answer within [`STEER_SETTLE`] may yet have been taken: its outcome is recorded
    /// `unknown` (a replay says so), it is never delivered again on a guess, and its correlation
    /// is kept for a late answer, which settles it (`delivery_control`), for
    /// [`LATE_STEER_WINDOW`] times the wait.
    pub(super) fn arm_steer_settle(self: &Rc<Self>, worker: &WorkerId, tag: String) {
        let wait = self.state.borrow().steer_settle.unwrap_or(STEER_SETTLE);
        let weak = self.weak.clone();
        let worker = worker.clone();
        gtk4::glib::spawn_future_local(async move {
            gtk4::glib::timeout_future(wait).await;
            let Some(sup) = weak.upgrade() else {
                return;
            };
            let entry = take_scoped_control(&mut sup.state.borrow_mut().controls, &worker, &tag);
            let Some(entry) = entry else {
                return;
            };
            warn!(%worker, operation = %entry.input.operation, ?wait,
                "a steer got no answer in time; its outcome is unknown");
            sup.record_fact(entry.input.operation.clone(), DispatchFact::Unknown);
            {
                let mut state = sup.state.borrow_mut();
                if state.late_steers.len() >= LATE_STEERS {
                    state.late_steers.pop_front();
                }
                state.late_steers.push_back((tag.clone(), entry));
            }
            drop(sup);
            // No answer at all: stop waiting, so the worker is not held up for ever.
            gtk4::glib::timeout_future(wait * LATE_STEER_WINDOW).await;
            let Some(sup) = weak.upgrade() else {
                return;
            };
            sup.state
                .borrow_mut()
                .late_steers
                .retain(|(t, _)| t != &tag);
        });
    }
    /// A late answer accepted an overdue steer: it was delivered, into the run it was pinned to.
    fn confirm_late_steer(&self, entry: Delivery) {
        let InputSource::Message { id, .. } = entry.input.source else {
            return;
        };
        let run = match &entry.input.plan {
            DeliveryPlan::SteerNow(run) => Some(run.clone()),
            _ => None,
        };
        info!(worker = %entry.worker, operation = %entry.input.operation,
            "an overdue steer was accepted after all");
        self.services
            .write("confirm overdue steer", None, true, move |s| {
                s.confirm_late_delegation_steer(&id, run.as_ref())
            });
    }
    fn confirm_delivery(&self, entry: Delivery) {
        // A steer the provider took was taken into the run it was pinned to, even when its
        // answer comes after that run ended; anything else, into the worker's current run.
        let run = match &entry.input.plan {
            DeliveryPlan::SteerNow(run) => Some(run.clone()),
            _ => self
                .state
                .borrow()
                .workers
                .get(&entry.worker)
                .and_then(|w| w.run.clone()),
        };
        self.record_fact(entry.input.operation.clone(), DispatchFact::Confirmed);
        if let InputSource::Message { id, .. } = entry.input.source {
            self.services
                .write("confirm delegated message", None, true, move |s| {
                    s.record_delegation_delivery(
                        &id,
                        DeliveryState::ProviderConfirmed,
                        run.as_ref(),
                    )?;
                    Ok(())
                });
        }
    }
    pub(super) fn delivery_echo(&self, worker: &WorkerId, v: &Value) {
        let text = if v["type"] == "user" && v["isReplay"] == true {
            v.pointer("/message/content")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else if v.pointer("/params/item/type").and_then(Value::as_str) == Some("userMessage") {
            v.pointer("/params/item/content")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
        } else {
            None
        };
        let Some(text) = text else {
            return;
        };
        let tags = echo_tags(&self.state.borrow().controls, worker, &text);
        if tags.len() > 1 {
            let entries = {
                let mut state = self.state.borrow_mut();
                tags.iter()
                    .filter_map(|tag| state.controls.remove(tag))
                    .collect::<Vec<_>>()
            };
            for entry in entries {
                self.record_fact(entry.input.operation, DispatchFact::Unknown);
            }
            (self.services.report)(
                "A delegated input echo was ambiguous; its delivery remains unknown.",
            );
            return;
        }
        if let Some(tag) = tags.first() {
            let entry = self.state.borrow_mut().controls.remove(tag);
            if let Some(e) = entry {
                self.confirm_delivery(e);
            }
        }
    }
    /// A delivery that failed before anything reached the agent. Its operation fails, and the
    /// run it would have started ends with it, in memory and in the store alike, so `list` (which
    /// reads the store's run, as `cancel` does) never names a run that is already over. A spawn's
    /// task that cannot be delivered leaves the worker nothing to do: the worker fails, saying why.
    ///
    /// The handling is scoped to the delivery's own run and process generation. If either moved
    /// on, or the worker was closed, that path owns the outcome and nothing else is done. A
    /// cancel that landed after the task left `pending` (so `cancel_bound_run` could not end it)
    /// ends the run as interrupted, as a cancel does, and never turns into a worker failure.
    fn fail_dispatch(self: &Rc<Self>, entry: &Delivery, used: &Dispatch, e: ToolError) {
        warn!(
            worker = %entry.worker,
            operation = %entry.input.operation,
            code = %e.code,
            reason = %e.message,
            "delegated dispatch failed"
        );
        self.state
            .borrow_mut()
            .dispatching
            .remove(&entry.input.operation);
        let fact_only = |e: ToolError| {
            self.record_fact(entry.input.operation.clone(), DispatchFact::Failed(e));
        };
        let Some(run) = used.run.clone() else {
            fact_only(e);
            return;
        };
        let (current, stopping) = {
            let state = self.state.borrow();
            state
                .workers
                .get(&entry.worker)
                .map_or((false, false), |w| {
                    (
                        w.generation == used.generation
                            && w.run.as_ref() == Some(&run)
                            && w.row.state != "closed",
                        w.row.state == "stopping",
                    )
                })
        };
        if !current {
            fact_only(e);
            return;
        }
        let task = matches!(entry.input.source, InputSource::Task);
        // Only a run this delivery owns (the task's, or one it reserved) and whose turn never
        // opened is ended here; a cancel of a live turn ends that turn itself.
        if stopping && !(task || used.fresh) {
            fact_only(e);
            return;
        }
        if task && !stopping {
            fact_only(e.clone());
            let reason = format!("its task could not be delivered: {}", e.message);
            self.fail_worker(&entry.worker, error(e.code, &reason));
            return;
        }
        if !task && !used.fresh {
            fact_only(e);
            return;
        }
        // A cancel that stopped it: the run ends interrupted as any cancelled run does, through
        // the one completion (its end, the worker's state, the terminal event and links, the
        // failed dispatch after the run's end, the parent's report), and the parent hears of it
        // once that committed. A task never delivered leaves its child nothing to do: it is
        // stopped then, never before, or a turn ending in flight would end the run first.
        if stopping {
            self.complete_run_with(
                &entry.worker,
                agent_core::event::TurnState::Interrupted,
                None,
                super::publish::RunEnd {
                    reason: Some("cancelled".into()),
                    result: Some(CANCELLED_BEFORE_ITS_TURN.into()),
                    dispatch_failure: Some((entry.input.operation.clone(), e)),
                    stop_after: task.then_some(used.generation),
                },
            );
            self.notify_registry();
            return;
        }
        // Failed with the delivery. The run ends first, and the failed dispatch is recorded
        // after it in the same transaction: recording the fact on a spawn's still-open run would
        // otherwise end that run as `failed`. A run that already ended is left as it is.
        if let Some(w) = self.state.borrow_mut().workers.get_mut(&entry.worker) {
            w.last_run = w.run.take();
        }
        let reason = e.code.as_str().to_owned();
        let operation = entry.input.operation.clone();
        let fact = DispatchFact::Failed(e);
        self.services
            .write("end undelivered delegated run", None, true, move |s| {
                s.atomically(|s| {
                    let at = super::supervisor::now();
                    s.end_delegation_run_if_open(run.as_str(), "failed", Some(&reason), at)?;
                    // As `record_fact` does: a fact already recorded is not recorded again.
                    s.record_delegation_dispatch_once(&operation, &fact)?;
                    Ok(())
                })
            });
        self.notify_registry();
        if !task {
            self.drain_deferred(&entry.worker);
        }
    }
    /// A task handed to the agent must open its turn within the deadline, or the worker is
    /// stalled: it fails with that reason (stopping its process and ending its run) rather than
    /// staying `starting` with nothing to show. A run whose turn opened (`opened_run`), a run
    /// that already ended, and a worker being cancelled or closed are left alone.
    fn arm_task_start(self: &Rc<Self>, worker: &WorkerId, run: &RunId) {
        let deadline = self
            .state
            .borrow()
            .task_start_deadline
            .unwrap_or(TASK_START_DEADLINE);
        let Some(generation) = self
            .state
            .borrow()
            .workers
            .get(worker)
            .map(|w| w.generation)
        else {
            return;
        };
        let sup = self.weak.clone();
        let worker = worker.clone();
        let run = run.clone();
        gtk4::glib::spawn_future_local(async move {
            gtk4::glib::timeout_future(deadline).await;
            let Some(sup) = sup.upgrade() else {
                return;
            };
            // Evidence is per run: a follow-up inherits `waiting` or `failed` from the run
            // before it until its own turn opens, so the worker's state proves nothing here.
            let stalled = sup.state.borrow().workers.get(&worker).is_some_and(|w| {
                w.generation == generation
                    && w.run.as_ref() == Some(&run)
                    && w.opened_run.as_ref() != Some(&run)
                    && !matches!(w.row.state.as_str(), "stopping" | "closed")
            });
            if stalled {
                warn!(%worker, %run, ?deadline, "delegated task never started; worker stalled");
                sup.fail_worker(
                    &worker,
                    error(
                        ErrorCode::ProviderUnavailable,
                        &format!(
                            "the agent was given its task but did not start it within {deadline:?}"
                        ),
                    ),
                );
            }
        });
    }
    pub(super) fn queue_human(self: &Rc<Self>, worker: &WorkerId, text: &str) {
        let text = text.to_owned();
        self.act_as_parent(worker, |sup, caller, worker, key| async move {
            let r = SendRequest {
                target: Target::Worker(worker),
                text,
                kind: agent_core::delegation::mailbox::MessageKind::Instruction,
                reply_to: None,
                delivery: DeliveryPolicy::NextTurn,
                expected_run_id: None,
                expected_authority_epoch: None,
                idempotency_key: key,
            };
            sup.send_worker(&caller, r).await
        });
    }
}
/// A correlation tag no earlier attempt of `operation` used.
fn fresh_tag(operation: &agent_core::delegation::OperationId) -> String {
    format!("{operation}#{}", gtk4::glib::uuid_string_random())
}

fn take_scoped_control(
    controls: &mut std::collections::HashMap<String, Delivery>,
    worker: &WorkerId,
    tag: &str,
) -> Option<Delivery> {
    if controls
        .get(tag)
        .is_some_and(|entry| &entry.worker == worker)
    {
        controls.remove(tag)
    } else {
        None
    }
}
fn echo_tags(
    controls: &std::collections::HashMap<String, Delivery>,
    worker: &WorkerId,
    text: &str,
) -> Vec<String> {
    controls
        .iter()
        .filter(|(_, entry)| {
            &entry.worker == worker
                && crate::chat::session::ChatSession::render_delegated_input(&entry.input) == text
        })
        .map(|(tag, _)| tag.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::delegation::mailbox::MessageKind;
    use agent_core::delegation::{MessageId, OperationId};
    fn entry(worker: &WorkerId, id: &str, text: &str) -> Delivery {
        Delivery {
            worker: worker.clone(),
            input: DelegatedInput {
                operation: OperationId::from_suffix(&id.repeat(32)).unwrap(),
                epoch: agent_core::delegation::AuthorityEpoch(0),
                source: InputSource::Message {
                    id: MessageId::from_suffix(&id.repeat(32)).unwrap(),
                    kind: MessageKind::Instruction,
                },
                from: WorkerId::from_suffix(&"f".repeat(32)).unwrap(),
                from_label: "parent".into(),
                text: text.into(),
                plan: DeliveryPlan::StartTurn,
                tag: format!("tag-{id}"),
            },
            policy: DeliveryPolicy::NextTurn,
            reserved: true,
        }
    }
    #[test]
    fn identical_bodies_match_only_their_exact_message_frame() {
        let worker = WorkerId::from_suffix(&"a".repeat(32)).unwrap();
        let first = entry(&worker, "1", "same body");
        let second = entry(&worker, "2", "same body");
        let text = crate::chat::session::ChatSession::render_delegated_input(&second.input);
        let controls =
            std::collections::HashMap::from([("first".into(), first), ("second".into(), second)]);
        assert_eq!(echo_tags(&controls, &worker, &text), vec!["second"]);
    }
    #[test]
    fn suffix_collision_and_unframed_echo_never_confirm_a_delivery() {
        let worker = WorkerId::from_suffix(&"a".repeat(32)).unwrap();
        let short = entry(&worker, "1", "suffix");
        let long = entry(&worker, "2", "prefix suffix");
        let text = crate::chat::session::ChatSession::render_delegated_input(&long.input);
        let controls =
            std::collections::HashMap::from([("short".into(), short), ("long".into(), long)]);
        assert_eq!(echo_tags(&controls, &worker, &text), vec!["long"]);
        assert!(echo_tags(&controls, &worker, "unframed prefix suffix").is_empty());
    }
    #[test]
    fn ambiguous_duplicate_frame_is_detected_and_sibling_control_cannot_consume_it() {
        let worker = WorkerId::from_suffix(&"a".repeat(32)).unwrap();
        let sibling = WorkerId::from_suffix(&"b".repeat(32)).unwrap();
        let input = entry(&worker, "1", "body");
        let text = crate::chat::session::ChatSession::render_delegated_input(&input.input);
        let mut controls =
            std::collections::HashMap::from([("one".into(), input.clone()), ("two".into(), input)]);
        assert_eq!(echo_tags(&controls, &worker, &text).len(), 2);
        assert!(take_scoped_control(&mut controls, &sibling, "one").is_none());
        assert_eq!(controls.len(), 2);
        assert!(take_scoped_control(&mut controls, &worker, "one").is_some());
        assert_eq!(controls.len(), 1);
    }
}
