//! Spending history: every completed turn's cost and tokens, read from the events of the
//! threads still in the store and from `spend_ledger`, where a deleted thread's turns are kept.
//!
//! Live threads are read through `events_turns` ([`Store::ensure_spend_index`]); the query
//! repeats that index's WHERE term with the literal `'turn_completed'`. A deleted thread's turns
//! are folded into the ledger inside the delete itself ([`Store::delete_thread`]), so a total
//! never drops when a thread goes, and a turn is never counted twice: once folded, its events
//! are gone.
//!
//! Claude stored its process's running total as a turn's cost before it stored each turn's own.
//! The two cannot be told apart in the events, so the store remembers where its trusted turns
//! begin ([`SPEND_COST_FROM_KEY`], written once by the store writer when it first starts on a
//! store, and by an in-memory store when it is made): a cost recorded before that is withheld
//! (its turn counts tokens only, and in [`SpendRow::withheld_turns`]), never summed. Until the
//! marker exists no cost is trusted.

use std::collections::HashMap;

use agent_core::event::Usage;
use agent_core::spend::{
    real_money, warning_due, Billing, Budget, BudgetLevel, BudgetPeriod, BudgetWarning, DayKey,
    SpendRow,
};
use rusqlite::params;

use super::{Result, Store};

/// [`Store::meta`] key: the highest event `seq` stored before this build's first start on the
/// store. A turn's cost is trusted only above it.
pub const SPEND_COST_FROM_KEY: &str = "spend_cost_from_seq";

/// The turns the spending history counts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpendData {
    pub rows: Vec<SpendRow>,
}

/// The local calendar day of `at_ms` (epoch milliseconds), from the C library's local time (the
/// same `TZ` and zone file the rest of the desktop uses). A time the library cannot convert
/// falls back to its UTC day.
pub fn local_day(at_ms: i64) -> DayKey {
    let secs = at_ms.div_euclid(1000);
    let Some(t) = libc::time_t::try_from(secs).ok() else {
        return DayKey::at_offset(at_ms, 0);
    };
    // SAFETY: `tm` is plain data that `localtime_r` fills; both pointers are valid for the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let filled = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !filled {
        return DayKey::at_offset(at_ms, 0);
    }
    let (Ok(month), Ok(day)) = (u32::try_from(tm.tm_mon + 1), u32::try_from(tm.tm_mday)) else {
        return DayKey::at_offset(at_ms, 0);
    };
    DayKey::new(tm.tm_year + 1900, month, day).unwrap_or_else(|| DayKey::at_offset(at_ms, 0))
}

/// One completed turn, attributed. `{filter}` narrows the events read; the type term stays a
/// literal so the plan uses `events_turns`.
const TURNS_SQL: &str = "
SELECT e.seq, e.at, e.thread_id,
       COALESCE(pt.driver, apt.driver),
       COALESCE(json_extract(e.envelope_json, '$.event.model'), pt.model, apt.model, ''),
       json_extract(e.envelope_json, '$.event.billing'),
       json_extract(e.envelope_json, '$.event.cost_usd'),
       json_extract(e.envelope_json, '$.event.usage.input_tokens'),
       json_extract(e.envelope_json, '$.event.usage.cached_input_tokens'),
       json_extract(e.envelope_json, '$.event.usage.output_tokens'),
       json_extract(e.envelope_json, '$.event.usage.reasoning_tokens'),
       COALESCE(rt.cwd, t.cwd, ''),
       COALESCE(w.profile_id, m.value, ''),
       w.id IS NOT NULL
FROM events e
JOIN threads t ON t.id = e.thread_id
LEFT JOIN provider_threads pt ON pt.id = e.provider_thread_id
LEFT JOIN provider_threads apt ON apt.id = t.active_provider_thread
LEFT JOIN delegation_workers w ON w.backing_thread_id = e.thread_id
LEFT JOIN delegation_roots r ON r.id = w.root_id
LEFT JOIN threads rt ON rt.id = r.root_thread_id
LEFT JOIN meta m ON m.key = 'thread_profile:' || e.thread_id
WHERE json_extract(e.envelope_json, '$.event.type') = 'turn_completed' AND {filter}
ORDER BY e.at, e.seq";

/// The live threads' turns from `?1` (epoch milliseconds) on.
pub(crate) fn live_turns_sql() -> String {
    TURNS_SQL.replace("{filter}", "e.at >= ?1")
}

/// A turn as read, before the day is known.
struct Turn {
    seq: i64,
    at: i64,
    thread: String,
    driver: Option<String>,
    model: String,
    billing: Option<String>,
    cost: Option<f64>,
    tokens: [Option<i64>; 4],
    folder: String,
    profile_id: String,
    delegated: bool,
}

fn turn(r: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        seq: r.get(0)?,
        at: r.get(1)?,
        thread: r.get(2)?,
        driver: r.get(3)?,
        model: r.get(4)?,
        billing: r.get(5)?,
        cost: r.get(6)?,
        tokens: [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?],
        folder: r.get(11)?,
        profile_id: r.get(12)?,
        delegated: r.get(13)?,
    })
}

/// A worker's backing thread as its delegation rows named it: its root thread's folder and its
/// own profile. Read before a delete removes those rows.
type Delegated = HashMap<String, (String, String)>;

fn count(value: Option<i64>) -> u64 {
    value.and_then(|v| u64::try_from(v).ok()).unwrap_or(0)
}

impl Store {
    /// Records where trusted turn costs begin ([`SPEND_COST_FROM_KEY`]), once: the store writer
    /// calls it first thing, and the Spending dialog's first write again in case that failed.
    /// When the key exists this is one read and takes no write lock.
    pub fn ensure_spend_cost_marker(&self) -> Result<()> {
        if self.meta(SPEND_COST_FROM_KEY)?.is_some() {
            return Ok(());
        }
        let tx = self.write_txn()?;
        let max: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
        self.conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params![SPEND_COST_FROM_KEY, max.to_string()],
        )?;
        tx.commit()
    }

    /// The `seq` above which a turn's cost is trusted. With no key yet (the writer could not
    /// record it) or an unreadable one, none is: a running total is never summed.
    fn spend_cost_from(&self) -> Result<i64> {
        Ok(match self.meta(SPEND_COST_FROM_KEY)? {
            None => {
                tracing::debug!("no spending marker yet; stored costs are withheld");
                i64::MAX
            }
            Some(v) => v.trim().parse().unwrap_or_else(|_| {
                tracing::warn!("unreadable spending marker; earlier costs are withheld");
                i64::MAX
            }),
        })
    }

    fn turns(&self, sql: &str, param: &dyn rusqlite::ToSql) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([param], turn)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every completed turn from `since_ms` on, of the threads in the store and (from the day
    /// `since_ms` falls on) of deleted ones, one [`SpendRow`] per live turn and per ledger row.
    /// `day_of` names a time's local day ([`local_day`]; a test passes a fixed offset). A
    /// delegated worker's turns are counted under its root thread's folder. A read only: make
    /// sure of the index first ([`Store::ensure_spend_index`], a write, through the writer).
    ///
    /// The marker, the live turns and the ledger are read in one read transaction (one WAL
    /// snapshot), so a delete committed meanwhile is seen whole or not at all: never both as
    /// events and as its ledger rows.
    pub fn spend_rows(&self, since_ms: i64, day_of: &dyn Fn(i64) -> DayKey) -> Result<SpendData> {
        let snapshot = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let data = self.spend_rows_in(since_ms, day_of)?;
        snapshot.commit()?;
        Ok(data)
    }

    fn spend_rows_in(&self, since_ms: i64, day_of: &dyn Fn(i64) -> DayKey) -> Result<SpendData> {
        let cost_from = self.spend_cost_from()?;
        let mut data = SpendData::default();
        for t in self.turns(&live_turns_sql(), &since_ms)? {
            if let Some(row) = spend_row(t, cost_from, day_of, None) {
                data.rows.push(row);
            }
        }
        #[cfg(test)]
        if let Some(hook) = self.between_spend_reads.borrow_mut().take() {
            hook();
        }
        let since_day = day_of(since_ms).to_string();
        let mut stmt = self.conn.prepare(
            "SELECT day, driver, profile_id, model, folder, billing, delegated, cost_usd,
                    cost_turns, turns, input_tokens, cached_input_tokens, output_tokens,
                    reasoning_tokens
             FROM spend_ledger WHERE day >= ?1",
        )?;
        let ledger = stmt.query_map(params![since_day], |r| {
            Ok((
                r.get::<_, String>(0)?,
                SpendRow {
                    day: DayKey::at_offset(0, 0),
                    driver: r.get(1)?,
                    profile_id: r.get(2)?,
                    model: r.get(3)?,
                    folder: r.get(4)?,
                    billing: Billing::parse(&r.get::<_, String>(5)?),
                    delegated: r.get(6)?,
                    cost_usd: r.get(7)?,
                    cost_turns: count(r.get(8)?),
                    turns: count(r.get(9)?),
                    // The ledger keeps no count of them.
                    withheld_turns: 0,
                    tokens: Usage {
                        input_tokens: count(r.get(10)?),
                        cached_input_tokens: count(r.get(11)?),
                        output_tokens: count(r.get(12)?),
                        reasoning_tokens: count(r.get(13)?),
                    },
                },
            ))
        })?;
        for entry in ledger {
            let (day, mut row) = entry?;
            match DayKey::parse(&day) {
                Some(d) => {
                    row.day = d;
                    data.rows.push(row);
                }
                None => tracing::warn!(day, "skipping a spending ledger row with no real day"),
            }
        }
        Ok(data)
    }

    /// The budget warnings due as of `now_ms`, each given once a period: what today and this
    /// month spent in real money (metered turns, deleted threads' included, plus this month's
    /// `extra_usd`, a plan's extra usage in US dollars), against `budget`, above the level
    /// already warned of in that period (kept in `meta` under [`BudgetPeriod::meta_key`], so a
    /// restart does not warn again). The levels due are recorded in the same transaction that
    /// read them, so two checks never both warn. Keys of past periods are removed. A write:
    /// run it through the store writer.
    pub fn spend_budget_check(
        &self,
        now_ms: i64,
        day_of: &dyn Fn(i64) -> DayKey,
        budget: &Budget,
        extra_usd: Option<f64>,
    ) -> Result<Vec<BudgetWarning>> {
        if budget.limit(BudgetPeriod::Day).is_none() && budget.limit(BudgetPeriod::Month).is_none()
        {
            return Ok(Vec::new());
        }
        self.atomically(|s| {
            let today = day_of(now_ms);
            // A month's first day is at most 31 days back; the sum keeps only its own days.
            let rows = s.spend_rows(now_ms - 32 * 86_400_000, day_of)?.rows;
            let keys: Vec<String> = BudgetPeriod::ALL
                .iter()
                .map(|p| p.meta_key(today))
                .collect();
            s.conn.execute(
                "DELETE FROM meta WHERE key LIKE 'spend_warned:%' AND key NOT IN (?1, ?2)",
                params![keys[0], keys[1]],
            )?;
            let mut due = Vec::new();
            for (period, key) in BudgetPeriod::ALL.into_iter().zip(&keys) {
                let mut spent = real_money(&rows, period.first_day(today));
                if period == BudgetPeriod::Month {
                    spent += extra_usd
                        .filter(|e| e.is_finite() && *e > 0.0)
                        .unwrap_or(0.0);
                }
                let warned = s.meta(key)?.as_deref().and_then(BudgetLevel::parse);
                if let Some(warning) = warning_due(period, spent, budget, warned) {
                    s.set_meta(key, warning.level.as_str())?;
                    due.push(warning);
                }
            }
            Ok(due)
        })
    }

    /// The backing threads of the delegation tree `thread` belongs to, with their root thread's
    /// folder and their own profile: read before a delete releases those rows.
    pub(super) fn spend_delegated_before_delete(&self, thread: &str) -> Result<Delegated> {
        let mut stmt = self.conn.prepare(
            "SELECT w.backing_thread_id, rt.cwd, w.profile_id
             FROM delegation_workers w
             JOIN delegation_roots r ON r.id = w.root_id
             JOIN threads rt ON rt.id = r.root_thread_id
             WHERE w.backing_thread_id IS NOT NULL AND w.root_id IN (
                 SELECT id FROM delegation_roots WHERE root_thread_id = ?1
                 UNION SELECT root_id FROM delegation_workers WHERE backing_thread_id = ?1)",
        )?;
        let rows = stmt.query_map(params![thread], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds `threads`' completed turns to `spend_ledger`, by their own local day, inside the
    /// caller's delete transaction (before their rows and per-thread metadata go). `delegated`
    /// is what [`Store::spend_delegated_before_delete`] read.
    pub(super) fn fold_spend_for_delete(
        &self,
        threads: &[&str],
        delegated: &Delegated,
        day_of: &dyn Fn(i64) -> DayKey,
    ) -> Result<()> {
        let cost_from = self.spend_cost_from()?;
        let ids = serde_json::to_string(threads)?;
        let sql = TURNS_SQL.replace(
            "{filter}",
            "e.thread_id IN (SELECT value FROM json_each(?1))",
        );
        let mut stmt = self.conn.prepare(
            "INSERT INTO spend_ledger (day, driver, profile_id, model, folder, billing, delegated,
                 cost_usd, cost_turns, turns, input_tokens, cached_input_tokens, output_tokens,
                 reasoning_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT (day, driver, profile_id, model, folder, billing, delegated) DO UPDATE SET
                 cost_usd = cost_usd + excluded.cost_usd,
                 cost_turns = cost_turns + excluded.cost_turns,
                 turns = turns + excluded.turns,
                 input_tokens = input_tokens + excluded.input_tokens,
                 cached_input_tokens = cached_input_tokens + excluded.cached_input_tokens,
                 output_tokens = output_tokens + excluded.output_tokens,
                 reasoning_tokens = reasoning_tokens + excluded.reasoning_tokens",
        )?;
        let known: std::collections::HashSet<String> = self
            .conn
            .prepare("SELECT key FROM drivers")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for t in self.turns(&sql, &ids)? {
            let Some(row) = spend_row(t, cost_from, day_of, Some(delegated)) else {
                continue;
            };
            if !known.contains(&row.driver) {
                tracing::warn!(
                    driver = row.driver,
                    "not keeping the turns of an unknown agent"
                );
                continue;
            }
            let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
            stmt.execute(params![
                row.day.to_string(),
                row.driver,
                row.profile_id,
                row.model,
                row.folder,
                row.billing.as_str(),
                row.delegated,
                row.cost_usd,
                n(row.cost_turns),
                n(row.turns),
                n(row.tokens.input_tokens),
                n(row.tokens.cached_input_tokens),
                n(row.tokens.output_tokens),
                n(row.tokens.reasoning_tokens),
            ])?;
        }
        Ok(())
    }
}

/// The row a turn counts as; `None` when it names no agent. `delegated`, for a fold, overrides
/// the attribution the joins found (the delegation rows may be gone by then).
fn spend_row(
    t: Turn,
    cost_from: i64,
    day_of: &dyn Fn(i64) -> DayKey,
    delegated: Option<&Delegated>,
) -> Option<SpendRow> {
    let Some(driver) = t.driver else {
        tracing::debug!(
            thread = t.thread,
            "a turn whose agent is unknown is not counted"
        );
        return None;
    };
    let mut cost = t.cost.filter(|c| c.is_finite() && *c >= 0.0);
    let mut withheld_turns = 0;
    if cost.is_some() && t.seq <= cost_from {
        withheld_turns = 1;
        cost = None;
    }
    let (folder, profile_id, is_delegated) = match delegated.and_then(|d| d.get(&t.thread)) {
        Some((folder, profile)) => (folder.clone(), profile.clone(), true),
        None => (t.folder, t.profile_id.trim().to_owned(), t.delegated),
    };
    let [input, cached, output, reasoning] = t.tokens.map(count);
    Some(SpendRow {
        day: day_of(t.at),
        driver,
        profile_id,
        model: t.model,
        folder,
        billing: t
            .billing
            .as_deref()
            .map(Billing::parse)
            .unwrap_or(Billing::Unknown),
        delegated: is_delegated,
        cost_usd: cost.unwrap_or(0.0),
        cost_turns: u64::from(cost.is_some()),
        turns: 1,
        withheld_turns,
        tokens: Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            reasoning_tokens: reasoning,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{Envelope, Event, TurnState};

    /// 2026-11-15 12:00 UTC.
    const NOON: i64 = 1_794_744_000_000;
    const DAY_MS: i64 = 86_400_000;

    fn utc(at: i64) -> DayKey {
        DayKey::at_offset(at, 0)
    }

    fn day(text: &str) -> DayKey {
        DayKey::parse(text).expect(text)
    }

    fn store() -> Store {
        let s = Store::open_in_memory().expect("open");
        s.ensure_spend_index().expect("index");
        s
    }

    /// A completed turn on `pt`, stored at `at`; its `seq`.
    fn turn(s: &Store, thread: &str, pt: &str, cost: Option<f64>, tokens: u64, at: i64) -> i64 {
        let env = Envelope::new(Event::TurnCompleted {
            state: TurnState::Completed,
            usage: Some(Usage {
                input_tokens: tokens,
                output_tokens: tokens,
                cached_input_tokens: 0,
                reasoning_tokens: 0,
            }),
            cost_usd: cost,
            error: None,
            model: None,
            billing: None,
        });
        let seq = s.append_event(thread, Some(pt), &env).expect("append");
        s.conn
            .execute("UPDATE events SET at = ?1 WHERE seq = ?2", params![at, seq])
            .expect("date it");
        seq
    }

    fn thread(s: &Store, cwd: &str, driver: &str, model: &str) -> (String, String) {
        let t = s.create_thread(cwd, None).expect("thread");
        let pt = s.add_provider_thread(&t, driver, model).expect("provider");
        (t, pt)
    }

    fn withheld(data: &SpendData) -> u64 {
        data.rows.iter().map(|r| r.withheld_turns).sum()
    }

    fn total_cost(data: &SpendData) -> f64 {
        data.rows.iter().map(|r| r.cost_usd).sum()
    }

    fn ledger_rows(s: &Store) -> i64 {
        s.conn
            .query_row("SELECT COUNT(*) FROM spend_ledger", [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn spend_rows_attribute_each_turn_to_its_agent_model_folder_and_profile() {
        let s = store();
        let (a, apt) = thread(&s, "/a", "claude", "opus");
        s.set_thread_profile(&a, Some("work")).expect("profile");
        let (b, bpt) = thread(&s, "/b", "codex", "gpt-5");
        turn(&s, &a, &apt, Some(0.5), 10, NOON);
        turn(&s, &b, &bpt, None, 1000, NOON - DAY_MS);
        // Before the window: not read.
        turn(&s, &a, &apt, Some(9.0), 10, NOON - 40 * DAY_MS);
        let data = s.spend_rows(NOON - 30 * DAY_MS, &utc).expect("rows");
        assert_eq!(withheld(&data), 0);
        let got: Vec<(String, String, String, String, f64, u64, u64)> = data
            .rows
            .iter()
            .map(|r| {
                (
                    r.day.to_string(),
                    r.driver.clone(),
                    r.model.clone(),
                    format!("{}|{}", r.folder, r.profile_id),
                    r.cost_usd,
                    r.cost_turns,
                    r.tokens.input_tokens + r.tokens.output_tokens,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "2026-11-14".into(),
                    "codex".into(),
                    "gpt-5".into(),
                    "/b|".into(),
                    0.0,
                    0,
                    2000
                ),
                (
                    "2026-11-15".into(),
                    "claude".into(),
                    "opus".into(),
                    "/a|work".into(),
                    0.5,
                    1,
                    20
                ),
            ]
        );
        assert!(data.rows.iter().all(|r| r.billing == Billing::Unknown));
    }

    /// A turn stamped with its model and billing is read with them (its own model over the
    /// provider thread's current one); one stored before the stamp reads as unknown billing.
    #[test]
    fn a_stamped_turn_keeps_its_model_and_billing_and_an_old_one_reads_unknown() {
        let s = store();
        let (t, pt) = thread(&s, "/a", "claude", "sonnet");
        turn(&s, &t, &pt, Some(0.25), 1, NOON);
        for (model, billing) in [("opus", Billing::Metered), ("haiku", Billing::Plan)] {
            let env = Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                usage: None,
                cost_usd: Some(1.0),
                error: None,
                model: Some(model.into()),
                billing: Some(billing),
            });
            s.append_event(&t, Some(&pt), &env).expect("append");
        }
        let data = s.spend_rows(0, &utc).expect("rows");
        let got: Vec<(String, Billing)> = data
            .rows
            .iter()
            .map(|r| (r.model.clone(), r.billing))
            .collect();
        assert_eq!(
            got,
            [
                ("sonnet".into(), Billing::Unknown),
                ("opus".into(), Billing::Metered),
                ("haiku".into(), Billing::Plan),
            ]
        );
        // Deleted, they keep it in the ledger.
        s.delete_thread(&t).expect("delete");
        let mut kept: Vec<(String, Billing)> = s
            .spend_rows(0, &utc)
            .expect("rows")
            .rows
            .iter()
            .map(|r| (r.model.clone(), r.billing))
            .collect();
        kept.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            kept,
            [
                ("haiku".into(), Billing::Plan),
                ("opus".into(), Billing::Metered),
                ("sonnet".into(), Billing::Unknown),
            ]
        );
    }

    fn billed(s: &Store, thread: &str, pt: &str, cost: f64, billing: Billing, at: i64) {
        let env = Envelope::new(Event::TurnCompleted {
            state: TurnState::Completed,
            usage: None,
            cost_usd: Some(cost),
            error: None,
            model: None,
            billing: Some(billing),
        });
        let seq = s.append_event(thread, Some(pt), &env).expect("append");
        s.conn
            .execute("UPDATE events SET at = ?1 WHERE seq = ?2", params![at, seq])
            .expect("date it");
    }

    /// Each level is warned of once a period, across restarts of the store; only metered
    /// turns and the plan's extra usage count, and a new day warns again.
    #[test]
    fn budget_warning_once_per_period_across_restarts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("threads.db");
        let budget = Budget {
            daily: Some(10.0),
            monthly: Some(100.0),
        };
        let levels = |w: Vec<BudgetWarning>| -> Vec<(BudgetPeriod, BudgetLevel)> {
            w.into_iter().map(|w| (w.period, w.level)).collect()
        };
        let (t, pt) = {
            let s = Store::open(&path).expect("open");
            s.ensure_spend_cost_marker().expect("marker");
            s.ensure_spend_index().expect("index");
            let (t, pt) = thread(&s, "/a", "claude", "opus");
            // List price and unknown billing are never real money.
            billed(&s, &t, &pt, 500.0, Billing::Plan, NOON);
            billed(&s, &t, &pt, 500.0, Billing::Unknown, NOON);
            billed(&s, &t, &pt, 8.5, Billing::Metered, NOON);
            let first = s
                .spend_budget_check(NOON, &utc, &budget, None)
                .expect("check");
            assert_eq!(levels(first), [(BudgetPeriod::Day, BudgetLevel::Near)]);
            assert!(s
                .spend_budget_check(NOON, &utc, &budget, None)
                .expect("again")
                .is_empty());
            (t, pt)
        };
        // A new start of the app: nothing is warned of again.
        let s = Store::open(&path).expect("reopen");
        assert!(s
            .spend_budget_check(NOON + 1, &utc, &budget, None)
            .expect("after a restart")
            .is_empty());
        billed(&s, &t, &pt, 2.0, Billing::Metered, NOON + 2);
        let over = s
            .spend_budget_check(NOON + 2, &utc, &budget, None)
            .expect("over");
        assert_eq!(levels(over), [(BudgetPeriod::Day, BudgetLevel::Over)]);
        // The plan's extra usage counts toward the month (10.5 + 75 is past 80% of 100).
        let month = s
            .spend_budget_check(NOON + 3, &utc, &budget, Some(75.0))
            .expect("month");
        assert_eq!(levels(month), [(BudgetPeriod::Month, BudgetLevel::Near)]);
        // The next day warns again, from its own spending, and the old day's key goes.
        let next = NOON + DAY_MS;
        billed(&s, &t, &pt, 9.0, Billing::Metered, next);
        let tomorrow = s
            .spend_budget_check(next, &utc, &budget, Some(75.0))
            .expect("next");
        assert_eq!(levels(tomorrow), [(BudgetPeriod::Day, BudgetLevel::Near)]);
        assert_eq!(s.meta("spend_warned:day:2026-11-15").expect("read"), None);
        // With no budget, nothing is read or written.
        assert!(s
            .spend_budget_check(next, &utc, &Budget::default(), Some(1e6))
            .expect("none")
            .is_empty());
    }

    /// The adapter stores each turn's own cost (the running total 0.0173, 0.0259, 0.0553 becomes
    /// 0.0173, 0.0086, 0.0294): summing them gives the process's total once, never again.
    #[test]
    fn per_turn_costs_sum_to_the_process_total_once() {
        let s = store();
        let (t, pt) = thread(&s, "/a", "claude", "sonnet");
        for (i, cost) in [0.0173, 0.0086, 0.0294].into_iter().enumerate() {
            turn(&s, &t, &pt, Some(cost), 1, NOON + i as i64);
        }
        let data = s.spend_rows(NOON - DAY_MS, &utc).expect("rows");
        assert_eq!(data.rows.len(), 3);
        assert!((total_cost(&data) - 0.0553).abs() < 1e-9);
    }

    /// Costs stored before the store first ran with per-turn costs were the process's running
    /// total: their turns count tokens only, and are counted as withheld.
    #[test]
    fn costs_stored_before_the_marker_are_withheld_never_summed() {
        let s = store();
        // A store from before per-turn costs: no marker yet.
        s.delete_meta(SPEND_COST_FROM_KEY).expect("unmark");
        let (t, pt) = thread(&s, "/a", "claude", "sonnet");
        turn(&s, &t, &pt, Some(0.0173), 1, NOON);
        turn(&s, &t, &pt, Some(0.0259), 1, NOON + 1);
        // Until the marker is recorded, no cost is trusted.
        let data = s.spend_rows(NOON - DAY_MS, &utc).expect("rows");
        assert_eq!(withheld(&data), 2);
        assert_eq!(total_cost(&data), 0.0);
        s.ensure_spend_cost_marker().expect("marker");
        let marked = s.meta(SPEND_COST_FROM_KEY).expect("read");
        s.ensure_spend_cost_marker().expect("again");
        assert_eq!(
            s.meta(SPEND_COST_FROM_KEY).expect("read"),
            marked,
            "written once"
        );
        turn(&s, &t, &pt, Some(0.03), 1, NOON + 2);
        let data = s.spend_rows(NOON - DAY_MS, &utc).expect("rows");
        assert_eq!(withheld(&data), 2);
        assert_eq!(data.rows.len(), 3, "their tokens still count");
        assert_eq!(data.rows.iter().map(|r| r.cost_turns).sum::<u64>(), 1);
        assert!((total_cost(&data) - 0.03).abs() < 1e-12);

        // An unreadable marker trusts no cost.
        s.set_meta(SPEND_COST_FROM_KEY, "garbage").expect("spoil");
        let data = s.spend_rows(NOON - DAY_MS, &utc).expect("rows");
        assert_eq!(withheld(&data), 3);
        assert_eq!(total_cost(&data), 0.0);
    }

    #[test]
    fn deleted_thread_keeps_its_cost_in_the_ledger() {
        let s = store();
        let (gone, gpt) = thread(&s, "/a", "claude", "opus");
        s.set_thread_profile(&gone, Some("work")).expect("profile");
        let (kept, kpt) = thread(&s, "/a", "claude", "opus");
        turn(&s, &gone, &gpt, Some(1.25), 100, NOON);
        turn(&s, &gone, &gpt, Some(0.75), 100, NOON + 1);
        turn(&s, &gone, &gpt, None, 50, NOON - DAY_MS);
        turn(&s, &kept, &kpt, Some(2.0), 10, NOON);
        let before = s.spend_rows(NOON - 30 * DAY_MS, &utc).expect("rows");

        // The fold uses the local day, as the reads of live turns do.
        s.delete_thread(&gone).expect("delete");
        let after = s.spend_rows(NOON - 30 * DAY_MS, &local_day).expect("rows");
        assert!((total_cost(&after) - total_cost(&before)).abs() < 1e-12);
        assert!((total_cost(&before) - 4.0).abs() < 1e-12);
        let turns = |d: &SpendData| d.rows.iter().map(|r| r.turns).sum::<u64>();
        assert_eq!(turns(&after), turns(&before));
        assert_eq!(
            after.rows.iter().map(|r| r.cost_turns).sum::<u64>(),
            3,
            "the turn without a cost stays not reported"
        );
        let tokens = |d: &SpendData| {
            d.rows
                .iter()
                .map(|r| r.tokens.input_tokens + r.tokens.output_tokens)
                .sum::<u64>()
        };
        assert_eq!(tokens(&after), tokens(&before));
        // One ledger row per day and attribution, the profile kept from the thread's metadata.
        assert_eq!(ledger_rows(&s), 2);
        let folded: Vec<&SpendRow> = after
            .rows
            .iter()
            .filter(|r| r.profile_id == "work")
            .collect();
        assert_eq!(folded.len(), 2);
        assert!(folded.iter().all(|r| r.folder == "/a" && r.model == "opus"));
        let noon_day = folded
            .iter()
            .find(|r| r.day == local_day(NOON))
            .expect("the turns' own day");
        assert_eq!((noon_day.turns, noon_day.cost_turns), (2, 2));
        assert!((noon_day.cost_usd - 2.0).abs() < 1e-12);

        // Deleting another thread with the same attribution adds to the same row.
        let (again, apt) = thread(&s, "/a", "claude", "opus");
        s.set_thread_profile(&again, Some("work")).expect("profile");
        turn(&s, &again, &apt, Some(0.5), 1, NOON + 2);
        s.delete_thread(&again).expect("delete");
        assert_eq!(ledger_rows(&s), 2);
        let after = s.spend_rows(NOON - 30 * DAY_MS, &local_day).expect("rows");
        assert!((total_cost(&after) - 4.5).abs() < 1e-12);

        // A ledger row is read from the window's first day.
        let late = s.spend_rows(NOON + DAY_MS, &local_day).expect("rows");
        assert!(late.rows.is_empty());
    }

    /// A delete committed by another connection between the read of the live turns and the
    /// read of the ledger is not seen by either: the reads share one snapshot.
    #[test]
    fn a_delete_between_the_reads_is_never_counted_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("threads.db");
        let s = Store::open(&path).expect("open");
        s.ensure_spend_cost_marker().expect("marker");
        s.ensure_spend_index().expect("index");
        let (t, pt) = thread(&s, "/a", "claude", "opus");
        turn(&s, &t, &pt, Some(1.0), 1, NOON);
        let other = path.clone();
        *s.between_spend_reads.borrow_mut() = Some(Box::new(move || {
            let writer = Store::open(&other).expect("second connection");
            writer.delete_thread(&t).expect("delete meanwhile");
            let ledger: i64 = writer
                .conn
                .query_row("SELECT COUNT(*) FROM spend_ledger", [], |r| r.get(0))
                .expect("count");
            assert_eq!(ledger, 1, "the delete committed its ledger row");
        }));
        let data = s.spend_rows(NOON - DAY_MS, &local_day).expect("rows");
        assert!(s.between_spend_reads.borrow().is_none(), "the hook ran");
        assert_eq!(data.rows.len(), 1, "{:?}", data.rows);
        assert!((total_cost(&data) - 1.0).abs() < 1e-12);
        // Read again, the turn is the ledger's alone.
        let data = s.spend_rows(NOON - DAY_MS, &local_day).expect("rows");
        assert_eq!(data.rows.len(), 1);
        assert!((total_cost(&data) - 1.0).abs() < 1e-12);
    }

    /// A file store no writer has marked trusts no cost until it is marked.
    #[test]
    fn a_store_without_its_marker_trusts_no_cost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Store::open(&dir.path().join("threads.db")).expect("open");
        assert_eq!(s.meta(SPEND_COST_FROM_KEY).expect("read"), None);
        let (t, pt) = thread(&s, "/a", "claude", "opus");
        turn(&s, &t, &pt, Some(1.0), 1, NOON);
        let data = s.spend_rows(NOON - DAY_MS, &utc).expect("rows");
        assert_eq!((total_cost(&data), withheld(&data)), (0.0, 1));
        // Nor does a delete fold one into the ledger.
        s.delete_thread(&t).expect("delete");
        let data = s.spend_rows(NOON - DAY_MS, &utc).expect("rows");
        assert_eq!(total_cost(&data), 0.0);
        assert_eq!(data.rows.iter().map(|r| r.turns).sum::<u64>(), 1);
    }

    #[test]
    fn a_blank_thread_deleted_leaves_no_ledger_row() {
        let s = store();
        let (t, _) = thread(&s, "/a", "claude", "opus");
        assert!(s.delete_if_blank(&t).expect("delete"));
        assert_eq!(ledger_rows(&s), 0);
    }

    fn delegation_tree(s: &Store, root: &str, backing: &str) {
        s.conn
            .execute_batch(&format!(
                "BEGIN;
                 INSERT INTO delegation_roots (id, root_thread_id, root_worker_id, task_state,
                     policy_json, continuation_budget, created_at, updated_at)
                     VALUES ('r1', '{root}', 'w_root', 'active', '{{}}', 3, 1, 2);
                 INSERT INTO delegation_workers (id, root_id, parent_id, origin, depth, name,
                     driver, profile_id, launch_json, backing_thread_id, state, workspace_json,
                     delegation_status, created_at, updated_at)
                     VALUES ('w_root', 'r1', NULL, 'root', 0, 'root', 'claude', 'p', '{{}}', NULL,
                             'running', '{{}}', 'available', 1, 2),
                            ('w_kid', 'r1', 'w_root', 'supervised', 1, 'kid', 'codex', 'kidprof',
                             '{{}}', '{backing}', 'closed', '{{}}', 'available', 1, 2);
                 COMMIT;"
            ))
            .expect("a supervised child");
    }

    #[test]
    fn worker_cost_rolls_up_to_its_root_thread() {
        let s = store();
        let (root, rpt) = thread(&s, "/project", "claude", "opus");
        // A worker's hidden thread may sit in a worktree of its own: it counts under its root.
        let (backing, bpt) = thread(&s, "/elsewhere/worktree", "claude", "sonnet");
        delegation_tree(&s, &root, &backing);
        turn(&s, &root, &rpt, Some(1.0), 1, NOON);
        turn(&s, &backing, &bpt, Some(0.25), 1, NOON + 1);
        let live = s.spend_rows(NOON - DAY_MS, &local_day).expect("rows");
        let worker = live.rows.iter().find(|r| r.delegated).expect("worker row");
        assert_eq!(worker.folder, "/project");
        assert_eq!(worker.profile_id, "kidprof");
        assert_eq!(worker.model, "sonnet");
        assert_eq!(live.rows.iter().filter(|r| r.delegated).count(), 1);

        // Deleting the tree keeps the worker's turns the same way:
        // `delegation_typed::tests::a_deleted_root_keeps_its_workers_cost_under_its_folder`.
    }

    #[test]
    fn the_turn_query_reads_through_the_spend_index() {
        let s = store();
        let plan: Vec<String> = s
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", live_turns_sql()))
            .expect("prepare")
            .query_map([0_i64], |r| r.get::<_, String>(3))
            .expect("plan")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        assert!(
            plan.iter()
                .any(|step| step == "SEARCH e USING INDEX events_turns (at>?)"),
            "{plan:?}"
        );
    }

    #[test]
    fn a_turn_of_an_unknown_agent_is_skipped_and_a_bad_ledger_day_is_ignored() {
        let s = store();
        let t = s.create_thread("/a", None).expect("thread");
        let env = Envelope::new(Event::TurnCompleted {
            state: TurnState::Completed,
            usage: None,
            cost_usd: Some(1.0),
            error: None,
            model: None,
            billing: None,
        });
        s.append_event(&t, None, &env).expect("append");
        s.conn
            .execute(
                "INSERT INTO spend_ledger (day, driver, billing, cost_usd, cost_turns, turns)
                 VALUES ('not-a-day', 'claude', 'metered', 5, 1, 1)",
                [],
            )
            .expect("a bad row");
        let data = s.spend_rows(0, &utc).expect("rows");
        assert!(data.rows.is_empty(), "{:?}", data.rows);
        s.delete_thread(&t).expect("delete");
        assert_eq!(ledger_rows(&s), 1, "nothing was folded for it");
    }

    #[test]
    fn local_day_names_a_real_day() {
        let d = local_day(NOON);
        assert!(d == day("2026-11-15") || d == day("2026-11-14") || d == day("2026-11-16"));
        let _ = local_day(i64::MAX);
    }
}
