//! Terminal color schemes.
//!
//! Each [`Theme`] is built from infallible [`RGBA::new`] values (no runtime
//! string parsing, so applying a theme can never panic). Themes are cheap to
//! build, so one is constructed on demand each time it is applied to a tab.
//! [`ThemeChoice::System`] has no palette: terminals take the desktop's text and
//! background, and VTE's default colours for the rest. [`ThemeChoice::Automatic`] has none
//! either: it takes the palette of its member in effect (`crate::palette::in_effect`).

use crate::config::ThemeChoice;
use crate::palette::{Rgb, MIN_TEXT_CONTRAST};
use gtk4::gdk::RGBA;
#[cfg(feature = "terminal")]
use vte4::prelude::*;
#[cfg(feature = "terminal")]
use vte4::Terminal;

#[cfg(not(feature = "terminal"))]
use crate::terminal_stub::Terminal;

/// Converts 8-bit sRGB components to a fully opaque [`RGBA`].
fn rgb(r: u8, g: u8, b: u8) -> RGBA {
    RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        1.0,
    )
}

/// Converts a `0xRRGGBB` hex color to a fully opaque [`RGBA`].
fn hex(color: u32) -> RGBA {
    rgb(
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
    )
}

/// Colours for the diff panel: the terminal's own background and text, and a
/// foreground for each kind of diff line.
#[derive(Clone)]
pub struct DiffColours {
    pub background: RGBA,
    pub text: RGBA,
    pub added: RGBA,
    pub removed: RGBA,
    pub hunk: RGBA,
    pub meta: RGBA,
    pub file: RGBA,
}

/// What the app's window takes from a terminal theme (`crate::palette` derives the rest): its
/// background and text, its accent (the ANSI magenta), and its red, green, yellow and blue.
pub struct ChromeBase {
    pub background: RGBA,
    pub foreground: RGBA,
    pub accent: RGBA,
    pub red: RGBA,
    pub green: RGBA,
    pub yellow: RGBA,
    pub blue: RGBA,
}

/// A resolved terminal color scheme: foreground/background, cursor, selection,
/// and the 16-color ANSI palette.
pub struct Theme {
    foreground: RGBA,
    background: RGBA,
    bold: RGBA,
    cursor: RGBA,
    highlight_bg: RGBA,
    highlight_fg: RGBA,
    palette: [RGBA; 16],
}

impl Theme {
    /// Builds the requested theme and applies it to a terminal.
    pub fn apply(terminal: &Terminal, choice: ThemeChoice) {
        match Self::for_choice(choice) {
            Some(theme) => theme.apply_to(terminal),
            None => Self::apply_system(terminal),
        }
    }

    /// The system theme sets no terminal colours of its own: the text and background are the
    /// desktop's (as `crate::palette` resolved them), everything else is VTE's default.
    fn apply_system(terminal: &Terminal) {
        let chrome = crate::palette::current();
        terminal.set_colors(Some(&chrome.fg.to_rgba()), Some(&chrome.bg.to_rgba()), &[]);
        terminal.set_color_bold(None);
        terminal.set_color_cursor(None);
        terminal.set_color_highlight(None);
        terminal.set_color_highlight_foreground(None);
    }

    /// The bundled palette for `choice`; `None` for the system theme, which has none.
    fn for_choice(choice: ThemeChoice) -> Option<Self> {
        Some(match choice {
            ThemeChoice::System => return None,
            // No palette of its own: the member in effect's (never Automatic itself).
            ThemeChoice::Automatic => match crate::palette::concrete(choice) {
                ThemeChoice::Automatic => return None,
                member => return Self::for_choice(member),
            },
            ThemeChoice::AgentTerminal => Self::agent_terminal(),
            ThemeChoice::Dracula => Self::scheme(
                0x282a36,
                0xf8f8f2,
                0xf8f8f2,
                0x44475a,
                [
                    0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                    0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
                ],
            ),
            ThemeChoice::Nord => Self::scheme(
                0x2e3440,
                0xd8dee9,
                0xd8dee9,
                0x434c5e,
                [
                    0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                    0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
                ],
            ),
            ThemeChoice::GruvboxDark => Self::scheme(
                0x282828,
                0xebdbb2,
                0xebdbb2,
                0x504945,
                [
                    0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                    0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
                ],
            ),
            ThemeChoice::SolarizedDark => Self::scheme(
                0x002b36,
                0x839496,
                0x93a1a1,
                0x073642,
                [
                    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                    0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
                ],
            ),
            ThemeChoice::OneDark => Self::scheme(
                0x282c34,
                0xabb2bf,
                0x528bff,
                0x3e4451,
                [
                    0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
                    0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
                ],
            ),
            ThemeChoice::Monokai => Self::scheme(
                0x272822,
                0xf8f8f2,
                0xf8f8f0,
                0x49483e,
                [
                    0x272822, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf8f8f2,
                    0x75715e, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf9f8f5,
                ],
            ),
            ThemeChoice::AgentTerminalLight => Self::scheme(
                0xfbf9ff,
                0x2a2547,
                0x6a4fd6,
                0xd9d0ff,
                [
                    0x2d283e, 0xc2323f, 0x1a7f5a, 0x8a5a00, 0x1f6fb5, 0x6a4fd6, 0x0e7490, 0xd8d4e8,
                    0x6d6794, 0xd9455a, 0x23956c, 0xa66a00, 0x2f82cf, 0x8468ea, 0x1792b0, 0xffffff,
                ],
            ),
            ThemeChoice::SolarizedLight => Self {
                // Selected text is Solarized's darker base: its text colour reads only 4.4:1 on
                // the selection.
                highlight_fg: hex(0x073642),
                ..Self::scheme(
                    0xfdf6e3,
                    0x586e75,
                    0x586e75,
                    0xeee8d5,
                    [
                        0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198,
                        0xeee8d5, 0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4,
                        0x93a1a1, 0xfdf6e3,
                    ],
                )
            },
            ThemeChoice::GruvboxLight => Self::scheme(
                0xfbf1c7,
                0x3c3836,
                0x3c3836,
                0xd5c4a1,
                [
                    0xfbf1c7, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0x7c6f64,
                    0x928374, 0x9d0006, 0x79740e, 0xb57614, 0x076678, 0x8f3f71, 0x427b58, 0x3c3836,
                ],
            ),
            ThemeChoice::OneLight => Self::scheme(
                0xfafafa,
                0x383a42,
                0x526fff,
                0xe5e5e6,
                [
                    0x383a42, 0xe45649, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0xa0a1a7,
                    0x4f525e, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
                ],
            ),
            ThemeChoice::GithubLight => Self::scheme(
                0xffffff,
                0x24292f,
                0x0969da,
                0xb6e3ff,
                [
                    0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
                    0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
                ],
            ),
        })
    }

    /// Builds a theme from hex colors. `bold` and the selection foreground both
    /// use `fg`, which suits the standard schemes below.
    fn scheme(bg: u32, fg: u32, cursor: u32, selection: u32, palette: [u32; 16]) -> Self {
        Self {
            foreground: hex(fg),
            background: hex(bg),
            bold: hex(fg),
            cursor: hex(cursor),
            highlight_bg: hex(selection),
            highlight_fg: hex(fg),
            palette: palette.map(hex),
        }
    }

    /// The app's own hand-tuned palette, matching its window chrome (the default).
    fn agent_terminal() -> Self {
        let foreground = rgb(200, 200, 255);
        let bold = rgb(180, 155, 255); // accent violet, reused for cursor + magenta

        let palette = [
            rgb(45, 40, 62),    // 0  black (dark grey-violet)
            rgb(255, 120, 120), // 1  red
            rgb(78, 232, 176),  // 2  green
            rgb(255, 224, 102), // 3  yellow
            rgb(116, 192, 252), // 4  blue
            bold,               // 5  magenta
            rgb(102, 217, 232), // 6  cyan
            foreground,         // 7  white
            rgb(170, 162, 185), // 8  bright black
            rgb(255, 135, 135), // 9  bright red
            rgb(99, 241, 195),  // 10 bright green
            rgb(255, 236, 153), // 11 bright yellow
            rgb(165, 216, 255), // 12 bright blue
            rgb(208, 191, 255), // 13 bright magenta
            rgb(154, 230, 242), // 14 bright cyan
            rgb(230, 230, 255), // 15 bright white
        ];

        Self {
            foreground,
            background: rgb(24, 20, 37),
            bold,
            cursor: bold,
            highlight_bg: rgb(68, 58, 94),
            highlight_fg: rgb(230, 230, 255),
            palette,
        }
    }

    /// What the window's colours derive from (see [`ChromeBase`]); `None` for the system theme,
    /// whose window colours are the desktop's.
    pub fn chrome_base(choice: ThemeChoice) -> Option<ChromeBase> {
        let t = Self::for_choice(choice)?;
        Some(ChromeBase {
            background: t.background,
            foreground: t.foreground,
            accent: t.palette[5],
            red: t.palette[1],
            green: t.palette[2],
            yellow: t.palette[3],
            blue: t.palette[4],
        })
    }

    /// Colours for the diff panel, taken from the theme's own palette so it
    /// matches the terminal beside it. The system theme's come from the desktop's colours.
    pub fn diff_colours(choice: ThemeChoice) -> DiffColours {
        let choice = crate::palette::concrete(choice);
        let Some(theme) = Self::for_choice(choice) else {
            let c = crate::palette::current();
            return DiffColours {
                background: c.bg.to_rgba(),
                text: c.fg.to_rgba(),
                added: c.success.to_rgba(),
                removed: c.danger.to_rgba(),
                hunk: c.info.to_rgba(),
                meta: c.fg_dim.to_rgba(),
                file: c.accent.to_rgba(),
            };
        };
        // Bright black names the file in `diff --git`/`index` lines, yet some schemes draw it
        // at or near their background (Solarized's is the background): lift it toward the
        // text until it reads.
        let rgb = |c: &RGBA| {
            Rgb(
                f64::from(c.red()),
                f64::from(c.green()),
                f64::from(c.blue()),
            )
        };
        // A light scheme's red, green and cyan sit below AA on its background (Solarized's green
        // is 2.9:1 on cream): its lines take the window's lifted success, danger and info.
        let lifted = (choice.is_light() == Some(true))
            .then(|| crate::palette::Chrome::for_choice(choice))
            .flatten();
        let meta = rgb(&theme.palette[8])
            .readable_on(
                rgb(&theme.background),
                rgb(&theme.foreground),
                MIN_TEXT_CONTRAST,
            )
            .to_rgba();
        DiffColours {
            background: theme.background,
            text: theme.foreground,
            added: lifted
                .as_ref()
                .map_or(theme.palette[2], |c| c.success.to_rgba()),
            removed: lifted
                .as_ref()
                .map_or(theme.palette[1], |c| c.danger.to_rgba()),
            hunk: lifted
                .as_ref()
                .map_or(theme.palette[6], |c| c.info.to_rgba()),
            meta,
            file: theme.bold,
        }
    }

    fn apply_to(&self, terminal: &Terminal) {
        let palette: [&RGBA; 16] = std::array::from_fn(|i| &self.palette[i]);
        terminal.set_colors(Some(&self.foreground), Some(&self.background), &palette);
        terminal.set_color_bold(Some(&self.bold));
        terminal.set_color_cursor(Some(&self.cursor));
        terminal.set_color_highlight(Some(&self.highlight_bg));
        terminal.set_color_highlight_foreground(Some(&self.highlight_fg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_of(c: &RGBA) -> Rgb {
        Rgb(
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
        )
    }

    #[test]
    fn every_bundled_theme_draws_diff_meta_lines_readably() {
        for choice in ThemeChoice::ALL {
            if choice == ThemeChoice::System {
                continue;
            }
            let diff = Theme::diff_colours(choice);
            let bg = rgb_of(&diff.background);
            let ratio = rgb_of(&diff.meta).contrast(bg);
            assert!(
                ratio >= MIN_TEXT_CONTRAST,
                "{choice:?}: meta at {ratio:.2}:1"
            );
        }
    }

    #[test]
    fn a_meta_colour_that_already_reads_is_left_as_the_scheme_has_it() {
        // The app's own bright black reads on its background: unchanged.
        let theme = Theme::agent_terminal();
        let diff = Theme::diff_colours(ThemeChoice::AgentTerminal);
        assert_eq!(rgb_of(&diff.meta).hex(), rgb_of(&theme.palette[8]).hex());
        // Solarized's bright black is its background: lifted.
        let diff = Theme::diff_colours(ThemeChoice::SolarizedDark);
        assert_ne!(rgb_of(&diff.meta).hex(), rgb_of(&diff.background).hex());
    }

    fn light_choices() -> impl Iterator<Item = ThemeChoice> {
        ThemeChoice::ALL
            .into_iter()
            .filter(|c| c.is_light() == Some(true))
    }

    #[test]
    fn every_light_theme_diff_lines_read() {
        for choice in light_choices() {
            let diff = Theme::diff_colours(choice);
            let bg = rgb_of(&diff.background);
            for (role, colour) in [
                ("text", &diff.text),
                ("added", &diff.added),
                ("removed", &diff.removed),
                ("hunk", &diff.hunk),
                ("meta", &diff.meta),
                ("file", &diff.file),
            ] {
                let ratio = rgb_of(colour).contrast(bg);
                assert!(
                    ratio >= MIN_TEXT_CONTRAST,
                    "{choice:?}: {role} at {ratio:.2}:1"
                );
            }
        }
    }

    /// A dark scheme's diff lines are its own palette slots, untouched by the light handling.
    #[test]
    fn dark_diff_colours_stay_the_palettes_own() {
        for choice in ThemeChoice::ALL {
            if choice.is_light() != Some(false) {
                continue;
            }
            let theme = Theme::for_choice(choice).expect("bundled");
            let diff = Theme::diff_colours(choice);
            assert_eq!(diff.added, theme.palette[2], "{choice:?}");
            assert_eq!(diff.removed, theme.palette[1], "{choice:?}");
            assert_eq!(diff.hunk, theme.palette[6], "{choice:?}");
        }
    }

    #[test]
    fn light_terminal_text_reads() {
        for choice in light_choices() {
            let theme = Theme::for_choice(choice).expect("bundled");
            let bg = rgb_of(&theme.background);
            for (role, colour, ground) in [
                ("text", &theme.foreground, bg),
                ("bold", &theme.bold, bg),
                (
                    "selected text",
                    &theme.highlight_fg,
                    rgb_of(&theme.highlight_bg),
                ),
            ] {
                let ratio = rgb_of(colour).contrast(ground);
                assert!(
                    ratio >= MIN_TEXT_CONTRAST,
                    "{choice:?}: {role} at {ratio:.2}:1"
                );
            }
        }
    }
}
