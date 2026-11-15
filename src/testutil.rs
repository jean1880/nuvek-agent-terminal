//! Test helpers for code that runs on the glib main loop. No display, no real HOME or XDG.

use std::time::{Duration, Instant};

use gtk4::glib;

/// Capture only a synthetic test window when the QA runner requests artefacts.
pub fn capture_window(window: &gtk4::Window, name: &str) {
    use gtk4::prelude::*;
    let Some(directory) = std::env::var_os("AGENT_TERMINAL_QA_ARTIFACTS") else {
        return;
    };
    let capture = || -> Result<(), String> {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("invalid synthetic screenshot name".into());
        }
        let paintable = gtk4::WidgetPaintable::new(Some(window));
        let snapshot = gtk4::Snapshot::new();
        paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
        let node = snapshot.to_node().ok_or("window snapshot unavailable")?;
        let renderer = window.renderer().ok_or("window renderer unavailable")?;
        let texture = renderer.render_texture(&node, None);
        let directory = std::path::PathBuf::from(&directory);
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        texture
            .save_to_png(directory.join(format!("{name}.png")))
            .map_err(|error| error.to_string())
    };
    // Artefacts aid diagnosis; capture errors must not replace the geometry assertion.
    if let Err(error) = capture() {
        eprintln!("Synthetic screenshot {name} unavailable: {error}");
    }
}

/// A session that runs `program` with `extra_args` in `cwd`, in Ask mode, with no model, effort,
/// resume, hook or delegation: the base every session test changes what it needs on.
pub fn open_session(
    program: impl Into<String>,
    extra_args: Vec<String>,
    cwd: impl Into<String>,
) -> agent_core::adapter::OpenSession {
    agent_core::adapter::OpenSession {
        program: program.into(),
        extra_args,
        cwd: cwd.into(),
        model: None,
        effort: None,
        mode: agent_core::adapter::Mode::Ask,
        resume: None,
        new_session_id: None,
        approval_hook: false,
        forced_permissive: false,
        delegation: None,
    }
}

/// Runs `body` with a private main context as the thread default, so every test drives its own
/// loop and tests running in parallel threads never share a context.
pub fn in_loop<R>(body: impl FnOnce(&glib::MainContext) -> R) -> R {
    let ctx = glib::MainContext::new();
    ctx.with_thread_default(|| body(&ctx))
        .expect("acquire the test main context")
}

/// Iterates `ctx` until `cond` holds. Returns false on timeout (`secs`).
pub fn pump_until(ctx: &glib::MainContext, secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        if Instant::now() > deadline {
            return false;
        }
        if !ctx.iteration(false) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    true
}

/// The kept warnings: `Some` while a [`GtkWarningCapture`] is alive, `None` otherwise. The flag
/// and the buffer are one value under one lock, so checking and appending cannot interleave
/// with a guard starting, draining or dropping.
static GTK_WARNINGS: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// Keeps GTK, Adwaita and GDK warnings and criticals (the layout ones: "Trying to measure ...
/// for width of 0", "... exceeds ... width", "attempt to allocate ... width -N") while it lives.
/// Only the domain and message text are kept. GLib lets a log writer be installed once, so the
/// first guard installs a permanent one that forwards every record to the default writer
/// unchanged and collects only while a guard is alive. Exclusive: the collection is
/// process-wide, so starting a guard while another is alive panics (a test bug).
pub struct GtkWarningCapture {
    /// Only [`Self::start`] makes one.
    _private: (),
}

impl GtkWarningCapture {
    pub fn start() -> Self {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            glib::log_set_writer_func(|level, fields| {
                if matches!(level, glib::LogLevel::Warning | glib::LogLevel::Critical) {
                    let field = |key: &str| {
                        fields
                            .iter()
                            .find(|f| f.key() == key)
                            .and_then(|f| f.value_str())
                            .unwrap_or("")
                    };
                    let domain = field("GLIB_DOMAIN");
                    if matches!(domain, "Gtk" | "Adwaita" | "Gdk") {
                        let message = format!("{domain}: {}", field("MESSAGE"));
                        let mut kept = GTK_WARNINGS
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(all) = kept.as_mut() {
                            all.push(message);
                        }
                    }
                }
                glib::log_writer_default(level, fields)
            });
        });
        let mut kept = GTK_WARNINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            kept.is_none(),
            "a GtkWarningCapture is already alive: captures do not nest"
        );
        *kept = Some(Vec::new());
        Self { _private: () }
    }

    /// The warnings kept since the guard started or this was last called.
    pub fn take(&self) -> Vec<String> {
        let mut kept = GTK_WARNINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        kept.as_mut().map(std::mem::take).unwrap_or_default()
    }
}

impl Drop for GtkWarningCapture {
    fn drop(&mut self) {
        *GTK_WARNINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// A widget's minimum size along `orientation` (unconstrained in the other).
pub fn min_size(widget: &gtk4::Widget, orientation: gtk4::Orientation) -> i32 {
    use gtk4::prelude::*;
    widget.measure(orientation, -1).0
}

/// Every mapped `AdwBreakpointBin` under `root` whose child needs more room than the bin was
/// given. A bin with breakpoints reports only its size request as its minimum, so its overflow
/// never shows in `root`'s: each must be checked at its own allocation. Also check clamp widths,
/// since scroll containers can conceal an unbounded row's horizontal minimum.
pub fn overflowing_bins(root: &gtk4::Widget) -> Vec<String> {
    use gtk4::prelude::*;
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if !widget.is_mapped() {
            continue;
        }
        if widget.is::<gtk4::Button>() && widget.has_css_class("pill") {
            let mut parent = widget.parent();
            let mut interruption = false;
            while let Some(ancestor) = parent {
                interruption |= ancestor.has_css_class("interruption-shelf");
                parent = ancestor.parent();
            }
            if interruption {
                if let Some(bounds) = widget.compute_bounds(&widget) {
                    // 34 px: the minimum height for primary actions.
                    if bounds.width() < 34.0 || bounds.height() < 34.0 {
                        out.push(format!(
                            "permission target is {}×{}; needs 34×34",
                            bounds.width(),
                            bounds.height()
                        ));
                    }
                }
            }
        }
        if let Some(bin) = widget.downcast_ref::<adw::BreakpointBin>() {
            if let Some(child) = adw::prelude::BreakpointBinExt::child(bin) {
                let (w, h) = (bin.width(), bin.height());
                let (cw, ch) = (
                    min_size(&child, gtk4::Orientation::Horizontal),
                    min_size(&child, gtk4::Orientation::Vertical),
                );
                if cw > w || ch > h {
                    out.push(format!(
                        "{} {}×{} holds a child needing {cw}×{ch}",
                        child.css_classes().join("."),
                        w,
                        h
                    ));
                }
            }
        }
        if let Some(clamp) = widget.downcast_ref::<adw::Clamp>() {
            if let Some(child) = clamp.child() {
                let needed = min_size(&child, gtk4::Orientation::Horizontal);
                if needed > clamp.width() {
                    out.push(format!(
                        "clamp {} px holds a child needing {needed} px\n{}",
                        clamp.width(),
                        min_size_report(&child, gtk4::Orientation::Horizontal, 100)
                    ));
                }
            }
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    out
}

/// Why `root` needs at least `limit` px along `orientation`: every laid-out descendant that
/// alone needs `threshold` px or more, indented by depth, with its type, CSS classes and any
/// label text. A widget whose minimum is too big shows up with the children that make it so.
pub fn min_size_report(
    root: &gtk4::Widget,
    orientation: gtk4::Orientation,
    threshold: i32,
) -> String {
    use gtk4::prelude::*;
    fn describe(widget: &gtk4::Widget) -> String {
        let classes = widget.css_classes().join(".");
        let text = widget
            .downcast_ref::<gtk4::Label>()
            .map(|l| {
                let t: String = l.text().chars().take(40).collect();
                format!(" {t:?}")
            })
            .unwrap_or_default();
        format!(
            "{}{}{text}",
            widget.type_().name(),
            if classes.is_empty() {
                String::new()
            } else {
                format!(".{classes}")
            }
        )
    }
    fn walk(
        widget: &gtk4::Widget,
        orientation: gtk4::Orientation,
        threshold: i32,
        depth: usize,
        out: &mut String,
    ) {
        let min = min_size(widget, orientation);
        out.push_str(&format!(
            "{:indent$}{min:>5} {}\n",
            "",
            describe(widget),
            indent = depth * 2
        ));
        let mut child = widget.first_child();
        while let Some(c) = child {
            if c.should_layout() && min_size(&c, orientation) >= threshold {
                walk(&c, orientation, threshold, depth + 1, out);
            }
            child = c.next_sibling();
        }
    }
    let mut out = String::new();
    walk(root, orientation, threshold, 0, &mut out);
    out
}

/// `root` and every widget below it.
pub fn descendants(root: &gtk4::Widget) -> Vec<gtk4::Widget> {
    use gtk4::prelude::*;
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
        out.push(widget);
    }
    out
}

/// The first widget of type `T` at or below `root`, in document order (a widget before its
/// children, children in order).
pub fn find_first<T: gtk4::prelude::IsA<gtk4::Widget>>(root: &gtk4::Widget) -> Option<T> {
    find_where(root, &|_| true)
}

/// The first widget of type `T` with CSS class `class` at or below `root`, in document order.
pub fn find_by_class<T: gtk4::prelude::IsA<gtk4::Widget>>(
    root: &gtk4::Widget,
    class: &str,
) -> Option<T> {
    use gtk4::prelude::*;
    find_where(root, &|w: &gtk4::Widget| w.has_css_class(class))
}

/// Every widget of type `T` at or below `root`, in document order.
pub fn find_all<T: gtk4::prelude::IsA<gtk4::Widget>>(root: &gtk4::Widget) -> Vec<T> {
    let mut out = Vec::new();
    collect_where(root, &|_| true, &mut out);
    out
}

/// Every widget of type `T` with CSS class `class` at or below `root`, in document order.
pub fn find_all_by_class<T: gtk4::prelude::IsA<gtk4::Widget>>(
    root: &gtk4::Widget,
    class: &str,
) -> Vec<T> {
    use gtk4::prelude::*;
    let mut out = Vec::new();
    collect_where(root, &|w: &gtk4::Widget| w.has_css_class(class), &mut out);
    out
}

fn collect_where<T: gtk4::prelude::IsA<gtk4::Widget>>(
    widget: &gtk4::Widget,
    keep: &dyn Fn(&gtk4::Widget) -> bool,
    out: &mut Vec<T>,
) {
    use gtk4::prelude::*;
    if keep(widget) {
        if let Some(found) = widget.downcast_ref::<T>() {
            out.push(found.clone());
        }
    }
    let mut child = widget.first_child();
    while let Some(w) = child {
        collect_where(&w, keep, out);
        child = w.next_sibling();
    }
}

fn find_where<T: gtk4::prelude::IsA<gtk4::Widget>>(
    widget: &gtk4::Widget,
    keep: &dyn Fn(&gtk4::Widget) -> bool,
) -> Option<T> {
    use gtk4::prelude::*;
    if keep(widget) {
        if let Some(found) = widget.downcast_ref::<T>() {
            return Some(found.clone());
        }
    }
    let mut child = widget.first_child();
    while let Some(w) = child {
        if let Some(found) = find_where(&w, keep) {
            return Some(found);
        }
        child = w.next_sibling();
    }
    None
}

/// The text of every label at or below `root`.
pub fn label_texts(root: &gtk4::Widget) -> Vec<String> {
    use gtk4::prelude::*;
    descendants(root)
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk4::Label>())
        .map(|l| l.text().to_string())
        .collect()
}

/// The marker `make` sets (`DISPLAY_ENV`) for a run on its private Xvfb display and D-Bus session.
pub const VIRTUAL_DISPLAY_MARKER: &str = "AGENT_TERMINAL_TEST_DISPLAY";

/// Panics unless the test runs on the private display the make targets provide. A GTK test opens
/// real windows, and a bare `cargo test` would put them on the developer's own desktop.
pub fn require_virtual_display() {
    assert!(
        std::env::var(VIRTUAL_DISPLAY_MARKER).as_deref() == Ok("xvfb"),
        "GTK tests open windows: run them through `make test` / `make test-one T=...`, which use \
         a private Xvfb display ({VIRTUAL_DISPLAY_MARKER}=xvfb is not set)"
    );
}

/// `gtk4::init()` for tests: the one way a test initialises GTK, behind
/// [`require_virtual_display`].
pub fn gtk_init() -> Result<(), glib::BoolError> {
    require_virtual_display();
    gtk4::init()
}

/// `adw::init()` for tests, behind [`require_virtual_display`] like [`gtk_init`].
pub fn adw_init() -> Result<(), glib::BoolError> {
    require_virtual_display();
    adw::init()
}

/// GTK, libadwaita, the bundled icons and the stylesheet, as the app sets them up: for the
/// native tests that present windows. Returns the default main context they run on.
pub fn native_init() -> glib::MainContext {
    gtk_init().expect("GTK must be available for the native gate");
    adw_init().expect("libadwaita init");
    crate::icons::register();
    crate::load_css();
    glib::MainContext::default()
}

/// Whether pressing `widget` does something by itself: a button, a list row or anything with a
/// click gesture of its own.
fn is_click_target(widget: &gtk4::Widget) -> bool {
    use gtk4::prelude::*;
    widget.is::<gtk4::Button>()
        || widget.is::<gtk4::MenuButton>()
        || widget.is::<gtk4::ListBoxRow>()
        || widget.is::<gtk4::Expander>()
        || widget.is::<gtk4::CheckButton>()
        || widget
            .observe_controllers()
            .into_iter()
            .flatten()
            .any(|c| c.is::<gtk4::GestureClick>())
}

/// Describes every selectable label under `root` that sits inside a button or click target. A
/// selectable label claims the press (it places a caret), so the target never fires.
pub fn selectable_labels_in_click_targets(root: &gtk4::Widget) -> Vec<String> {
    use gtk4::prelude::*;
    let mut out = Vec::new();
    for widget in descendants(root) {
        let Some(label) = widget.downcast_ref::<gtk4::Label>() else {
            continue;
        };
        if !label.is_selectable() {
            continue;
        }
        let mut parent = label.parent();
        while let Some(ancestor) = parent {
            if is_click_target(&ancestor) {
                out.push(format!(
                    "selectable label {:?} ({}) inside {} ({})",
                    label.text().chars().take(40).collect::<String>(),
                    label.css_classes().join("."),
                    ancestor.type_().name(),
                    ancestor.css_classes().join(".")
                ));
                break;
            }
            parent = ancestor.parent();
        }
    }
    out
}

/// Accessibility and hit-target problems of every button under `root`, popover contents
/// included (a popover is audited while closed, as long as its button is shown): no accessible
/// name, or smaller than `min` px in either direction (natural size, so it works unmapped).
pub fn button_problems(root: &gtk4::Widget, min: i32) -> Vec<String> {
    use gtk4::prelude::*;
    let mut out = Vec::new();
    for widget in descendants(root) {
        let button = widget.is::<gtk4::Button>()
            || widget.is::<gtk4::MenuButton>()
            || widget.is::<gtk4::ToggleButton>();
        // A MenuButton's inner toggle is the MenuButton's business; hidden buttons are not
        // targets.
        let inner = widget
            .parent()
            .is_some_and(|p| p.is::<gtk4::MenuButton>() || p.is::<adw::SplitButton>());
        if !button || inner || !shown_or_in_popover(&widget) {
            continue;
        }
        let named = widget
            .downcast_ref::<gtk4::Button>()
            .and_then(|b| b.label())
            .is_some_and(|l| !l.is_empty())
            || widget
                .downcast_ref::<gtk4::MenuButton>()
                .and_then(|b| b.label())
                .is_some_and(|l| !l.is_empty())
            || gtk4::test_accessible_has_property(
                widget.upcast_ref::<gtk4::Widget>(),
                gtk4::AccessibleProperty::Label,
            )
            || descendants(&widget).iter().any(|w| {
                w.downcast_ref::<gtk4::Label>()
                    .is_some_and(|l| !l.text().is_empty())
            });
        let describe = || describe_button(&widget);
        if !named {
            out.push(format!("{} has no accessible name", describe()));
        }
        let (w, h) = (
            widget.measure(gtk4::Orientation::Horizontal, -1).1,
            widget.measure(gtk4::Orientation::Vertical, -1).1,
        );
        if w < min || h < min {
            out.push(format!("{} is {w}x{h}, under {min}", describe()));
        }
    }
    out
}

/// Describes every `.card-section` and `.code-block` under `root` that has no Copy button.
pub fn blocks_without_copy(root: &gtk4::Widget) -> Vec<String> {
    use gtk4::prelude::*;
    descendants(root)
        .into_iter()
        .filter(|w| w.has_css_class("card-section") || w.has_css_class("code-block"))
        .filter(|block| {
            !descendants(block).iter().any(|w| {
                w.is::<gtk4::Button>()
                    && w.tooltip_text()
                        .is_some_and(|tip| tip.to_lowercase().starts_with("copy"))
            })
        })
        .map(|block| {
            format!(
                "{} ({}) has no Copy button",
                block.type_().name(),
                block.css_classes().join(".")
            )
        })
        .collect()
}

/// A button for a failure message: its type, CSS classes and the ancestors above it, with a row's title.
fn describe_button(widget: &gtk4::Widget) -> String {
    use gtk4::prelude::*;
    let mut place = Vec::new();
    let mut at = widget.parent();
    while let Some(parent) = at {
        if place.len() == 6 {
            break;
        }
        let title = parent
            .downcast_ref::<adw::PreferencesRow>()
            .map(|row| format!(" {:?}", adw::prelude::PreferencesRowExt::title(row)))
            .unwrap_or_default();
        place.push(format!(
            "{}({}){title}",
            parent.type_().name(),
            parent.css_classes().join(".")
        ));
        at = parent.parent();
    }
    format!(
        "{} ({}) in {}",
        widget.type_().name(),
        widget.css_classes().join("."),
        place.join(" < ")
    )
}

/// Whether `widget` and every ancestor is visible (a hidden widget measures 0x0), except that a
/// popover counts as visible whenever its own parent is: its contents are built and measured
/// while it is closed, and are what opens on a click.
pub fn shown_or_in_popover(widget: &gtk4::Widget) -> bool {
    use gtk4::prelude::*;
    let mut at = Some(widget.clone());
    while let Some(w) = at {
        if !w.is::<gtk4::Popover>() && !w.is_visible() {
            return false;
        }
        at = w.parent();
    }
    true
}

/// Every `Button`, `MenuButton` and `ToggleButton` under `root` (popover contents included) that
/// shows an icon and no text, yet has no explicit accessible label. A tooltip alone does not
/// count: it is not reliably exposed as a name.
pub fn icon_only_without_label(root: &gtk4::Widget) -> Vec<String> {
    use gtk4::prelude::*;
    let mut out = Vec::new();
    for widget in descendants(root) {
        let icon_name = if let Some(b) = widget.downcast_ref::<gtk4::Button>() {
            b.icon_name().filter(|n| !n.is_empty())
        } else if let Some(b) = widget.downcast_ref::<gtk4::MenuButton>() {
            b.icon_name().filter(|n| !n.is_empty())
        } else {
            None
        };
        if !widget.is::<gtk4::Button>() && !widget.is::<gtk4::MenuButton>() {
            continue;
        }
        let inner = widget
            .parent()
            .is_some_and(|p| p.is::<gtk4::MenuButton>() || p.is::<adw::SplitButton>());
        if inner || !shown_or_in_popover(&widget) {
            continue;
        }
        let has_text = descendants(&widget).iter().any(|w| {
            w.downcast_ref::<gtk4::Label>()
                .is_some_and(|l| !l.text().is_empty())
        });
        let has_icon = icon_name.is_some()
            || descendants(&widget)
                .iter()
                .any(|w| w.is::<gtk4::Image>() || w.is::<gtk4::Picture>());
        if has_icon
            && !has_text
            && !gtk4::test_accessible_has_property(&widget, gtk4::AccessibleProperty::Label)
        {
            out.push(format!(
                "{} shows an icon with no text and no accessible label (tooltip {:?})",
                describe_button(&widget),
                widget.tooltip_text()
            ));
        }
    }
    out
}

/// Runs the loop until nothing has happened for 100 ms (layout, scrolling and idle work have
/// finished), giving up after 1 s (a spinner never goes quiet). A bounded poll on quiescence, not a fixed sleep.
pub fn settle(ctx: &glib::MainContext) {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut quiet_since = Instant::now();
    while Instant::now() < deadline && quiet_since.elapsed() < Duration::from_millis(100) {
        if ctx.iteration(false) {
            quiet_since = Instant::now();
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod guard_tests {
    use std::path::{Path, PathBuf};

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// No test can start GTK or libadwaita without the virtual-display guard: the raw calls
    /// live only in this file (and the integration test's own copy of the check).
    #[test]
    fn gtk_is_initialised_only_through_the_guard() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        rust_files(&root.join("tests"), &mut files);
        assert!(!files.is_empty(), "no source files found under {root:?}");
        let mut bad = Vec::new();
        for file in files {
            let rel = file.strip_prefix(root).unwrap_or(&file).to_path_buf();
            if rel == Path::new("src/testutil.rs") {
                continue;
            }
            let gtk_allowed = rel == Path::new("tests/icons.rs");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (n, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let raw_gtk = line.contains("gtk4::init(") || line.contains("gtk::init(");
                if (raw_gtk && !gtk_allowed) || line.contains("adw::init(") {
                    bad.push(format!("{}:{}: {}", rel.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "initialise GTK through testutil::gtk_init / adw_init / native_init:\n{}",
            bad.join("\n")
        );
    }
}
