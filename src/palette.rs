//! The app-wide colours, from the theme chosen in Settings.
//!
//! Every stylesheet (the brand chrome in `main.rs`, the chat view's, the loading screens') names
//! its colours (`@at_bg`, `@at_fg_dim`, `@at_accent`…) instead of writing them out; this module
//! defines those names for the selected [`ThemeChoice`] in one CSS provider, together with the
//! libadwaita variables (`--window-bg-color`…) and the legacy named colours older libadwaita
//! reads, and swaps it when the theme changes. So the theme restyles the whole window, not just
//! the terminals.
//!
//! The app's own theme keeps its hand-tuned values exactly. Every other bundled theme derives the
//! same tokens from its terminal palette: background, foreground, and its red, green, yellow,
//! blue and magenta (the accent). The agents' brand colours (Claude, agy, Codex) are not themed:
//! they identify the agent.
//!
//! [`ThemeChoice::System`] has no values of its own. Each name is an alias of a colour the
//! desktop's GTK theme defines ([`SYSTEM_COLOURS`]), libadwaita's own colours are left alone and
//! the colour scheme is not forced, so light or dark, the accent colour and any `gtk.css` all come
//! through. What Rust code needs as values (terminal and diff colours, Pango markup) is read
//! back from the resolved style, again whenever the desktop's style changes.

use std::cell::{Cell, RefCell};

use gtk4::gdk;
use gtk4::prelude::*;

use crate::config::ThemeChoice;
use crate::theme::Theme;

/// WCAG AA for body text: what every text colour must reach on what it is drawn on.
pub const MIN_TEXT_CONTRAST: f64 = 4.5;

/// An opaque sRGB colour, components 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    pub const WHITE: Rgb = Rgb(1.0, 1.0, 1.0);
    pub const BLACK: Rgb = Rgb(0.0, 0.0, 0.0);

    pub fn from_hex(hex: u32) -> Rgb {
        let c = |shift: u32| f64::from((hex >> shift) & 0xff) / 255.0;
        Rgb(c(16), c(8), c(0))
    }

    fn from_rgba(c: &gdk::RGBA) -> Rgb {
        Rgb(
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
        )
    }

    /// `c` drawn over `under`: what a translucent colour (Adwaita's text is one) looks like.
    fn over(c: &gdk::RGBA, under: Rgb) -> Rgb {
        under.mix(Rgb::from_rgba(c), f64::from(c.alpha()))
    }

    pub fn to_rgba(self) -> gdk::RGBA {
        gdk::RGBA::new(self.0 as f32, self.1 as f32, self.2 as f32, 1.0)
    }

    /// `self` moved `t` (0..=1) of the way to `other`.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let m = |a: f64, b: f64| a + (b - a) * t;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    /// WCAG 2 relative luminance.
    pub fn luminance(self) -> f64 {
        let l = |c: f64| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * l(self.0) + 0.7152 * l(self.1) + 0.0722 * l(self.2)
    }

    /// WCAG 2 contrast ratio with `other` (1..=21).
    pub fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `self`, moved just far enough toward `toward` to reach `min` contrast on `bg` (all the
    /// way, if even `toward` falls short).
    pub fn readable_on(self, bg: Rgb, toward: Rgb, min: f64) -> Rgb {
        self.readable_on_all(&[bg], toward, min)
    }

    /// [`Self::readable_on`] against every one of `grounds` at once.
    pub fn readable_on_all(self, grounds: &[Rgb], toward: Rgb, min: f64) -> Rgb {
        (0..=20)
            .map(|step| self.mix(toward, f64::from(step) / 20.0))
            .find(|c| c.reads_on_all(grounds, min))
            .unwrap_or(toward)
    }

    /// Whether `self` reaches `min` contrast on every one of `grounds`.
    pub fn reads_on_all(self, grounds: &[Rgb], min: f64) -> bool {
        grounds.iter().all(|g| self.contrast(*g) >= min)
    }

    /// Darker (`f` < 1) or lighter (`f` > 1), clamped.
    pub fn shade(self, f: f64) -> Rgb {
        let s = |a: f64| (a * f).clamp(0.0, 1.0);
        Rgb(s(self.0), s(self.1), s(self.2))
    }

    pub fn hex(self) -> String {
        let b = |a: f64| (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", b(self.0), b(self.1), b(self.2))
    }

    /// `rgba(…)` at `alpha`, for the CSS variables that want a translucent colour.
    pub fn rgba(self, alpha: f64) -> String {
        let b = |a: f64| (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("rgba({}, {}, {}, {alpha})", b(self.0), b(self.1), b(self.2))
    }
}

/// Every named colour the stylesheets use, by role.
#[derive(Debug, Clone, PartialEq)]
pub struct Chrome {
    /// The window and chat background.
    pub bg: Rgb,
    /// Header bars, the sidebar, code blocks: a step darker.
    pub bg_deep: Rgb,
    /// Wells a little below the background (diff and output panes).
    pub bg_sunken: Rgb,
    /// Cards, popovers, the composer.
    pub surface: Rgb,
    /// Hovered or nested surfaces.
    pub surface_raised: Rgb,
    /// Fields and chips set into a surface.
    pub surface_strong: Rgb,
    /// A surface tinted with the accent (selection, the user's own messages).
    pub accent_surface: Rgb,
    pub border: Rgb,
    pub border_strong: Rgb,
    pub border_hover: Rgb,
    /// Titles and the text you type.
    pub fg_strong: Rgb,
    /// Body text.
    pub fg: Rgb,
    /// Secondary text (folder names, labels).
    pub fg_soft: Rgb,
    /// Captions and hints.
    pub fg_dim: Rgb,
    /// The faintest text (timestamps, placeholders).
    pub fg_faint: Rgb,
    pub accent: Rgb,
    pub accent_hover: Rgb,
    pub success: Rgb,
    pub success_soft: Rgb,
    pub warning: Rgb,
    pub warning_soft: Rgb,
    pub danger: Rgb,
    pub danger_soft: Rgb,
    /// Text on a danger background.
    pub danger_text: Rgb,
    pub danger_bg: Rgb,
    pub danger_border: Rgb,
    /// Background work, links.
    pub info: Rgb,
}

/// The system theme: each name, as an expression of the desktop theme's named colours (the ones
/// libadwaita documents, which a `gtk.css` theme redefines). Mixes and alphas only place a role
/// between two of the desktop's colours; none brings a colour of its own. Same names, same
/// order as [`Chrome::named`].
pub const SYSTEM_COLOURS: [(&str, &str); 27] = [
    ("at_bg", "@window_bg_color"),
    ("at_bg_deep", "@headerbar_bg_color"),
    ("at_bg_sunken", "@view_bg_color"),
    ("at_surface", "@card_bg_color"),
    (
        "at_surface_raised",
        "mix(@window_bg_color, @window_fg_color, 0.08)",
    ),
    (
        "at_surface_strong",
        "mix(@window_bg_color, @accent_bg_color, 0.14)",
    ),
    (
        "at_accent_surface",
        "mix(@window_bg_color, @accent_bg_color, 0.28)",
    ),
    ("at_border", "alpha(@window_fg_color, 0.15)"),
    ("at_border_strong", "alpha(@window_fg_color, 0.25)"),
    ("at_border_hover", "alpha(@accent_color, 0.6)"),
    ("at_fg_strong", "@window_fg_color"),
    ("at_fg", "@window_fg_color"),
    // Secondary text stays at 4.5:1 on Adwaita's light window, whose text is itself 80 % opaque
    // (`a_system_themes_secondary_text_reads_on_adwaita`).
    ("at_fg_soft", "alpha(@window_fg_color, 0.9)"),
    ("at_fg_dim", "alpha(@window_fg_color, 0.8)"),
    ("at_fg_faint", "alpha(@window_fg_color, 0.72)"),
    ("at_accent", "@accent_color"),
    (
        "at_accent_hover",
        "mix(@accent_color, @window_fg_color, 0.15)",
    ),
    ("at_success", "@success_color"),
    (
        "at_success_soft",
        "mix(@success_color, @window_fg_color, 0.25)",
    ),
    ("at_warning", "@warning_color"),
    (
        "at_warning_soft",
        "mix(@warning_color, @window_fg_color, 0.25)",
    ),
    ("at_danger", "@error_color"),
    ("at_danger_soft", "mix(@error_color, @window_fg_color, 0.2)"),
    ("at_danger_text", "mix(@error_color, @window_fg_color, 0.5)"),
    (
        "at_danger_bg",
        "mix(@window_bg_color, @error_bg_color, 0.14)",
    ),
    (
        "at_danger_border",
        "mix(@window_bg_color, @error_color, 0.42)",
    ),
    ("at_info", "@accent_color"),
];

/// The shadow under floating chrome (the interruption shelf). Translucent, so it stays apart
/// from [`Chrome::named`] (opaque colours, read back by probes): a bundled palette draws a
/// neutral black, the system theme libadwaita's own shade, so the desktop decides.
pub const SHADOW_NAME: &str = "at_shadow";
const PALETTE_SHADOW: &str = "rgba(0, 0, 0, 0.35)";
const SYSTEM_SHADOW: &str = "@shade_color";

/// The class a probe label takes to be coloured `name`, so its value can be read back.
fn probe_class(name: &str) -> String {
    format!("at-probe-{}", name.replace('_', "-"))
}

/// The system theme's stylesheet: the [`SYSTEM_COLOURS`] aliases, and a probe rule per name
/// ([`resolve_system`] reads them). Nothing of libadwaita's is redefined.
fn system_css() -> String {
    let mut css = String::new();
    for (name, value) in SYSTEM_COLOURS {
        css.push_str(&format!("@define-color {name} {value};\n"));
    }
    css.push_str(&format!("@define-color {SHADOW_NAME} {SYSTEM_SHADOW};\n"));
    for (name, _) in SYSTEM_COLOURS {
        css.push_str(&format!(
            "label.{} {{ color: @{name}; }}\n",
            probe_class(name)
        ));
    }
    css
}

/// The system theme's colours as they now resolve: each name read from a probe label's style.
/// Translucent ones are composited over the background, as they are drawn.
fn resolve_system() -> Chrome {
    let read = |name: &str| {
        let probe = gtk4::Label::new(None);
        probe.add_css_class(&probe_class(name));
        probe.color()
    };
    let bg = Rgb::from_rgba(&read("at_bg"));
    let c = |name: &str| Rgb::over(&read(name), bg);
    Chrome {
        bg,
        bg_deep: c("at_bg_deep"),
        bg_sunken: c("at_bg_sunken"),
        surface: c("at_surface"),
        surface_raised: c("at_surface_raised"),
        surface_strong: c("at_surface_strong"),
        accent_surface: c("at_accent_surface"),
        border: c("at_border"),
        border_strong: c("at_border_strong"),
        border_hover: c("at_border_hover"),
        fg_strong: c("at_fg_strong"),
        fg: c("at_fg"),
        fg_soft: c("at_fg_soft"),
        fg_dim: c("at_fg_dim"),
        fg_faint: c("at_fg_faint"),
        accent: c("at_accent"),
        accent_hover: c("at_accent_hover"),
        success: c("at_success"),
        success_soft: c("at_success_soft"),
        warning: c("at_warning"),
        warning_soft: c("at_warning_soft"),
        danger: c("at_danger"),
        danger_soft: c("at_danger_soft"),
        danger_text: c("at_danger_text"),
        danger_bg: c("at_danger_bg"),
        danger_border: c("at_danger_border"),
        info: c("at_info"),
    }
}

impl Chrome {
    /// The colours of a bundled palette; `None` for the system theme, whose colours are the
    /// desktop's (see [`resolve_system`]).
    pub fn for_choice(choice: ThemeChoice) -> Option<Chrome> {
        match choice {
            ThemeChoice::System => None,
            ThemeChoice::AgentTerminal => Some(Self::agent_terminal().with_readable_text()),
            other => Theme::chrome_base(other).map(|base| {
                let (bg, fg, accent) = (
                    Rgb::from_rgba(&base.background),
                    Rgb::from_rgba(&base.foreground),
                    Rgb::from_rgba(&base.accent),
                );
                let states = [
                    Rgb::from_rgba(&base.green),
                    Rgb::from_rgba(&base.yellow),
                    Rgb::from_rgba(&base.red),
                    Rgb::from_rgba(&base.blue),
                ];
                if other.is_light() == Some(true) {
                    Self::derived_light(bg, fg, accent, states)
                        .with_readable_accents()
                        .with_readable_text()
                } else {
                    Self::derived(bg, fg, accent, states).with_readable_text()
                }
            }),
        }
    }

    /// The secondary text colours (soft, dim, faint) lifted to [`MIN_TEXT_CONTRAST`] on every
    /// surface text is drawn on, each moved toward the body text only as far as it must (toward
    /// the strong text where even the body text falls short), so their order is kept.
    fn with_readable_text(mut self) -> Chrome {
        let grounds = [self.bg, self.surface, self.surface_raised];
        let lift = |c: Rgb| {
            let toward_fg = c.readable_on_all(&grounds, self.fg, MIN_TEXT_CONTRAST);
            if toward_fg.reads_on_all(&grounds, MIN_TEXT_CONTRAST) {
                toward_fg
            } else {
                toward_fg.readable_on_all(&grounds, self.fg_strong, MIN_TEXT_CONTRAST)
            }
        };
        self.fg_soft = lift(self.fg_soft);
        self.fg_dim = lift(self.fg_dim);
        self.fg_faint = lift(self.fg_faint);
        self
    }

    /// The app's own palette, as hand-tuned (the values the stylesheets were written with).
    fn agent_terminal() -> Chrome {
        let h = Rgb::from_hex;
        Chrome {
            bg: h(0x181425),
            bg_deep: h(0x120f1d),
            bg_sunken: h(0x171226),
            surface: h(0x1d1830),
            surface_raised: h(0x241d38),
            surface_strong: h(0x2a2344),
            accent_surface: h(0x3a2d6b),
            border: h(0x2d2444),
            border_strong: h(0x3b3160),
            border_hover: h(0x52467f),
            fg_strong: h(0xebe9ff),
            fg: h(0xc8c8ff),
            fg_soft: h(0xa8a2dc),
            fg_dim: h(0x918bbd),
            fg_faint: h(0x6d6794),
            accent: h(0xb49bff),
            accent_hover: h(0xc4b0ff),
            success: h(0x4ee8b0),
            success_soft: h(0x8ff3cd),
            warning: h(0xffc46b),
            warning_soft: h(0xffd08a),
            danger: h(0xff7878),
            danger_soft: h(0xff9a9a),
            danger_text: h(0xffc4c4),
            danger_bg: h(0x3a1f2b),
            danger_border: h(0x7a3b4c),
            info: h(0x7cc7ff),
        }
    }

    /// Every token from a theme's background, foreground and accent, and its success, warning,
    /// danger and info colours (green, yellow, red, blue). The proportions reproduce the app's
    /// own palette from its own base colours, near enough.
    pub fn derived(
        bg: Rgb,
        fg: Rgb,
        accent: Rgb,
        [success, warning, danger, info]: [Rgb; 4],
    ) -> Chrome {
        Chrome {
            bg,
            bg_deep: bg.shade(0.75),
            bg_sunken: bg.shade(0.9),
            surface: bg.mix(fg, 0.035),
            surface_raised: bg.mix(fg, 0.07),
            surface_strong: bg.mix(accent, 0.12),
            accent_surface: bg.mix(accent, 0.25),
            border: bg.mix(accent, 0.14),
            border_strong: bg.mix(accent, 0.24),
            border_hover: bg.mix(accent, 0.4),
            fg_strong: fg.mix(Rgb::WHITE, 0.55),
            fg,
            fg_soft: fg.mix(bg, 0.18),
            fg_dim: fg.mix(bg, 0.32),
            fg_faint: fg.mix(bg, 0.52),
            accent,
            accent_hover: accent.mix(Rgb::WHITE, 0.2),
            success,
            success_soft: success.mix(Rgb::WHITE, 0.35),
            warning,
            warning_soft: warning.mix(Rgb::WHITE, 0.25),
            danger,
            danger_soft: danger.mix(Rgb::WHITE, 0.2),
            danger_text: danger.mix(Rgb::WHITE, 0.6),
            danger_bg: bg.mix(danger, 0.14),
            danger_border: bg.mix(danger, 0.42),
            info,
        }
    }

    /// [`Self::derived`] for a light background: surfaces step toward the text instead of
    /// toward black, and the emphasised and hover colours darken instead of lightening. The
    /// accent, success, warning, danger and info colours are taken as given; see
    /// [`Self::with_readable_accents`].
    pub fn derived_light(
        bg: Rgb,
        fg: Rgb,
        accent: Rgb,
        [success, warning, danger, info]: [Rgb; 4],
    ) -> Chrome {
        Chrome {
            bg,
            bg_deep: bg.mix(fg, 0.06),
            bg_sunken: bg.mix(fg, 0.03),
            surface: bg.mix(Rgb::WHITE, 0.6),
            surface_raised: bg.mix(fg, 0.05),
            surface_strong: bg.mix(accent, 0.12),
            accent_surface: bg.mix(accent, 0.22),
            border: bg.mix(fg, 0.16),
            border_strong: bg.mix(fg, 0.26),
            border_hover: bg.mix(fg, 0.40),
            fg_strong: fg.mix(Rgb::BLACK, 0.5),
            fg,
            fg_soft: fg.mix(bg, 0.18),
            fg_dim: fg.mix(bg, 0.32),
            fg_faint: fg.mix(bg, 0.52),
            accent,
            accent_hover: accent.mix(Rgb::BLACK, 0.2),
            success,
            success_soft: success.mix(Rgb::BLACK, 0.25),
            warning,
            warning_soft: warning.mix(Rgb::BLACK, 0.25),
            danger,
            danger_soft: danger.mix(Rgb::BLACK, 0.25),
            danger_text: danger.mix(Rgb::BLACK, 0.45),
            danger_bg: bg.mix(danger, 0.12),
            danger_border: bg.mix(danger, 0.42),
            info,
        }
    }

    /// A light scheme's accent, success, warning, danger and info colours lifted to
    /// [`MIN_TEXT_CONTRAST`] on the background, the surface and the raised surface, each moved
    /// toward the text only as far as it must, and every colour derived from them rebuilt from
    /// the lifted ones. Lifting the accent also keeps the text on an accent fill (the window
    /// background) readable.
    fn with_readable_accents(self) -> Chrome {
        let grounds = [self.bg, self.surface, self.surface_raised];
        let lift = |c: Rgb| c.readable_on_all(&grounds, self.fg, MIN_TEXT_CONTRAST);
        let lift_strong = |c: Rgb| {
            let lifted = lift(c);
            if lifted.reads_on_all(&grounds, MIN_TEXT_CONTRAST) {
                lifted
            } else {
                lifted.readable_on_all(&grounds, self.fg_strong, MIN_TEXT_CONTRAST)
            }
        };
        Self::derived_light(
            self.bg,
            self.fg,
            lift_strong(self.accent),
            [
                lift_strong(self.success),
                lift_strong(self.warning),
                lift_strong(self.danger),
                lift_strong(self.info),
            ],
        )
    }

    /// The names the stylesheets use, and their colours.
    fn named(&self) -> [(&'static str, Rgb); 27] {
        [
            ("at_bg", self.bg),
            ("at_bg_deep", self.bg_deep),
            ("at_bg_sunken", self.bg_sunken),
            ("at_surface", self.surface),
            ("at_surface_raised", self.surface_raised),
            ("at_surface_strong", self.surface_strong),
            ("at_accent_surface", self.accent_surface),
            ("at_border", self.border),
            ("at_border_strong", self.border_strong),
            ("at_border_hover", self.border_hover),
            ("at_fg_strong", self.fg_strong),
            ("at_fg", self.fg),
            ("at_fg_soft", self.fg_soft),
            ("at_fg_dim", self.fg_dim),
            ("at_fg_faint", self.fg_faint),
            ("at_accent", self.accent),
            ("at_accent_hover", self.accent_hover),
            ("at_success", self.success),
            ("at_success_soft", self.success_soft),
            ("at_warning", self.warning),
            ("at_warning_soft", self.warning_soft),
            ("at_danger", self.danger),
            ("at_danger_soft", self.danger_soft),
            ("at_danger_text", self.danger_text),
            ("at_danger_bg", self.danger_bg),
            ("at_danger_border", self.danger_border),
            ("at_info", self.info),
        ]
    }

    /// The stylesheet that defines every name, and maps libadwaita's own colours onto them.
    pub fn css(&self) -> String {
        let mut css = String::new();
        for (name, colour) in self.named() {
            css.push_str(&format!("@define-color {name} {};\n", colour.hex()));
        }
        css.push_str(&format!("@define-color {SHADOW_NAME} {PALETTE_SHADOW};\n"));
        // libadwaita before 1.6 reads these names; 1.6 and later the variables below.
        let legacy = [
            ("accent_color", self.accent),
            ("accent_bg_color", self.accent),
            ("accent_fg_color", self.bg),
            ("window_bg_color", self.bg),
            ("window_fg_color", self.fg),
            ("view_bg_color", self.bg),
            ("view_fg_color", self.fg),
            ("headerbar_bg_color", self.bg_deep),
            ("headerbar_fg_color", self.fg),
            ("sidebar_bg_color", self.bg_deep),
            ("dialog_bg_color", self.surface),
            ("dialog_fg_color", self.fg),
            ("popover_bg_color", self.surface),
            ("popover_fg_color", self.fg),
        ];
        for (name, colour) in legacy {
            css.push_str(&format!("@define-color {name} {};\n", colour.hex()));
        }
        css.push_str(":root {\n");
        for (name, colour) in legacy {
            css.push_str(&format!(
                "    --{}: {};\n",
                name.replace('_', "-"),
                colour.hex()
            ));
        }
        css.push_str(&format!("    --card-bg-color: {};\n", self.fg.rgba(0.05)));
        css.push_str("}\n");
        css
    }
}

thread_local! {
    static PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
    static CURRENT: RefCell<Option<Chrome>> = const { RefCell::new(None) };
    /// Whether the system theme is the one applied, so a desktop change re-resolves it.
    static SYSTEM_ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// Whether the desktop's style is being watched (connected once per process).
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    /// Whether a re-resolve is queued: one desktop switch notifies several properties at once,
    /// and they share one refresh.
    static REFRESH_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Recolours what holds colours as values (terminals, diff panels) after the system
    /// theme re-resolved. Set by the app.
    static ON_SYSTEM_CHANGE: Cell<Option<fn()>> = const { Cell::new(None) };
    /// Bumped each time the colours in use change.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// What to run after the colours in use change (see [`on_change`]).
    static ON_CHANGE: RefCell<Vec<fn()>> = const { RefCell::new(Vec::new()) };
}

/// Counts the changes of the colours in use. Anything that took colours as values (Pango
/// markup, a source view's scheme) compares it with the one it was drawn under.
pub fn generation() -> u64 {
    GENERATION.with(Cell::get)
}

/// Runs `f` after every change of the colours in use, once registered (the same `f` is kept
/// once). It runs on the main thread, with the new colours already in [`current`].
pub fn on_change(f: fn()) {
    ON_CHANGE.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        if !hooks.contains(&f) {
            hooks.push(f);
        }
    });
}

/// Whether the colours in use are light: a background nearer white than black, which holds for
/// every bundled scheme and for the desktop's own light and dark windows alike.
pub fn is_light() -> bool {
    current().bg.luminance() > 0.179
}

/// The colours in use (the app's own until [`apply`] first runs).
pub fn current() -> Chrome {
    CURRENT
        .with(|c| c.borrow().clone())
        .unwrap_or_else(Chrome::agent_terminal)
}

/// What to run after the system theme follows a change of the desktop's style: recolour what
/// took its colours as values.
pub fn set_on_system_change(f: fn()) {
    ON_SYSTEM_CHANGE.with(|slot| slot.set(Some(f)));
}

fn set_current(chrome: Chrome) {
    crate::icons::set_hero_tint(chrome.fg.hex());
    CURRENT.with(|c| *c.borrow_mut() = Some(chrome));
    GENERATION.with(|g| g.set(g.get() + 1));
    let hooks: Vec<fn()> = ON_CHANGE.with(|hooks| hooks.borrow().clone());
    for hook in hooks {
        hook();
    }
}

/// The libadwaita colour scheme forced for a bundled palette: light for a light one, dark
/// otherwise, so popovers, menus and entries follow the palette instead of the opposite default
/// (pale text on white, or the reverse).
fn forced_scheme(choice: ThemeChoice) -> adw::ColorScheme {
    if choice.is_light() == Some(true) {
        adw::ColorScheme::ForceLight
    } else {
        adw::ColorScheme::ForceDark
    }
}

/// Restyles the whole app for `choice`: defines every named colour for it, replacing the last
/// theme's. Above the app's own stylesheets, so the names resolve however they load.
pub fn apply(choice: ThemeChoice) {
    let system = choice == ThemeChoice::System;
    SYSTEM_ACTIVE.with(|s| s.set(system));
    let Some(chrome) = Chrome::for_choice(choice) else {
        apply_system();
        return;
    };
    let css = chrome.css();
    set_current(chrome);
    if gdk::Display::default().is_none() {
        return;
    }
    adw::StyleManager::default().set_color_scheme(forced_scheme(choice));
    load(&css);
}

/// The system theme: the desktop's colour scheme, the aliases, and the colours they resolve to.
fn apply_system() {
    if gdk::Display::default().is_none() {
        set_current(Chrome::agent_terminal());
        return;
    }
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
    load(&system_css());
    set_current(resolve_system());
    watch_system();
}

/// Re-resolves the system theme when the desktop's style changes: dark or light, accent,
/// contrast, or the GTK theme. On idle, once the new stylesheet is in.
fn watch_system() {
    if WATCHING.with(|w| w.replace(true)) {
        return;
    }
    let refresh = || {
        if REFRESH_PENDING.with(|p| p.replace(true)) {
            return;
        }
        gtk4::glib::idle_add_local_once(|| {
            REFRESH_PENDING.with(|p| p.set(false));
            if !SYSTEM_ACTIVE.with(Cell::get) {
                return;
            }
            set_current(resolve_system());
            if let Some(f) = ON_SYSTEM_CHANGE.with(Cell::get) {
                f();
            }
        });
    };
    let style = adw::StyleManager::default();
    // `accent-color` exists from libadwaita 1.6; on older ones it never notifies, and the
    // accent never changes either.
    for property in ["dark", "accent-color", "high-contrast"] {
        style.connect_notify_local(Some(property), move |_, _| refresh());
    }
    if let Some(settings) = gtk4::Settings::default() {
        for property in ["gtk-theme-name", "gtk-application-prefer-dark-theme"] {
            settings.connect_notify_local(Some(property), move |_, _| refresh());
        }
    }
}

/// `css` without its `:root` block when the libadwaita in use (`adw_minor`, of major version 1)
/// predates 1.6: GTK before 4.16 has no `:root` selector and warns about the rule, and such a
/// libadwaita reads the legacy named colours instead.
fn without_unsupported_root(css: &str, adw_minor: u32) -> &str {
    if adw_minor >= 6 {
        return css;
    }
    css.find(":root {").map_or(css, |at| &css[..at])
}

/// Replaces the theme stylesheet with `css`.
fn load(css: &str) {
    let css = without_unsupported_root(css, adw::minor_version());
    let Some(display) = gdk::Display::default() else {
        return;
    };
    PROVIDER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let provider = slot.get_or_insert_with(|| {
            let provider = gtk4::CssProvider::new();
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
            );
            provider
        });
        provider.load_from_data(css);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `:root` variables go only to a libadwaita that reads them (1.6 and later); the
    /// named colours before them stay for every version.
    #[test]
    fn the_root_block_is_left_out_before_libadwaita_1_6() {
        let css = Chrome::agent_terminal().css();
        assert!(css.contains(":root {"));
        assert_eq!(without_unsupported_root(&css, 6), css);
        let old = without_unsupported_root(&css, 5);
        assert!(!old.contains(":root") && !old.contains("--window-bg-color"));
        assert!(old.contains("@define-color window_bg_color"));
    }

    /// Every theme defines every name: a stylesheet naming one a theme lacks would draw it
    /// transparent (or not at all).
    #[test]
    fn every_theme_defines_every_name() {
        for choice in ThemeChoice::ALL {
            let Some(chrome) = Chrome::for_choice(choice) else {
                assert_eq!(
                    choice,
                    ThemeChoice::System,
                    "only the system theme has no palette"
                );
                continue;
            };
            let css = chrome.css();
            for (name, _) in chrome.named() {
                assert!(
                    css.contains(&format!("@define-color {name} #")),
                    "{choice}: {name}"
                );
            }
            assert!(
                css.contains("--window-bg-color:"),
                "{choice}: libadwaita variables"
            );
        }
    }

    /// The system theme defines the same names, each from the desktop's colours only, and
    /// leaves libadwaita's own colours alone so the desktop's theme shows through.
    #[test]
    fn the_system_theme_only_aliases_the_desktops_colours() {
        let names: Vec<&str> = Chrome::agent_terminal()
            .named()
            .iter()
            .map(|(n, _)| *n)
            .collect();
        let system: Vec<&str> = SYSTEM_COLOURS.iter().map(|(n, _)| *n).collect();
        assert_eq!(system, names);
        for (name, value) in SYSTEM_COLOURS {
            assert!(!value.contains('#'), "{name}: a colour of its own");
            assert!(value.contains('@'), "{name}: not from the desktop");
        }
        let css = system_css();
        assert!(!css.contains("define-color window_bg_color"));
        assert!(!css.contains("--window-bg-color"));
        assert!(css.contains("label.at-probe-at-bg { color: @at_bg; }"));
    }

    /// Every `@at_*` name a stylesheet uses is one the palette defines: GTK drops a property
    /// whose colour does not resolve, so a typo would silently unstyle it.
    #[test]
    fn the_stylesheets_name_only_defined_colours() {
        let mut defined: Vec<&str> = Chrome::agent_terminal()
            .named()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        defined.push(SHADOW_NAME);
        let sheets = [
            ("main.rs", include_str!("main.rs")),
            ("style.css", include_str!("chat/view/style.css")),
            ("loading.css", include_str!("window/loading.css")),
        ];
        for (sheet, text) in sheets {
            for (at, _) in text.match_indices("@at_") {
                let name: String = text[at + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                assert!(defined.contains(&name.as_str()), "{sheet}: @{name}");
            }
        }
    }

    /// The stylesheets bring no colour of their own: every colour is a palette name, so the
    /// system theme never shows a literal.
    #[test]
    fn the_stylesheets_hold_no_literal_colour() {
        for (sheet, text) in [
            ("style.css", include_str!("chat/view/style.css")),
            ("loading.css", include_str!("window/loading.css")),
        ] {
            for literal in ["#", "rgb(", "rgba("] {
                let found: Vec<&str> = text
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("/*") && l.contains(literal))
                    .collect();
                assert!(found.is_empty(), "{sheet}: {literal} in {found:?}");
            }
        }
    }

    /// The shadow is defined by every theme, from the desktop's own shade under the system one.
    #[test]
    fn every_theme_defines_the_shadow() {
        let line = format!("@define-color {SHADOW_NAME} ");
        for choice in ThemeChoice::ALL {
            if let Some(chrome) = Chrome::for_choice(choice) {
                assert!(chrome.css().contains(&line), "{choice}: no shadow");
            }
        }
        assert!(system_css().contains(&format!("{line}{SYSTEM_SHADOW};")));
        assert!(SYSTEM_SHADOW.starts_with('@') && !SYSTEM_SHADOW.contains('#'));
    }

    /// The app's own theme is exactly what the stylesheets were tuned with.
    #[test]
    fn the_default_theme_keeps_its_values() {
        let css = Chrome::agent_terminal().css();
        assert!(css.contains("@define-color at_bg #181425;"));
        assert!(css.contains("@define-color at_accent #b49bff;"));
        assert!(css.contains("@define-color at_fg_dim #918bbd;"));
        // Applied, its faint text (3.45:1 as tuned) is lifted to AA; what already reads is kept.
        let applied = Chrome::for_choice(ThemeChoice::AgentTerminal).expect("bundled");
        assert_ne!(applied.fg_faint, Chrome::agent_terminal().fg_faint);
        assert_eq!(applied.bg, Chrome::agent_terminal().bg);
    }

    /// A derived theme reads as its terminal does: its background, its text, its accent, and a
    /// darker header than body.
    #[test]
    fn a_derived_theme_follows_its_terminal() {
        let c = Chrome::for_choice(ThemeChoice::Dracula).expect("a bundled palette");
        assert_eq!(c.bg.hex(), "#282a36");
        assert_eq!(c.fg.hex(), "#f8f8f2");
        assert!(c.bg_deep.0 < c.bg.0 && c.bg_deep.2 < c.bg.2);
        assert_ne!(c.accent, c.bg);
    }

    /// Secondary text (soft, dim, faint) reads at WCAG AA on every surface of every bundled theme.
    #[test]
    fn every_bundled_themes_secondary_text_reads() {
        for choice in ThemeChoice::ALL {
            let Some(c) = Chrome::for_choice(choice) else {
                continue;
            };
            for (role, text) in [
                ("soft", c.fg_soft),
                ("dim", c.fg_dim),
                ("faint", c.fg_faint),
            ] {
                for (ground, bg) in [
                    ("bg", c.bg),
                    ("surface", c.surface),
                    ("raised", c.surface_raised),
                ] {
                    let ratio = text.contrast(bg);
                    assert!(
                        ratio >= MIN_TEXT_CONTRAST,
                        "{choice}: fg_{role} on {ground} at {ratio:.2}:1"
                    );
                }
            }
        }
    }

    /// The system theme's alphas reach AA on libadwaita's own light and dark windows (its text
    /// colour is translucent in light: rgba(0, 0, 6, 0.8) on #fafafb).
    #[test]
    fn a_system_themes_secondary_text_reads_on_adwaita() {
        let alpha_of = |name: &str| -> f64 {
            let (_, value) = SYSTEM_COLOURS
                .iter()
                .find(|(n, _)| *n == name)
                .expect("a system colour");
            value
                .trim_start_matches("alpha(@window_fg_color, ")
                .trim_end_matches(')')
                .parse()
                .expect("an alpha")
        };
        let windows = [
            (
                "light",
                Rgb::from_hex(0xfafafb),
                Rgb::from_hex(0x000006),
                0.8,
            ),
            ("dark", Rgb::from_hex(0x222226), Rgb::WHITE, 1.0),
        ];
        for (mode, bg, fg, fg_alpha) in windows {
            for name in ["at_fg_soft", "at_fg_dim", "at_fg_faint"] {
                let text = bg.mix(fg, fg_alpha * alpha_of(name));
                let ratio = text.contrast(bg);
                assert!(ratio >= MIN_TEXT_CONTRAST, "{mode}: {name} at {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn colour_maths() {
        assert!((Rgb::WHITE.contrast(Rgb(0.0, 0.0, 0.0)) - 21.0).abs() < 1e-9);
        let black = Rgb(0.0, 0.0, 0.0);
        assert_eq!(black.mix(Rgb::WHITE, 0.5).hex(), "#808080");
        assert_eq!(Rgb::from_hex(0x336699).hex(), "#336699");
        assert_eq!(Rgb::WHITE.shade(2.0).hex(), "#ffffff", "clamped");
        assert_eq!(Rgb::from_hex(0xff0000).rgba(0.5), "rgba(255, 0, 0, 0.5)");
    }

    fn light_choices() -> impl Iterator<Item = ThemeChoice> {
        ThemeChoice::ALL
            .into_iter()
            .filter(|c| c.is_light() == Some(true))
    }

    /// The dark schemes' colours, in [`Chrome::named`] order, as they were before the light
    /// schemes existed: adding them changes no dark value.
    #[test]
    fn dark_chrome_is_unchanged() {
        let golden: [(ThemeChoice, [&str; 27]); 7] = [
            (
                ThemeChoice::AgentTerminal,
                [
                    "181425", "120f1d", "171226", "1d1830", "241d38", "2a2344", "3a2d6b", "2d2444",
                    "3b3160", "52467f", "ebe9ff", "c8c8ff", "a8a2dc", "918bbd", "8884b4", "b49bff",
                    "c4b0ff", "4ee8b0", "8ff3cd", "ffc46b", "ffd08a", "ff7878", "ff9a9a", "ffc4c4",
                    "3a1f2b", "7a3b4c", "7cc7ff",
                ],
            ),
            (
                ThemeChoice::Dracula,
                [
                    "282a36", "1e2029", "242631", "2f313d", "373843", "423347", "5e3e5a", "46354a",
                    "5c3d59", "7e4a70", "fcfcf9", "f8f8f2", "d3d3d0", "b5b6b6", "a1a2a4", "ff79c6",
                    "ff94d1", "50fa7b", "8dfca9", "f1fa8c", "f5fba9", "ff5555", "ff7777", "ffbbbb",
                    "46303a", "823c43", "bd93f9",
                ],
            ),
            (
                ThemeChoice::Nord,
                [
                    "2e3440", "232730", "292f3a", "343a46", "3a404c", "3e3f4d", "504b5b", "41414f",
                    "4e4a5a", "64586c", "edf0f5", "d8dee9", "b9bfcb", "a7adb8", "a7adb9", "b48ead",
                    "c3a5bd", "a3be8c", "c3d5b4", "ebcb8b", "f0d8a8", "bf616a", "cc8188", "e5c0c3",
                    "423a46", "6b4752", "81a1c1",
                ],
            ),
            (
                ThemeChoice::GruvboxDark,
                [
                    "282828", "1e1e1e", "242424", "2f2e2d", "363532", "382f33", "4a3740", "3b3035",
                    "49363f", "5f3f4e", "f6efdc", "ebdbb2", "c8bb99", "ada286", "a99e83", "b16286",
                    "c1819e", "98971a", "bcbb6a", "d79921", "e1b359", "cc241d", "d6504a", "eba7a5",
                    "3f2726", "6d2623", "458588",
                ],
            ),
            (
                ThemeChoice::SolarizedDark,
                [
                    "002b36", "002029", "002731", "052f39", "09323d", "192c3f", "352e49", "1e2d41",
                    "332e48", "542f54", "c7cfd0", "839496", "8a9a9c", "8a9a9c", "8a9a9c", "d33682",
                    "dc5e9b", "859900", "b0bd59", "b58900", "c8a740", "dc322f", "e35b59", "f1adac",
                    "1f2c35", "5c2e33", "268bd2",
                ],
            ),
            (
                ThemeChoice::OneDark,
                [
                    "282c34", "1e2127", "24282f", "2d3139", "31353e", "3b3548", "503f5e", "3e374c",
                    "4e3e5d", "674a78", "d9dce2", "abb2bf", "979daa", "989fab", "9aa1ad", "c678dd",
                    "d193e4", "98c379", "bcd8a8", "e5c07b", "ecd09c", "e06c75", "e68991", "f3c4c8",
                    "42353d", "75474f", "61afef",
                ],
            ),
            (
                ThemeChoice::Monokai,
                [
                    "272822", "1d1e1a", "23241f", "2e2f29", "363731", "37333d", "493e59", "3a3441",
                    "473d57", "5d4c7a", "fcfcf9", "f8f8f2", "d2d3cd", "b5b5af", "a1a19b", "ae81ff",
                    "be9aff", "a6e22e", "c5ec77", "f4bf75", "f7cf98", "f92672", "fa518e", "fda8c7",
                    "44282d", "7f2744", "66d9ef",
                ],
            ),
        ];
        for (choice, expected) in golden {
            let c = Chrome::for_choice(choice).expect("bundled");
            let got: Vec<String> = c
                .named()
                .iter()
                .map(|(_, v)| v.hex()[1..].to_owned())
                .collect();
            assert_eq!(got, expected, "{choice}");
        }
    }

    /// Every text colour of a light scheme reads at AA on the surfaces text is drawn on.
    #[test]
    fn every_light_theme_text_reads() {
        for choice in light_choices() {
            let c = Chrome::for_choice(choice).expect("bundled");
            for (role, text) in [
                ("fg", c.fg),
                ("fg_strong", c.fg_strong),
                ("fg_soft", c.fg_soft),
                ("fg_dim", c.fg_dim),
                ("fg_faint", c.fg_faint),
                ("accent", c.accent),
                ("accent_hover", c.accent_hover),
                ("success", c.success),
                ("success_soft", c.success_soft),
                ("warning", c.warning),
                ("warning_soft", c.warning_soft),
                ("danger", c.danger),
                ("danger_soft", c.danger_soft),
                ("info", c.info),
            ] {
                for (ground, bg) in [
                    ("bg", c.bg),
                    ("surface", c.surface),
                    ("raised", c.surface_raised),
                ] {
                    let ratio = text.contrast(bg);
                    assert!(
                        ratio >= MIN_TEXT_CONTRAST,
                        "{choice}: {role} on {ground} at {ratio:.2}:1"
                    );
                }
            }
            let ratio = c.danger_text.contrast(c.danger_bg);
            assert!(
                ratio >= MIN_TEXT_CONTRAST,
                "{choice}: danger_text on danger_bg at {ratio:.2}:1"
            );
            // The accent's own text (the window background) reads on an accent fill.
            let ratio = c.bg.contrast(c.accent);
            assert!(
                ratio >= MIN_TEXT_CONTRAST,
                "{choice}: text on an accent fill at {ratio:.2}:1"
            );
        }
    }

    /// A light scheme steps darker for wells and text, and brings in no pure white of its own.
    #[test]
    fn light_surfaces_step_the_right_way() {
        for choice in light_choices() {
            let c = Chrome::for_choice(choice).expect("bundled");
            assert!(
                c.bg_deep.luminance() < c.bg.luminance(),
                "{choice}: bg_deep"
            );
            assert!(
                c.bg_sunken.luminance() < c.bg.luminance(),
                "{choice}: bg_sunken"
            );
            assert!(
                c.bg_sunken.luminance() > c.bg_deep.luminance(),
                "{choice}: sunken is between"
            );
            assert!(
                c.fg_strong.luminance() < c.fg.luminance(),
                "{choice}: fg_strong"
            );
            assert!(c.fg.luminance() < c.bg.luminance(), "{choice}: text on bg");
            for (name, colour) in c.named() {
                if colour.hex() == "#ffffff" {
                    assert_eq!(
                        c.bg.hex(),
                        "#ffffff",
                        "{choice}: {name} is white on a background that is not"
                    );
                }
            }
        }
    }

    /// The window is forced light for a light scheme, dark otherwise; the system theme is not
    /// forced at all.
    #[test]
    fn the_forced_colour_scheme_follows_the_polarity() {
        assert_eq!(
            forced_scheme(ThemeChoice::GithubLight),
            adw::ColorScheme::ForceLight
        );
        assert_eq!(
            forced_scheme(ThemeChoice::Nord),
            adw::ColorScheme::ForceDark
        );
        for choice in ThemeChoice::ALL {
            let want = match choice.is_light() {
                Some(true) => adw::ColorScheme::ForceLight,
                _ => adw::ColorScheme::ForceDark,
            };
            assert_eq!(forced_scheme(choice), want, "{choice}");
        }
    }

    #[test]
    fn the_generation_moves_with_every_palette() {
        let before = generation();
        set_current(Chrome::agent_terminal());
        assert_eq!(generation(), before + 1);
    }
}
