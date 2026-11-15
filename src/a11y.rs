//! Accessible names for the controls GTK and libadwaita build inside their own widgets, which
//! the app cannot label where it builds them.

use adw::prelude::*;

/// Names the increase and decrease buttons inside `spin` after what it sets, such as
/// "Text Scale". GTK gives them no accessible name of their own.
pub fn name_spin_buttons(spin: &gtk4::SpinButton, what: &str) {
    let mut child = spin.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if !widget.is::<gtk4::Button>() {
            continue;
        }
        let verb = if widget.has_css_class("up") {
            "Increase"
        } else if widget.has_css_class("down") {
            "Decrease"
        } else {
            continue;
        };
        widget.update_property(&[gtk4::accessible::Property::Label(&format!("{verb} {what}"))]);
    }
}

/// Names the checkmark button an entry row shows once its text has changed. libadwaita builds
/// that button when the text first changes, so the row is named again on each change.
pub fn name_apply_button(row: &adw::EntryRow) {
    name_apply_button_now(row);
    row.connect_changed(name_apply_button_now);
}

fn name_apply_button_now(row: &adw::EntryRow) {
    let mut stack = vec![row.clone().upcast::<gtk4::Widget>()];
    while let Some(widget) = stack.pop() {
        if widget.is::<gtk4::Button>() && widget.has_css_class("apply-button") {
            widget.update_property(&[gtk4::accessible::Property::Label("Apply")]);
            return;
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
}
