//! F6 and Shift+F6: keyboard focus moves between the window's regions, as in other GTK and
//! GNOME apps. The regions, in order, are those shown: the thread sidebar, the thread's
//! transcript, its message box, the terminal (a terminal page, or a thread's drawer while
//! open) and the diff panel while docked beside the thread. A region with nothing that can take
//! focus (an empty transcript) is passed over.

use adw::prelude::*;
use adw::subclass::prelude::*;

use super::AgentTerminalWindow;

/// One region: the widget that holds it, and how it takes focus.
pub(super) struct Region {
    pub(super) name: &'static str,
    root: gtk4::Widget,
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

impl Region {
    fn holds(&self, widget: &gtk4::Widget) -> bool {
        widget == &self.root || widget.is_ancestor(&self.root)
    }

    fn focus(&self) -> bool {
        match &self.take {
            Take::Sidebar(list) => {
                if let Some(row) = list.selected_row().filter(|r| r.is_mapped()) {
                    if row.grab_focus() {
                        return true;
                    }
                }
                self.root.child_focus(gtk4::DirectionType::TabForward)
            }
            Take::Last => self.root.child_focus(gtk4::DirectionType::TabBackward),
            Take::First => self.root.child_focus(gtk4::DirectionType::TabForward),
            Take::Widget(w) => w.is_sensitive() && w.grab_focus(),
        }
    }
}

impl AgentTerminalWindow {
    /// The regions shown now, in F6 order.
    pub(super) fn focus_regions(&self) -> Vec<Region> {
        let window: gtk4::Widget = self.obj().clone().upcast();
        // Only what is drawn in this window counts (a popped-out diff panel is in its own).
        let here = |w: &gtk4::Widget| w.is_mapped() && w.root().is_some_and(|r| r == window);
        let mut regions = Vec::new();
        if let Some(sidebar) = self.sidebar.borrow().as_ref() {
            let root: gtk4::Widget = sidebar.root.clone().upcast();
            if here(&root) {
                regions.push(Region {
                    name: "sidebar",
                    root,
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
            (
                view,
                terminal,
                tab.diff_panel.content.clone().upcast::<gtk4::Widget>(),
            )
        });
        let Some((view, terminal, diff)) = parts else {
            return regions;
        };
        if let Some((transcript, composer, field)) = view.and_then(|v| v.region_widgets()) {
            if here(&transcript) {
                regions.push(Region {
                    name: "transcript",
                    root: transcript,
                    take: Take::Last,
                });
            }
            if here(&composer) {
                regions.push(Region {
                    name: "composer",
                    root: composer,
                    take: Take::Widget(field),
                });
            }
        }
        if here(&terminal) {
            regions.push(Region {
                name: "terminal",
                root: terminal.clone(),
                take: Take::Widget(terminal),
            });
        }
        if here(&diff) {
            regions.push(Region {
                name: "changes",
                root: diff,
                take: Take::First,
            });
        }
        regions
    }

    /// Moves focus to the next region (`forward`) or the previous one; the name of the region
    /// that took it.
    pub(super) fn cycle_region(&self, forward: bool) -> Option<&'static str> {
        let regions = self.focus_regions();
        let focus = gtk4::prelude::GtkWindowExt::focus(&*self.obj());
        let current = focus
            .as_ref()
            .and_then(|f| regions.iter().position(|r| r.holds(f)));
        let mut at = current;
        for _ in 0..regions.len() {
            at = step(regions.len(), at, forward);
            let region = &regions[at?];
            if Some(at?) != current && region.focus() {
                return Some(region.name);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::step;

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
}
