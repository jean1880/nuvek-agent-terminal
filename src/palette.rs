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
//!
//! [`ThemeChoice::Automatic`] has no values of its own either: it applies one member of the
//! user's pair ([`ThemePair`]), the light scheme while the desktop prefers light and the dark
//! one while it prefers dark, and swaps them when the desktop's preference changes, through the
//! same watch as the system theme. It never forces libadwaita's colour scheme: each member has
//! the desktop's polarity, so libadwaita already agrees and nothing feeds back. Every lookup by
//! choice resolves it to the member in effect first ([`concrete`]).

use std::cell::{Cell, RefCell};

use gtk4::gdk;
use gtk4::prelude::*;

use crate::config::{ThemeChoice, ThemePair};
use crate::theme::Theme;

/// WCAG AA for body text: what every text colour must reach on what it is drawn on.
pub const MIN_TEXT_CONTRAST: f64 = 4.5;

/// WCAG AA for what is not text but must be seen (1.4.11): a control's outline, the focus
/// ring, a state colour (a bar's fill, a status dot) on what it is drawn on.
pub const MIN_NON_TEXT_CONTRAST: f64 = 3.0;

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
    /// The keyboard focus ring: opaque, and at [`MIN_NON_TEXT_CONTRAST`] on every surface.
    pub focus: Rgb,
}

/// The system theme: each name, as an expression of the desktop theme's named colours (the ones
/// libadwaita documents, which a `gtk.css` theme redefines). Mixes and alphas only place a role
/// between two of the desktop's colours; none brings a colour of its own. Same names, same
/// order as [`Chrome::named`].
pub const SYSTEM_COLOURS: [(&str, &str); 28] = [
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
    // A control's outline reaches 3:1 on Adwaita's light and dark windows
    // (`a_system_themes_controls_are_seen_on_adwaita`).
    ("at_border_strong", "alpha(@window_fg_color, 0.55)"),
    // Pointing at a control brightens its outline to the accent, opaque: 3:1 or more there too.
    ("at_border_hover", "@accent_color"),
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
    ("at_focus", "@accent_color"),
];

/// How much of a tag's colour tints its chip's ground (the rest is the row beneath). The chip
/// text contrast test decides it: low enough that the strong text colour still reads at AA on
/// every scheme, on a plain, hovered, selected or glowing row.
pub const TAG_CHIP_TINT: f64 = 0.12;
/// How much of its colour a tinted chip's border takes.
const TAG_CHIP_BORDER: f64 = 0.7;

/// The sidebar's tag chips: their shape, a text colour of their own (`@at_fg_strong`, set on
/// the label itself, since the app-wide `label` rule would otherwise win), and for each
/// [`agent_core::thread_tag::TagColour`] a tint and a border from its palette name (`tag-<key>`
/// draws `@at_<key>`), never a literal colour, so the system theme draws the desktop's.
/// Pointing at or pressing a chip changes only its border: the flat button's own wash, toward
/// the text colour, would take the text below AA on a tinted ground.
pub fn tag_chip_css() -> String {
    let mut css = String::from(
        "button.tag-chip {
            min-height: 24px;
            min-width: 24px;
            padding: 0 6px;
            border-radius: 12px;
            font-size: 0.8em;
            color: @at_fg_strong;
            border: 1px solid alpha(@at_fg_soft, 0.35);
        }
        button.tag-chip label {
            color: @at_fg_strong;
        }
        button.tag-chip:hover,
        button.tag-chip:active,
        button.tag-chip:checked {
            background-color: transparent;
            border-color: @at_fg_soft;
        }
",
    );
    for colour in agent_core::thread_tag::TagColour::ALL {
        let key = colour.key();
        css.push_str(&format!(
            "        button.tag-chip.tag-{key} {{
            background-color: alpha(@at_{key}, {TAG_CHIP_TINT});
            border-color: alpha(@at_{key}, {TAG_CHIP_BORDER});
        }}
        button.tag-chip.tag-{key}:hover,
        button.tag-chip.tag-{key}:active,
        button.tag-chip.tag-{key}:checked {{
            background-color: alpha(@at_{key}, {TAG_CHIP_TINT});
            border-color: @at_{key};
        }}
"
        ));
    }
    css
}

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
fn system_css(high_contrast: bool) -> String {
    let mut css = String::new();
    for (name, value) in SYSTEM_COLOURS {
        let value = if high_contrast && matches!(name, "at_fg_soft" | "at_fg_dim" | "at_fg_faint") {
            "@window_fg_color"
        } else {
            value
        };
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
        focus: c("at_focus"),
    }
}

impl Chrome {
    /// The colours of a bundled palette; `None` for the system theme, whose colours are the
    /// desktop's (see [`resolve_system`]).
    pub fn for_choice(choice: ThemeChoice) -> Option<Chrome> {
        Self::tuned_for_choice(choice).map(Self::with_visible_controls)
    }

    /// A bundled palette with its text made readable, before what must be seen without being
    /// text is lifted ([`Self::with_visible_controls`]).
    fn tuned_for_choice(choice: ThemeChoice) -> Option<Chrome> {
        match concrete(choice) {
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

    /// The surfaces a control, a focus ring or a state colour is drawn on: the window, the
    /// header and sidebar, cards and popovers, raised and hovered surfaces.
    fn control_grounds(&self) -> [Rgb; 4] {
        [self.bg, self.bg_deep, self.surface, self.surface_raised]
    }

    /// The troughs a bar's fill is drawn in (style.css): the header's context bar
    /// (`mix(@at_surface_strong, @at_border, 0.5)`), and a usage bar's (`alpha(@at_fg, 0.08)`)
    /// in the sidebar footer and in its popover.
    fn bar_troughs(&self) -> [Rgb; 3] {
        let sidebar = self.bg_deep.mix(self.bg, 0.35);
        [
            self.surface_strong.mix(self.border, 0.5),
            sidebar.mix(self.fg, 0.08),
            self.surface.mix(self.fg, 0.08),
        ]
    }

    /// What must be seen without being text lifted to [`MIN_NON_TEXT_CONTRAST`] on every
    /// [`Self::control_grounds`] (the focus ring on the field tint too), each moved toward the
    /// body text only as far as it must, else toward the strong text: a control's outline
    /// (`border_strong`, and its hover a step past it), the accent (selection, toggles, the
    /// context bar), the success, warning, danger and info colours (bars, dots, badges), and
    /// the focus ring. Readability wins over a scheme's own values; one that already reaches
    /// it is kept.
    fn with_visible_controls(mut self) -> Chrome {
        let grounds = self.control_grounds();
        let (fg, strong) = (self.fg, self.fg_strong);
        let lift = |c: Rgb, grounds: &[Rgb], min: f64| {
            let toward_fg = c.readable_on_all(grounds, fg, min);
            if toward_fg.reads_on_all(grounds, min) {
                toward_fg
            } else {
                toward_fg.readable_on_all(grounds, strong, min)
            }
        };
        self.border_strong = lift(self.border_strong, &grounds, MIN_NON_TEXT_CONTRAST);
        // Pointing at a control brightens its outline: never less than the outline itself.
        let hover_floor = MIN_NON_TEXT_CONTRAST.max(self.border_strong.contrast(self.bg) + 0.5);
        self.border_hover = lift(self.border_hover, &grounds, hover_floor);
        // The colours a bar fills with are seen in its trough too.
        let filled: Vec<Rgb> = grounds.iter().chain(&self.bar_troughs()).copied().collect();
        self.accent = lift(self.accent, &filled, MIN_NON_TEXT_CONTRAST);
        self.success = lift(self.success, &filled, MIN_NON_TEXT_CONTRAST);
        self.warning = lift(self.warning, &filled, MIN_NON_TEXT_CONTRAST);
        self.danger = lift(self.danger, &filled, MIN_NON_TEXT_CONTRAST);
        self.info = lift(self.info, &grounds, MIN_NON_TEXT_CONTRAST);
        let ring_grounds = [
            self.bg,
            self.bg_deep,
            self.surface,
            self.surface_raised,
            self.surface_strong,
        ];
        self.focus = lift(self.accent, &ring_grounds, MIN_NON_TEXT_CONTRAST);
        self
    }

    /// The secondary text colours (soft, dim, faint) and the body text lifted to
    /// [`MIN_TEXT_CONTRAST`] on every surface text is drawn on, each moved toward the body text
    /// only as far as it must (toward the strong text where even the body text falls short), so
    /// their order is kept. The body text itself moves toward the strong text only as far as it
    /// must, last, so a scheme whose own text reads on every surface keeps it.
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
        self.fg = self
            .fg
            .readable_on_all(&grounds, self.fg_strong, MIN_TEXT_CONTRAST);
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
            focus: h(0xb49bff),
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
            focus: accent,
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
            focus: accent,
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
    fn named(&self) -> [(&'static str, Rgb); 28] {
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
            ("at_focus", self.focus),
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
            ("sidebar_fg_color", self.fg),
            ("sidebar_backdrop_color", self.bg_deep),
            ("sidebar_shade_color", self.bg_sunken),
            ("sidebar_border_color", self.border),
            ("secondary_sidebar_bg_color", self.bg_deep),
            ("secondary_sidebar_fg_color", self.fg),
            ("secondary_sidebar_backdrop_color", self.bg_deep),
            ("secondary_sidebar_shade_color", self.bg_sunken),
            ("secondary_sidebar_border_color", self.border),
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
    /// The pair while Automatic is the one applied, so a desktop change picks its member again.
    static AUTOMATIC: Cell<Option<ThemePair>> = const { Cell::new(None) };
    /// The user's choice stays intact while desktop high contrast temporarily takes precedence.
    static CONFIGURED: Cell<(ThemeChoice, ThemePair)> = const { Cell::new((ThemeChoice::AgentTerminal, ThemePair { dark: ThemeChoice::AgentTerminal, light: ThemeChoice::AgentTerminalLight })) };
    static HIGH_CONTRAST_OVERRIDE: Cell<bool> = const { Cell::new(false) };
    /// The choice whose colours are in use: never Automatic, which is resolved to its member.
    static IN_EFFECT: Cell<ThemeChoice> = const { Cell::new(ThemeChoice::AgentTerminal) };
    /// Whether the desktop's style is being watched (connected once per process).
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    /// Whether a re-resolve is queued: one desktop switch notifies several properties at once,
    /// and they share one refresh.
    static REFRESH_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Recolours what holds colours as values (terminals, diff panels) after the system
    /// theme re-resolved or Automatic changed member. Set by the app.
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

/// The choice whose colours are in use: the applied one, or for Automatic its member in effect.
/// Never Automatic.
pub fn in_effect() -> ThemeChoice {
    IN_EFFECT.with(Cell::get)
}

/// `choice` as a lookup takes it: desktop high contrast overrides every choice temporarily;
/// otherwise Automatic is the member in effect and every other choice is itself.
pub fn concrete(choice: ThemeChoice) -> ThemeChoice {
    if HIGH_CONTRAST_OVERRIDE.with(Cell::get) {
        return ThemeChoice::System;
    }
    match choice {
        ThemeChoice::Automatic => match in_effect() {
            // Never stored, but a lookup must not resolve to itself.
            ThemeChoice::Automatic => ThemePair::default().dark,
            member => member,
        },
        other => other,
    }
}

/// The colours in use (the app's own until [`apply`] first runs).
pub fn current() -> Chrome {
    CURRENT
        .with(|c| c.borrow().clone())
        .unwrap_or_else(Chrome::agent_terminal)
}

/// What to run after the system theme or Automatic follows a change of the desktop's style:
/// recolour what took its colours as values.
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

/// The libadwaita colour scheme set for a choice: light for a light palette and dark for a dark
/// one, so popovers, menus and entries follow the palette instead of the opposite default (pale
/// text on white, or the reverse); the desktop's own for the system theme and Automatic, which
/// follow it rather than force it.
fn forced_scheme(choice: ThemeChoice) -> adw::ColorScheme {
    match choice.is_light() {
        Some(true) => adw::ColorScheme::ForceLight,
        Some(false) => adw::ColorScheme::ForceDark,
        None => adw::ColorScheme::Default,
    }
}

/// Restyles the whole app for `choice` (`pair` is what Automatic switches between): defines
/// every named colour for it, replacing the last theme's. Above the app's own stylesheets, so
/// the names resolve however they load.
pub fn apply(choice: ThemeChoice, pair: ThemePair) {
    CONFIGURED.with(|c| c.set((choice, pair)));
    SYSTEM_ACTIVE.with(|s| s.set(choice == ThemeChoice::System));
    AUTOMATIC.with(|a| a.set((choice == ThemeChoice::Automatic).then_some(pair)));
    let high_contrast =
        gdk::Display::default().is_some() && adw::StyleManager::default().is_high_contrast();
    HIGH_CONTRAST_OVERRIDE.with(|h| h.set(high_contrast && choice != ThemeChoice::System));
    if high_contrast {
        apply_system();
        return;
    }
    apply_configured(choice, pair);
    if gdk::Display::default().is_some() {
        watch_system();
    }
}

fn apply_configured(choice: ThemeChoice, pair: ThemePair) {
    match choice {
        ThemeChoice::System => apply_system(),
        ThemeChoice::Automatic => apply_automatic(pair),
        bundled => {
            if gdk::Display::default().is_some() {
                adw::StyleManager::default().set_color_scheme(forced_scheme(bundled));
            }
            apply_palette(bundled);
        }
    }
}

/// Defines every name for a bundled scheme and makes it the one in effect, leaving the colour
/// scheme as it is.
fn apply_palette(bundled: ThemeChoice) {
    let Some(chrome) = Chrome::for_choice(bundled) else {
        return;
    };
    let css = chrome.css();
    IN_EFFECT.with(|e| e.set(bundled));
    set_current(chrome);
    load(&css);
}

/// The system theme: the desktop's colour scheme, the aliases, and the colours they resolve to.
fn apply_system() {
    IN_EFFECT.with(|e| e.set(ThemeChoice::System));
    if gdk::Display::default().is_none() {
        set_current(Chrome::agent_terminal());
        return;
    }
    adw::StyleManager::default().set_color_scheme(forced_scheme(ThemeChoice::System));
    load(&system_css(adw::StyleManager::default().is_high_contrast()));
    set_current(resolve_system());
    watch_system();
}

/// Automatic: the desktop's colour scheme, and the member of `pair` that matches it. With no
/// display there is no desktop to ask, and the dark member (the app's default polarity) is used.
fn apply_automatic(pair: ThemePair) {
    if gdk::Display::default().is_none() {
        apply_palette(pair.pick(true));
        return;
    }
    let style = adw::StyleManager::default();
    style.set_color_scheme(forced_scheme(ThemeChoice::Automatic));
    apply_palette(pair.pick(style.is_dark()));
    watch_system();
}

/// After the desktop's style changed (`desktop_dark` is its preference now): the system theme
/// resolves its colours again, Automatic takes the member that matches, and what holds colours
/// as values is recoloured when anything changed. Nothing for a bundled scheme.
fn desktop_changed(desktop_dark: bool) {
    if gdk::Display::default().is_some() {
        desktop_changed_with_contrast(
            desktop_dark,
            adw::StyleManager::default().is_high_contrast(),
        );
        return;
    }
    if SYSTEM_ACTIVE.with(Cell::get) {
        set_current(resolve_system());
    } else if !follow_desktop(desktop_dark) {
        return;
    }
    if let Some(f) = ON_SYSTEM_CHANGE.with(Cell::get) {
        f();
    }
}

fn desktop_changed_with_contrast(desktop_dark: bool, high_contrast: bool) {
    let (choice, pair) = CONFIGURED.with(Cell::get);
    let was_override = HIGH_CONTRAST_OVERRIDE.with(Cell::get);
    if high_contrast && choice != ThemeChoice::System {
        HIGH_CONTRAST_OVERRIDE.with(|h| h.set(true));
        if was_override {
            set_current(resolve_system());
        } else {
            apply_system();
        }
    } else if was_override {
        HIGH_CONTRAST_OVERRIDE.with(|h| h.set(false));
        apply_configured(choice, pair);
    } else if SYSTEM_ACTIVE.with(Cell::get) {
        load(&system_css(high_contrast));
        set_current(resolve_system());
    } else if !follow_desktop(desktop_dark) {
        return;
    }
    if let Some(f) = ON_SYSTEM_CHANGE.with(Cell::get) {
        f();
    }
}

/// Under Automatic, after the desktop's preference became `desktop_dark`: the member that
/// matches it, if it is not the one in effect already. Whether anything changed.
fn follow_desktop(desktop_dark: bool) -> bool {
    let Some(pair) = AUTOMATIC.with(Cell::get) else {
        return false;
    };
    let member = pair.pick(desktop_dark);
    if member == in_effect() {
        return false;
    }
    apply_palette(member);
    true
}

/// Re-resolves the system theme when the desktop's style changes: dark or light, accent,
/// contrast, or the GTK theme; under Automatic, picks the member for the desktop's preference.
/// On idle, once the new stylesheet is in.
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
            desktop_changed(adw::StyleManager::default().is_dark());
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
            for name in LIBADWAITA_SIDEBAR_NAMES {
                assert!(
                    css.contains(&format!("@define-color {name} #")),
                    "{choice}: {name}"
                );
                assert!(
                    css.contains(&format!("--{}:", name.replace('_', "-"))),
                    "{choice}: --{name}"
                );
            }
        }
    }

    /// The sidebar colours libadwaita 1.5 and later read. Libadwaita's `.sidebar-pane` sets
    /// its text to `sidebar_fg_color`, so a theme that leaves it undefined draws every widget
    /// without a colour of its own in libadwaita's dark default.
    const LIBADWAITA_SIDEBAR_NAMES: [&str; 10] = [
        "sidebar_bg_color",
        "sidebar_fg_color",
        "sidebar_backdrop_color",
        "sidebar_shade_color",
        "sidebar_border_color",
        "secondary_sidebar_bg_color",
        "secondary_sidebar_fg_color",
        "secondary_sidebar_backdrop_color",
        "secondary_sidebar_shade_color",
        "secondary_sidebar_border_color",
    ];

    /// The sidebar's text (libadwaita's `sidebar_fg_color`) reads on its background, in every
    /// bundled scheme.
    #[test]
    fn every_themes_sidebar_text_reads_on_its_background() {
        for choice in ThemeChoice::ALL {
            let Some(chrome) = Chrome::for_choice(choice) else {
                continue;
            };
            let css = chrome.css();
            let colour = |name: &str| {
                let key = format!("@define-color {name} #");
                let at = css.find(&key).expect(name) + key.len();
                Rgb::from_hex(u32::from_str_radix(&css[at..at + 6], 16).expect(name))
            };
            for (fg, bg) in [
                ("sidebar_fg_color", "sidebar_bg_color"),
                ("sidebar_fg_color", "sidebar_backdrop_color"),
                ("secondary_sidebar_fg_color", "secondary_sidebar_bg_color"),
            ] {
                let ratio = colour(fg).contrast(colour(bg));
                assert!(
                    ratio >= MIN_TEXT_CONTRAST,
                    "{choice}: {fg} on {bg} at {ratio:.2}:1"
                );
            }
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
        let css = system_css(false);
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
        assert!(system_css(false).contains(&format!("{line}{SYSTEM_SHADOW};")));
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

    /// What must be seen without being text reaches 3:1 (WCAG 1.4.11) on every surface it is
    /// drawn on, in every bundled scheme: a control's outline and its hover, the accent, the
    /// state colours and the focus ring.
    #[test]
    fn every_palettes_controls_are_seen() {
        for choice in ThemeChoice::ALL {
            let Some(c) = Chrome::for_choice(choice) else {
                continue; // the desktop's own colours: see the Adwaita test below
            };
            let grounds = [
                ("bg", c.bg),
                ("bg_deep", c.bg_deep),
                ("surface", c.surface),
                ("raised", c.surface_raised),
            ];
            for (role, colour) in [
                ("border_strong", c.border_strong),
                ("border_hover", c.border_hover),
                ("accent", c.accent),
                ("success", c.success),
                ("warning", c.warning),
                ("danger", c.danger),
                ("info", c.info),
                ("focus", c.focus),
            ] {
                for (ground, bg) in grounds {
                    let ratio = colour.contrast(bg);
                    assert!(
                        ratio >= MIN_NON_TEXT_CONTRAST,
                        "{choice}: {role} on {ground} at {ratio:.2}:1"
                    );
                }
            }
            let ring = c.focus.contrast(c.surface_strong);
            assert!(
                ring >= MIN_NON_TEXT_CONTRAST,
                "{choice}: focus on a field at {ring:.2}:1"
            );
            assert!(
                c.border_hover.contrast(c.bg) >= c.border_strong.contrast(c.bg),
                "{choice}: pointing at a control dims its outline"
            );
        }
    }

    /// The header's context bar (`levelbar.context-level` in style.css) and the usage bars:
    /// each fill is seen on its trough and on the ground around it, in every bundled scheme.
    /// Neither says its state by colour alone: the gauge and the usage meter print their
    /// figures beside the bar.
    #[test]
    fn the_context_and_usage_bars_are_seen_on_their_troughs() {
        let style = include_str!("chat/view/style.css");
        assert!(
            style.contains("background-color: mix(@at_surface_strong, @at_border, 0.5);"),
            "the context bar's trough is what this test measures"
        );
        assert!(
            style.contains(
                ".usage-bar trough { min-height: 4px; min-width: 24px; background-color: \
                 alpha(@at_fg, 0.08);"
            ),
            "the usage bar's trough is what this test measures"
        );
        for choice in ThemeChoice::ALL {
            let Some(c) = Chrome::for_choice(choice) else {
                continue;
            };
            let [trough, sidebar_trough, popover_trough] = c.bar_troughs();
            for (state, fill) in [("ok", c.accent), ("warn", c.warning), ("hot", c.danger)] {
                for (ground, bg) in [("trough", trough), ("header", c.bg), ("bg_deep", c.bg_deep)] {
                    let ratio = fill.contrast(bg);
                    assert!(
                        ratio >= MIN_NON_TEXT_CONTRAST,
                        "{choice}: context bar {state} on {ground} at {ratio:.2}:1"
                    );
                }
            }
            // The sidebar footer (`.thread-sidebar`) and the usage popover (a surface).
            let sidebar = c.bg_deep.mix(c.bg, 0.35);
            assert_eq!(sidebar_trough, sidebar.mix(c.fg, 0.08));
            assert_eq!(popover_trough, c.surface.mix(c.fg, 0.08));
            assert_eq!(trough, c.surface_strong.mix(c.border, 0.5));
            for (place, trough) in [("sidebar", sidebar_trough), ("popover", popover_trough)] {
                for (state, fill) in [
                    ("ok", c.success),
                    ("warn", c.warning),
                    ("crit", c.danger),
                    ("stale", c.fg_dim),
                ] {
                    let ratio = fill.contrast(trough);
                    assert!(
                        ratio >= MIN_NON_TEXT_CONTRAST,
                        "{choice}: usage bar {state} in the {place} at {ratio:.2}:1"
                    );
                }
            }
        }
    }

    /// Every keyboard focus ring is drawn in `@at_focus`: the app's own rings, and libadwaita's,
    /// whose colour the brand stylesheet replaces (libadwaita draws it half transparent).
    #[test]
    fn focus_rings_use_the_focus_colour() {
        let style = include_str!("chat/view/style.css");
        for line in style.lines().filter(|l| l.contains("outline:")) {
            assert!(
                !line.contains("@at_accent"),
                "a focus ring in the accent: {line}"
            );
        }
        assert!(style.contains("outline: 2px solid @at_focus;"));
        let main = include_str!("main.rs");
        assert!(main.contains(":focus-visible {\n            outline-color: @at_focus;"));
    }

    /// The system theme's control outline, and the same outline pointed at, reach 3:1 on
    /// libadwaita's own light and dark windows (light text is itself 80 % opaque; the accent is
    /// libadwaita's blue).
    #[test]
    fn a_system_themes_controls_are_seen_on_adwaita() {
        let value_of = |name: &str| -> &str {
            SYSTEM_COLOURS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)
                .expect("a system colour")
        };
        // `@base` or `alpha(@base, a)`: how much of the named colour is drawn.
        let alpha_of = |value: &str, base: &str| -> f64 {
            if value == base {
                return 1.0;
            }
            value
                .strip_prefix(&format!("alpha({base}, "))
                .and_then(|v| v.strip_suffix(')'))
                .unwrap_or_else(|| panic!("{value}: not {base} or an alpha of it"))
                .parse()
                .expect("an alpha")
        };
        let outline = alpha_of(value_of("at_border_strong"), "@window_fg_color");
        let hover = alpha_of(value_of("at_border_hover"), "@accent_color");
        for (mode, bg, fg, fg_alpha, accent) in [
            (
                "light",
                Rgb::from_hex(0xfafafb),
                Rgb::from_hex(0x000006),
                0.8,
                Rgb::from_hex(0x1c71d8),
            ),
            (
                "dark",
                Rgb::from_hex(0x222226),
                Rgb::WHITE,
                1.0,
                Rgb::from_hex(0x78aeed),
            ),
        ] {
            let ratio = bg.mix(fg, fg_alpha * outline).contrast(bg);
            assert!(
                ratio >= MIN_NON_TEXT_CONTRAST,
                "{mode}: at_border_strong at {ratio:.2}:1"
            );
            let pointed = bg.mix(accent, hover).contrast(bg);
            assert!(
                pointed >= MIN_NON_TEXT_CONTRAST,
                "{mode}: at_border_hover at {pointed:.2}:1"
            );
            assert!(pointed >= ratio, "{mode}: pointing at a control dims it");
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

    /// A tag chip's text (`@at_fg_strong`) reads at AA in every bundled scheme, for every tag
    /// colour and an uncoloured chip, over the chip's own ground: the sidebar, as a row draws it
    /// (plain, hovered, selected, glowing for attention), tinted by the tag's colour. A pointed
    /// at or pressed chip keeps that ground ([`tag_chip_css`]), so it is the same.
    #[test]
    fn tag_chip_text_reads_on_every_tint() {
        use agent_core::thread_tag::TagColour;
        for choice in ThemeChoice::ALL {
            let Some(c) = Chrome::for_choice(choice) else {
                continue; // the desktop's own colours: nothing bundled to measure
            };
            // `.thread-sidebar`: mix(@at_bg_deep, @at_bg, 0.35).
            let sidebar = c.bg_deep.mix(c.bg, 0.35);
            let rows = [
                ("plain", sidebar),
                ("hovered", sidebar.mix(c.fg, 0.04)),
                ("selected", sidebar.mix(c.fg, 0.10)),
                ("selected and hovered", sidebar.mix(c.fg, 0.14)),
                ("selected by accent", sidebar.mix(c.accent, 0.25)),
                ("glowing", sidebar.mix(c.warning, 0.22)),
            ];
            let tints: Vec<(&str, Option<Rgb>)> = std::iter::once(("none", None))
                .chain(TagColour::ALL.into_iter().map(|t| {
                    let tone = match t {
                        TagColour::Accent => c.accent,
                        TagColour::Success => c.success,
                        TagColour::Warning => c.warning,
                        TagColour::Danger => c.danger,
                    };
                    (t.key(), Some(tone))
                }))
                .collect();
            for (row, ground) in rows {
                for (tag, tone) in &tints {
                    // Hovered or pressed, a chip keeps this ground: only its border changes.
                    let chip = tone.map_or(ground, |t| ground.mix(t, TAG_CHIP_TINT));
                    let ratio = c.fg_strong.contrast(chip);
                    assert!(
                        ratio >= MIN_TEXT_CONTRAST,
                        "{choice}: chip tinted {tag} on a {row} row at {ratio:.2}:1"
                    );
                }
            }
        }
    }

    /// The chips' stylesheet has a rule for every tag colour, names only palette colours, and
    /// sets the label's own colour (the app-wide `label` rule would otherwise beat the button's).
    #[test]
    fn the_tag_chip_stylesheet_names_only_palette_colours() {
        let css = tag_chip_css();
        for colour in agent_core::thread_tag::TagColour::ALL {
            assert!(css.contains(&format!("button.tag-chip.tag-{} {{", colour.key())));
            assert!(css.contains(&format!("alpha(@at_{}, {TAG_CHIP_TINT})", colour.key())));
            // Pointing or pressing keeps the tint, never the flat button's wash.
            let key = colour.key();
            assert!(css.contains(&format!(
                "button.tag-chip.tag-{key}:checked {{\n            background-color: \
                 alpha(@at_{key}, {TAG_CHIP_TINT});"
            )));
        }
        assert!(
            css.contains("button.tag-chip:checked {\n            background-color: transparent;")
        );
        assert!(css.contains("button.tag-chip label {\n            color: @at_fg_strong;"));
        let defined: Vec<&str> = Chrome::agent_terminal()
            .named()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        for (at, _) in css.match_indices("@at_") {
            let name: String = css[at + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert!(defined.contains(&name.as_str()), "@{name}");
        }
        for literal in ["#", "rgb(", "rgba("] {
            assert!(!css.contains(literal), "{literal}");
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
                    "332e48", "542f54", "c7cfd0", "8a9a9c", "8a9a9c", "8a9a9c", "8a9a9c", "d33682",
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
            // As tuned, before the non-text lift (`lifting_controls_changes_only_what_is_not_text`),
            // and without the focus ring, which came later.
            let c = Chrome::tuned_for_choice(choice).expect("bundled");
            let got: Vec<String> = c
                .named()
                .iter()
                .filter(|(name, _)| *name != "at_focus")
                .map(|(_, v)| v.hex()[1..].to_owned())
                .collect();
            assert_eq!(got, expected, "{choice}");
        }
    }

    /// Lifting what must be seen to 3:1 moves only the outlines, the accent, the state colours
    /// and the focus ring: every text colour and surface of every scheme stays as tuned.
    #[test]
    fn lifting_controls_changes_only_what_is_not_text() {
        const LIFTED: [&str; 8] = [
            "at_border_strong",
            "at_border_hover",
            "at_accent",
            "at_success",
            "at_warning",
            "at_danger",
            "at_info",
            "at_focus",
        ];
        for choice in ThemeChoice::ALL {
            let (Some(tuned), Some(applied)) =
                (Chrome::tuned_for_choice(choice), Chrome::for_choice(choice))
            else {
                continue;
            };
            for ((name, before), (_, after)) in tuned.named().iter().zip(applied.named().iter()) {
                if !LIFTED.contains(name) {
                    assert_eq!(before.hex(), after.hex(), "{choice}: {name}");
                }
            }
        }
    }

    /// A dark scheme's body and strong text read at AA on every surface text is drawn on.
    #[test]
    fn every_dark_theme_body_text_reads_on_every_surface() {
        for choice in ThemeChoice::ALL {
            if choice.is_light() != Some(false) {
                continue;
            }
            let c = Chrome::for_choice(choice).expect("bundled");
            for (role, text) in [("fg", c.fg), ("fg_strong", c.fg_strong)] {
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

    /// The window is forced light for a light scheme and dark for a dark one; the system theme
    /// and Automatic are not forced at all.
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
        assert_eq!(
            forced_scheme(ThemeChoice::System),
            adw::ColorScheme::Default
        );
        assert_eq!(
            forced_scheme(ThemeChoice::Automatic),
            adw::ColorScheme::Default
        );
        for choice in ThemeChoice::ALL {
            let want = match choice.is_light() {
                Some(true) => adw::ColorScheme::ForceLight,
                Some(false) => adw::ColorScheme::ForceDark,
                None => adw::ColorScheme::Default,
            };
            assert_eq!(forced_scheme(choice), want, "{choice}");
        }
    }

    /// Both halves of every pair the Automatic theme can be given read at AA: every scheme each
    /// slot offers, with the body text on the background and the secondary text on every
    /// surface, and for a light one every text, accent and status colour on every surface (as
    /// [`every_light_theme_text_reads`] holds them); its diff text and meta lines too.
    #[test]
    fn every_pair_member_reads() {
        for light in [true, false] {
            let offered = ThemeChoice::pair_choices(light);
            assert!(!offered.is_empty());
            for choice in offered {
                let c = Chrome::for_choice(choice).expect("a bundled scheme");
                assert_eq!(is_light_chrome(&c), light, "{choice}: polarity");
                for (role, text) in [("fg", c.fg), ("fg_strong", c.fg_strong)] {
                    let ratio = text.contrast(c.bg);
                    assert!(
                        ratio >= MIN_TEXT_CONTRAST,
                        "{choice}: {role} on bg at {ratio:.2}:1"
                    );
                }
                let mut roles = vec![
                    ("fg_soft", c.fg_soft),
                    ("fg_dim", c.fg_dim),
                    ("fg_faint", c.fg_faint),
                ];
                if light {
                    roles.extend([
                        ("fg", c.fg),
                        ("fg_strong", c.fg_strong),
                        ("accent", c.accent),
                        ("success", c.success),
                        ("warning", c.warning),
                        ("danger", c.danger),
                        ("info", c.info),
                    ]);
                }
                for (role, text) in roles {
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
                let diff = Theme::diff_colours(choice);
                let bg = Rgb::from_rgba(&diff.background);
                for (role, colour) in [("text", &diff.text), ("meta", &diff.meta)] {
                    let ratio = Rgb::from_rgba(colour).contrast(bg);
                    assert!(
                        ratio >= MIN_TEXT_CONTRAST,
                        "{choice}: diff {role} at {ratio:.2}:1"
                    );
                }
            }
        }
    }

    fn is_light_chrome(c: &Chrome) -> bool {
        c.bg.luminance() > 0.179
    }

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn high_contrast_temporarily_follows_system_and_restores_choice() {
        use vte4::prelude::TerminalExt;
        crate::testutil::native_init();
        let pair = ThemePair::default();
        apply(ThemeChoice::Nord, pair);
        assert_eq!(in_effect(), ThemeChoice::Nord);
        desktop_changed_with_contrast(true, true);
        assert_eq!(in_effect(), ThemeChoice::System);
        assert_eq!(concrete(ThemeChoice::Nord), ThemeChoice::System);
        assert_eq!(concrete(ThemeChoice::Automatic), ThemeChoice::System);
        assert_eq!(CONFIGURED.with(Cell::get), (ThemeChoice::Nord, pair));
        // Newly created panes and settings reloads pass the saved choice. Both must still
        // draw the effective high-contrast palette without rewriting that saved choice.
        let terminal = vte4::Terminal::new();
        crate::theme::Theme::apply(&terminal, ThemeChoice::Nord);
        assert_eq!(terminal.color_background_for_draw(), current().bg.to_rgba());
        assert_eq!(
            crate::theme::Theme::diff_colours(ThemeChoice::Nord).background,
            current().bg.to_rgba()
        );
        crate::theme::Theme::apply(&terminal, ThemeChoice::Nord);
        assert_eq!(terminal.color_background_for_draw(), current().bg.to_rgba());
        desktop_changed_with_contrast(true, false);
        assert_eq!(in_effect(), ThemeChoice::Nord);
        assert_eq!(concrete(ThemeChoice::Nord), ThemeChoice::Nord);
        assert_eq!(
            current().bg,
            Chrome::for_choice(ThemeChoice::Nord).unwrap().bg
        );
    }

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn high_contrast_desktop_text_meets_seven_to_one() {
        std::env::set_var("ADW_DEBUG_HIGH_CONTRAST", "1");
        let ctx = crate::testutil::native_init();
        let style = adw::StyleManager::default();
        assert!(style.is_high_contrast());
        apply(ThemeChoice::Nord, ThemePair::default());
        for scheme in [adw::ColorScheme::ForceLight, adw::ColorScheme::ForceDark] {
            style.set_color_scheme(scheme);
            crate::testutil::settle(&ctx);
            let chrome = resolve_system();
            for (name, text) in [
                ("fg", chrome.fg),
                ("soft", chrome.fg_soft),
                ("dim", chrome.fg_dim),
                ("faint", chrome.fg_faint),
            ] {
                for surface in [chrome.bg, chrome.surface, chrome.surface_raised] {
                    let ratio = text.contrast(surface);
                    assert!(ratio >= 7.0, "{scheme:?}: {name} at {ratio:.2}:1");
                }
            }
        }
    }

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn high_contrast_updates_an_already_selected_system_theme() {
        let ctx = crate::testutil::native_init();
        apply(ThemeChoice::System, ThemePair::default());
        crate::testutil::settle(&ctx);
        let normal = current().fg_dim;
        desktop_changed_with_contrast(false, true);
        assert_eq!(current().fg_dim, current().fg);
        assert_ne!(current().fg_dim, normal);
        desktop_changed_with_contrast(false, false);
        assert_eq!(current().fg_dim, normal);
        assert_eq!(CONFIGURED.with(Cell::get).0, ThemeChoice::System);
    }

    /// Automatic applies the member for the desktop's preference without forcing libadwaita's
    /// colour scheme, swaps to the other member (recolouring what holds colours as values) when
    /// the desktop's preference changes, back again, and stops following once a bundled scheme
    /// replaces it. Lookups by Automatic give the member in effect.
    #[test]
    fn an_automatic_theme_follows_the_desktop_live() {
        crate::testutil::adw_init().expect("libadwaita init");
        thread_local! {
            static RECOLOURED: Cell<u32> = const { Cell::new(0) };
        }
        fn recoloured() {
            RECOLOURED.with(|n| n.set(n.get() + 1));
        }
        set_on_system_change(recoloured);
        let pair = ThemePair {
            light: ThemeChoice::SolarizedLight,
            dark: ThemeChoice::Nord,
        };
        let style = adw::StyleManager::default();
        let bg_of = |choice| Chrome::for_choice(choice).expect("bundled").bg;

        // From a forced scheme, Automatic hands the colour scheme back to the desktop.
        apply(ThemeChoice::Dracula, pair);
        assert_eq!(style.color_scheme(), adw::ColorScheme::ForceDark);
        apply(ThemeChoice::Automatic, pair);
        assert_eq!(style.color_scheme(), adw::ColorScheme::Default);
        let member = pair.pick(style.is_dark());
        assert_eq!(in_effect(), member);
        assert_eq!(current().bg, bg_of(member));
        assert_eq!(concrete(ThemeChoice::Automatic), member);
        assert_eq!(
            Theme::diff_colours(ThemeChoice::Automatic).background,
            Theme::diff_colours(member).background
        );

        // The desktop turns dark, then light: each time the matching member, and a recolour.
        let before = RECOLOURED.with(Cell::get);
        let generation_before = generation();
        desktop_changed(true);
        assert_eq!(in_effect(), ThemeChoice::Nord);
        assert_eq!(current().bg, bg_of(ThemeChoice::Nord));
        assert!(!is_light());
        assert!(generation() > generation_before);
        desktop_changed(false);
        assert_eq!(in_effect(), ThemeChoice::SolarizedLight);
        assert_eq!(current().bg, bg_of(ThemeChoice::SolarizedLight));
        assert!(is_light());
        assert_eq!(RECOLOURED.with(Cell::get), before + 2);
        // Never forced, whichever member is in effect.
        assert_eq!(style.color_scheme(), adw::ColorScheme::Default);
        // The same preference again changes nothing.
        let generation_before = generation();
        desktop_changed(false);
        assert_eq!(generation(), generation_before);
        assert_eq!(RECOLOURED.with(Cell::get), before + 2);

        // A bundled scheme ends it: the desktop's preference no longer moves the colours.
        apply(ThemeChoice::GruvboxDark, pair);
        assert_eq!(in_effect(), ThemeChoice::GruvboxDark);
        desktop_changed(false);
        assert_eq!(in_effect(), ThemeChoice::GruvboxDark);
        assert_eq!(RECOLOURED.with(Cell::get), before + 2);
    }

    #[test]
    fn the_generation_moves_with_every_palette() {
        let before = generation();
        set_current(Chrome::agent_terminal());
        assert_eq!(generation(), before + 1);
    }
}
