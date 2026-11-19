//! Every application accelerator in one place, and a test that no two actions share one.
//!
//! All shortcuts are application accelerators bound to window actions, so they are caught
//! before VTE sees the key press. Anything handled by a controller on the terminal widget stops
//! working the moment focus moves, which is exactly when a user reaches for copy or zoom.

use gtk4::prelude::*;

/// An action and the accelerators that activate it.
type Binding = (&'static str, &'static [&'static str]);

#[cfg(target_os = "macos")]
const PLATFORM: &[Binding] = &[
    ("win.new-tab", &["<Meta>t", "<Primary><Shift>T"]),
    ("win.close-tab", &["<Meta>w", "<Primary><Shift>W"]),
    ("win.copy", &["<Meta>c", "<Primary><Shift>C"]),
    ("win.paste", &["<Meta>v", "<Primary><Shift>V"]),
    ("win.search", &["<Meta>f", "<Primary><Shift>F"]),
    (
        "win.toggle-sidebar",
        &["F9", "<Meta>b", "<Primary><Shift>B"],
    ),
];

#[cfg(not(target_os = "macos"))]
const PLATFORM: &[Binding] = &[
    ("win.new-tab", &["<Ctrl><Shift>T"]),
    ("win.close-tab", &["<Ctrl><Shift>W"]),
    ("win.copy", &["<Ctrl><Shift>C"]),
    ("win.paste", &["<Ctrl><Shift>V"]),
    ("win.search", &["<Ctrl><Shift>F"]),
    ("win.toggle-sidebar", &["F9", "<Primary><Shift>B"]),
];

const COMMON: &[Binding] = &[
    ("win.restart-tab", &["<Primary><Shift>R"]),
    ("win.toggle-diff", &["<Primary><Shift>D"]),
    ("win.new-tab-worktree", &["<Primary><Shift>G"]),
    ("win.new-tab-folder", &["<Primary><Shift>O"]),
    ("win.checkpoint-now", &["<Primary><Shift>S"]),
    ("win.resume-session", &["<Primary><Shift>E"]),
    ("win.schedule", &["<Primary><Shift>L"]),
    ("win.toggle-pin", &["<Primary><Shift>P"]),
    ("win.toggle-drawer", &["<Ctrl>grave", "<Primary>J"]),
    ("win.preferences", &["<Primary>comma"]),
    ("win.shortcuts", &["<Primary>question"]),
    // Both the shifted and unshifted key, so Ctrl+= works on layouts where + needs Shift.
    (
        "win.zoom-in",
        &["<Primary>plus", "<Primary>equal", "<Primary>KP_Add"],
    ),
    ("win.zoom-out", &["<Primary>minus", "<Primary>KP_Subtract"]),
    ("win.zoom-reset", &["<Primary>0", "<Primary>KP_0"]),
    // Region cycling: the sidebar, the transcript, the composer, the drawer and the diff panel,
    // whichever are shown. Bare F6 and Shift+F6 are matched by the window itself
    // (`window/imp/regions.rs`), which leaves them to a focused terminal, so a terminal program
    // keeps them; these work from a terminal too.
    ("win.focus-next-region", &["<Primary>F6"]),
    ("win.focus-previous-region", &["<Primary><Shift>F6"]),
    ("win.next-tab", &["<Primary>Tab", "<Primary>Page_Down"]),
    (
        "win.previous-tab",
        &["<Primary><Shift>Tab", "<Primary>Page_Up"],
    ),
];

/// Every action with its accelerators, the per-position ones included.
fn all() -> Vec<(String, Vec<String>)> {
    let mut all: Vec<(String, Vec<String>)> = PLATFORM
        .iter()
        .chain(COMMON)
        .map(|(action, accels)| {
            (
                (*action).to_owned(),
                accels.iter().map(|a| (*a).to_owned()).collect(),
            )
        })
        .collect();
    // Alt+1..9 select a tab by position; Alt+9 means "last", as is conventional, rather than
    // the ninth tab specifically. Ctrl+Alt+N opens a tab as the Nth configured profile, in the
    // current tab's directory.
    for n in 1..=9i32 {
        let index = if n == 9 { -1 } else { n - 1 };
        all.push((
            format!("win.select-tab({index})"),
            vec![format!("<Alt>{n}")],
        ));
        all.push((
            format!("win.new-tab-profile-at({})", n - 1),
            vec![format!("<Ctrl><Alt>{n}")],
        ));
    }
    all
}

/// The accelerators of `action`, as written in the table.
#[cfg(test)]
pub(crate) fn accels_for(action: &str) -> Vec<String> {
    all()
        .into_iter()
        .find(|(a, _)| a == action)
        .map(|(_, accels)| accels)
        .unwrap_or_default()
}

/// Registers every accelerator on the application.
pub fn apply(app: &impl IsA<gtk4::Application>) {
    for (action, accels) in all() {
        let accels: Vec<&str> = accels.iter().map(String::as_str).collect();
        app.set_accels_for_action(&action, &accels);
    }
}

/// A key matched by hand in a widget's own controller, which no table can enumerate. It is
/// listed so the test can check it against the application accelerators. Gathered by reading
/// every `EventControllerKey`, `ShortcutController` and `gdk::Key::` use under `src/`; a new
/// controller must be added here.
#[cfg(test)]
struct HandBound {
    accel: &'static str,
    /// When it is live: it only competes with an accelerator while this holds.
    context: &'static str,
    /// Where it is matched.
    site: &'static str,
}

#[cfg(test)]
const fn hand(accel: &'static str, context: &'static str, site: &'static str) -> HandBound {
    HandBound {
        accel,
        context,
        site,
    }
}

#[cfg(test)]
const HAND_BOUND: &[HandBound] = &[
    hand("<Control>Return", "approval waiting", "chat/view.rs"),
    hand("<Control>KP_Enter", "approval waiting", "chat/view.rs"),
    hand("<Control><Shift>Return", "approval waiting", "chat/view.rs"),
    hand(
        "<Control><Shift>KP_Enter",
        "approval waiting",
        "chat/view.rs",
    ),
    hand("<Control><Alt>Return", "approval waiting", "chat/view.rs"),
    hand("<Control><Alt>KP_Enter", "approval waiting", "chat/view.rs"),
    hand("<Control>BackSpace", "approval waiting", "chat/view.rs"),
    hand("<Control>period", "approval waiting", "chat/view.rs"),
    hand(
        "<Control><Shift>a",
        "a thread with sub-agents",
        "chat/view.rs",
    ),
    hand(
        "<Shift>F10",
        "terminal focused (context menu)",
        "window/imp.rs",
    ),
    hand("Menu", "terminal focused (context menu)", "window/imp.rs"),
    hand(
        "<Shift>F10",
        "sidebar thread row selected",
        "window/imp/thread_menu.rs",
    ),
    hand(
        "Menu",
        "sidebar thread row selected",
        "window/imp/thread_menu.rs",
    ),
    hand(
        "<Alt><Shift>Up",
        "sidebar thread row selected",
        "window/imp/thread_menu.rs",
    ),
    hand(
        "<Alt><Shift>Down",
        "sidebar thread row selected",
        "window/imp/thread_menu.rs",
    ),
    hand(
        "Delete",
        "sidebar thread row selected",
        "window/imp/thread_menu.rs",
    ),
    hand(
        "KP_Delete",
        "sidebar thread row selected",
        "window/imp/thread_menu.rs",
    ),
    hand(
        "F6",
        "focus not in a terminal (window capture phase)",
        "window/imp/regions.rs",
    ),
    hand(
        "<Shift>F6",
        "focus not in a terminal (window capture phase)",
        "window/imp/regions.rs",
    ),
    hand("Up", "composer focused", "chat/view/composer.rs"),
    hand("Down", "composer focused", "chat/view/composer.rs"),
    hand("Tab", "composer focused", "chat/view/composer.rs"),
    hand("Return", "composer focused", "chat/view/composer.rs"),
    hand("Escape", "composer focused", "chat/view/composer.rs"),
    hand("BackSpace", "composer focused", "chat/view/composer.rs"),
    hand("Tab", "transcript focused", "chat/view/transcript.rs"),
    hand("Delete", "composer image tray focused", "chat/view/tray.rs"),
    hand(
        "BackSpace",
        "composer image tray focused",
        "chat/view/tray.rs",
    ),
    hand("Return", "image tile focused", "chat/view/images.rs"),
    hand("space", "image tile focused", "chat/view/images.rs"),
    hand("Left", "image viewer open", "chat/view/images.rs"),
    hand("Right", "image viewer open", "chat/view/images.rs"),
    hand("z", "image viewer open", "chat/view/images.rs"),
    hand(
        "<Control><Alt>m",
        "diff panel file view focused",
        "window/diff_panel/review.rs",
    ),
    hand(
        "<Control><Alt>m",
        "file-change card diff focused",
        "chat/view/difftext.rs",
    ),
    hand("Return", "diff comment editor focused", "review_popover.rs"),
    hand(
        "KP_Enter",
        "diff comment editor focused",
        "review_popover.rs",
    ),
    hand(
        "ISO_Enter",
        "diff comment editor focused",
        "review_popover.rs",
    ),
];

/// A hand-matched key that is allowed to equal an application accelerator, with the reason
/// the two can never be live together. Empty: none does today.
#[cfg(test)]
const ALLOWED_OVERLAPS: &[(&str, &str, &str)] = &[];

/// A known clash between two application accelerators. Empty: none does today.
#[cfg(test)]
const KNOWN_DUPLICATES: &[(&str, &str)] = &[];

/// Reduces an accelerator string to a form that is equal for equal bindings: modifiers sorted
/// and spelled one way (`<Primary>`, `<Control>` and `<Ctrl>` agree; on macOS `<Primary>` is
/// the Command key), the key lower-cased (`<Shift>T` and `<Shift>t` agree).
#[cfg(test)]
fn normalise(accel: &str) -> String {
    let mut mods = std::collections::BTreeSet::new();
    let mut rest = accel.trim();
    while let Some(tail) = rest.strip_prefix('<') {
        let (name, after) = tail
            .split_once('>')
            .unwrap_or_else(|| panic!("unterminated modifier in {accel:?}"));
        let name = match name.to_ascii_lowercase().as_str() {
            "primary" if cfg!(target_os = "macos") => "meta",
            "primary" | "control" | "ctrl" => "ctrl",
            "alt" | "mod1" => "alt",
            "meta" | "mod2" | "command" => "meta",
            "super" | "mod4" => "super",
            "shift" => "shift",
            other => panic!("unknown modifier {other:?} in {accel:?}"),
        };
        mods.insert(name);
        rest = after;
    }
    assert!(!rest.is_empty(), "no key in {accel:?}");
    let mods: String = mods.iter().map(|m| format!("<{m}>")).collect();
    format!("{mods}{}", rest.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn known(a: &str, b: &str) -> bool {
        KNOWN_DUPLICATES
            .iter()
            .any(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a))
    }

    #[test]
    fn normalise_ignores_spelling_and_order() {
        assert_eq!(normalise("<Primary><Shift>T"), normalise("<Shift><Ctrl>t"));
        assert_eq!(normalise("<Control>Return"), normalise("<Ctrl>return"));
        assert_ne!(normalise("<Primary>t"), normalise("<Primary><Shift>t"));
        assert_ne!(normalise("<Alt>1"), normalise("<Ctrl><Alt>1"));
    }

    #[test]
    fn no_two_actions_share_an_accelerator() {
        let mut seen: HashMap<String, String> = HashMap::new();
        for (action, accels) in all() {
            for accel in accels {
                let key = normalise(&accel);
                if let Some(other) = seen.insert(key.clone(), action.clone()) {
                    assert!(
                        known(&other, &action),
                        "{other} and {action} are both bound to {accel} ({key})"
                    );
                }
            }
        }
    }

    #[test]
    fn no_hand_bound_key_clashes_with_an_accelerator() {
        let app: HashMap<String, String> = all()
            .into_iter()
            .flat_map(|(action, accels)| {
                accels
                    .into_iter()
                    .map(move |a| (normalise(&a), action.clone()))
            })
            .collect();
        for h in HAND_BOUND {
            let key = normalise(h.accel);
            let Some(action) = app.get(&key) else {
                continue;
            };
            assert!(
                ALLOWED_OVERLAPS
                    .iter()
                    .any(|(a, k, _)| a == action && normalise(k) == key),
                "{} ({}, {}) is also bound to {action}",
                h.accel,
                h.context,
                h.site
            );
        }
    }

    /// An application accelerator is caught before the focused widget: a bare F6 bound there
    /// would never reach a terminal program. Region cycling has Ctrl+F6 instead.
    #[test]
    fn bare_f6_is_left_to_the_focused_widget() {
        for (action, accels) in all() {
            for accel in accels {
                let key = normalise(&accel);
                assert!(
                    key != "f6" && key != "<shift>f6",
                    "{action} takes {accel} from terminals"
                );
            }
        }
        for (action, accel) in [
            ("win.focus-next-region", "<Primary>F6"),
            ("win.focus-previous-region", "<Primary><Shift>F6"),
        ] {
            assert_eq!(
                accels_for(action)
                    .iter()
                    .map(|a| normalise(a))
                    .collect::<Vec<_>>(),
                [normalise(accel)],
                "{action}"
            );
        }
    }

    #[test]
    fn exceptions_are_still_needed() {
        let all = all();
        for (action, accel, reason) in ALLOWED_OVERLAPS {
            assert!(!reason.is_empty(), "{action}: give the reason");
            let key = normalise(accel);
            assert!(
                all.iter()
                    .any(|(a, l)| a == action && l.iter().any(|x| normalise(x) == key)),
                "{action} no longer has {accel}"
            );
            assert!(HAND_BOUND.iter().any(|h| normalise(h.accel) == key));
        }
        for (a, b) in KNOWN_DUPLICATES {
            assert!(all.iter().any(|(x, _)| x == a) && all.iter().any(|(x, _)| x == b));
        }
    }

    #[test]
    fn every_hand_bound_entry_parses_and_is_unique_in_its_context() {
        let mut seen = std::collections::HashSet::new();
        for h in HAND_BOUND {
            assert!(!h.context.is_empty() && !h.site.is_empty());
            assert!(
                seen.insert((normalise(h.accel), h.context)),
                "{} listed twice for {}",
                h.accel,
                h.context
            );
        }
    }
}
