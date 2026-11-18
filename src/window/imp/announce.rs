//! The window's live regions: what a screen reader is told when a thread needs the user.
//!
//! GTK 4.10 has no announcement call, so each window holds two off-screen labels whose text
//! changes are what the screen reader reads: a status (polite) for finished and failed turns and
//! errors, an alert for approvals, questions and forms. What to say is decided by
//! [`agent_core::a11y_announce`]; what waits is bounded there ([`a11y_announce::Pending`]) and
//! read as at most three items and a count of the rest. Alerts are shown at idle, on the main
//! loop; polite texts after [`POLITE_DELAY`], so a burst (several threads finishing at once) is
//! one update. Never stored: the text lives in the labels only.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use adw::subclass::prelude::*;
use agent_core::a11y_announce::{self, Announcement, Urgency};
use gtk4::glib;
use gtk4::prelude::*;

use super::AgentTerminalWindow;

/// How long a polite announcement waits for others to join it.
const POLITE_DELAY: Duration = Duration::from_millis(200);

/// The two labels, and what waits to be shown in them.
#[derive(Default)]
pub(super) struct LiveRegions {
    status: RefCell<Option<gtk4::Label>>,
    alert: RefCell<Option<gtk4::Label>>,
    pending: RefCell<a11y_announce::Pending>,
    /// A flush is queued: the polite one on its timer, the alert one at idle.
    polite_queued: Cell<bool>,
    alert_queued: Cell<bool>,
}

/// One live region: drawn nowhere (no opacity, no size worth the name, no pointer), but in the
/// accessibility tree with its role.
fn region(role: gtk4::AccessibleRole, name: &str) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .accessible_role(role)
        .halign(gtk4::Align::Start)
        .valign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .max_width_chars(1)
        .single_line_mode(true)
        .can_target(false)
        .can_focus(false)
        .focusable(false)
        .opacity(0.0)
        .css_classes(["live-region"])
        .build();
    label.set_widget_name(name);
    label
}

/// Shows `text` in `label` so its change is seen: the same words again are cleared first, so a
/// repeat is still announced.
///
/// Only the label's text is set. A GtkLabel keeps its accessible name equal to its text, and
/// that name changing is the one event a screen reader (Orca) reads a status or alert by; setting
/// the name again as a property besides would be a second route to the same event, which GTK
/// drops as unchanged at best and a screen reader reads twice at worst.
fn say(label: &gtk4::Label, text: &str) {
    if label.text() == text {
        label.set_text("");
    }
    label.set_text(text);
}

impl AgentTerminalWindow {
    /// The live regions' holder, for the window's overlay: never measured, never hit.
    pub(super) fn build_live_regions(&self) -> gtk4::Widget {
        let holder = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .can_target(false)
            .build();
        let status = region(gtk4::AccessibleRole::Status, "announcements");
        let alert = region(gtk4::AccessibleRole::Alert, "announcement-alerts");
        holder.append(&status);
        holder.append(&alert);
        *self.live.status.borrow_mut() = Some(status);
        *self.live.alert.borrow_mut() = Some(alert);
        holder.upcast()
    }

    /// Queues `announcement` for its live region, unless announcements are off.
    pub(super) fn announce(&self, announcement: Announcement) {
        if !self.config.borrow().announce_events {
            return;
        }
        let urgency = announcement.urgency;
        self.live.pending.borrow_mut().push(announcement);
        let queued = match urgency {
            Urgency::Polite => &self.live.polite_queued,
            Urgency::Assertive => &self.live.alert_queued,
        };
        if queued.replace(true) {
            return;
        }
        let weak = self.obj().downgrade();
        let flush = move || {
            if let Some(obj) = weak.upgrade() {
                obj.imp().flush_announcements(urgency);
            }
        };
        match urgency {
            Urgency::Polite => {
                glib::timeout_add_local_once(POLITE_DELAY, flush);
            }
            Urgency::Assertive => {
                glib::idle_add_local_once(flush);
            }
        }
    }

    /// What `event` on `thread` (titled `title`) says, if anything, queued.
    pub(super) fn announce_event(
        &self,
        event: &agent_core::event::Event,
        title: &str,
        looking: bool,
        notified: bool,
        finished: bool,
    ) {
        let cx = a11y_announce::Context {
            thread: title,
            looking,
            notified,
            finished,
        };
        if let Some(announcement) = a11y_announce::announce(event, &cx) {
            self.announce(announcement);
        }
    }

    /// Shows what waits of `urgency` in its region.
    fn flush_announcements(&self, urgency: Urgency) {
        let (queued, label) = match urgency {
            Urgency::Polite => (&self.live.polite_queued, &self.live.status),
            Urgency::Assertive => (&self.live.alert_queued, &self.live.alert),
        };
        queued.set(false);
        let text = self.live.pending.borrow_mut().take(urgency);
        // Turned off while they waited: nothing is said.
        if !self.config.borrow().announce_events {
            self.live.pending.borrow_mut().clear();
            return;
        }
        if let (Some(label), Some(text)) = (label.borrow().as_ref(), text) {
            say(label, &text);
        }
    }

    /// The status and alert regions' texts.
    #[cfg(test)]
    pub(super) fn live_region_texts(&self) -> (String, String) {
        let text = |l: &RefCell<Option<gtk4::Label>>| {
            l.borrow()
                .as_ref()
                .map(|l| l.text().to_string())
                .unwrap_or_default()
        };
        (text(&self.live.status), text(&self.live.alert))
    }

    /// The status and alert regions.
    #[cfg(test)]
    pub(super) fn live_region_labels(&self) -> (gtk4::Label, gtk4::Label) {
        let get = |l: &RefCell<Option<gtk4::Label>>| l.borrow().clone().expect("a live region");
        (get(&self.live.status), get(&self.live.alert))
    }

    /// Whether a flush of either region is queued.
    #[cfg(test)]
    pub(super) fn live_region_queued(&self) -> bool {
        self.live.polite_queued.get() || self.live.alert_queued.get()
    }
}
