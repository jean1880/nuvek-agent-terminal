//! Spending: turning a running cost total into the cost of one turn, and summing turns into the
//! totals the Spending dialog shows.
//!
//! Three kinds of spend are kept apart and never added into one number: metered dollars (an
//! API key, a model server), a plan's list price (what the same turn would have cost, not a
//! charge), and a cost whose billing was not recorded. Agents that report no cost count tokens
//! only: their cost is "not reported", never $0.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::event::Usage;

/// The running total to measure the next turn from, once a turn reported `now`.
///
/// A reading that is not finite, or an exact zero after a paid turn (an interrupted or failed
/// turn reports no total of its own), leaves `prev` as it was.
pub fn next_base(prev: Option<f64>, now: f64) -> Option<f64> {
    let prev = prev.filter(|p| p.is_finite());
    if !now.is_finite() || (now == 0.0 && prev.is_some_and(|p| p > 0.0)) {
        return prev;
    }
    Some(now)
}

/// What one turn cost, from the agent's running total before it (`prev`, `None` at the first
/// report) and after it (`now`).
///
/// A total that falls to a lower non-zero value means a new process or a reset, so `now` is the
/// whole of that turn's cost. Float noise that leaves `now` a hair under `prev` is not a reset:
/// it counts as nothing. An exact zero after a paid turn is no reset either (see [`next_base`]):
/// it costs nothing. A `now` that is not finite costs nothing, and a `prev` that is not finite
/// is taken as none.
pub fn cumulative_to_delta(prev: Option<f64>, now: f64) -> f64 {
    if !now.is_finite() {
        return 0.0;
    }
    let Some(prev) = prev.filter(|p| p.is_finite()) else {
        return now.max(0.0);
    };
    if now >= prev {
        return now - prev;
    }
    if now == 0.0 || prev - now <= 1e-12 * prev.abs().max(1.0) {
        0.0
    } else {
        now.max(0.0)
    }
}

/// A calendar day in the user's own time zone: what a turn is counted under. The caller turns
/// a time into one (the store from the C library's local time, a test from a fixed offset), so
/// this module needs no time zone of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DayKey {
    year: i32,
    month: u8,
    day: u8,
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

impl DayKey {
    /// The day, when it is a real one.
    pub fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let key = Self {
            year,
            month: month as u8,
            day: day as u8,
        };
        // A day past the month's end comes back as another date.
        (Self::from_days(key.days()) == key).then_some(key)
    }

    /// The day `days` after 1970-01-01.
    pub fn from_days(days: i64) -> Self {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Self { year, month, day }
    }

    /// Days since 1970-01-01.
    pub fn days(self) -> i64 {
        let m = i64::from(self.month);
        let y = i64::from(self.year) - i64::from(m <= 2);
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The day of a time `at_ms` (epoch milliseconds) at a fixed `offset_secs` east of UTC.
    pub fn at_offset(at_ms: i64, offset_secs: i64) -> Self {
        Self::from_days((at_ms.div_euclid(1000) + offset_secs).div_euclid(86_400))
    }

    /// Reads `YYYY-MM-DD`, as [`fmt::Display`] writes it.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.splitn(3, '-');
        let year = parts.next()?.parse().ok()?;
        let month = parts.next()?;
        let day = parts.next()?;
        if month.len() != 2 || day.len() != 2 {
            return None;
        }
        Self::new(year, month.parse().ok()?, day.parse().ok()?)
    }

    pub fn add_days(self, n: i64) -> Self {
        Self::from_days(self.days() + n)
    }

    /// The Monday of its week (weeks start on Monday, as ISO 8601 has them).
    pub fn week_start(self) -> Self {
        // 1970-01-01 was a Thursday.
        let days = self.days();
        Self::from_days(days - (days + 3).rem_euclid(7))
    }

    /// The first of its month.
    pub fn month_start(self) -> Self {
        Self { day: 1, ..self }
    }

    /// The first of the month `n` months from its own.
    pub fn add_months(self, n: i32) -> Self {
        let index = self.year * 12 + i32::from(self.month) - 1 + n;
        Self {
            year: index.div_euclid(12),
            month: (index.rem_euclid(12) + 1) as u8,
            day: 1,
        }
    }

    /// "November 2026".
    pub fn month_label(self) -> String {
        format!("{} {}", MONTHS[usize::from(self.month - 1)], self.year)
    }
}

impl fmt::Display for DayKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// How a turn was paid for, recorded with it so that a profile changed later does not relabel
/// its history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Billing {
    /// Real money: an API key or a metered model server.
    Metered,
    /// A plan: the reported cost is the list price, not a charge.
    Plan,
    /// A profile whose models cost nothing.
    Free,
    /// Not recorded (turns stored before billing was).
    Unknown,
}

impl Billing {
    pub const ALL: [Billing; 4] = [
        Billing::Metered,
        Billing::Plan,
        Billing::Free,
        Billing::Unknown,
    ];

    /// The stored name (the `spend_ledger.billing` column).
    pub fn as_str(self) -> &'static str {
        match self {
            Billing::Metered => "metered",
            Billing::Plan => "plan",
            Billing::Free => "free",
            Billing::Unknown => "unknown",
        }
    }

    /// A stored name; anything else is [`Billing::Unknown`].
    pub fn parse(name: &str) -> Billing {
        Billing::ALL
            .into_iter()
            .find(|b| b.as_str() == name)
            .unwrap_or(Billing::Unknown)
    }
}

/// Stored as its name ([`Billing::as_str`]).
impl Serialize for Billing {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Read leniently: a name a newer build wrote, or anything that is not a string, is
/// [`Billing::Unknown`], so the turn it belongs to still reads.
impl<'de> Deserialize<'de> for Billing {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        Ok(value.as_str().map_or(Billing::Unknown, Billing::parse))
    }
}

/// How a Claude process is paid for, from what it said of itself: its `init` frame's
/// `apiKeySource` and its `initialize` reply's account (`apiProvider`, `subscriptionType`).
///
/// An API key (any source but `none`) is metered whatever the account says, as is a cloud
/// provider other than Anthropic's own (Bedrock, Vertex). Signed in with no key, an account on
/// Anthropic's own service, or one that names a plan, is a plan: its reported cost is the list
/// price. Anything less is not known, and never counted as real money.
pub fn claude_billing(
    api_key_source: Option<&str>,
    api_provider: Option<&str>,
    plan: Option<&str>,
) -> Option<Billing> {
    let api_key_source = api_key_source.map(str::trim).filter(|s| !s.is_empty());
    let api_provider = api_provider.map(str::trim).filter(|s| !s.is_empty());
    let has_plan = plan.is_some_and(|p| !p.trim().is_empty());
    if api_key_source.is_some_and(|s| s != "none") {
        return Some(Billing::Metered);
    }
    if api_provider.is_some_and(|p| p != "firstParty") {
        return Some(Billing::Metered);
    }
    let signed_in_without_key = api_key_source == Some("none");
    if has_plan || (signed_in_without_key && api_provider == Some("firstParty")) {
        return Some(Billing::Plan);
    }
    None
}

/// How a turn was paid for, from what the agent said (`reported`, [`claude_billing`] for
/// Claude) and the profile it was launched on: a profile marked free to run costs nothing, and
/// one on a model server is metered there (the server bills it, whatever the agent thinks of
/// its account). Otherwise the agent's own word stands.
pub fn turn_billing(reported: Option<Billing>, free: bool, on_server: bool) -> Option<Billing> {
    if free {
        Some(Billing::Free)
    } else if on_server {
        Some(Billing::Metered)
    } else {
        reported
    }
}

/// One turn, or (from the ledger of deleted threads) one day's sum of turns with the same
/// attribution. Numbers and ids only.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendRow {
    pub day: DayKey,
    /// The agent's key (`claude`, `codex`, …).
    pub driver: String,
    /// The profile's id; empty for the agent's default.
    pub profile_id: String,
    /// The model; `default` (or empty) for the agent's own default.
    pub model: String,
    /// The thread's folder; a delegated worker's is its root thread's.
    pub folder: String,
    pub billing: Billing,
    /// The turn was a delegated worker's, counted under its root thread.
    pub delegated: bool,
    /// The cost of the turns that reported one (`cost_turns` of them).
    pub cost_usd: f64,
    pub cost_turns: u64,
    pub turns: u64,
    /// Turns whose stored cost was a running total, not their own: counted for their tokens,
    /// their cost withheld.
    pub withheld_turns: u64,
    pub tokens: Usage,
}

/// What a set of turns cost, one sum per kind of spend, never added together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Costs {
    pub metered: f64,
    pub list_price: f64,
    /// A cost whose billing was not recorded: metered or list price, unknown which.
    pub reported: f64,
}

impl Costs {
    fn add(&mut self, billing: Billing, usd: f64) {
        match billing {
            Billing::Metered => self.metered += usd,
            Billing::Plan => self.list_price += usd,
            Billing::Free => {}
            Billing::Unknown => self.reported += usd,
        }
    }

    /// Used only to order and scale rows, never shown: it adds the kinds together.
    fn magnitude(&self) -> f64 {
        self.metered + self.list_price + self.reported
    }

    /// Each kind that is not zero, as "$1.20 metered", "$3.40 at list price", "$0.50
    /// reported"; "$0.00" when all are zero.
    pub fn parts(&self) -> Vec<String> {
        let parts: Vec<String> = [
            (self.metered, "metered"),
            (self.list_price, "at list price"),
            (self.reported, "reported"),
        ]
        .into_iter()
        .filter(|(usd, _)| *usd > 0.0)
        .map(|(usd, kind)| format!("{} {kind}", format_usd(usd)))
        .collect();
        if parts.is_empty() {
            vec![format_usd(0.0)]
        } else {
            parts
        }
    }

    pub fn describe(&self) -> String {
        self.parts().join(" · ")
    }
}

/// The sum of a set of turns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Total {
    pub costs: Costs,
    /// The part of this total that came from delegated workers, summed the same way (so its
    /// turns that reported no cost read as not reported); `None` when none did.
    pub delegated: Option<Box<Total>>,
    pub turns: u64,
    /// Turns that reported a cost.
    pub cost_turns: u64,
    /// Turns on a free profile.
    pub free_turns: u64,
    /// Turns whose cost was withheld ([`SpendRow::withheld_turns`]).
    pub withheld_turns: u64,
    pub tokens: Usage,
}

/// What a [`Total`] shows as its amount: never a $0 for turns that reported no cost.
#[derive(Debug, Clone, PartialEq)]
pub enum Amount {
    /// No turns at all.
    Nothing,
    /// Every turn was on a free profile.
    Free,
    /// Turns, none of which reported a cost.
    NotReported,
    Cost(Costs),
}

impl Total {
    pub fn add(&mut self, row: &SpendRow) {
        self.add_own(row);
        if row.delegated {
            self.delegated.get_or_insert_with(Box::default).add_own(row);
        }
    }

    /// The delegated workers' part ([`Total::delegated`]); empty when there is none.
    pub fn delegated_total(&self) -> Total {
        self.delegated.as_deref().cloned().unwrap_or_default()
    }

    fn add_own(&mut self, row: &SpendRow) {
        let usd = if row.cost_usd.is_finite() {
            row.cost_usd.max(0.0)
        } else {
            0.0
        };
        self.turns += row.turns;
        if row.billing == Billing::Free {
            // What an agent reports of a free profile's turn prices nothing anyone pays.
            self.free_turns += row.turns;
        } else {
            self.costs.add(row.billing, usd);
            self.cost_turns += row.cost_turns;
        }
        self.withheld_turns += row.withheld_turns;
        self.tokens.input_tokens += row.tokens.input_tokens;
        self.tokens.output_tokens += row.tokens.output_tokens;
        self.tokens.cached_input_tokens += row.tokens.cached_input_tokens;
        self.tokens.reasoning_tokens += row.tokens.reasoning_tokens;
    }

    pub fn amount(&self) -> Amount {
        if self.cost_turns > 0 {
            Amount::Cost(self.costs.clone())
        } else if self.turns == 0 {
            Amount::Nothing
        } else if self.free_turns == self.turns {
            Amount::Free
        } else {
            Amount::NotReported
        }
    }

    /// Turns that reported no cost and were not free.
    pub fn not_reported_turns(&self) -> u64 {
        self.turns
            .saturating_sub(self.cost_turns)
            .saturating_sub(self.free_turns)
    }

    /// Input (cache included) plus output tokens.
    pub fn token_count(&self) -> u64 {
        self.tokens.input_tokens + self.tokens.output_tokens
    }
}

/// "$1.23"; a cost under a cent is "<$0.01", so a small cost never reads as nothing.
pub fn format_usd(usd: f64) -> String {
    if !usd.is_finite() || usd <= 0.0 {
        "$0.00".to_owned()
    } else if usd < 0.01 {
        "<$0.01".to_owned()
    } else {
        format!("${usd:.2}")
    }
}

/// An amount in `currency`: "$1.23" in US dollars, "1.23 CAD" in another, a bare "1.23" when
/// the currency was not reported (never assumed to be dollars).
pub fn format_money(amount: f64, currency: Option<&str>) -> String {
    let plain = if amount.is_finite() {
        amount.max(0.0)
    } else {
        0.0
    };
    match currency {
        Some("USD") => format_usd(amount),
        Some(code) => format!("{plain:.2} {code}"),
        None => format!("{plain:.2}"),
    }
}

/// "Extra usage: $2.50 of $40.00 this month", "Extra usage: off".
pub fn extra_usage_text(extra: &crate::event::ExtraUsage) -> String {
    let currency = extra.currency.as_deref();
    if !extra.enabled && extra.used <= 0.0 {
        return "Extra usage: off".to_owned();
    }
    let used = format_money(extra.used, currency);
    match extra.limit {
        Some(limit) => format!(
            "Extra usage: {used} of {} this month",
            format_money(limit, currency)
        ),
        None => format!("Extra usage: {used} this month"),
    }
}

/// "950", "12.3k", "4.5M".
pub fn format_tokens(n: u64) -> String {
    let short = |value: f64, unit: &str| {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => short(n as f64 / 1e3, "k"),
        _ => short(n as f64 / 1e6, "M"),
    }
}

/// The time span the dialog sums over, and how it buckets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// The last 30 days, day by day.
    Day,
    /// The last 12 weeks.
    Week,
    /// The last 12 months.
    Month,
}

impl Period {
    /// The selector's order: a dropdown or toggle index is a position in it.
    pub const ALL: [Period; 3] = [Period::Day, Period::Week, Period::Month];

    pub fn label(self) -> &'static str {
        match self {
            Period::Day => "Day",
            Period::Week => "Week",
            Period::Month => "Month",
        }
    }

    /// "Last 30 days", …
    pub fn span_label(self) -> &'static str {
        match self {
            Period::Day => "Last 30 days",
            Period::Week => "Last 12 weeks",
            Period::Month => "Last 12 months",
        }
    }

    /// The bucket `day` falls in.
    pub fn bucket(self, day: DayKey) -> DayKey {
        match self {
            Period::Day => day,
            Period::Week => day.week_start(),
            Period::Month => day.month_start(),
        }
    }

    /// The first day the span covers, ending with `today`'s bucket.
    pub fn first_day(self, today: DayKey) -> DayKey {
        match self {
            Period::Day => today.add_days(-29),
            Period::Week => today.week_start().add_days(-7 * 11),
            Period::Month => today.month_start().add_months(-11),
        }
    }

    /// The first day any period covers: what to read to serve them all.
    pub fn earliest(today: DayKey) -> DayKey {
        Period::ALL
            .into_iter()
            .map(|p| p.first_day(today))
            .min()
            .unwrap_or(today)
    }

    /// How a bucket starting on `start` is named.
    pub fn bucket_label(self, start: DayKey) -> String {
        match self {
            Period::Day => start.to_string(),
            Period::Week => format!("Week of {start}"),
            Period::Month => start.month_label(),
        }
    }
}

/// What the dialog's rows are grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Folder,
    Agent,
    Profile,
    Model,
}

impl Group {
    /// The selector's order: a dropdown index is a position in it.
    pub const ALL: [Group; 4] = [Group::Folder, Group::Agent, Group::Profile, Group::Model];

    pub fn label(self) -> &'static str {
        match self {
            Group::Folder => "Folder",
            Group::Agent => "Agent",
            Group::Profile => "Profile",
            Group::Model => "Model",
        }
    }

    /// The row's raw key for this grouping; the caller names it (a profile id becomes its
    /// name, a driver key its agent's).
    pub fn key(self, row: &SpendRow) -> &str {
        match self {
            Group::Folder => &row.folder,
            Group::Agent => &row.driver,
            Group::Profile => &row.profile_id,
            Group::Model => &row.model,
        }
    }
}

/// The dialog's numbers for one period and grouping.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub period: Period,
    pub group: Group,
    /// The first day summed.
    pub first: DayKey,
    pub total: Total,
    /// By the chosen grouping: largest first.
    pub groups: Vec<(String, Total)>,
    /// By period bucket, newest first; only buckets with turns.
    pub buckets: Vec<(DayKey, Total)>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.total.turns == 0
    }
}

/// Sums `rows` from `period`'s first day on (through `today`, and any later day a clock set
/// back left), by `group` and by bucket.
pub fn report(rows: &[SpendRow], period: Period, group: Group, today: DayKey) -> Report {
    let first = period.first_day(today);
    let mut total = Total::default();
    let mut groups: HashMap<&str, Total> = HashMap::new();
    let mut buckets: HashMap<DayKey, Total> = HashMap::new();
    for row in rows.iter().filter(|r| r.day >= first) {
        total.add(row);
        groups.entry(group.key(row)).or_default().add(row);
        buckets.entry(period.bucket(row.day)).or_default().add(row);
    }
    let mut groups: Vec<(String, Total)> =
        groups.into_iter().map(|(k, t)| (k.to_owned(), t)).collect();
    groups.sort_by(|(ka, a), (kb, b)| {
        b.costs
            .magnitude()
            .total_cmp(&a.costs.magnitude())
            .then(b.token_count().cmp(&a.token_count()))
            .then(ka.cmp(kb))
    });
    let mut buckets: Vec<(DayKey, Total)> = buckets.into_iter().collect();
    buckets.sort_by_key(|b| std::cmp::Reverse(b.0));
    Report {
        period,
        group,
        first,
        total,
        groups,
        buckets,
    }
}

/// How long each row's bar is, 0.0 to 1.0: by cost when any row reported one (a row with none
/// then has no bar), else by tokens.
pub fn bar_fractions<'a>(totals: impl IntoIterator<Item = &'a Total>) -> Vec<f64> {
    let totals: Vec<&Total> = totals.into_iter().collect();
    let by_cost = totals.iter().any(|t| t.cost_turns > 0);
    let value = |t: &Total| {
        if by_cost {
            t.costs.magnitude()
        } else {
            t.token_count() as f64
        }
    };
    let max = totals.iter().map(|t| value(t)).fold(0.0_f64, f64::max);
    totals
        .iter()
        .map(|t| if max > 0.0 { value(t) / max } else { 0.0 })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_report_is_the_whole_total() {
        assert_eq!(cumulative_to_delta(None, 0.25), 0.25);
    }

    #[test]
    fn a_rising_total_gives_the_step() {
        let d = cumulative_to_delta(Some(0.25), 0.75);
        assert!((d - 0.5).abs() < 1e-12);
    }

    #[test]
    fn an_unchanged_total_is_free() {
        assert_eq!(cumulative_to_delta(Some(0.25), 0.25), 0.0);
    }

    #[test]
    fn a_fallen_total_is_a_reset() {
        assert_eq!(cumulative_to_delta(Some(2.0), 0.5), 0.5);
        assert_eq!(next_base(Some(2.0), 0.5), Some(0.5));
    }

    #[test]
    fn float_noise_below_the_previous_total_is_zero() {
        assert_eq!(cumulative_to_delta(Some(0.3), 0.3 - 1e-15), 0.0);
    }

    #[test]
    fn a_negative_total_is_never_reported() {
        assert_eq!(cumulative_to_delta(None, -1.0), 0.0);
        assert_eq!(cumulative_to_delta(Some(1.0), -1.0), 0.0);
    }

    #[test]
    fn a_zero_total_after_a_paid_turn_costs_nothing_and_keeps_the_base() {
        assert_eq!(cumulative_to_delta(Some(0.5), 0.0), 0.0);
        assert_eq!(next_base(Some(0.5), 0.0), Some(0.5));
        assert_eq!(next_base(None, 0.0), Some(0.0));
    }

    #[test]
    fn non_finite_inputs_are_ignored() {
        assert_eq!(cumulative_to_delta(Some(1.0), f64::NAN), 0.0);
        assert_eq!(cumulative_to_delta(Some(1.0), f64::INFINITY), 0.0);
        assert_eq!(cumulative_to_delta(Some(f64::NAN), 0.25), 0.25);
        assert_eq!(cumulative_to_delta(Some(f64::INFINITY), 0.25), 0.25);
        assert_eq!(next_base(Some(1.0), f64::NAN), Some(1.0));
        assert_eq!(next_base(Some(f64::NAN), 0.25), Some(0.25));
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn day(text: &str) -> DayKey {
        DayKey::parse(text).expect(text)
    }

    fn row(on: &str, driver: &str, folder: &str, cost: Option<f64>, tokens: u64) -> SpendRow {
        SpendRow {
            day: day(on),
            driver: driver.into(),
            profile_id: String::new(),
            model: "default".into(),
            folder: folder.into(),
            billing: Billing::Unknown,
            delegated: false,
            cost_usd: cost.unwrap_or(0.0),
            cost_turns: u64::from(cost.is_some()),
            turns: 1,
            withheld_turns: 0,
            tokens: Usage {
                input_tokens: tokens,
                output_tokens: tokens,
                ..Usage::default()
            },
        }
    }

    #[test]
    fn days_round_trip_through_the_civil_calendar() {
        for text in [
            "1970-01-01",
            "2000-02-29",
            "2026-11-15",
            "2024-12-31",
            "1969-12-31",
        ] {
            let d = day(text);
            assert_eq!(d.to_string(), text);
            assert_eq!(DayKey::from_days(d.days()), d);
        }
        assert_eq!(day("1970-01-01").days(), 0);
        assert_eq!(DayKey::parse("2026-02-29"), None, "not a leap year");
        assert_eq!(DayKey::parse("2026-13-01"), None);
        assert_eq!(DayKey::parse("2026-1-01"), None);
        assert_eq!(DayKey::parse("garbage"), None);
        assert_eq!(day("2026-11-15").week_start(), day("2026-11-09"));
        assert_eq!(day("2026-11-09").week_start(), day("2026-11-09"));
        assert_eq!(day("2026-01-01").add_months(-1), day("2025-12-01"));
        assert_eq!(day("2026-11-15").add_months(-11), day("2025-12-01"));
        assert_eq!(day("2026-11-15").month_label(), "November 2026");
    }

    #[test]
    fn aggregate_buckets_by_day_week_month_in_a_fixed_offset() {
        // 2026-11-01 04:30 UTC: past midnight in Toronto before the clocks go back (UTC-4), still
        // the evening before after (UTC-5). The caller's day function decides; the sums follow.
        let at = 1_793_507_400_000;
        assert_eq!(DayKey::at_offset(at, -4 * 3600), day("2026-11-01"));
        assert_eq!(DayKey::at_offset(at, -5 * 3600), day("2026-10-31"));

        let today = day("2026-11-15");
        let rows = vec![
            row("2026-11-15", "claude", "/a", Some(1.0), 10),
            row("2026-11-14", "claude", "/a", Some(2.0), 10),
            row("2026-11-01", "claude", "/a", Some(4.0), 10),
            row("2026-10-31", "claude", "/a", Some(8.0), 10),
            row("2026-10-16", "claude", "/a", Some(16.0), 10),
            row("2026-10-15", "claude", "/a", Some(32.0), 10),
            row("2025-12-01", "claude", "/a", Some(64.0), 10),
            row("2025-11-30", "claude", "/a", Some(128.0), 10),
        ];
        let by_day = report(&rows, Period::Day, Group::Folder, today);
        assert_eq!(by_day.first, day("2026-10-17"));
        assert_eq!(by_day.total.costs.reported, 15.0, "the last 30 days only");
        assert_eq!(by_day.buckets.first().map(|b| b.0), Some(today));
        assert_eq!(by_day.buckets.len(), 4);

        let by_week = report(&rows, Period::Week, Group::Folder, today);
        assert_eq!(by_week.first, day("2026-08-24"));
        let weeks: Vec<(String, f64)> = by_week
            .buckets
            .iter()
            .map(|(d, t)| (d.to_string(), t.costs.reported))
            .collect();
        assert_eq!(
            weeks,
            [
                ("2026-11-09".to_owned(), 3.0),
                ("2026-10-26".to_owned(), 12.0),
                ("2026-10-12".to_owned(), 48.0),
            ]
        );

        let by_month = report(&rows, Period::Month, Group::Folder, today);
        assert_eq!(by_month.first, day("2025-12-01"));
        let months: Vec<(String, f64)> = by_month
            .buckets
            .iter()
            .map(|(d, t)| (Period::Month.bucket_label(*d), t.costs.reported))
            .collect();
        assert_eq!(
            months,
            [
                ("November 2026".to_owned(), 7.0),
                ("October 2026".to_owned(), 56.0),
                ("December 2025".to_owned(), 64.0),
            ]
        );
        assert_eq!(Period::earliest(today), day("2025-12-01"));
    }

    #[test]
    fn group_by_folder_agent_profile_model_sums_correctly() {
        let today = day("2026-11-15");
        let mut a = row("2026-11-15", "claude", "/a", Some(1.5), 100);
        a.profile_id = "work".into();
        a.model = "opus".into();
        let mut b = row("2026-11-14", "claude", "/b", Some(0.5), 100);
        b.model = "sonnet".into();
        let mut c = row("2026-11-13", "codex", "/a", None, 1000);
        c.profile_id = "work".into();
        let rows = vec![a, b, c];
        let keys = |group: Group| -> Vec<(String, u64, f64)> {
            report(&rows, Period::Day, group, today)
                .groups
                .into_iter()
                .map(|(k, t)| (k, t.turns, t.costs.reported))
                .collect()
        };
        assert_eq!(
            keys(Group::Folder),
            [("/a".into(), 2, 1.5), ("/b".into(), 1, 0.5)]
        );
        assert_eq!(
            keys(Group::Agent),
            [("claude".into(), 2, 2.0), ("codex".into(), 1, 0.0)]
        );
        assert_eq!(
            keys(Group::Profile),
            [("work".into(), 2, 1.5), (String::new(), 1, 0.5)]
        );
        assert_eq!(
            keys(Group::Model),
            [
                ("opus".into(), 1, 1.5),
                ("sonnet".into(), 1, 0.5),
                ("default".into(), 1, 0.0)
            ]
        );
    }

    #[test]
    fn tokens_only_agents_never_show_zero_dollars() {
        let today = day("2026-11-15");
        let rows = vec![row("2026-11-15", "codex", "/a", None, 5000)];
        let r = report(&rows, Period::Day, Group::Agent, today);
        assert_eq!(r.total.amount(), Amount::NotReported);
        assert_eq!(r.total.token_count(), 10_000);
        assert_eq!(r.total.not_reported_turns(), 1);
        assert_eq!(bar_fractions(r.groups.iter().map(|(_, t)| t)), [1.0]);

        let mut free = rows[0].clone();
        free.billing = Billing::Free;
        let r = report(&[free], Period::Day, Group::Agent, today);
        assert_eq!(r.total.amount(), Amount::Free);
        assert_eq!(r.total.not_reported_turns(), 0);

        assert_eq!(Total::default().amount(), Amount::Nothing);
        let zero = row("2026-11-15", "claude", "/a", Some(0.0), 1);
        let r = report(&[zero], Period::Day, Group::Agent, today);
        assert_eq!(
            r.total.amount(),
            Amount::Cost(Costs::default()),
            "a reported zero is a cost"
        );
    }

    #[test]
    fn the_kinds_of_spend_are_never_added_together() {
        let today = day("2026-11-15");
        let mut metered = row("2026-11-15", "claude", "/a", Some(1.25), 1);
        metered.billing = Billing::Metered;
        let mut plan = row("2026-11-15", "claude", "/a", Some(3.4), 1);
        plan.billing = Billing::Plan;
        plan.delegated = true;
        let unknown = row("2026-11-15", "claude", "/a", Some(0.004), 1);
        let r = report(&[metered, plan, unknown], Period::Day, Group::Folder, today);
        assert_eq!(
            r.total.costs.describe(),
            "$1.25 metered · $3.40 at list price · <$0.01 reported"
        );
        let delegated = r.total.delegated_total();
        assert_eq!(delegated.costs.describe(), "$3.40 at list price");
        assert_eq!(delegated.turns, 1);
        assert_eq!(delegated.cost_turns, 1);

        // A worker that reports tokens only is not reported, never $0, beside a priced one.
        let mut codex = row("2026-11-15", "codex", "/a", None, 1);
        codex.delegated = true;
        let r = report(&[codex.clone()], Period::Day, Group::Folder, today);
        assert_eq!(r.total.delegated_total().amount(), Amount::NotReported);
        let mut priced = row("2026-11-15", "claude", "/a", Some(0.5), 1);
        priced.delegated = true;
        let r = report(&[codex, priced], Period::Day, Group::Folder, today);
        let delegated = r.total.delegated_total();
        assert_eq!(delegated.turns, 2);
        assert_eq!(delegated.not_reported_turns(), 1);
        assert!(matches!(delegated.amount(), Amount::Cost(_)));
        assert_eq!(Total::default().delegated_total(), Total::default());
    }

    #[test]
    fn withheld_turns_count_only_inside_the_period() {
        let today = day("2026-11-15");
        let mut recent = row("2026-11-14", "claude", "/a", None, 1);
        recent.withheld_turns = 1;
        let mut old = row("2026-09-01", "claude", "/a", None, 1);
        old.withheld_turns = 1;
        let rows = [recent, old];
        assert_eq!(
            report(&rows, Period::Day, Group::Folder, today)
                .total
                .withheld_turns,
            1
        );
        assert_eq!(
            report(&rows, Period::Month, Group::Folder, today)
                .total
                .withheld_turns,
            2
        );
    }

    #[test]
    fn bars_follow_cost_when_any_row_has_one() {
        let a = Total {
            cost_turns: 1,
            costs: Costs {
                metered: 2.0,
                ..Costs::default()
            },
            turns: 1,
            ..Total::default()
        };
        let b = Total {
            cost_turns: 1,
            costs: Costs {
                reported: 1.0,
                ..Costs::default()
            },
            turns: 1,
            ..Total::default()
        };
        let tokens_only = Total {
            turns: 1,
            tokens: Usage {
                input_tokens: 1_000_000,
                ..Usage::default()
            },
            ..Total::default()
        };
        assert_eq!(bar_fractions([&a, &b, &tokens_only]), [1.0, 0.5, 0.0]);
        assert_eq!(bar_fractions([&tokens_only]), [1.0]);
        assert_eq!(bar_fractions([&Total::default()]), [0.0]);
    }

    #[test]
    fn extra_usage_is_shown_in_its_own_currency() {
        use crate::event::ExtraUsage;
        let extra = |enabled, used, limit, currency: Option<&str>| ExtraUsage {
            enabled,
            used,
            limit,
            currency: currency.map(str::to_owned),
        };
        assert_eq!(
            extra_usage_text(&extra(true, 2.5, Some(40.0), Some("USD"))),
            "Extra usage: $2.50 of $40.00 this month"
        );
        assert_eq!(
            extra_usage_text(&extra(true, 2.5, None, Some("CAD"))),
            "Extra usage: 2.50 CAD this month"
        );
        assert_eq!(
            extra_usage_text(&extra(false, 0.0, Some(40.0), Some("CAD"))),
            "Extra usage: off"
        );
        // Switched off after spending: what was spent still shows.
        assert_eq!(
            extra_usage_text(&extra(false, 1.0, None, None)),
            "Extra usage: 1.00 this month"
        );
    }

    #[test]
    fn amounts_and_tokens_are_formatted_for_reading() {
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(0.004), "<$0.01");
        assert_eq!(format_usd(12.345), "$12.35");
        assert_eq!(format_usd(f64::NAN), "$0.00");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_300), "12.3k");
        assert_eq!(format_tokens(2_000_000), "2M");
    }

    #[test]
    fn billing_names_round_trip() {
        for b in Billing::ALL {
            assert_eq!(Billing::parse(b.as_str()), b);
            let json = serde_json::to_string(&b).expect("encode");
            assert_eq!(json, format!("\"{}\"", b.as_str()));
            assert_eq!(serde_json::from_str::<Billing>(&json).expect("decode"), b);
        }
        assert_eq!(Billing::parse("invoice"), Billing::Unknown);
        // A name a newer build wrote, or a value of another type, still reads.
        assert_eq!(
            serde_json::from_str::<Billing>("\"invoice\"").expect("decode"),
            Billing::Unknown
        );
        assert_eq!(
            serde_json::from_str::<Billing>("7").expect("decode"),
            Billing::Unknown
        );
    }

    #[test]
    fn plan_turns_are_list_price_not_metered() {
        // What Claude says of itself.
        let key = claude_billing(Some("ANTHROPIC_API_KEY"), Some("firstParty"), Some("max"));
        assert_eq!(key, Some(Billing::Metered), "a key is metered, plan or not");
        assert_eq!(
            claude_billing(Some("apiKeyHelper"), None, None),
            Some(Billing::Metered)
        );
        assert_eq!(
            claude_billing(Some("none"), Some("bedrock"), None),
            Some(Billing::Metered),
            "another cloud bills it"
        );
        assert_eq!(
            claude_billing(Some("none"), Some("firstParty"), None),
            Some(Billing::Plan)
        );
        assert_eq!(
            claude_billing(None, None, Some("Claude Pro")),
            Some(Billing::Plan)
        );
        assert_eq!(claude_billing(Some("none"), None, None), None);
        assert_eq!(claude_billing(None, Some("firstParty"), None), None);
        assert_eq!(claude_billing(None, None, None), None);
        assert_eq!(claude_billing(Some(" "), None, Some(" ")), None);

        // Then the profile it ran on.
        assert_eq!(
            turn_billing(Some(Billing::Plan), true, true),
            Some(Billing::Free)
        );
        assert_eq!(
            turn_billing(Some(Billing::Plan), false, true),
            Some(Billing::Metered),
            "a model server bills its turns"
        );
        assert_eq!(turn_billing(None, false, true), Some(Billing::Metered));
        assert_eq!(
            turn_billing(Some(Billing::Plan), false, false),
            Some(Billing::Plan)
        );
        assert_eq!(turn_billing(None, false, false), None);

        // The sums keep them apart, and a free turn's reported price is no spend.
        let today = day("2026-11-15");
        let mut plan = row("2026-11-15", "claude", "/a", Some(2.0), 1);
        plan.billing = Billing::Plan;
        let mut metered = row("2026-11-15", "claude", "/a", Some(0.5), 1);
        metered.billing = Billing::Metered;
        let mut free = row("2026-11-15", "claude", "/a", Some(9.0), 1);
        free.billing = Billing::Free;
        let r = report(
            &[plan, metered, free.clone()],
            Period::Day,
            Group::Folder,
            today,
        );
        assert_eq!(r.total.costs.metered, 0.5);
        assert_eq!(r.total.costs.list_price, 2.0);
        assert_eq!(r.total.costs.reported, 0.0);
        assert_eq!(
            (r.total.turns, r.total.cost_turns, r.total.free_turns),
            (3, 2, 1)
        );
        let r = report(&[free], Period::Day, Group::Folder, today);
        assert_eq!(r.total.amount(), Amount::Free);
    }

    /// The selectors drive dropdowns by index: a new variant fails to compile here until its
    /// array is updated.
    #[test]
    fn spend_period_and_group_all_arrays_exhaustive() {
        for (i, p) in Period::ALL.into_iter().enumerate() {
            let expected = match p {
                Period::Day => 0,
                Period::Week => 1,
                Period::Month => 2,
            };
            assert_eq!(i, expected);
        }
        for (i, g) in Group::ALL.into_iter().enumerate() {
            let expected = match g {
                Group::Folder => 0,
                Group::Agent => 1,
                Group::Profile => 2,
                Group::Model => 3,
            };
            assert_eq!(i, expected);
        }
        for (i, b) in Billing::ALL.into_iter().enumerate() {
            let expected = match b {
                Billing::Metered => 0,
                Billing::Plan => 1,
                Billing::Free => 2,
                Billing::Unknown => 3,
            };
            assert_eq!(i, expected);
        }
    }
}
