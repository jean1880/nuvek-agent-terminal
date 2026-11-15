//! The context-left meter: what a thread's token reading says about its window, in three honest
//! states, and the actions the header offers from it. Pure: the header, the `/context` panel
//! and the popover all read it, so the thresholds and the wording live here once.
//!
//! A window nobody reported is `Unknown`, never a guessed size: no percentage, no bar. Codex
//! sends `used` as 0 when it has no figure, so a reading of 0 with no window says nothing at all.

/// How the reading came to be: reported by the agent, or worked out by the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Measured,
    Estimated,
}

/// How full the window is. `Warn` is at 75% or at the agent's own auto-compact point when that
/// comes first; `Hot` is at 90%.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Hot,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Ok, Level::Warn, Level::Hot];
    /// Fraction of the window at which a reading is `Warn`, before the agent's own point.
    pub const WARN_AT: f64 = 0.75;
    /// Fraction of the window at which a reading is `Hot`.
    pub const HOT_AT: f64 = 0.90;

    /// A short phrase for the popover.
    pub fn phrase(self) -> &'static str {
        match self {
            Level::Ok => "Plenty of room left.",
            Level::Warn => "Filling up: compacting soon avoids losing detail.",
            Level::Hot => "Nearly full: compact or continue in a new thread.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContextState {
    /// No usable window: say so, show no percentage and no bar.
    Unknown,
    Known {
        /// Tokens left, never below 0.
        left: u64,
        /// Used share of the window, clamped to 0..=1.
        fraction: f64,
        level: Level,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMeter {
    pub used: u64,
    pub window: Option<u64>,
    pub auto_compact_at: Option<u64>,
    pub source: Source,
}

impl ContextMeter {
    pub fn new(used: u64, window: Option<u64>, auto_compact_at: Option<u64>) -> Self {
        Self {
            used,
            window,
            auto_compact_at,
            source: Source::Measured,
        }
    }

    fn window(&self) -> Option<u64> {
        self.window.filter(|w| *w > 0)
    }

    /// Whether the agent reported anything: a window, or tokens used. Codex's 0 with no window
    /// is its "no figure", not an empty thread.
    pub fn has_reading(&self) -> bool {
        self.window().is_some() || self.used > 0
    }

    /// The fraction of the window at which the bar turns to a warning.
    pub fn warn_at(&self) -> f64 {
        let own = match (self.auto_compact_at, self.window()) {
            (Some(at), Some(w)) if at > 0 => Some((at as f64 / w as f64).clamp(0.0, 1.0)),
            _ => None,
        };
        own.map_or(Level::WARN_AT, |o| o.min(Level::WARN_AT))
    }

    pub fn state(&self) -> ContextState {
        let Some(window) = self.window() else {
            return ContextState::Unknown;
        };
        let fraction = (self.used as f64 / window as f64).clamp(0.0, 1.0);
        let level = if fraction >= Level::HOT_AT {
            Level::Hot
        } else if fraction >= self.warn_at() {
            Level::Warn
        } else {
            Level::Ok
        };
        ContextState::Known {
            left: window.saturating_sub(self.used),
            fraction,
            level,
        }
    }

    /// The bar's fill and the auto-compact point as fractions, when the window is known.
    pub fn bar(&self) -> Option<(f64, Option<f64>)> {
        let ContextState::Known { fraction, .. } = self.state() else {
            return None;
        };
        let w = self.window()? as f64;
        let compact = self.auto_compact_at.map(|a| (a as f64 / w).clamp(0.0, 1.0));
        Some((fraction, compact))
    }

    /// Percent of the window left, whole, when known.
    pub fn percent_left(&self) -> Option<u32> {
        match self.state() {
            ContextState::Known { fraction, .. } => Some(((1.0 - fraction) * 100.0).round() as u32),
            ContextState::Unknown => None,
        }
    }

    /// The header button's text: "37% left", or a dim "context ?" with no window.
    pub fn text(&self) -> String {
        match self.percent_left() {
            Some(p) => format!("{p}% left"),
            None => "context ?".to_owned(),
        }
    }

    /// What the window holds so far, for the panel and the tooltip: "50k / 200k", "1.2k tokens".
    pub fn used_text(&self) -> String {
        match self.window() {
            Some(w) => format!("{} / {}", format_tokens(self.used), format_tokens(w)),
            None => format!("{} tokens", format_tokens(self.used)),
        }
    }

    /// The header button's tooltip.
    pub fn tooltip(&self) -> String {
        let ContextState::Known { left, .. } = self.state() else {
            return if self.used > 0 {
                format!(
                    "This agent has not reported its context window size. {} used so far.",
                    self.used_text()
                )
            } else {
                "This agent has not reported its context window size.".to_owned()
            };
        };
        let mut t = format!(
            "Context window: {} used, {} left.",
            self.used_text(),
            format_tokens(left)
        );
        if let Some(at) = self.auto_compact_at {
            t.push_str(&format!(" Auto-compacts at {}.", format_tokens(at)));
        }
        if self.source == Source::Estimated {
            t.push_str(" An estimate.");
        }
        t
    }
}

/// A token count as "950", "12k", "1.5M".
pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        if k < 10.0 {
            format!("{k:.1}k").replace(".0k", "k")
        } else {
            format!("{}k", k.round() as u64)
        }
    } else {
        let m = n as f64 / 1_000_000.0;
        format!("{m:.1}M").replace(".0M", "M")
    }
}

/// What the gauge's popover can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterAction {
    /// The agent's own compaction, or compact-by-handoff where it has none.
    Compact,
    /// A new thread carrying a budgeted hand-off of this one.
    Continue,
    /// The `/context` breakdown.
    Breakdown,
}

impl MeterAction {
    pub const ALL: [MeterAction; 3] = [
        MeterAction::Compact,
        MeterAction::Continue,
        MeterAction::Breakdown,
    ];
}

/// One popover button: its words, and why it is off when it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow {
    pub action: MeterAction,
    pub label: String,
    pub enabled: bool,
    pub reason: Option<String>,
}

/// The popover's rows. `compact_command` is the agent's native one (`/compact`), if any.
/// Compact and Continue wait for the turn to end; the breakdown only reads.
pub fn actions(
    compact_command: Option<&str>,
    agent: &str,
    running: bool,
    has_breakdown: bool,
) -> [ActionRow; 3] {
    let busy = running.then(|| "Wait for the current turn to finish.".to_owned());
    let compact_label = match compact_command.map(str::trim).filter(|c| !c.is_empty()) {
        Some(cmd) => format!("Compact ({agent}'s {cmd})"),
        None => "Compact by hand-off".to_owned(),
    };
    [
        ActionRow {
            action: MeterAction::Compact,
            label: compact_label,
            enabled: !running,
            reason: busy.clone(),
        },
        ActionRow {
            action: MeterAction::Continue,
            label: "Continue in a new thread".to_owned(),
            enabled: !running,
            reason: busy,
        },
        ActionRow {
            action: MeterAction::Breakdown,
            label: "Context breakdown (/context)".to_owned(),
            enabled: has_breakdown,
            reason: (!has_breakdown).then(|| format!("{agent} has no context breakdown.")),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter(used: u64, window: Option<u64>, at: Option<u64>) -> ContextMeter {
        ContextMeter::new(used, window, at)
    }

    fn level(m: ContextMeter) -> Level {
        match m.state() {
            ContextState::Known { level, .. } => level,
            ContextState::Unknown => panic!("known"),
        }
    }

    #[test]
    fn unknown_without_window_never_shows_a_percentage() {
        for window in [None, Some(0)] {
            let m = meter(1_234, window, Some(100));
            assert_eq!(m.state(), ContextState::Unknown);
            assert_eq!(m.percent_left(), None);
            assert_eq!(m.bar(), None);
            assert_eq!(m.text(), "context ?");
            assert!(m
                .tooltip()
                .contains("has not reported its context window size"));
            assert_eq!(m.used_text(), "1.2k tokens");
        }
    }

    #[test]
    fn a_codex_zero_with_no_window_is_unknown_not_empty() {
        let m = meter(0, None, None);
        assert_eq!(m.state(), ContextState::Unknown);
        assert!(!m.has_reading());
        assert_eq!(m.text(), "context ?");
        assert_eq!(
            m.tooltip(),
            "This agent has not reported its context window size."
        );
        // Used tokens with no window are a reading, though still not a percentage.
        assert!(meter(5, None, None).has_reading());
        // A known window with nothing used is genuinely empty.
        assert_eq!(meter(0, Some(1_000), None).text(), "100% left");
    }

    #[test]
    fn levels_cross_at_thresholds_and_auto_compact() {
        assert_eq!(level(meter(749, Some(1_000), None)), Level::Ok);
        assert_eq!(level(meter(750, Some(1_000), None)), Level::Warn);
        assert_eq!(level(meter(899, Some(1_000), None)), Level::Warn);
        assert_eq!(level(meter(900, Some(1_000), None)), Level::Hot);
        assert_eq!(level(meter(5_000, Some(1_000), None)), Level::Hot);
        // The agent's own point wins when it is lower than 75%.
        assert_eq!(level(meter(600, Some(1_000), Some(600))), Level::Warn);
        assert_eq!(level(meter(599, Some(1_000), Some(600))), Level::Ok);
        // A higher point never delays the 75% warning.
        assert_eq!(level(meter(750, Some(1_000), Some(950))), Level::Warn);
    }

    #[test]
    fn level_array_is_exhaustive() {
        for l in Level::ALL {
            // Adding a variant fails to compile until it is handled here and in ALL.
            match l {
                Level::Ok | Level::Warn | Level::Hot => {}
            }
            assert!(!l.phrase().is_empty());
        }
        assert_eq!(Level::ALL.len(), 3);
        for a in MeterAction::ALL {
            match a {
                MeterAction::Compact | MeterAction::Continue | MeterAction::Breakdown => {}
            }
        }
        let rows = actions(None, "agy", false, false);
        for (row, a) in rows.iter().zip(MeterAction::ALL) {
            assert_eq!(row.action, a);
        }
    }

    #[test]
    fn text_tooltip_and_bar() {
        let m = meter(50_000, Some(200_000), Some(160_000));
        assert_eq!(m.text(), "75% left");
        assert_eq!(m.used_text(), "50k / 200k");
        assert_eq!(m.bar(), Some((0.25, Some(0.8))));
        assert_eq!(
            m.tooltip(),
            "Context window: 50k / 200k used, 150k left. Auto-compacts at 160k."
        );
        let over = meter(300, Some(200), None);
        assert_eq!(over.bar(), Some((1.0, None)));
        assert_eq!(over.text(), "0% left");
        assert!(ContextMeter {
            source: Source::Estimated,
            ..m
        }
        .tooltip()
        .ends_with("An estimate."));
    }

    #[test]
    fn actions_name_the_native_command_and_wait_for_the_turn() {
        let idle = actions(Some("/compact"), "Claude", false, true);
        assert_eq!(idle[0].label, "Compact (Claude's /compact)");
        assert!(idle.iter().all(|r| r.enabled && r.reason.is_none()));
        let busy = actions(None, "agy", true, true);
        assert_eq!(busy[0].label, "Compact by hand-off");
        assert!(!busy[0].enabled && !busy[1].enabled);
        assert!(busy[0].reason.is_some() && busy[1].reason.is_some());
        // Reading the breakdown is allowed mid-turn.
        assert!(busy[2].enabled);
        let none = actions(None, "Local", false, false);
        assert!(!none[2].enabled && none[2].reason.is_some());
    }
}
