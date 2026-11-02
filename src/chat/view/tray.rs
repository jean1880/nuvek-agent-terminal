//! The composer's attachment tray: the images staged with the next message, above the field.
//!
//! An image is taken in off the main thread (`crate::attachments::stage`), shown as a spinner tile
//! meanwhile, then as a thumbnail with a remove button. The tray is hidden when empty. A failure
//! goes to the host as a toast naming the image; nothing is dropped silently.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use agent_core::attachment::{AttachmentRef, MAX_PER_MESSAGE};
use gtk4::prelude::*;
use gtk4::{gdk, glib};

use super::cards::label;
use crate::attachments::Source;
use crate::chat::PromptImage;

/// A tile's side in the tray.
const TILE: i32 = 56;

/// Called when the tray's contents change.
type Changed = Rc<dyn Fn()>;
/// Called with a sentence for the user.
type Notify = Rc<dyn Fn(&str)>;

struct Entry {
    key: u64,
    image: Option<PromptImage>,
    widget: gtk4::Overlay,
}

pub struct Tray {
    revealer: gtk4::Revealer,
    row: gtk4::Box,
    note: gtk4::Label,
    hint: gtk4::Label,
    entries: RefCell<Vec<Entry>>,
    next: Cell<u64>,
    /// Told when the tray's contents change (the composer refreshes its placeholder and Send).
    changed: RefCell<Option<Changed>>,
    /// Told a sentence the user should see (a toast).
    notify: RefCell<Option<Notify>>,
}

impl Tray {
    pub fn new() -> Rc<Self> {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        row.add_css_class("attachment-tray");
        let hint = label("Drop images to attach", &["drop-hint"]);
        hint.set_visible(false);
        let note = label("", &["tray-note"]);
        note.set_xalign(0.0);
        note.set_visible(false);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        column.append(&row);
        column.append(&hint);
        column.append(&note);
        let revealer = gtk4::Revealer::new();
        revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
        revealer.set_transition_duration(crate::brand::FAST_MS);
        revealer.set_child(Some(&column));
        revealer.set_reveal_child(false);
        Rc::new(Self {
            revealer,
            row,
            note,
            hint,
            entries: RefCell::default(),
            next: Cell::new(0),
            changed: RefCell::default(),
            notify: RefCell::default(),
        })
    }

    pub fn widget(&self) -> &gtk4::Revealer {
        &self.revealer
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.changed.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_notify(&self, f: impl Fn(&str) + 'static) {
        *self.notify.borrow_mut() = Some(Rc::new(f));
    }

    fn changed(&self) {
        let shown = !self.entries.borrow().is_empty()
            || self.hint.is_visible()
            || self.note.is_visible() && !self.entries.borrow().is_empty();
        self.revealer.set_reveal_child(shown);
        let f = self.changed.borrow().clone();
        if let Some(f) = f {
            f();
        }
    }

    fn tell(&self, text: &str) {
        let f = self.notify.borrow().clone();
        if let Some(f) = f {
            f(text);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// Whether an image is still being taken in.
    pub fn preparing(&self) -> bool {
        self.entries.borrow().iter().any(|e| e.image.is_none())
    }

    /// The staged images, in order.
    pub fn images(&self) -> Vec<PromptImage> {
        self.entries
            .borrow()
            .iter()
            .filter_map(|e| e.image.clone())
            .collect()
    }

    /// The fit warning under the tray ("Antigravity can't take images."), shown only while
    /// images are staged.
    pub fn set_note(&self, note: Option<&str>) {
        let text = note.unwrap_or("");
        if self.note.text() != text {
            self.note.set_text(text);
        }
        let visible = note.is_some() && !self.is_empty();
        if self.note.is_visible() != visible {
            self.note.set_visible(visible);
            self.changed();
        }
    }

    /// Shows "Drop images to attach" while something is dragged over the thread.
    pub fn set_drop_hint(&self, on: bool) {
        if self.hint.is_visible() != on {
            self.hint.set_visible(on);
            self.changed();
        }
    }

    /// Empties the tray (the message was sent).
    pub fn clear(&self) {
        for e in self.entries.borrow_mut().drain(..) {
            self.row.remove(&e.widget);
        }
        self.changed();
    }

    /// Puts `images` back (a queued message pulled back to edit).
    pub fn set_images(self: &Rc<Self>, images: Vec<PromptImage>) {
        self.clear();
        for image in images {
            let key = self.next_key();
            let widget = self.ready_tile(key, &image);
            self.row.append(&widget);
            self.entries.borrow_mut().push(Entry {
                key,
                image: Some(image),
                widget,
            });
        }
        self.changed();
    }

    fn next_key(&self) -> u64 {
        let key = self.next.get();
        self.next.set(key + 1);
        key
    }

    /// Takes `source` in: a spinner tile now, the thumbnail once it is stored. Past
    /// [`MAX_PER_MESSAGE`] it is refused with a toast.
    pub fn add(self: &Rc<Self>, source: Source, what: String) {
        if self.entries.borrow().len() >= MAX_PER_MESSAGE {
            self.tell(&format!("Up to {MAX_PER_MESSAGE} images per message"));
            return;
        }
        let key = self.next_key();
        let widget = pending_tile(&what);
        self.row.append(&widget);
        self.entries.borrow_mut().push(Entry {
            key,
            image: None,
            widget,
        });
        self.changed();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let staged = crate::attachments::stage(source).await;
            let Some(tray) = weak.upgrade() else {
                return;
            };
            let at = tray.entries.borrow().iter().position(|e| e.key == key);
            // Removed while it was being prepared: nothing to fill in.
            let Some(at) = at else {
                return;
            };
            match staged {
                Ok(staged) => {
                    let image = staged.into_prompt_image();
                    let tile = tray.ready_tile(key, &image);
                    let old =
                        std::mem::replace(&mut tray.entries.borrow_mut()[at].widget, tile.clone());
                    tray.row.insert_child_after(&tile, Some(&old));
                    tray.row.remove(&old);
                    tray.entries.borrow_mut()[at].image = Some(image);
                }
                Err(why) => {
                    let e = tray.entries.borrow_mut().remove(at);
                    tray.row.remove(&e.widget);
                    tray.tell(&format!("{what}: {why}"));
                }
            }
            tray.changed();
        });
    }

    fn remove(&self, key: u64) {
        let at = self.entries.borrow().iter().position(|e| e.key == key);
        if let Some(at) = at {
            let e = self.entries.borrow_mut().remove(at);
            self.row.remove(&e.widget);
            self.changed();
        }
    }

    /// Moves focus to the last tile: BackSpace at the start of an empty field.
    pub fn focus_last(&self) -> bool {
        self.entries
            .borrow()
            .last()
            .is_some_and(|e| e.widget.grab_focus())
    }

    fn ready_tile(self: &Rc<Self>, key: u64, image: &PromptImage) -> gtk4::Overlay {
        let refs: Rc<[AttachmentRef]> = vec![image.meta.clone()].into();
        let tile = super::images::tile(
            &refs,
            0,
            (TILE, TILE),
            gtk4::ContentFit::Cover,
            &Rc::new(None),
        );
        tile.add_css_class("tray-tile");
        // The viewer opens on every staged image, starting at this one.
        let remove = gtk4::Button::from_icon_name("at-window-close-symbolic");
        remove.add_css_class("circular");
        remove.add_css_class("osd");
        remove.add_css_class("tray-remove");
        remove.set_halign(gtk4::Align::End);
        remove.set_valign(gtk4::Align::Start);
        remove.set_tooltip_text(Some("Remove Image"));
        remove.update_property(&[gtk4::accessible::Property::Label("Remove image")]);
        tile.add_overlay(&remove);
        let weak = Rc::downgrade(self);
        remove.connect_clicked(move |_| {
            if let Some(tray) = weak.upgrade() {
                tray.remove(key);
            }
        });
        let keys = gtk4::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, k, _, _| match k {
            gdk::Key::Delete | gdk::Key::BackSpace | gdk::Key::KP_Delete => {
                if let Some(tray) = weak.upgrade() {
                    tray.remove(key);
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        tile.add_controller(keys);
        tile
    }
}

/// A tile while its image is taken in.
fn pending_tile(what: &str) -> gtk4::Overlay {
    let tile = gtk4::Overlay::new();
    tile.add_css_class("image-tile");
    tile.add_css_class("tray-tile");
    tile.add_css_class("loading");
    tile.set_size_request(TILE, TILE);
    let spinner = gtk4::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_halign(gtk4::Align::Center);
    spinner.set_valign(gtk4::Align::Center);
    tile.set_child(Some(&spinner));
    tile.set_tooltip_text(Some(&format!("Preparing {what}…")));
    tile
}
