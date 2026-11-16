//! The context-left meter: what a thread's token reading says about its window, in three honest
//! states, and the actions the header offers from it. Pure: the header, the `/context` panel
//! and the popover all read it, so the thresholds and the wording live here once.
//!
//! A window nobody reported is `Unknown`, never a guessed size: no percentage, no bar. Codex
//! sends `used` as 0 when it has no figure, so a reading of 0 with no window says nothing at all.
//!
//! The warning point is one global setting ([`ContextWarning`]). When a reading first passes it,
//! the thread offers to compact or continue in a new thread, once per crossing ([`WarnLatch`]).

/// How the reading came to be: reported by the agent, or worked out by the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Measured,
    Estimated,
}

/// How full the window is. `Warn` is at the warning threshold (75% unless the setting says
/// otherwise) or at the agent's own auto-compact point when that comes first; `Hot` is at 90%.
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

/// When the thread offers to compact: "Warn When Context Runs Low" in Settings, one setting for
/// every thread. Stored as the percentage used (0 for never); the bar turns amber at the same
/// point (at 75% when the offer is off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextWarning {
    Off,
    At60,
    #[default]
    At75,
    At90,
}

impl ContextWarning {
    /// Every choice, in the order the Settings dropdown lists them.
    pub const ALL: [ContextWarning; 4] = [
        ContextWarning::Off,
        ContextWarning::At60,
        ContextWarning::At75,
        ContextWarning::At90,
    ];

    /// The stored value: percent of the window used, 0 for never.
    pub fn percent(self) -> u8 {
        match self {
            ContextWarning::Off => 0,
            ContextWarning::At60 => 60,
            ContextWarning::At75 => 75,
            ContextWarning::At90 => 90,
        }
    }

    /// The choice a stored value names; `None` for any other number.
    pub fn from_percent(percent: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.percent() == percent)
    }

    /// Whether the offer is made at all.
    pub fn enabled(self) -> bool {
        self != ContextWarning::Off
    }

    /// Fraction of the window at which the bar warns: the threshold, or 75% when off.
    pub fn fraction(self) -> f64 {
        match self {
            ContextWarning::Off => Level::WARN_AT,
            other => f64::from(other.percent()) / 100.0,
        }
    }

    /// The dropdown's words.
    pub fn label(self) -> &'static str {
        match self {
            ContextWarning::Off => "Never",
            ContextWarning::At60 => "At 60% Used",
            ContextWarning::At75 => "At 75% Used",
            ContextWarning::At90 => "At 90% Used",
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
    /// The warning threshold the reading is judged against.
    pub warning: ContextWarning,
}

impl ContextMeter {
    pub fn new(used: u64, window: Option<u64>, auto_compact_at: Option<u64>) -> Self {
        Self {
            used,
            window,
            auto_compact_at,
            source: Source::Measured,
            warning: ContextWarning::default(),
        }
    }

    /// The same reading judged against `warning`.
    pub fn with_warning(self, warning: ContextWarning) -> Self {
        Self { warning, ..self }
    }

    /// The same reading, marked as worked out by the app when `estimated`.
    pub fn estimated(self, estimated: bool) -> Self {
        Self {
            source: if estimated {
                Source::Estimated
            } else {
                Source::Measured
            },
            ..self
        }
    }

    fn is_estimate(&self) -> bool {
        self.source == Source::Estimated
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
        let threshold = self.warning.fraction();
        own.map_or(threshold, |o| o.min(threshold))
    }

    /// Whether the reading is past the warning point (`Warn` or `Hot`). An unknown window is
    /// never low: nothing says how much is left.
    pub fn is_low(&self) -> bool {
        matches!(self.state(), ContextState::Known { level, .. } if level != Level::Ok)
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

    /// The header button's text: "37% left" ("~37% left" for an estimate), or a dim
    /// "context ?" with no window.
    pub fn text(&self) -> String {
        match self.percent_left() {
            Some(p) if self.is_estimate() => format!("~{p}% left"),
            Some(p) => format!("{p}% left"),
            None => "context ?".to_owned(),
        }
    }

    /// The popover's heading: the header's text, saying when it is an estimate.
    pub fn summary(&self) -> String {
        match self.state() {
            ContextState::Unknown => "Context size not reported".to_owned(),
            ContextState::Known { .. } if self.is_estimate() => {
                format!("{} (estimate)", self.text())
            }
            ContextState::Known { .. } => self.text(),
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
        // An estimate's window is the profile's own budget, never the model's native size.
        let what = if self.is_estimate() {
            "Context budget"
        } else {
            "Context window"
        };
        let mut t = format!(
            "{what}: {} used, {} left.",
            self.used_text(),
            format_tokens(left)
        );
        if let Some(at) = self.auto_compact_at {
            t.push_str(&format!(" Auto-compacts at {}.", format_tokens(at)));
        }
        if self.is_estimate() {
            t.push_str(
                " An estimate: the budget is the one set for this profile in Settings, not the \
                 model's own window.",
            );
        }
        t
    }

    /// What the offer to compact says while the reading is low: how much is left, and that
    /// nothing happens until a button is pressed. `None` when it is not low.
    pub fn offer_text(&self) -> Option<String> {
        let ContextState::Known { left, .. } = self.state() else {
            return None;
        };
        if !self.is_low() {
            return None;
        }
        let pct = self.percent_left().unwrap_or(0);
        let room = if self.is_estimate() {
            format!(
                "about {pct}% of this profile's context budget is left ({} tokens, an estimate)",
                format_tokens(left)
            )
        } else {
            format!(
                "{pct}% of the context window is left ({} tokens)",
                format_tokens(left)
            )
        };
        Some(format!(
            "Context is running low: {room}. Compact the conversation, or continue it in a new \
             thread from a summary, before detail is lost. Nothing is sent until you choose."
        ))
    }
}

/// Offers once per crossing: the offer is made when a reading first passes the warning point,
/// and again only after a reading below it (a compaction, a new agent or model) re-arms it. A
/// reading with no window says nothing either way. Kept in memory only, per open thread: a
/// reopened thread already past the point is primed from its history, not offered again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WarnLatch {
    warned: bool,
}

impl WarnLatch {
    /// A new reading (`None`: the gauge was cleared). True when the offer is to be shown now:
    /// the first low reading since the last one that was not, with the warning on. A crossing
    /// while the warning is off is still a crossing: turning it on later does not offer for it.
    pub fn observe(&mut self, meter: Option<&ContextMeter>) -> bool {
        let Some(meter) = meter else {
            self.warned = false;
            return false;
        };
        if meter.state() == ContextState::Unknown {
            return false;
        }
        if !meter.is_low() {
            self.warned = false;
            return false;
        }
        if std::mem::replace(&mut self.warned, true) {
            return false;
        }
        meter.warning.enabled()
    }

    /// Takes a reading without offering (history replayed, a changed threshold): one already
    /// past the point counts as warned.
    pub fn prime(&mut self, meter: Option<&ContextMeter>) {
        let Some(meter) = meter else {
            self.warned = false;
            return;
        };
        if meter.state() != ContextState::Unknown {
            self.warned = meter.is_low();
        }
    }

    /// Whether the current crossing was already offered (or primed).
    pub fn warned(&self) -> bool {
        self.warned
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

/// The offer's buttons: the popover's Compact (where the agent has its own) and Continue, never
/// the breakdown.
pub fn offer_actions(compact_command: Option<&str>, agent: &str, running: bool) -> Vec<ActionRow> {
    actions(compact_command, agent, running, false)
        .into_iter()
        .filter(|row| row.action != MeterAction::Breakdown)
        .collect()
}

/// The popover's rows. `compact_command` is the agent's native one (`/compact`), if any.
/// Compact and Continue wait for the turn to end; the breakdown only reads. An agent with no
/// native command gets no Compact row, since compacting by hand-off is Continue.
pub fn actions(
    compact_command: Option<&str>,
    agent: &str,
    running: bool,
    has_breakdown: bool,
) -> Vec<ActionRow> {
    let busy = running.then(|| "Wait for the current turn to finish.".to_owned());
    let mut rows = Vec::with_capacity(3);
    // With no native command, Compact would do what Continue does: leave it out.
    if let Some(cmd) = compact_command.map(str::trim).filter(|c| !c.is_empty()) {
        rows.push(ActionRow {
            action: MeterAction::Compact,
            label: format!("Compact ({agent}'s {cmd})"),
            enabled: !running,
            reason: busy.clone(),
        });
    }
    rows.push(ActionRow {
        action: MeterAction::Continue,
        label: "Continue in a new thread".to_owned(),
        enabled: !running,
        reason: busy,
    });
    rows.push(ActionRow {
        action: MeterAction::Breakdown,
        label: "Context breakdown (/context)".to_owned(),
        enabled: has_breakdown,
        reason: (!has_breakdown).then(|| format!("{agent} has no context breakdown.")),
    });
    rows
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
        let rows = actions(Some("/compact"), "Claude", false, false);
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
        let estimate = ContextMeter {
            source: Source::Estimated,
            ..m
        };
        assert!(estimate.tooltip().starts_with("Context budget: "));
        assert!(estimate.tooltip().ends_with("not the model's own window."));
    }

    #[test]
    fn an_estimate_says_so_everywhere_it_is_shown() {
        // The Local agent: its window is the profile's budget, its use the planner's figure.
        let m = meter(24_576, Some(32_768), None).estimated(true);
        assert_eq!(m.source, Source::Estimated);
        assert_eq!(m.text(), "~25% left");
        assert_eq!(m.summary(), "~25% left (estimate)");
        assert!(m
            .tooltip()
            .contains("budget is the one set for this profile"));
        let offer = m.offer_text().expect("low");
        assert!(offer.contains("this profile's context budget"));
        assert!(offer.contains("an estimate"));
        // A reported reading says none of that.
        let measured = meter(24_576, Some(32_768), None);
        assert_eq!(measured.text(), "25% left");
        assert_eq!(measured.summary(), "25% left");
        assert!(!measured.offer_text().expect("low").contains("estimate"));
        assert_eq!(meter(5, None, None).summary(), "Context size not reported");
    }

    #[test]
    fn agy_tokens_with_no_window_are_shown_as_used_and_never_offered() {
        // agy reports tokens used and no window: no percentage, no bar, no offer, ever.
        let mut latch = WarnLatch::default();
        for used in [10_000, 900_000, 5_000_000] {
            let m = meter(used, None, None).with_warning(ContextWarning::At60);
            assert_eq!(m.state(), ContextState::Unknown);
            assert!(!m.is_low());
            assert_eq!(m.offer_text(), None);
            assert!(m.tooltip().contains("used so far"));
            assert!(!latch.observe(Some(&m)));
        }
        assert!(!latch.warned());
    }

    #[test]
    fn warning_choices_round_trip_and_move_the_warn_point() {
        for w in ContextWarning::ALL {
            match w {
                ContextWarning::Off
                | ContextWarning::At60
                | ContextWarning::At75
                | ContextWarning::At90 => {}
            }
            assert_eq!(ContextWarning::from_percent(w.percent()), Some(w));
            assert!(!w.label().is_empty());
        }
        assert_eq!(ContextWarning::ALL.len(), 4);
        assert_eq!(ContextWarning::default(), ContextWarning::At75);
        for bad in [1, 50, 74, 76, 100, 255] {
            assert_eq!(ContextWarning::from_percent(bad), None, "{bad}");
        }
        let m = meter(650, Some(1_000), None);
        assert!(!m.is_low());
        assert!(m.with_warning(ContextWarning::At60).is_low());
        assert_eq!(
            level(m.with_warning(ContextWarning::At60)),
            Level::Warn,
            "the bar follows the threshold"
        );
        assert!(!meter(850, Some(1_000), None)
            .with_warning(ContextWarning::At90)
            .is_low());
        // Off keeps the bar's own 75% and Hot stays at 90%.
        assert!(meter(800, Some(1_000), None)
            .with_warning(ContextWarning::Off)
            .is_low());
        assert_eq!(
            level(meter(950, Some(1_000), None).with_warning(ContextWarning::At90)),
            Level::Hot
        );
        // The agent's own earlier point still comes first.
        assert!(meter(550, Some(1_000), Some(500))
            .with_warning(ContextWarning::At90)
            .is_low());
    }

    #[test]
    fn warn_latch_fires_once_and_resets_after_compaction() {
        let mut latch = WarnLatch::default();
        let at = |used| meter(used, Some(1_000), None);
        assert!(!latch.observe(Some(&at(500))));
        // First crossing: offered once.
        assert!(latch.observe(Some(&at(760))));
        // Still past it, and further: no repeat, whatever the level.
        for used in [780, 900, 990, 760] {
            assert!(!latch.observe(Some(&at(used))), "{used}");
        }
        // A reading with no window (Codex's blank) neither offers nor re-arms.
        assert!(!latch.observe(Some(&meter(0, None, None))));
        assert!(!latch.observe(Some(&at(800))));
        // A compaction brings it below: re-armed, so the next crossing offers again.
        assert!(!latch.observe(Some(&at(200))));
        assert!(latch.observe(Some(&at(800))));
        // A cleared gauge (an agent or model switch) re-arms too.
        assert!(!latch.observe(None));
        assert!(latch.observe(Some(&at(800))));
    }

    #[test]
    fn warn_latch_off_or_primed_never_offers_for_that_crossing() {
        let at = |used, w| meter(used, Some(1_000), None).with_warning(w);
        // Crossing while the setting is off: nothing offered, and turning it on later does not
        // offer for the same crossing.
        let mut latch = WarnLatch::default();
        assert!(!latch.observe(Some(&at(800, ContextWarning::Off))));
        assert!(latch.warned());
        assert!(!latch.observe(Some(&at(850, ContextWarning::At75))));
        // History replayed past the point: primed, not offered.
        let mut reopened = WarnLatch::default();
        reopened.prime(Some(&at(900, ContextWarning::At75)));
        assert!(reopened.warned());
        assert!(!reopened.observe(Some(&at(910, ContextWarning::At75))));
        // A threshold raised past the reading re-arms it; the next crossing offers.
        let mut raised = WarnLatch::default();
        assert!(raised.observe(Some(&at(700, ContextWarning::At60))));
        raised.prime(Some(&at(700, ContextWarning::At90)));
        assert!(!raised.warned());
        assert!(raised.observe(Some(&at(920, ContextWarning::At90))));
        // Priming with an unknown window keeps what it knew.
        raised.prime(Some(&meter(5, None, None)));
        assert!(raised.warned());
    }

    #[test]
    fn the_offer_has_compact_and_continue_and_waits_for_the_turn() {
        let rows = offer_actions(Some("/compact"), "Claude", false);
        let kinds: Vec<_> = rows.iter().map(|r| r.action).collect();
        assert_eq!(kinds, [MeterAction::Compact, MeterAction::Continue]);
        assert!(rows.iter().all(|r| r.enabled));
        let busy = offer_actions(Some("/compact"), "Claude", true);
        assert!(busy.iter().all(|r| !r.enabled && r.reason.is_some()));
        let local = offer_actions(None, "Local", false);
        let kinds: Vec<_> = local.iter().map(|r| r.action).collect();
        assert_eq!(kinds, [MeterAction::Continue]);
        assert_eq!(meter(100, Some(1_000), None).offer_text(), None);
        let text = meter(800, Some(1_000), None).offer_text().expect("low");
        assert!(text.contains("20% of the context window is left"));
        assert!(text.contains("Nothing is sent until you choose."));
    }

    #[test]
    fn actions_name_the_native_command_and_wait_for_the_turn() {
        let idle = actions(Some("/compact"), "Claude", false, true);
        assert_eq!(idle[0].label, "Compact (Claude's /compact)");
        assert!(idle.iter().all(|r| r.enabled && r.reason.is_none()));
        let busy = actions(Some("/compact"), "Claude", true, true);
        assert!(!busy[0].enabled && !busy[1].enabled);
        assert!(busy[0].reason.is_some() && busy[1].reason.is_some());
        // Reading the breakdown is allowed mid-turn.
        assert!(busy[2].enabled);
        // No native command: no Compact row, only Continue and the breakdown.
        let none = actions(None, "Local", false, false);
        let kinds: Vec<_> = none.iter().map(|r| r.action).collect();
        assert_eq!(kinds, [MeterAction::Continue, MeterAction::Breakdown]);
        assert!(none[0].enabled);
        assert!(!none[1].enabled && none[1].reason.is_some());
    }
}
