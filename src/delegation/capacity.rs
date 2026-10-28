//! Only an observed exit releases process capacity. An idle delegated worker's process can be
//! paused to make room (least recently active first) or after being idle for the user's idle
//! time: a lossless, silent suspension its next instruction resumes ([`Supervisor::ensure_child`]).
use super::supervisor::{Supervisor, WorkerRt};
use crate::agent_proc::{ExitWatch, StopPhase};
use agent_core::delegation::limits::Usage;
use agent_core::delegation::{ToolError, WorkerId};
use gtk4::glib;
use std::rc::Rc;
use std::time::{Duration, Instant};
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
}
impl Supervisor {
    pub(super) fn usage(&self, root: &WorkerId) -> Usage {
        let state = self.state.borrow();
        let mut u = Usage::default();
        for w in state.workers.values().filter(|w| w.row.parent_id.is_some()) {
            u.live_processes += w.generations.len() as u32;
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
    /// failed, with its process up and started, nothing of its at work, no input queued for it,
    /// and no live descendant (whose link through it a stop would cut). The session has its
    /// own checks too ([`crate::chat::session::ChatSession::suspend_child_if_idle`]).
    pub(super) fn child_suspendable(&self, worker: &WorkerId) -> bool {
        {
            let state = self.state.borrow();
            let Some(w) = state.workers.get(worker) else {
                return false;
            };
            let queued = state.deferred.get(worker).is_some_and(|q| !q.is_empty())
                || state.controls.values().any(|c| &c.worker == worker);
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
    fn idle_children(&self) -> Vec<(WorkerId, Instant)> {
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
    #[cfg_attr(not(test), allow(dead_code))] // the window's minute sweep calls it
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
    /// Room for one more agent process: when every slot is taken, pauses the least recently
    /// active idle worker that can resume losslessly (never `except`, the one about to start)
    /// and waits for its exit. Refused with [`agent_core::delegation::limits::processes_full`]
    /// when none can be paused.
    pub(super) async fn make_room(
        self: &Rc<Self>,
        root: &WorkerId,
        except: Option<&WorkerId>,
    ) -> Result<(), ToolError> {
        let mut tried: Vec<WorkerId> = Vec::new();
        loop {
            let usage = self.usage(root);
            if usage.live_processes < self.limits.live_processes_app {
                return Ok(());
            }
            let next = self
                .idle_children()
                .into_iter()
                .map(|(id, _)| id)
                .find(|id| Some(id) != except && !tried.contains(id));
            let Some(worker) = next else {
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
