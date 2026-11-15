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

/// Whether `widget` is an entry row's apply button, on any libadwaita the app supports.
/// libadwaita 1.9 gives it an `apply-button` style class; 1.5 has only `suggested-action` and
/// `circular`, so there it is the row's round accent button that is not one of the row's
/// prefixes or suffixes (which hold the app's own widgets).
pub fn is_apply_button(widget: &gtk4::Widget) -> bool {
    if !widget.is::<gtk4::Button>() {
        return false;
    }
    if widget.has_css_class("apply-button") {
        return true;
    }
    if !(widget.has_css_class("suggested-action") && widget.has_css_class("circular")) {
        return false;
    }
    let mut at = widget.parent();
    while let Some(parent) = at {
        if parent.has_css_class("suffixes") || parent.has_css_class("prefixes") {
            return false;
        }
        if parent.is::<adw::EntryRow>() {
            return true;
        }
        at = parent.parent();
    }
    false
}

/// Names the checkmark button an entry row has (it is built with the row and shown once the
/// text has been edited). Found by what it is, not where it sits: libadwaita 1.5 puts it
/// beside the editable area, 1.9 inside it.
pub fn name_apply_button(row: &adw::EntryRow) {
    let mut stack = vec![row.clone().upcast::<gtk4::Widget>()];
    while let Some(widget) = stack.pop() {
        if is_apply_button(&widget) {
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
