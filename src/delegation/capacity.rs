//! Only an observed exit releases process capacity. An idle delegated worker's process can be
//! paused to make room (least recently active first) or after being idle for the user's idle
//! time: a lossless, silent suspension its next instruction resumes ([`Supervisor::ensure_child`]).
//!
//! Admission reserves: a slot admitted for a launch counts as a process from the moment it is
//! admitted ([`LaunchSlot`]) until the launch holds one (or fails), so starts that await before
//! their process exists can never take the same slot twice.
use super::supervisor::{Supervisor, WorkerRt};
use crate::agent_proc::{ExitWatch, StopPhase};
use agent_core::delegation::limits::Usage;
use agent_core::delegation::{ToolError, WorkerId};
use gtk4::glib;
use std::collections::HashSet;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// The longest an admission waits for a process already stopping to free its slot: a stop's
/// own worst case ([`wait_exit`] kills after five seconds), with room for its exit to be seen.
const STOPPING_WAIT: Duration = Duration::from_secs(8);

pub(super) async fn wait_exit(watch: &ExitWatch) {
    let started = Instant::now();
    let mut killed = false;
    while watch.phase() != StopPhase::Exited {
        if !killed && started.elapsed() >= Duration::from_secs(5) {
            watch.kill_now();
            killed = true;
        }
        glib::timeout_future(Duration::from_millis(20)).await;
    }
}

/// Runs `release` on the supervisor's state now, or, if that is borrowed (a guard dropped
/// inside a borrow), as soon as the main context is free: a guard never leaks its claim.
fn release(sup: &Weak<Supervisor>, release: impl FnOnce(&Supervisor) + 'static) {
    let Some(s) = sup.upgrade() else {
        return;
    };
    if s.state.try_borrow_mut().is_ok() {
        release(&s);
        return;
    }
    let sup = sup.clone();
    glib::idle_add_local_once(move || {
        if let Some(s) = sup.upgrade() {
            release(&s);
        }
    });
}

/// One process slot admitted for a launch and not yet holding a process. Counted as a process
/// in [`Supervisor::usage`] until dropped; once [`Self::bind`] names the worker it launches, it
/// is counted at most once with that worker's own process or start.
pub(super) struct LaunchSlot {
    sup: Weak<Supervisor>,
    id: u64,
}
impl LaunchSlot {
    /// The slot is `worker`'s launch.
    pub(super) fn bind(&self, worker: &WorkerId) {
        if let Some(s) = self.sup.upgrade() {
            if let Some(slot) = s.state.borrow_mut().launch_slots.get_mut(&self.id) {
                *slot = Some(worker.clone());
            }
        }
    }
}
impl Drop for LaunchSlot {
    fn drop(&mut self) {
        let id = self.id;
        release(&self.sup, move |s| {
            s.state.borrow_mut().launch_slots.remove(&id);
        });
    }
}

/// A delivery on its way to a worker: it is never paused under it ([`Supervisor::hold_delivery`]).
pub(super) struct DeliveryHold {
    sup: Weak<Supervisor>,
    worker: WorkerId,
}
impl Drop for DeliveryHold {
    fn drop(&mut self) {
        let worker = self.worker.clone();
        release(&self.sup, move |s| {
            let mut state = s.state.borrow_mut();
            if let Some(n) = state.delivery_holds.get_mut(&worker) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    state.delivery_holds.remove(&worker);
                }
            }
        });
    }
}

impl WorkerRt {
    /// Something of the worker's is under way: a run, input waiting for or on its way to the
    /// process, a binding being prepared, or a request put to the user.
    pub(super) fn at_work(&self) -> bool {
        self.run.is_some()
            || self.pending.is_some()
            || self.preparing
            || self.dispatching
            || self.waiting_on_user()
    }
    /// A dispatch to the worker is over, delivered or not: it was active until now.
    pub(super) fn end_dispatch(&mut self) {
        self.dispatching = false;
        self.last_active = Instant::now();
    }
}
impl Supervisor {
    pub(super) fn usage(&self, root: &WorkerId) -> Usage {
        let state = self.state.borrow();
        let mut u = Usage::default();
        let mut bound = HashSet::new();
        for worker in state.launch_slots.values() {
            match worker {
                Some(w) => {
                    bound.insert(w.clone());
                }
                None => u.live_processes += 1,
            }
        }
        for (id, w) in state
            .workers
            .iter()
            .filter(|(_, w)| w.row.parent_id.is_some())
        {
            let (alive, current) = w.session.as_ref().map_or((false, None), |s| {
                (s.lifecycle().alive, Some(s.process_generation()))
            });
            // A process of a session no longer running it, or of an earlier generation, is on
            // its way out.
            let stopping = w
                .generations
                .keys()
                .filter(|g| !alive || Some(**g) != current)
                .count() as u32;
            // Started, or admitted to start, with its process not spawned yet.
            let starting = alive && current.is_some_and(|g| !w.generations.contains_key(&g));
            let launching = u32::from(starting || bound.contains(id));
            u.live_processes += (w.generations.len() as u32).max(launching);
            // A stop whose slot the worker's own restart carries over frees nothing.
            if !bound.contains(id) {
                u.stopping_processes += stopping;
            }
            if !w.generations.is_empty() && w.run.is_none() {
                u.idle_workers += 1;
            }
            if w.run.is_some() {
                u.active_runs_app += 1;
                if &w.root == root {
                    u.active_runs_root += 1;
                }
            }
            if w.pending.is_some() {
                u.pending_app += 1;
                if &w.root == root {
                    u.pending_root += 1;
                }
            }
        }
        // A slot naming a worker not registered yet (a spawn between its acceptance and the
        // worker's registration) is still a launch on its way.
        u.live_processes += bound
            .iter()
            .filter(|w| !state.workers.contains_key(*w))
            .count() as u32;
        u.waits_app = state.waits.len() as u32;
        u
    }
    /// The agent processes delegated workers hold now, idle ones included.
    pub(super) fn live_processes(&self) -> u32 {
        self.state
            .borrow()
            .workers
            .values()
            .filter(|w| w.row.parent_id.is_some())
            .map(|w| w.generations.len() as u32)
            .sum()
    }
    /// Takes one process slot for a launch, counted from now ([`LaunchSlot`]).
    pub(super) fn reserve_launch(&self) -> LaunchSlot {
        let mut state = self.state.borrow_mut();
        let id = state.next_launch_slot;
        state.next_launch_slot = id.wrapping_add(1);
        state.launch_slots.insert(id, None);
        LaunchSlot {
            sup: self.weak.clone(),
            id,
        }
    }
    /// Keeps `worker` from being paused while a delivery is on its way to it, until the hold
    /// is dropped.
    pub(super) fn hold_delivery(&self, worker: &WorkerId) -> DeliveryHold {
        *self
            .state
            .borrow_mut()
            .delivery_holds
            .entry(worker.clone())
            .or_default() += 1;
        DeliveryHold {
            sup: self.weak.clone(),
            worker: worker.clone(),
        }
    }
    /// Whether a worker below `worker` is still alive: its process up, or work under way.
    fn has_live_descendant(&self, worker: &WorkerId) -> bool {
        let below: Vec<WorkerId> = {
            let state = self.state.borrow();
            state
                .workers
                .iter()
                .filter(|(id, w)| {
                    *id != worker
                        && (!w.generations.is_empty()
                            || (!matches!(w.row.state.as_str(), "closed" | "failed")
                                && w.at_work()))
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        below.iter().any(|id| self.visible(worker, id, false))
    }
    /// Whether the supervisor may pause `worker`'s process: a delegated worker, not closed or
    /// failed, with its process up and started, nothing of its at work, no input queued or on
    /// its way to it, and no live descendant (whose link through it a stop would cut). The
    /// session has its own checks too
    /// ([`crate::chat::session::ChatSession::suspend_child_if_idle`]).
    pub(super) fn child_suspendable(&self, worker: &WorkerId) -> bool {
        {
            let state = self.state.borrow();
            let Some(w) = state.workers.get(worker) else {
                return false;
            };
            let queued = state.deferred.get(worker).is_some_and(|q| !q.is_empty())
                || state.controls.values().any(|c| &c.worker == worker)
                || state.delivery_holds.contains_key(worker);
            if w.row.parent_id.is_none()
                || matches!(w.row.state.as_str(), "closed" | "failed")
                || w.at_work()
                || queued
                || w.generations.is_empty()
                || !w.readback.ready
                || w.session.is_none()
            {
                return false;
            }
        }
        !self.has_live_descendant(worker)
    }
    /// Every worker [`Self::child_suspendable`] would pause, least recently active first, with
    /// when it was last active.
    pub(super) fn idle_children(&self) -> Vec<(WorkerId, Instant)> {
        let candidates: Vec<(WorkerId, Instant)> = self
            .state
            .borrow()
            .workers
            .iter()
            .filter(|(_, w)| w.row.parent_id.is_some() && !w.generations.is_empty())
            .map(|(id, w)| (id.clone(), w.last_active))
            .collect();
        let mut idle: Vec<_> = candidates
            .into_iter()
            .filter(|(id, _)| self.child_suspendable(id))
            .collect();
        idle.sort_by_key(|(_, at)| *at);
        idle
    }
    /// Asks `worker`'s session to pause its process (it refuses when anything would be lost).
    /// Whether it stopped, and the watch of the generation that stopped. Never with the
    /// supervisor's state borrowed: the session calls back into it.
    fn pause_child(&self, worker: &WorkerId) -> Option<Option<ExitWatch>> {
        let session = self.state.borrow().workers.get(worker)?.session.clone()?;
        let watch = session.exit_watch();
        if !session.suspend_child_if_idle() {
            return None;
        }
        tracing::info!(%worker, "paused an idle delegated worker's process; its next instruction resumes it");
        Some(watch)
    }
    /// Pauses every delegated worker idle for at least `idle_minutes` (0: never), as the idle
    /// threads of a window are. Each resumes on its next instruction. How many were paused.
    pub(crate) fn stop_idle_children(&self, idle_minutes: u32) -> usize {
        if idle_minutes == 0 || self.state.borrow().shutting_down {
            return 0;
        }
        let threshold = Duration::from_secs(u64::from(idle_minutes) * 60);
        let now = Instant::now();
        let due: Vec<WorkerId> = self
            .idle_children()
            .into_iter()
            .filter(|(_, at)| now.saturating_duration_since(*at) >= threshold)
            .map(|(id, _)| id)
            .collect();
        let paused = due
            .iter()
            .filter(|id| self.pause_child(id).is_some())
            .count();
        if paused > 0 {
            self.notify_registry();
        }
        paused
    }
    /// A process slot for one more launch, reserved at once ([`LaunchSlot`]): when every slot
    /// is taken, pauses the least recently active idle worker that can resume losslessly (never
    /// `except`, the one about to start) and waits for its exit, or waits (bounded) for a
    /// process already stopping. Refused with [`agent_core::delegation::limits::processes_full`]
    /// when neither frees one.
    pub(super) async fn make_room(
        self: &Rc<Self>,
        root: &WorkerId,
        except: Option<&WorkerId>,
    ) -> Result<LaunchSlot, ToolError> {
        let mut tried: Vec<WorkerId> = Vec::new();
        let started = Instant::now();
        loop {
            let usage = self.usage(root);
            if usage.live_processes < self.limits.live_processes_app {
                // Checked and taken with no await between: no other start can take it too.
                return Ok(self.reserve_launch());
            }
            let next = self
                .idle_children()
                .into_iter()
                .map(|(id, _)| id)
                .find(|id| Some(id) != except && !tried.contains(id));
            let Some(worker) = next else {
                // Another start's pause (or any stop) is under way: its exit frees a slot.
                if usage.stopping_processes > 0 && started.elapsed() < STOPPING_WAIT {
                    glib::timeout_future(Duration::from_millis(20)).await;
                    continue;
                }
                return Err(agent_core::delegation::limits::processes_full(
                    &self.limits,
                    &usage,
                ));
            };
            tried.push(worker.clone());
            let Some(watch) = self.pause_child(&worker) else {
                continue;
            };
            let generation = self
                .state
                .borrow()
                .workers
                .get(&worker)
                .map(|w| w.generation);
            if let Some(watch) = watch {
                wait_exit(&watch).await;
            }
            // Its watch reports the exit too; whichever comes first frees the slot once.
            if let Some(generation) = generation {
                self.observed_exit(&worker, generation);
            }
            self.notify_registry();
        }
    }
}
