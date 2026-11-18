//! F6 and Shift+F6: keyboard focus moves between the window's regions, as in other GTK and
//! GNOME apps. The regions, in order, are those shown: the thread sidebar, the thread's
//! transcript, its message box, the terminal (a terminal page, or a thread's drawer while
//! open) and the diff panel while docked beside the thread. A region with nothing that can take
//! focus (an empty transcript) is passed over.
//!
//! Bare F6 is not an application accelerator: it is matched in the window's bubble phase
//! ([`AgentTerminalWindow::install_region_keys`]), after the focused widget had it, so a
//! terminal program (mc, htop, vim) still gets its F6, and the key goes on unhandled when there
//! is nowhere to move (one region, a dialog open, the window closing). Ctrl+F6 and
//! Ctrl+Shift+F6 are the accelerators (`accels.rs`), which work from a terminal too.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib;

use super::AgentTerminalWindow;

/// One region: the widgets that hold it, and how it takes focus.
pub(super) struct Region {
    pub(super) name: &'static str,
    /// The first is the region's own box; a terminal page's search bar is a second.
    roots: Vec<gtk4::Widget>,
    take: Take,
}

enum Take {
    /// The sidebar: its selected row, else whatever comes first in it.
    Sidebar(gtk4::ListBox),
    /// The newest thing in it that takes focus (the transcript: the latest message).
    Last,
    /// The first thing in it that takes focus.
    First,
    /// This widget itself (the message box, a terminal).
    Widget(gtk4::Widget),
}

/// The region after `current` (forward) or before it, of `count`, wrapping round. With none
/// focused, the first (forward) or the last.
pub(super) fn step(count: usize, current: Option<usize>, forward: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (current, forward) {
        (None, true) => 0,
        (None, false) => count - 1,
        (Some(i), true) => (i + 1) % count,
        (Some(i), false) => (i + count - 1) % count,
    })
}

/// What a bare F6 press asks for: `Some(forward)` for F6 and Shift+F6, `None` for any other key
/// or with another modifier held (Ctrl+F6 is the accelerator's).
pub(super) fn region_key(key: gtk4::gdk::Key, state: gtk4::gdk::ModifierType) -> Option<bool> {
    use gtk4::gdk::ModifierType as M;
    if key != gtk4::gdk::Key::F6 {
        return None;
    }
    let others = M::CONTROL_MASK | M::ALT_MASK | M::SUPER_MASK | M::META_MASK | M::HYPER_MASK;
    if state.intersects(others) {
        return None;
    }
    Some(!state.contains(M::SHIFT_MASK))
}

impl Region {
    fn holds(&self, widget: &gtk4::Widget) -> bool {
        self.roots
            .iter()
            .any(|root| widget == root || widget.is_ancestor(root))
    }

    fn focus(&self) -> bool {
        let root = &self.roots[0];
        match &self.take {
            Take::Sidebar(list) => {
                if let Some(row) = list.selected_row().filter(|r| r.is_mapped()) {
                    if row.grab_focus() {
                        return true;
                    }
                }
                root.child_focus(gtk4::DirectionType::TabForward)
            }
            Take::Last => root.child_focus(gtk4::DirectionType::TabBackward),
            Take::First => root.child_focus(gtk4::DirectionType::TabForward),
            Take::Widget(w) => w.is_sensitive() && w.grab_focus(),
        }
    }
}

impl AgentTerminalWindow {
    /// The regions shown now, in F6 order. None while a dialog is open over the window (focus
    /// stays in it) or the window is closing.
    pub(super) fn focus_regions(&self) -> Vec<Region> {
        let obj = self.obj();
        if obj.visible_dialog().is_some() || self.is_closing() {
            return Vec::new();
        }
        let window: gtk4::Widget = obj.clone().upcast();
        // Only what is drawn and usable in this window counts (a popped-out diff panel is in
        // its own).
        let here = |w: &gtk4::Widget| {
            w.is_mapped() && w.is_sensitive() && w.root().is_some_and(|r| r == window)
        };
        let mut regions = Vec::new();
        // The sidebar only while it is shown: hidden, a collapsed one may still be on its way out.
        let sidebar_shown = self
            .split_view
            .borrow()
            .as_ref()
            .is_none_or(adw::OverlaySplitView::shows_sidebar);
        if let Some(sidebar) = self.sidebar.borrow().as_ref() {
            let root: gtk4::Widget = sidebar.root.clone().upcast();
            if sidebar_shown && here(&root) {
                regions.push(Region {
                    name: "sidebar",
                    roots: vec![root],
                    take: Take::Sidebar(sidebar.list.clone()),
                });
            }
        }
        let Some(page) = self.selected_page() else {
            return regions;
        };
        let parts = self.with_page_tab(&page, |tab| {
            let view = tab.chat.as_ref().and_then(|c| c.view.clone());
            let terminal: gtk4::Widget = tab.terminal.clone().upcast();
            // A thread's drawer is its box (search bar and terminal); a terminal page's region
            // is its terminal and its search bar, not the diff panel beside them.
            let terminal_roots: Vec<gtk4::Widget> = match tab.chat.as_ref() {
                Some(chat) => vec![chat.drawer.clone().upcast()],
                None => vec![terminal.clone(), tab.search_bar.clone().upcast()],
            };
            (
                view,
                terminal,
                terminal_roots,
                tab.diff_panel.content.clone().upcast::<gtk4::Widget>(),
            )
        });
        let Some((view, terminal, terminal_roots, diff)) = parts else {
            return regions;
        };
        if let Some((transcript, composer, field)) = view.and_then(|v| v.region_widgets()) {
            if here(&transcript) {
                regions.push(Region {
                    name: "transcript",
                    roots: vec![transcript],
                    take: Take::Last,
                });
            }
            if here(&composer) {
                regions.push(Region {
                    name: "composer",
                    roots: vec![composer],
                    take: Take::Widget(field),
                });
            }
        }
        if here(&terminal_roots[0]) && here(&terminal) {
            regions.push(Region {
                name: "terminal",
                roots: terminal_roots,
                take: Take::Widget(terminal),
            });
        }
        if here(&diff) {
            regions.push(Region {
                name: "changes",
                roots: vec![diff],
                take: Take::First,
            });
        }
        regions
    }

    /// Moves focus to the next region (`forward`) or the previous one; the name of the region
    /// that took it, or `None` when focus could not move. Leaving a sidebar laid over the
    /// thread (a collapsed split view) hides it, so focus never goes behind it.
    pub(super) fn cycle_region(&self, forward: bool) -> Option<&'static str> {
        // With one region and focus in it, or none, there is nowhere to go.
        let regions = self.focus_regions();
        let focus = gtk4::prelude::GtkWindowExt::focus(&*self.obj());
        let current = focus
            .as_ref()
            .and_then(|f| regions.iter().position(|r| r.holds(f)));
        let overlaid = self
            .split_view
            .borrow()
            .as_ref()
            .filter(|s| s.is_collapsed() && s.shows_sidebar())
            .cloned();
        let mut at = current;
        for _ in 0..regions.len() {
            at = step(regions.len(), at, forward);
            let i = at?;
            if Some(i) == current {
                continue;
            }
            let region = &regions[i];
            if region.name != "sidebar" {
                if let Some(split) = overlaid.as_ref() {
                    split.set_show_sidebar(false);
                }
            }
            if region.focus() {
                return Some(region.name);
            }
        }
        None
    }

    /// Bare F6 and Shift+F6, in the window's bubble phase: only once the focused widget (a
    /// terminal above all) has let the key go, and handled only when focus moved.
    pub(super) fn install_region_keys(&self) {
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Bubble);
        let weak = self.obj().downgrade();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(forward) = region_key(key, state) else {
                return glib::Propagation::Proceed;
            };
            match weak.upgrade() {
                Some(obj) if obj.imp().cycle_region(forward).is_some() => glib::Propagation::Stop,
                _ => glib::Propagation::Proceed,
            }
        });
        self.obj().add_controller(keys);
    }
}

#[cfg(test)]
mod tests {
    use super::{region_key, step};
    use gtk4::gdk::{Key, ModifierType as M};

    #[test]
    fn regions_wrap_both_ways() {
        assert_eq!(step(0, None, true), None);
        assert_eq!(step(3, None, true), Some(0));
        assert_eq!(step(3, None, false), Some(2));
        assert_eq!(step(3, Some(2), true), Some(0));
        assert_eq!(step(3, Some(0), false), Some(2));
        assert_eq!(step(3, Some(1), true), Some(2));
        assert_eq!(step(1, Some(0), true), Some(0));
    }

    #[test]
    fn only_bare_f6_and_shift_f6_cycle_in_the_bubble_phase() {
        assert_eq!(region_key(Key::F6, M::empty()), Some(true));
        assert_eq!(region_key(Key::F6, M::SHIFT_MASK), Some(false));
        // Caps Lock or Num Lock held changes nothing.
        assert_eq!(region_key(Key::F6, M::LOCK_MASK), Some(true));
        assert_eq!(region_key(Key::F6, M::CONTROL_MASK), None);
        assert_eq!(region_key(Key::F6, M::CONTROL_MASK | M::SHIFT_MASK), None);
        assert_eq!(region_key(Key::F6, M::ALT_MASK), None);
        assert_eq!(region_key(Key::F5, M::empty()), None);
    }
}
