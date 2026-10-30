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

/// Makes every dialog `window` shows close on a press outside it ([`close_on_outside_click`]),
/// hooked as each becomes its visible dialog: while a dialog is up, the window's own controllers
/// do not see presses on it. For an `adw::Window` or `adw::ApplicationWindow` (`visible-dialog`).
pub fn close_dialogs_on_outside_click(window: &impl IsA<gtk4::Window>) {
    window.connect_notify_local(Some("visible-dialog"), |window, _| {
        if let Some(dialog) = window.property::<Option<adw::Dialog>>("visible-dialog") {
            close_on_outside_click(&dialog);
        }
    });
}

/// Makes a press outside `dialog` (on the dimmed window around it) close it, as Escape does: an
/// alert gives its close response (its cancel), and a dialog that refuses to close (`can-close`
/// off) is asked, not forced. The press goes no further, so it never reaches what the dialog
/// covers. Hooked once per dialog.
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
        if press_closes(&dialog, x, y, on_dialog) {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            dialog.close();
        }
    });
    dialog.add_controller(click);
}

/// Whether a press at `(x, y)` (in `dialog`'s coordinates) closes it: on the dialog's own surface
/// (`on_dialog`; a popover of it, such as a dropdown's list, is another surface and never does)
/// and outside its content. Content not laid out yet has no bounds, and closes nothing on a guess.
fn press_closes(dialog: &adw::Dialog, x: f64, y: f64, on_dialog: bool) -> bool {
    let Some(content) = dialog.child() else {
        return false;
    };
    on_dialog
        && content.compute_bounds(dialog).is_some_and(|bounds| {
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
mod tests {
    use super::*;

    /// On a window of its own, presented, with animations off (libadwaita then finishes them at
    /// once, so nothing waits on a frame clock): the window hooks the alert it shows, once; a
    /// press beside the alert's content closes it, one on its content or in a popover does not;
    /// and closing it answers its cancel, as Escape does.
    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn a_shown_dialog_closes_on_a_press_outside_it() {
        let ctx = crate::testutil::native_init();
        let settings = gtk4::Settings::default().expect("GTK settings");
        let animations = settings.is_gtk_enable_animations();
        settings.set_gtk_enable_animations(false);

        let window = adw::Window::new();
        window.set_default_size(800, 600);
        close_dialogs_on_outside_click(&window);
        window.present();
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
        dialog.present(Some(&window));
        crate::testutil::settle(&ctx);

        assert!(
            dialog.has_css_class("closes-on-outside-click"),
            "the window hooks the dialog it shows"
        );
        let content = dialog.child().expect("the alert's content");
        let bounds = content
            .compute_bounds(&dialog)
            .expect("a presented dialog's content is laid out");
        let (cx, cy) = (
            f64::from(bounds.x() + bounds.width() / 2.0),
            f64::from(bounds.y() + bounds.height() / 2.0),
        );
        let beside = f64::from(bounds.x()) - 5.0;
        assert!(beside >= 0.0, "the dialog has room around it: {bounds:?}");
        assert!(
            !press_closes(dialog.upcast_ref(), cx, cy, true),
            "on its content"
        );
        assert!(
            press_closes(dialog.upcast_ref(), beside, cy, true),
            "beside it"
        );
        assert!(
            !press_closes(dialog.upcast_ref(), beside, cy, false),
            "a press in a popover of the dialog is not on the dimmed window"
        );

        // What a press beside it does: close it, as Escape does.
        dialog.close();
        crate::testutil::settle(&ctx);
        assert_eq!(answer.borrow().as_deref(), Some("cancel"));

        window.destroy();
        settings.set_gtk_enable_animations(animations);
    }
}
