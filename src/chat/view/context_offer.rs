//! The offer above the composer when a thread's context runs low: what is left, then Compact
//! (the agent's own command, where it has one), Continue in a New Thread, and Dismiss. It only
//! offers: nothing is sent until a button is pressed, and the press goes through the same path
//! as the gauge's popover. It is never stored, so a reopened thread does not show it again;
//! when it appears is [`agent_core::context_meter::WarnLatch`]'s to decide.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

use agent_core::context_meter::{ActionRow, MeterAction};

use super::cards::wrapped_text;

/// What the user pressed: an action, or Dismiss (`None`).
type PickHandler = Rc<dyn Fn(Option<MeterAction>)>;

pub struct ContextOffer {
    pub revealer: gtk4::Revealer,
    text: gtk4::Label,
    /// One button per offered action, in [`MeterAction::ALL`] order; Breakdown has none.
    buttons: Vec<(MeterAction, gtk4::Button)>,
    /// Why the buttons are off, while they are.
    reason: gtk4::Label,
    #[cfg(test)]
    dismiss: gtk4::Button,
    handler: Rc<RefCell<Option<PickHandler>>>,
}

impl ContextOffer {
    pub fn new() -> Self {
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        card.add_css_class("context-offer");
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        let icon = gtk4::Image::from_icon_name("agent-compact-symbolic");
        icon.add_css_class("context-offer-icon");
        icon.set_valign(gtk4::Align::Start);
        icon.set_accessible_role(gtk4::AccessibleRole::Presentation);
        top.append(&icon);
        let text = wrapped_text("", &["context-offer-text"]);
        text.set_hexpand(true);
        text.set_xalign(0.0);
        top.append(&text);
        let dismiss = gtk4::Button::from_icon_name("at-window-close-symbolic");
        dismiss.add_css_class("flat");
        dismiss.set_valign(gtk4::Align::Start);
        dismiss.set_tooltip_text(Some("Dismiss until the context runs low again"));
        dismiss.update_property(&[gtk4::accessible::Property::Label(
            "Dismiss the low context offer",
        )]);
        top.append(&dismiss);
        card.append(&top);

        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        row.set_halign(gtk4::Align::End);
        let handler: Rc<RefCell<Option<PickHandler>>> = Rc::new(RefCell::new(None));
        let mut buttons = Vec::new();
        for action in MeterAction::ALL {
            if action == MeterAction::Breakdown {
                continue;
            }
            let button = gtk4::Button::with_label(match action {
                MeterAction::Compact => "Compact",
                _ => "Continue in a New Thread",
            });
            button.add_css_class("pill");
            // A narrow window wraps the words rather than widening the thread.
            if let Some(label) = button.child().and_downcast::<gtk4::Label>() {
                label.set_wrap(true);
                label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                label.set_justify(gtk4::Justification::Center);
            }
            if action == MeterAction::Continue {
                button.add_css_class("suggested-action");
            }
            button.connect_clicked({
                let handler = handler.clone();
                move |_| {
                    let f = handler.borrow().clone();
                    if let Some(f) = f {
                        f(Some(action));
                    }
                }
            });
            row.append(&button);
            buttons.push((action, button));
        }
        card.append(&row);
        let reason = wrapped_text("", &["dim-label", "context-offer-reason"]);
        reason.set_xalign(1.0);
        reason.set_visible(false);
        card.append(&reason);
        dismiss.connect_clicked({
            let handler = handler.clone();
            move |_| {
                let f = handler.borrow().clone();
                if let Some(f) = f {
                    f(None);
                }
            }
        });

        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideUp)
            .transition_duration(180)
            .reveal_child(false)
            .child(&card)
            .visible(false)
            .build();
        // A closed revealer still measures its child's width; once it has slid away it is
        // hidden outright, so a note nobody sees never widens the thread.
        revealer.connect_child_revealed_notify(|r| {
            if !r.reveals_child() && !r.is_child_revealed() {
                r.set_visible(false);
            }
        });
        Self {
            revealer,
            text,
            buttons,
            reason,
            #[cfg(test)]
            dismiss,
            handler,
        }
    }

    /// Called with what the user pressed (`None`: Dismiss).
    pub fn connect_pick(&self, f: impl Fn(Option<MeterAction>) + 'static) {
        *self.handler.borrow_mut() = Some(Rc::new(f));
    }

    /// Shows the offer saying `text`.
    pub fn show(&self, text: &str) {
        self.text.set_text(text);
        self.revealer.set_visible(true);
        self.revealer.set_sensitive(true);
        self.revealer.set_reveal_child(true);
    }

    /// Updates what an offer already shown says (a newer reading), without showing a hidden one.
    pub fn refresh(&self, text: &str) {
        if self.is_shown() {
            self.text.set_text(text);
        }
    }

    /// Takes the offer down. Its buttons go dead at once, not when it has slid away, so a
    /// second press during the slide never acts again.
    pub fn hide(&self) {
        self.revealer.set_sensitive(false);
        self.revealer.set_reveal_child(false);
    }

    pub fn is_shown(&self) -> bool {
        self.revealer.reveals_child()
    }

    /// The buttons as the agent allows them now (Compact only where it has its own command;
    /// both off, with the reason, while a turn runs).
    pub fn set_actions(&self, rows: &[ActionRow]) {
        let mut reason = None;
        for (action, button) in &self.buttons {
            match rows.iter().find(|r| r.action == *action) {
                Some(row) => {
                    button.set_visible(true);
                    button.set_label(&row.label);
                    button.set_sensitive(row.enabled);
                    if reason.is_none() {
                        reason.clone_from(&row.reason);
                    }
                }
                None => button.set_visible(false),
            }
        }
        self.reason.set_text(reason.as_deref().unwrap_or(""));
        self.reason.set_visible(reason.is_some());
    }

    /// What the offer says while shown, and each visible button's label and sensitivity (tests).
    #[cfg(test)]
    pub fn shown(&self) -> Option<(String, Vec<(String, bool)>)> {
        self.is_shown().then(|| {
            (
                self.text.text().to_string(),
                self.buttons
                    .iter()
                    .filter(|(_, b)| b.is_visible())
                    .map(|(_, b)| (b.label().unwrap_or_default().to_string(), b.is_sensitive()))
                    .collect(),
            )
        })
    }

    /// Presses the button for `action` (`None`: Dismiss), as a click would (tests).
    #[cfg(test)]
    pub fn press(&self, action: Option<MeterAction>) {
        match action {
            None => self.dismiss.emit_clicked(),
            Some(a) => {
                if let Some((_, b)) = self.buttons.iter().find(|(x, _)| *x == a) {
                    b.emit_clicked();
                }
            }
        }
    }
}
