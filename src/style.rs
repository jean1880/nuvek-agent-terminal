//! Embedded style sheets, installed once per process, and the app's text scale.

use std::cell::{Cell, RefCell};
use std::thread::LocalKey;

use gtk4::gdk;

/// Installs `css` on the default display, above the app's brand CSS, the first time `flag`
/// (the caller's own) is seen unset; nothing happens without a display, and `flag` stays unset
/// so a later call tries again.
pub fn install_once(flag: &'static LocalKey<Cell<bool>>, css: &str) {
    if flag.with(Cell::get) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    crate::brand::install();
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(css);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    flag.with(|l| l.set(true));
}

/// The smallest and largest text scale (Settings → General → Appearance → Text Scale, and the
/// zoom keys).
pub const TEXT_SCALE_RANGE: (f64, f64) = (0.5, 3.0);

thread_local! {
    /// The one provider that scales the app's text, and the scale it holds.
    static TEXT_SCALE: RefCell<Option<(gtk4::CssProvider, f64)>> = const { RefCell::new(None) };
}

/// `scale` as the app uses it: within [`TEXT_SCALE_RANGE`], 1.0 if it is not a number.
pub fn clamp_text_scale(scale: f64) -> f64 {
    if scale.is_finite() {
        scale.clamp(TEXT_SCALE_RANGE.0, TEXT_SCALE_RANGE.1)
    } else {
        1.0
    }
}

/// The stylesheet that scales every window's text by `scale`: each toplevel (main windows,
/// Settings and other dialogs, popped-out panels, the compare window) takes it at its root,
/// and everything inside it, popovers and dialogs included, inherits it. The app's own font
/// sizes are relative, so they follow; a terminal's font is scaled by VTE itself.
pub fn text_scale_css(scale: f64) -> String {
    let percent = (clamp_text_scale(scale) * 1000.0).round() / 10.0;
    format!("window {{ font-size: {percent}%; }}\n")
}

/// Scales the whole app's text by `scale` (1.0 is the desktop's size), live. Nothing happens
/// without a display.
pub fn set_text_scale(scale: f64) {
    let scale = clamp_text_scale(scale);
    TEXT_SCALE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(_, s)| *s == scale) {
            return;
        }
        let provider = match slot.as_ref() {
            Some((provider, _)) => provider.clone(),
            None => {
                let Some(display) = gdk::Display::default() else {
                    return;
                };
                let provider = gtk4::CssProvider::new();
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
                );
                provider
            }
        };
        provider.load_from_data(&text_scale_css(scale));
        *slot = Some((provider, scale));
    });
}

/// The text scale in effect (1.0 until one is set).
#[cfg(test)]
pub fn text_scale() -> f64 {
    TEXT_SCALE.with(|slot| slot.borrow().as_ref().map_or(1.0, |(_, s)| *s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scale_is_clamped_and_written_as_a_percentage() {
        assert_eq!(clamp_text_scale(f64::NAN), 1.0);
        assert_eq!(clamp_text_scale(9.0), 3.0);
        assert_eq!(clamp_text_scale(0.1), 0.5);
        assert_eq!(text_scale_css(1.0), "window { font-size: 100%; }\n");
        assert_eq!(text_scale_css(1.25), "window { font-size: 125%; }\n");
        assert_eq!(text_scale_css(1.1), "window { font-size: 110%; }\n");
        assert_eq!(
            text_scale_css(f64::INFINITY),
            "window { font-size: 100%; }\n"
        );
    }
}
