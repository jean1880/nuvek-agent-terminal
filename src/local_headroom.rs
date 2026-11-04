//! Each Local server's headroom probe ([`agent_core::headroom_probe`]), run at the cadence the app
//! reads usage (the availability scan, and `cross_agent_capabilities` asking with `refresh`), off
//! the main thread ([`agent_kit::headroom_probe::run`] in `gio::spawn_blocking`) and bounded by
//! the probe's own time limit. What it read is kept per server profile id, reported as the
//! profile's usage and written to `usage.json` for tools outside the app.
//!
//! Three states, never two: a reading under the threshold is `low`, at or over it `ok`; a probe
//! that is invalid, fails, times out, prints no number or was read too long ago is `unknown`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::delegation::wire::UsageInfo;
use agent_core::headroom_probe::{self, HeadroomProbe};
use agent_core::model_tags::Headroom;
use gtk4::{gio, glib};
use tracing::debug;

use crate::config::Profile;
use crate::freshness::{age_of, is_due, latest, now_epoch, ACCOUNT_TTL, DUE_SLACK};
use crate::probe::ListenerSet;
use crate::usage_file::HEADROOM_MAX_AGE;

/// One server's probe and what it last read.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReading {
    /// The probe as last configured. A new command forgets what the old one read.
    pub probe: HeadroomProbe,
    /// The last number read, with when.
    pub value: Option<f64>,
    pub read_at: Option<i64>,
    /// The last attempt's end, successful or not: with `read_at`, the due clock.
    pub tried_at: Option<i64>,
    /// The last attempt gave no reading.
    pub failed: bool,
    pub running: bool,
}

impl ProbeReading {
    fn new(probe: HeadroomProbe) -> Self {
        Self {
            probe,
            value: None,
            read_at: None,
            tried_at: None,
            failed: false,
            running: false,
        }
    }

    /// The headroom this reading shows at `now` against the probe configured now.
    pub fn headroom(&self, now: i64) -> Headroom {
        if self.probe.validate().is_err() {
            return Headroom::Unknown;
        }
        self.probe
            .headroom(self.value, age_of(self.read_at, now), HEADROOM_MAX_AGE)
    }
}

/// The probe a server profile has, when it is a Local server with one.
pub fn probe_of(profile: &Profile) -> Option<&HeadroomProbe> {
    (crate::config::profile_driver(profile) == Some(Driver::Local))
        .then_some(profile.headroom_probe.as_ref())
        .flatten()
}

/// The enabled servers among `servers` that have a probe, by profile id.
pub fn configured<'a>(
    servers: impl IntoIterator<Item = &'a Profile>,
) -> Vec<(String, HeadroomProbe)> {
    servers
        .into_iter()
        .filter(|p| !p.disabled)
        .filter_map(|p| probe_of(p).map(|probe| (p.id.clone(), probe.clone())))
        .collect()
}

/// A probed server's usage at `now`: its headroom, the load it implies (0 while `ok`, 1 while
/// `low`, null while `unknown`), how old its reading is and whether the last attempt failed. A
/// reading of another command than `probe`'s counts as none.
pub fn usage(probe: &HeadroomProbe, reading: Option<&ProbeReading>, now: i64) -> UsageInfo {
    let reading = reading.filter(|r| r.probe.argv == probe.argv);
    let current = reading.map(|r| ProbeReading {
        probe: probe.clone(),
        ..r.clone()
    });
    let headroom = current
        .as_ref()
        .map_or(Headroom::Unknown, |r| r.headroom(now));
    UsageInfo {
        headroom,
        load: headroom_probe::load(headroom),
        windows: Vec::new(),
        age_s: current.as_ref().and_then(|r| age_of(r.read_at, now)),
        refresh_failed: current.as_ref().is_some_and(|r| r.failed),
        signed_out: false,
    }
}

/// The app-wide probe readings.
#[derive(Default)]
pub struct LocalHeadroom {
    readings: RefCell<BTreeMap<String, ProbeReading>>,
    listeners: ListenerSet,
}

thread_local! {
    static SHARED: Rc<LocalHeadroom> = Rc::new(LocalHeadroom::default());
}

impl LocalHeadroom {
    /// The app-wide instance (one per main thread).
    pub fn shared() -> Rc<LocalHeadroom> {
        SHARED.with(Rc::clone)
    }

    /// Calls `f` (on the main thread) after every change. The id disconnects it.
    pub fn connect_changed(&self, f: impl Fn() + 'static) -> u64 {
        self.listeners.add(f)
    }

    pub fn disconnect(&self, id: u64) {
        self.listeners.remove(id);
    }

    /// What server `id`'s probe last read.
    pub fn reading(&self, id: &str) -> Option<ProbeReading> {
        self.readings.borrow().get(id).cloned()
    }

    /// Every probed server's reading, by profile id.
    pub fn readings(&self) -> BTreeMap<String, ProbeReading> {
        self.readings.borrow().clone()
    }

    /// The usage of `profile`, when it is a Local server with a probe.
    pub fn usage_of(&self, profile: &Profile, now: i64) -> Option<UsageInfo> {
        let probe = probe_of(profile)?;
        Some(usage(probe, self.reading(&profile.id).as_ref(), now))
    }

    /// Keeps only the servers in `probes`, taking each one's probe as configured now (a new
    /// command forgets the old one's reading). Whether anything changed.
    fn sync(&self, probes: &[(String, HeadroomProbe)]) -> bool {
        let mut readings = self.readings.borrow_mut();
        let before = readings.len();
        readings.retain(|id, _| probes.iter().any(|(p, _)| p == id));
        let mut changed = readings.len() != before;
        for (id, probe) in probes {
            match readings.get_mut(id) {
                Some(r) if r.probe == *probe => {}
                Some(r) if r.probe.argv == probe.argv => {
                    r.probe = probe.clone();
                    changed = true;
                }
                _ => {
                    readings.insert(id.clone(), ProbeReading::new(probe.clone()));
                    changed = true;
                }
            }
        }
        changed
    }

    /// The scan's clock: probes each server whose last reading or attempt is [`ACCOUNT_TTL`] old.
    pub fn refresh_if_due(self: &Rc<Self>, probes: &[(String, HeadroomProbe)], now: i64) {
        self.refresh_older_than(probes, ACCOUNT_TTL, now);
    }

    /// Probes every server not being probed now (a settings change, Scan Now).
    pub fn refresh(self: &Rc<Self>, probes: &[(String, HeadroomProbe)]) {
        self.start(probes, |_| true);
    }

    /// Probes the servers whose last reading or attempt is at least `floor` seconds old at
    /// `now` (a failing probe is not restarted by every call). Returns the servers a caller may
    /// wait for: those started and those already running.
    pub fn refresh_older_than(
        self: &Rc<Self>,
        probes: &[(String, HeadroomProbe)],
        floor: i64,
        now: i64,
    ) -> Vec<String> {
        self.start(probes, |r| {
            is_due(
                latest(r.read_at, r.tried_at),
                now,
                floor.saturating_add(DUE_SLACK),
            )
        })
    }

    fn start(
        self: &Rc<Self>,
        probes: &[(String, HeadroomProbe)],
        due: impl Fn(&ProbeReading) -> bool,
    ) -> Vec<String> {
        let changed = self.sync(probes);
        let mut waiting = Vec::new();
        let mut jobs = Vec::new();
        for (id, reading) in self.readings.borrow_mut().iter_mut() {
            if reading.running {
                waiting.push(id.clone());
            } else if due(reading) {
                reading.running = true;
                waiting.push(id.clone());
                jobs.push((id.clone(), reading.probe.clone()));
            }
        }
        if changed {
            self.listeners.notify();
        }
        // Tests never run a command from the settings of this machine.
        if cfg!(test) {
            for (id, _) in &jobs {
                self.finish(id, None, None, now_epoch());
            }
            return waiting;
        }
        for (id, probe) in jobs {
            let me = Rc::downgrade(self);
            let path = crate::utils::known_shell_path().map(str::to_owned);
            glib::spawn_future_local(async move {
                let argv = probe.argv.clone();
                let value = gio::spawn_blocking(move || {
                    agent_kit::headroom_probe::run(&probe, path.as_deref())
                })
                .await
                .ok()
                .flatten();
                if let Some(me) = me.upgrade() {
                    me.finish(&id, Some(&argv), value, now_epoch());
                }
            });
        }
        waiting
    }

    /// A probe of server `id` ended at `now` with `value`. Dropped when the server's command
    /// changed meanwhile (`argv`, when given, is the command that ran).
    fn finish(&self, id: &str, argv: Option<&[String]>, value: Option<f64>, now: i64) {
        {
            let mut readings = self.readings.borrow_mut();
            let Some(r) = readings.get_mut(id) else {
                return;
            };
            if argv.is_some_and(|a| a != r.probe.argv.as_slice()) {
                r.running = false;
                return;
            }
            r.running = false;
            r.tried_at = Some(now);
            r.failed = value.is_none();
            if value.is_some() {
                r.value = value;
                r.read_at = Some(now);
            }
        }
        if value.is_none() {
            debug!(profile = %id, "the headroom probe gave no reading");
        }
        self.listeners.notify();
    }

    /// [`Self::refresh_older_than`], then waits (on the main loop, never blocking it) until each
    /// probe it may wait for has ended, or `timeout`. A probe still running then is left to
    /// finish.
    pub async fn refresh_and_wait(
        self: &Rc<Self>,
        probes: &[(String, HeadroomProbe)],
        floor: i64,
        now: i64,
        timeout: std::time::Duration,
    ) {
        let waiting = self.refresh_older_than(probes, floor, now);
        let done = {
            let me = Rc::downgrade(self);
            move || {
                me.upgrade().is_none_or(|st| {
                    let readings = st.readings.borrow();
                    waiting
                        .iter()
                        .all(|id| readings.get(id).is_none_or(|r| !r.running))
                })
            }
        };
        if done() {
            return;
        }
        let (send, answer) = tokio::sync::oneshot::channel::<()>();
        let send = RefCell::new(Some(send));
        let id = self.connect_changed(move || {
            if done() {
                if let Some(send) = send.borrow_mut().take() {
                    let _ = send.send(());
                }
            }
        });
        /// Disconnects the listener however the wait ends.
        struct Listening(std::rc::Weak<LocalHeadroom>, u64);
        impl Drop for Listening {
            fn drop(&mut self) {
                if let Some(st) = self.0.upgrade() {
                    st.disconnect(self.1);
                }
            }
        }
        let _listening = Listening(Rc::downgrade(self), id);
        if glib::future_with_timeout(timeout, answer).await.is_err() {
            debug!("headroom probes still running after the wait");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn probe(cmd: &str, low_below: f64) -> HeadroomProbe {
        HeadroomProbe {
            argv: vec![cmd.into()],
            low_below,
            timeout_s: 5,
        }
    }

    fn read(probe: HeadroomProbe, value: Option<f64>, read_at: Option<i64>) -> ProbeReading {
        ProbeReading {
            value,
            read_at,
            tried_at: read_at,
            ..ProbeReading::new(probe)
        }
    }

    #[test]
    fn a_probed_server_reports_its_reading_in_three_states() {
        let p = probe("probe-free", 4096.0);
        let busy = usage(&p, Some(&read(p.clone(), Some(512.0), Some(NOW - 30))), NOW);
        assert_eq!(busy.headroom, Headroom::Low);
        assert_eq!(busy.load, Some(1.0));
        assert_eq!(busy.age_s, Some(30));
        let idle = usage(&p, Some(&read(p.clone(), Some(9000.0), Some(NOW))), NOW);
        assert_eq!(idle.headroom, Headroom::Ok);
        assert_eq!(idle.load, Some(0.0));
        assert!(idle.windows.is_empty() && !idle.signed_out);
        // Never read, read too long ago, or by another command: unknown, never healthy.
        let never = usage(&p, None, NOW);
        assert_eq!((never.headroom, never.load), (Headroom::Unknown, None));
        let old = usage(
            &p,
            Some(&read(
                p.clone(),
                Some(9000.0),
                Some(NOW - HEADROOM_MAX_AGE - 1),
            )),
            NOW,
        );
        assert_eq!(old.headroom, Headroom::Unknown);
        let other = usage(
            &p,
            Some(&read(probe("other", 4096.0), Some(9000.0), Some(NOW))),
            NOW,
        );
        assert_eq!(other.headroom, Headroom::Unknown);
        // A failed attempt after a good reading keeps the reading, and says it failed.
        let failed = ProbeReading {
            failed: true,
            ..read(p.clone(), Some(9000.0), Some(NOW - 10))
        };
        let failing = usage(&p, Some(&failed), NOW);
        assert_eq!(failing.headroom, Headroom::Ok);
        assert!(failing.refresh_failed);
        // The threshold is the one configured now.
        let stricter = probe("probe-free", 10_000.0);
        let now_busy = usage(&stricter, Some(&read(p, Some(9000.0), Some(NOW))), NOW);
        assert_eq!(now_busy.headroom, Headroom::Low);
        // An invalid probe never reads healthy.
        let broken = HeadroomProbe {
            timeout_s: 0,
            ..probe("probe-free", 1.0)
        };
        let r = read(broken.clone(), Some(9000.0), Some(NOW));
        assert_eq!(usage(&broken, Some(&r), NOW).headroom, Headroom::Unknown);
    }

    #[test]
    fn only_enabled_local_servers_with_a_probe_are_probed() {
        let local = |id: &str, probe_: Option<HeadroomProbe>, disabled: bool| Profile {
            id: id.into(),
            driver: Some(Driver::Local),
            headroom_probe: probe_,
            disabled,
            ..Profile::default()
        };
        let p = probe("probe-free", 1.0);
        let claude = Profile {
            id: "c".into(),
            command: "claude".into(),
            headroom_probe: Some(p.clone()),
            ..Profile::default()
        };
        let servers = [
            local("a", Some(p.clone()), false),
            local("b", None, false),
            local("d", Some(p.clone()), true),
            claude.clone(),
        ];
        assert_eq!(configured(&servers), vec![("a".to_owned(), p)]);
        assert!(
            probe_of(&claude).is_none(),
            "only a Local server has a probe"
        );
    }

    #[test]
    fn a_changed_command_forgets_the_old_reading_and_a_removed_server_goes() {
        let st = Rc::new(LocalHeadroom::default());
        let p = probe("probe-free", 1.0);
        st.sync(&[("a".into(), p.clone()), ("b".into(), p.clone())]);
        st.finish("a", None, Some(5.0), NOW);
        assert_eq!(st.reading("a").and_then(|r| r.value), Some(5.0));
        // A new threshold keeps the number; a new command drops it.
        assert!(st.sync(&[("a".into(), probe("probe-free", 9.0))]));
        assert_eq!(st.reading("a").and_then(|r| r.value), Some(5.0));
        assert!(st.reading("b").is_none(), "a server without a probe goes");
        st.sync(&[("a".into(), probe("other", 9.0))]);
        assert_eq!(st.reading("a").and_then(|r| r.value), None);
        // A late answer from the old command is dropped.
        st.finish("a", Some(&["probe-free".to_owned()]), Some(7.0), NOW);
        assert_eq!(st.reading("a").and_then(|r| r.value), None);
    }

    #[test]
    fn probes_run_when_due_and_a_failure_keeps_the_last_reading() {
        let st = Rc::new(LocalHeadroom::default());
        let probes = [("a".to_owned(), probe("probe-free", 1.0))];
        // In tests nothing runs: each started probe ends at once with no reading.
        assert_eq!(st.refresh_older_than(&probes, 60, NOW), ["a"]);
        let r = st.reading("a").expect("tried");
        assert!(r.failed && !r.running && r.tried_at.is_some());
        // Tried just now: not due again within the floor.
        assert!(st.refresh_older_than(&probes, 60, now_epoch()).is_empty());
        st.finish("a", None, Some(3.0), NOW);
        st.finish("a", None, None, NOW + 5);
        let r = st.reading("a").expect("kept");
        assert_eq!((r.value, r.read_at), (Some(3.0), Some(NOW)));
        assert!(r.failed);
    }
}
