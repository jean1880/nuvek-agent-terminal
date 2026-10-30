//! Two-answer questions, as the window asks them: a cancel and an accept response, Escape
//! answering cancel.

use adw::prelude::*;

/// A question with a `cancel` and an `accept` response, each `(id, label)`. `look` styles the
/// accept button; `default_accept` makes Enter accept rather than cancel. An empty `body` sets
/// none.
pub fn confirm_dialog(
    heading: &str,
    body: &str,
    cancel: (&str, &str),
    accept: (&str, &str),
    look: adw::ResponseAppearance,
    default_accept: bool,
) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some(heading), (!body.is_empty()).then_some(body));
    dialog.add_responses(&[cancel, accept]);
    dialog.set_response_appearance(accept.0, look);
    dialog.set_default_response(Some(if default_accept { accept.0 } else { cancel.0 }));
    dialog.set_close_response(cancel.0);
    dialog
}

/// Makes a press outside `dialog` (on the dimmed window around it) close it, as Escape does: an
/// alert gives its close response (its cancel), and a dialog that refuses to close (`can-close`
/// off) is asked, not forced. The press goes no further, so it never reaches what the dialog
/// covers. A press in a popover of the dialog (a dropdown's list) is its own surface's, and left
/// alone. Hooked once per dialog, by the window that shows it (its `visible-dialog`): while a
/// dialog is up, the window's own controllers do not see presses on it.
pub fn close_on_outside_click(dialog: &adw::Dialog) {
    const HOOKED: &str = "closes-on-outside-click";
    if dialog.has_css_class(HOOKED) {
        return;
    }
    dialog.add_css_class(HOOKED);
    let click = gtk4::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    click.connect_pressed(|gesture, _, x, y| {
        let Some(dialog) = gesture.widget().and_downcast::<adw::Dialog>() else {
            return;
        };
        let on_dialog = gesture
            .current_event()
            .and_then(|e| e.surface())
            .zip(dialog.native().and_then(|n| n.surface()))
            .is_some_and(|(at, own)| at == own);
        let Some(content) = dialog.child() else {
            return;
        };
        if !on_dialog || !outside(&content, dialog.upcast_ref(), x, y) {
            return;
        }
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        dialog.close();
    });
    dialog.add_controller(click);
}

/// Whether `(x, y)`, in `host`'s coordinates, is outside `content`. A widget with no bounds
/// (not drawn yet) is never "outside": nothing is closed on a guess.
fn outside(content: &gtk4::Widget, host: &gtk4::Widget, x: f64, y: f64) -> bool {
    content.compute_bounds(host).is_some_and(|bounds| {
        !bounds.contains_point(&gtk4::graphene::Point::new(x as f32, y as f32))
    })
}

/// Shows `dialog` over `parent` and waits for its answer: whether it was `accept_id`.
pub async fn confirmed(
    dialog: &adw::AlertDialog,
    parent: &impl IsA<gtk4::Widget>,
    accept_id: &str,
) -> bool {
    dialog.clone().choose_future(Some(parent)).await == accept_id
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A dialog the window shows is hooked once, when it becomes visible; a point on the dimmed
    /// window is outside its content and its own centre is not; and closing it as an outside
    /// press does answers its cancel, as Escape would. Called by the window's display test.
    pub(crate) fn a_shown_dialog_closes_on_a_press_outside_it(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let ctx = gtk4::glib::MainContext::default();
        let dialog = confirm_dialog(
            "Remove?",
            "",
            ("cancel", "Cancel"),
            ("remove", "Remove"),
            adw::ResponseAppearance::Destructive,
            false,
        );
        let answer = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        dialog.connect_response(None, {
            let answer = answer.clone();
            move |_, response| *answer.borrow_mut() = Some(response.to_owned())
        });
        dialog.present(Some(window));
        while ctx.iteration(false) {}
        assert!(
            dialog.has_css_class("closes-on-outside-click"),
            "the window hooks the dialog it shows"
        );
        // Hooking again adds nothing (one press, one close).
        close_on_outside_click(dialog.upcast_ref());
        let content = dialog.child().expect("the alert's content");
        let host: &gtk4::Widget = dialog.upcast_ref();
        if let Some(bounds) = content.compute_bounds(host) {
            let (cx, cy) = (
                f64::from(bounds.x() + bounds.width() / 2.0),
                f64::from(bounds.y() + bounds.height() / 2.0),
            );
            assert!(!outside(&content, host, cx, cy), "its own centre is inside");
            assert!(
                outside(&content, host, f64::from(bounds.x()) - 5.0, cy),
                "the dimmed window beside it is outside"
            );
        }
        // What an outside press does once it is judged outside.
        dialog.close();
        while ctx.iteration(false) {}
        assert_eq!(answer.borrow().as_deref(), Some("cancel"));
    }
}
