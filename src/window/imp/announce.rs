//! The window's live regions: what a screen reader is told when a thread needs the user.
//!
//! GTK 4.10 has no announcement call, so each window holds two off-screen labels whose text
//! changes are what the screen reader reads: a status (polite) for finished and failed turns and
//! errors, an alert for approvals, questions and forms. What to say is decided by
//! [`agent_core::a11y_announce`]; announcements made while one is waiting are joined and shown
//! once, at idle, on the main loop. Never stored: the text lives in the labels only.

use std::cell::{Cell, RefCell};

use adw::subclass::prelude::*;
use agent_core::a11y_announce::{self, Announcement};
use gtk4::glib;
use gtk4::prelude::*;

use super::AgentTerminalWindow;

/// The two labels, and what waits to be shown in them.
#[derive(Default)]
pub(super) struct LiveRegions {
    status: RefCell<Option<gtk4::Label>>,
    alert: RefCell<Option<gtk4::Label>>,
    pending: RefCell<Vec<Announcement>>,
    queued: Cell<bool>,
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
fn say(label: &gtk4::Label, text: &str) {
    if label.text() == text {
        label.set_text("");
    }
    label.set_text(text);
    label.update_property(&[gtk4::accessible::Property::Label(text)]);
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

    /// Queues `announcement` for the live regions, unless announcements are off.
    pub(super) fn announce(&self, announcement: Announcement) {
        if !self.config.borrow().announce_events {
            return;
        }
        self.live.pending.borrow_mut().push(announcement);
        if self.live.queued.replace(true) {
            return;
        }
        let weak = self.obj().downgrade();
        glib::idle_add_local_once(move || {
            if let Some(obj) = weak.upgrade() {
                obj.imp().flush_announcements();
            }
        });
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

    fn flush_announcements(&self) {
        self.live.queued.set(false);
        let pending = self.live.pending.take();
        // Turned off while they waited: nothing is said.
        if !self.config.borrow().announce_events {
            return;
        }
        let (polite, assertive) = a11y_announce::coalesce(&pending);
        for (label, text) in [(&self.live.status, polite), (&self.live.alert, assertive)] {
            if let (Some(label), Some(text)) = (label.borrow().as_ref(), text) {
                say(label, &text);
            }
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
}
