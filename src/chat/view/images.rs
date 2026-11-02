//! Images in a thread: one component draws them everywhere (the composer's tray, the user's
//! bubble, an agent's shown images, a tool card, its header strip) and one viewer opens them.
//!
//! Every tile takes its final size from the ref's dimensions before its texture arrives (a
//! skeleton until then), so nothing moves when it loads and the transcript's stick-to-bottom
//! scrolling stays put. A blob that cannot be read shows "Image unavailable" with the reason,
//! never an empty box. Textures load off the main thread (`crate::attachments::texture`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use agent_core::attachment::{AttachmentRef, Origin};
use gtk4::{gdk, gio, glib};

use super::cards::label;

/// Where a group is drawn, which sets its sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Inside the user's bubble.
    Bubble,
    /// Below an agent's tool card (`show_image`) or in its reply.
    Agent,
    /// Inside an expanded tool card.
    Card,
}

impl Place {
    /// The box one image is fitted in.
    fn single(self) -> (f64, f64) {
        match self {
            Place::Bubble => (360.0, 270.0),
            Place::Agent => (480.0, 360.0),
            Place::Card => (320.0, 240.0),
        }
    }

    /// A tile's side when there are several.
    fn tile(self) -> i32 {
        match self {
            Place::Bubble | Place::Card => 160,
            Place::Agent => 200,
        }
    }
}

/// `width`×`height` fitted inside `max` (never enlarged), at least 24 px a side.
pub fn fitted(width: u32, height: u32, max: (f64, f64)) -> (i32, i32) {
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let scale = (max.0 / w).min(max.1 / h).min(1.0);
    let side = |v: f64| (v * scale).round().max(24.0) as i32;
    (side(w), side(h))
}

/// Columns of a grid of `n` images: two, or three for five (3 + 2).
pub fn columns(n: usize) -> i32 {
    if n == 5 {
        3
    } else {
        2
    }
}

/// Who a ref says put the image in the thread, as the viewer's subtitle ends.
pub fn from_whom(r: &AttachmentRef, agent: Option<&str>) -> String {
    match r.origin {
        Origin::User => "from you".to_owned(),
        Origin::AgentShown => format!("from {}", agent.unwrap_or("the agent")),
        Origin::ToolResult => "from a tool".to_owned(),
    }
}

/// A byte count as people read it ("1.2 MB", "84 KB").
pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1_000_000 => format!("{:.1} MB", b as f64 / 1_000_000.0),
        b if b >= 1_000 => format!("{} KB", (b + 500) / 1_000),
        b => format!("{b} B"),
    }
}

/// A group of images drawn at `place`. `agent` names who showed them, for the viewer.
pub fn group(refs: &[AttachmentRef], place: Place, agent: Option<&str>) -> gtk4::Widget {
    let refs: Rc<[AttachmentRef]> = refs.into();
    let agent: Rc<Option<String>> = Rc::new(agent.map(str::to_owned));
    if refs.len() == 1 {
        let (w, h) = fitted(refs[0].width, refs[0].height, place.single());
        let tile = tile(&refs, 0, (w, h), gtk4::ContentFit::Contain, &agent);
        tile.add_css_class("image-group");
        tile.set_halign(gtk4::Align::Start);
        return tile.upcast();
    }
    let grid = gtk4::Grid::new();
    grid.add_css_class("image-group");
    grid.set_row_spacing(6);
    grid.set_column_spacing(6);
    grid.set_halign(gtk4::Align::Start);
    let cols = columns(refs.len());
    let side = place.tile();
    for i in 0..refs.len() {
        let t = tile(&refs, i, (side, side), gtk4::ContentFit::Cover, &agent);
        let i = i32::try_from(i).unwrap_or(0);
        grid.attach(&t, i % cols, i / cols, 1, 1);
    }
    grid.update_property(&[gtk4::accessible::Property::Label(&format!(
        "{} images",
        refs.len()
    ))]);
    grid.upcast()
}

/// Up to three small thumbnails and "+N" for a collapsed tool card's header.
pub fn strip(refs: &[AttachmentRef]) -> gtk4::Box {
    let refs: Rc<[AttachmentRef]> = refs.into();
    let agent = Rc::new(None);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 3);
    row.add_css_class("image-strip");
    for i in 0..refs.len().min(3) {
        let t = tile(&refs, i, (24, 24), gtk4::ContentFit::Cover, &agent);
        t.add_css_class("thumb");
        row.append(&t);
    }
    if refs.len() > 3 {
        row.append(&label(&format!("+{}", refs.len() - 3), &["image-more"]));
    }
    row
}

/// One image at `size`: a skeleton until its texture arrives, then the picture or the
/// unavailable state. Clicks and Enter open the viewer; a right click opens its menu; it drags
/// out as an image.
pub fn tile(
    refs: &Rc<[AttachmentRef]>,
    index: usize,
    size: (i32, i32),
    fit: gtk4::ContentFit,
    agent: &Rc<Option<String>>,
) -> gtk4::Overlay {
    let r = refs[index].clone();
    let frame = gtk4::Overlay::new();
    frame.add_css_class("image-tile");
    frame.add_css_class("loading");
    frame.set_size_request(size.0, size.1);
    frame.set_overflow(gtk4::Overflow::Hidden);
    frame.set_focusable(true);
    frame.set_tooltip_text(Some(&format!(
        "{}{} × {} · {}",
        r.name
            .as_deref()
            .map(|n| format!("{n} · "))
            .unwrap_or_default(),
        r.width,
        r.height,
        human_size(r.bytes)
    )));
    frame.update_property(&[gtk4::accessible::Property::Label(&format!(
        "{}, {}",
        r.accessible_label(),
        from_whom(&r, agent.as_deref())
    ))]);
    let picture = gtk4::Picture::new();
    picture.set_content_fit(fit);
    picture.set_can_shrink(true);
    picture.set_size_request(size.0, size.1);
    // A picture asks for its image's own size; the size request is only a minimum. Two clamps
    // hold it to the reserved box both ways, so a tile is the same size loading and loaded.
    let clamp = |orientation, max: i32, child: &gtk4::Widget| {
        let c = adw::Clamp::new();
        c.set_orientation(orientation);
        c.set_maximum_size(max);
        c.set_tightening_threshold(max);
        c.set_child(Some(child));
        c
    };
    let tall = clamp(gtk4::Orientation::Vertical, size.1, picture.upcast_ref());
    let wide = clamp(gtk4::Orientation::Horizontal, size.0, tall.upcast_ref());
    frame.set_child(Some(&wide));
    let texture: Rc<RefCell<Option<gdk::Texture>>> = Rc::default();

    glib::spawn_future_local(glib::clone!(
        #[weak]
        frame,
        #[weak]
        picture,
        #[strong]
        texture,
        async move {
            match crate::attachments::texture(&r).await {
                Ok(t) => {
                    picture.set_paintable(Some(&t));
                    *texture.borrow_mut() = Some(t);
                }
                Err(why) => {
                    let gone = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
                    gone.add_css_class("image-unavailable");
                    gone.set_valign(gtk4::Align::Center);
                    gone.set_halign(gtk4::Align::Center);
                    gone.append(&gtk4::Image::from_icon_name("at-image-missing-symbolic"));
                    if frame.width_request() >= 96 {
                        gone.append(&label("Image unavailable", &["dim-label"]));
                    }
                    frame.add_overlay(&gone);
                    frame.set_tooltip_text(Some(&format!("Image unavailable: {why}")));
                }
            }
            frame.remove_css_class("loading");
        }
    ));

    let open = {
        let refs = refs.clone();
        let agent = agent.clone();
        move |from: &gtk4::Overlay| open_viewer(from, refs.clone(), index, agent.clone())
    };
    let click = gtk4::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.connect_released(glib::clone!(
        #[weak]
        frame,
        #[strong]
        open,
        move |g, _, _, _| {
            g.set_state(gtk4::EventSequenceState::Claimed);
            open(&frame);
        }
    ));
    frame.add_controller(click);
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        frame,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| match key {
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => {
                open(&frame);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    ));
    frame.add_controller(keys);
    let menu_click = gtk4::GestureClick::new();
    menu_click.set_button(gdk::BUTTON_SECONDARY);
    menu_click.connect_pressed(glib::clone!(
        #[weak]
        frame,
        #[strong]
        refs,
        #[strong]
        agent,
        #[strong]
        texture,
        move |g, _, x, y| {
            g.set_state(gtk4::EventSequenceState::Claimed);
            context_menu(&frame, &refs, index, &agent, &texture, (x, y));
        }
    ));
    frame.add_controller(menu_click);
    let drag = gtk4::DragSource::new();
    drag.set_actions(gdk::DragAction::COPY);
    drag.connect_prepare(glib::clone!(
        #[strong]
        texture,
        move |_, _, _| {
            texture
                .borrow()
                .as_ref()
                .map(|t| gdk::ContentProvider::for_value(&t.to_value()))
        }
    ));
    frame.add_controller(drag);
    frame
}

/// The image menu: Open, Copy Image, Save As….
fn context_menu(
    from: &gtk4::Overlay,
    refs: &Rc<[AttachmentRef]>,
    index: usize,
    agent: &Rc<Option<String>>,
    texture: &Rc<RefCell<Option<gdk::Texture>>>,
    at: (f64, f64),
) {
    let popover = gtk4::Popover::new();
    popover.set_has_arrow(false);
    popover.add_css_class("menu");
    let list = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let item = |text: &str| {
        let b = gtk4::Button::with_label(text);
        b.add_css_class("flat");
        b.add_css_class("image-menu-item");
        if let Some(l) = b.child().and_downcast::<gtk4::Label>() {
            l.set_xalign(0.0);
        }
        list.append(&b);
        b
    };
    let open = item("Open");
    let copy = item("Copy Image");
    let save = item("Save As…");
    popover.set_child(Some(&list));
    popover.set_parent(from);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(at.0 as i32, at.1 as i32, 1, 1)));
    // The tile can go while its menu is open (its card rebuilt as more images arrive): the
    // menu closes with it rather than outlive it.
    let unmapped = from.connect_unmap(glib::clone!(
        #[weak]
        popover,
        move |_| popover.popdown()
    ));
    let handler = RefCell::new(Some(unmapped));
    popover.connect_closed(glib::clone!(
        #[weak]
        from,
        move |p| {
            if let Some(id) = handler.borrow_mut().take() {
                from.disconnect(id);
            }
            // Unparented once closed: a popover must not outlive its menu.
            let p = p.clone();
            glib::idle_add_local_once(move || {
                if p.parent().is_some() {
                    p.unparent();
                }
            });
        }
    ));
    open.connect_clicked(glib::clone!(
        #[weak]
        popover,
        #[weak]
        from,
        #[strong]
        refs,
        #[strong]
        agent,
        move |_| {
            popover.popdown();
            open_viewer(&from, refs.clone(), index, agent.clone());
        }
    ));
    copy.connect_clicked(glib::clone!(
        #[weak]
        popover,
        #[weak]
        from,
        #[strong]
        texture,
        move |_| {
            popover.popdown();
            match texture.borrow().as_ref() {
                Some(t) => {
                    from.clipboard().set_texture(t);
                    toast(&from, "Image copied");
                }
                None => toast(&from, "The image isn't loaded yet"),
            }
        }
    ));
    let r = refs[index].clone();
    save.connect_clicked(glib::clone!(
        #[weak]
        popover,
        #[weak]
        from,
        move |_| {
            popover.popdown();
            save_as(&from, &r);
        }
    ));
    popover.popup();
}

/// A toast over the transcript (or the viewer) `from` sits in; nothing when there is none.
fn toast(from: &impl IsA<gtk4::Widget>, text: &str) {
    if let Some(overlay) = from
        .ancestor(adw::ToastOverlay::static_type())
        .and_downcast::<adw::ToastOverlay>()
    {
        overlay.add_toast(adw::Toast::new(text));
    }
}

/// The name Save As… suggests: the image's own name, else `image-<short id>`, with the stored
/// file's extension.
pub fn suggested_name(r: &AttachmentRef) -> String {
    let ext = AttachmentRef::extension_for(&r.mime).unwrap_or("png");
    let stem = r
        .name
        .as_deref()
        .map(|n| {
            std::path::Path::new(n)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("image-{}", &r.id[..r.id.len().min(8)]));
    format!("{stem}.{ext}")
}

/// Asks where to save the stored copy of `r`, then writes it there off the main thread.
fn save_as(from: &impl IsA<gtk4::Widget>, r: &AttachmentRef) {
    let dialog = gtk4::FileDialog::new();
    dialog.set_title("Save Image");
    dialog.set_initial_name(Some(&suggested_name(r)));
    let window = from.root().and_downcast::<gtk4::Window>();
    let from = from.clone().upcast::<gtk4::Widget>();
    let r = r.clone();
    glib::spawn_future_local(async move {
        let Ok(file) = dialog.save_future(window.as_ref()).await else {
            return; // Cancelled.
        };
        let Some(path) = file.path() else {
            toast(&from, "That place can't be written to");
            return;
        };
        let result = gio::spawn_blocking(move || -> Result<(), String> {
            let store = crate::attachments::blob_store().ok_or("no data folder")?;
            let bytes = store.read(&r).map_err(|e| e.to_string())?;
            std::fs::write(&path, bytes).map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|_| Err("the save failed".to_owned()));
        match result {
            Ok(()) => toast(&from, "Image saved"),
            Err(why) => toast(&from, &format!("Couldn't save the image: {why}")),
        }
    });
}

/// Opens the stored image in the user's image viewer, through a private copy.
fn open_externally(from: &impl IsA<gtk4::Widget>, r: &AttachmentRef) {
    let window = from.root().and_downcast::<gtk4::Window>();
    let from = from.clone().upcast::<gtk4::Widget>();
    let r = r.clone();
    glib::spawn_future_local(async move {
        let copy = gio::spawn_blocking(move || crate::attachments::private_copy(&r))
            .await
            .unwrap_or_else(|_| Err("the copy failed".to_owned()));
        match copy {
            Ok(path) => {
                let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(path)));
                let from = from.clone();
                launcher.launch(window.as_ref(), gio::Cancellable::NONE, move |res| {
                    if let Err(e) = res {
                        toast(&from, &format!("Couldn't open the image: {e}"));
                    }
                });
            }
            Err(why) => toast(&from, &format!("Couldn't open the image: {why}")),
        }
    });
}

/// The viewer: the image fitted to the window (a click or Z shows it at 100 %), its siblings a
/// Left/Right away, and Copy Image, Save As… and Open in Image Viewer in its header. Closes on
/// Escape or a click outside, as every dialog does.
pub fn open_viewer(
    from: &impl IsA<gtk4::Widget>,
    refs: Rc<[AttachmentRef]>,
    index: usize,
    agent: Rc<Option<String>>,
) {
    let dialog = adw::Dialog::new();
    dialog.add_css_class("image-viewer");
    dialog.set_content_width(960);
    dialog.set_content_height(720);
    let title = adw::WindowTitle::new("", "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let button = |icon: &str, tip: &str| {
        let b = gtk4::Button::from_icon_name(icon);
        b.set_tooltip_text(Some(tip));
        b.update_property(&[gtk4::accessible::Property::Label(tip)]);
        b
    };
    let copy = button("at-edit-copy-symbolic", "Copy Image");
    let save = button("at-document-save-symbolic", "Save As…");
    let external = button("agent-external-symbolic", "Open in Image Viewer");
    let zoom = gtk4::ToggleButton::new();
    zoom.set_icon_name("at-zoom-original-symbolic");
    zoom.set_tooltip_text(Some("Actual Size (Z)"));
    zoom.update_property(&[gtk4::accessible::Property::Label("Actual size")]);
    header.pack_end(&external);
    header.pack_end(&save);
    header.pack_end(&copy);
    header.pack_start(&zoom);

    let picture = gtk4::Picture::new();
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.add_css_class("image-viewer-picture");
    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_child(Some(&picture));
    scroller.set_vexpand(true);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&scroller));
    let nav = |icon: &str, tip: &str, align: gtk4::Align| {
        let b = button(icon, tip);
        b.add_css_class("circular");
        b.add_css_class("osd");
        b.set_halign(align);
        b.set_valign(gtk4::Align::Center);
        b.set_margin_start(12);
        b.set_margin_end(12);
        overlay.add_overlay(&b);
        b
    };
    let prev = nav(
        "at-go-previous-symbolic",
        "Previous Image",
        gtk4::Align::Start,
    );
    let next = nav("at-go-next-symbolic", "Next Image", gtk4::Align::End);
    let counter = label("", &["image-counter", "osd"]);
    counter.set_halign(gtk4::Align::Center);
    counter.set_valign(gtk4::Align::End);
    counter.set_margin_bottom(12);
    overlay.add_overlay(&counter);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&overlay));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    dialog.set_child(Some(&toolbar));

    let current = Rc::new(Cell::new(index.min(refs.len().saturating_sub(1))));
    let shown: Rc<RefCell<Option<gdk::Texture>>> = Rc::default();
    let show = {
        let (refs, agent, current, shown) =
            (refs.clone(), agent.clone(), current.clone(), shown.clone());
        // Weak: the buttons hold `step`, which holds this; strong widgets here would be a cycle
        // that keeps every viewer (and its texture) alive after it closes.
        let (title, picture, prev, next, counter) = (
            title.downgrade(),
            picture.downgrade(),
            prev.downgrade(),
            next.downgrade(),
            counter.downgrade(),
        );
        Rc::new(move || {
            let (Some(title), Some(picture), Some(prev), Some(next), Some(counter)) = (
                title.upgrade(),
                picture.upgrade(),
                prev.upgrade(),
                next.upgrade(),
                counter.upgrade(),
            ) else {
                return;
            };
            let i = current.get();
            let r = refs[i].clone();
            let heading = r.name.clone().unwrap_or_else(|| "Image".to_owned());
            title.set_title(&heading);
            title.set_subtitle(&format!(
                "{} × {} · {} · {}",
                r.width,
                r.height,
                human_size(r.bytes),
                from_whom(&r, agent.as_deref())
            ));
            let many = refs.len() > 1;
            prev.set_visible(many);
            next.set_visible(many);
            counter.set_visible(many);
            prev.set_sensitive(i > 0);
            next.set_sensitive(i + 1 < refs.len());
            counter.set_text(&format!("{} of {}", i + 1, refs.len()));
            picture.set_paintable(None::<&gdk::Paintable>);
            picture.update_property(&[gtk4::accessible::Property::Label(&r.accessible_label())]);
            *shown.borrow_mut() = None;
            let (picture, shown, current) = (picture.downgrade(), shown.clone(), current.clone());
            glib::spawn_future_local(async move {
                let loaded = crate::attachments::texture(&r).await;
                // A later image may have been chosen meanwhile, or the viewer closed.
                let Some(picture) = picture.upgrade() else {
                    return;
                };
                if current.get() != i {
                    return;
                }
                match loaded {
                    Ok(t) => {
                        picture.set_paintable(Some(&t));
                        *shown.borrow_mut() = Some(t);
                    }
                    Err(why) => picture.update_property(&[gtk4::accessible::Property::Label(
                        &format!("Image unavailable: {why}"),
                    )]),
                }
            });
        })
    };
    let step = {
        let (current, refs, show) = (current.clone(), refs.clone(), show.clone());
        Rc::new(move |delta: isize| {
            let i = current.get() as isize + delta;
            if i >= 0 && (i as usize) < refs.len() {
                current.set(i as usize);
                show();
            }
        })
    };
    prev.connect_clicked(glib::clone!(
        #[strong]
        step,
        move |_| step(-1)
    ));
    next.connect_clicked(glib::clone!(
        #[strong]
        step,
        move |_| step(1)
    ));
    let set_zoom = glib::clone!(
        #[weak]
        picture,
        move |actual: bool| {
            picture.set_can_shrink(!actual);
            picture.set_halign(if actual {
                gtk4::Align::Start
            } else {
                gtk4::Align::Fill
            });
        }
    );
    zoom.connect_toggled(glib::clone!(
        #[strong]
        set_zoom,
        move |z| set_zoom(z.is_active())
    ));
    let click = gtk4::GestureClick::new();
    click.connect_released(glib::clone!(
        #[weak]
        zoom,
        move |_, n, _, _| {
            if n == 1 {
                zoom.set_active(!zoom.is_active());
            }
        }
    ));
    picture.add_controller(click);
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        zoom,
        #[strong]
        step,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| match key {
            gdk::Key::Left | gdk::Key::KP_Left => {
                step(-1);
                glib::Propagation::Stop
            }
            gdk::Key::Right | gdk::Key::KP_Right => {
                step(1);
                glib::Propagation::Stop
            }
            gdk::Key::z | gdk::Key::Z => {
                zoom.set_active(!zoom.is_active());
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    ));
    dialog.add_controller(keys);
    copy.connect_clicked(glib::clone!(
        #[weak]
        toasts,
        #[strong]
        shown,
        move |b| match shown.borrow().as_ref() {
            Some(t) => {
                b.clipboard().set_texture(t);
                toasts.add_toast(adw::Toast::new("Image copied"));
            }
            None => toasts.add_toast(adw::Toast::new("The image isn't loaded yet")),
        }
    ));
    save.connect_clicked(glib::clone!(
        #[weak]
        toasts,
        #[strong]
        refs,
        #[strong]
        current,
        move |_| save_as(&toasts, &refs[current.get()])
    ));
    external.connect_clicked(glib::clone!(
        #[weak]
        toasts,
        #[strong]
        refs,
        #[strong]
        current,
        move |_| open_externally(&toasts, &refs[current.get()])
    ));
    show();
    dialog.present(Some(from.as_ref()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::attachment::AttachmentKind;

    fn r(name: Option<&str>, w: u32, h: u32, origin: Origin) -> AttachmentRef {
        AttachmentRef {
            id: "ab".repeat(32),
            kind: AttachmentKind::Image,
            mime: "image/jpeg".into(),
            width: w,
            height: h,
            bytes: 1_234_567,
            name: name.map(str::to_owned),
            origin,
        }
    }

    #[test]
    fn a_tile_reserves_its_fitted_size_and_never_enlarges() {
        assert_eq!(fitted(1280, 720, (360.0, 270.0)), (360, 203));
        assert_eq!(fitted(720, 1280, (360.0, 270.0)), (152, 270));
        assert_eq!(fitted(100, 50, (360.0, 270.0)), (100, 50));
        // A sliver still gets a clickable tile.
        assert_eq!(fitted(4000, 10, (360.0, 270.0)), (360, 24));
        assert_eq!(columns(2), 2);
        assert_eq!(columns(4), 2);
        assert_eq!(columns(5), 3);
    }

    #[test]
    fn the_viewer_says_whose_image_and_how_big() {
        assert_eq!(from_whom(&r(None, 1, 1, Origin::User), None), "from you");
        assert_eq!(
            from_whom(&r(None, 1, 1, Origin::AgentShown), Some("Claude")),
            "from Claude"
        );
        assert_eq!(
            from_whom(&r(None, 1, 1, Origin::ToolResult), None),
            "from a tool"
        );
        assert_eq!(human_size(1_234_567), "1.2 MB");
        assert_eq!(human_size(84_200), "84 KB");
        assert_eq!(human_size(12), "12 B");
    }

    #[test]
    fn save_as_suggests_a_plain_name_with_the_stored_extension() {
        assert_eq!(
            suggested_name(&r(Some("Screenshot 2026.png"), 1, 1, Origin::User)),
            "Screenshot 2026.jpg"
        );
        assert_eq!(
            suggested_name(&r(None, 1, 1, Origin::User)),
            "image-abababab.jpg"
        );
    }
}
