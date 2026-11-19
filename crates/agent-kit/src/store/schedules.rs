//! CRUD and run-state updates for scheduled prompts (`schedules` table, schema v11).
//!
//! Prompts are always redacted before touching SQLite, `Bypass` mode is refused both in Rust and
//! by schema `CHECK`, and `agy` new-thread schedules require `worktree = 1`. Writes go through
//! [`crate::store_writer::StoreHandle`] on the single writer thread; reads go through `store_job`.

use agent_core::adapter::{Driver, Mode};
use agent_core::schedule::{
    validate_ends, validate_prompt, validate_rule, validate_target, Ends, LastOutcome,
    MissedPolicy, PausedReason, Rule, Target,
};
use rusqlite::{params, OptionalExtension};

use super::{redact, Result, Store, StoreError};

/// One stored schedule row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleRecord {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub target: Target,
    pub prompt: String,
    pub rule: Rule,
    pub missed_policy: MissedPolicy,
    pub ends: Ends,
    pub next_due: Option<i64>,
    pub last_due: Option<i64>,
    pub last_outcome: Option<LastOutcome>,
    pub run_count: u32,
    pub unattended_runs: u32,
    pub paused_reason: Option<PausedReason>,
}

/// Parameters to insert a new schedule row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSchedule {
    pub target: Target,
    pub prompt: String,
    pub rule: Rule,
    pub missed_policy: MissedPolicy,
    pub ends: Ends,
    pub next_due: Option<i64>,
}

/// Atomic state transition recorded after checking or firing a schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleRunUpdate {
    pub id: String,
    pub next_due: Option<i64>,
    pub last_due: Option<i64>,
    pub last_outcome: Option<LastOutcome>,
    pub increment_run_count: bool,
    pub increment_unattended: bool,
    pub paused_reason: Option<PausedReason>,
}

fn driver_key(driver: Driver) -> &'static str {
    driver.info().key
}

fn parse_driver(raw: &str) -> Option<Driver> {
    Driver::ALL.into_iter().find(|d| d.info().key == raw)
}

fn mode_key(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Ask => Some("ask"),
        Mode::AcceptEdits => Some("accept_edits"),
        Mode::Plan => Some("plan"),
        Mode::Bypass => None,
    }
}

fn parse_mode(raw: &str) -> Option<Mode> {
    match raw {
        "ask" => Some(Mode::Ask),
        "accept_edits" => Some(Mode::AcceptEdits),
        "plan" => Some(Mode::Plan),
        _ => None,
    }
}

type TargetColumns<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    i64,
);

fn target_columns(target: &Target) -> Result<TargetColumns<'_>> {
    validate_target(target).map_err(|why| StoreError::Failed(why.to_owned()))?;
    match target {
        Target::ExistingThread { thread_id } => {
            Ok((Some(thread_id.as_str()), None, None, None, None, None, 0))
        }
        Target::NewThread {
            folder,
            driver,
            profile_id,
            mode,
            model,
            worktree,
        } => {
            let mode_str = mode_key(*mode)
                .ok_or_else(|| StoreError::Failed("Bypass mode cannot be scheduled".to_owned()))?;
            Ok((
                None,
                Some(folder.as_str()),
                Some(driver_key(*driver)),
                profile_id.as_deref(),
                Some(mode_str),
                model.as_deref(),
                i64::from(*worktree),
            ))
        }
    }
}

const SELECT_COLUMNS: &str = "id, created_at, updated_at, thread_id, target_folder, driver,
    profile_id, mode, model, worktree, prompt, rule, missed_policy, ends, next_due, last_due,
    last_outcome, run_count, unattended_runs, paused_reason";

fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<ScheduleRecord>> {
    let id: String = r.get(0)?;
    let created_at: i64 = r.get(1)?;
    let updated_at: i64 = r.get(2)?;
    let thread_id: Option<String> = r.get(3)?;
    let target_folder: Option<String> = r.get(4)?;
    let driver: Option<String> = r.get(5)?;
    let profile_id: Option<String> = r.get(6)?;
    let mode: Option<String> = r.get(7)?;
    let model: Option<String> = r.get(8)?;
    let worktree: i64 = r.get(9)?;
    let prompt: String = r.get(10)?;
    let rule_raw: String = r.get(11)?;
    let missed_raw: String = r.get(12)?;
    let ends_raw: Option<String> = r.get(13)?;
    let next_due: Option<i64> = r.get(14)?;
    let last_due: Option<i64> = r.get(15)?;
    let last_outcome_raw: Option<String> = r.get(16)?;
    let run_count: i64 = r.get(17)?;
    let unattended_runs: i64 = r.get(18)?;
    let paused_raw: Option<String> = r.get(19)?;

    let target = match (thread_id, target_folder) {
        (Some(thread_id), None) => Target::ExistingThread { thread_id },
        (None, Some(folder)) => {
            let Some(driver) = driver.as_deref().and_then(parse_driver) else {
                tracing::warn!(%id, "skipped schedule with unknown driver");
                return Ok(None);
            };
            let Some(mode) = mode.as_deref().and_then(parse_mode) else {
                tracing::warn!(%id, "skipped schedule with unreadable mode");
                return Ok(None);
            };
            Target::NewThread {
                folder,
                driver,
                profile_id,
                mode,
                model,
                worktree: worktree != 0,
            }
        }
        _ => {
            tracing::warn!(%id, "skipped schedule with ambiguous target");
            return Ok(None);
        }
    };
    let Ok(rule) = serde_json::from_str::<Rule>(&rule_raw) else {
        tracing::warn!(%id, "skipped schedule with unreadable rule JSON");
        return Ok(None);
    };
    let Some(missed_policy) = MissedPolicy::parse(&missed_raw) else {
        tracing::warn!(%id, "skipped schedule with unknown missed_policy");
        return Ok(None);
    };
    let ends = match ends_raw {
        None => Ends::Never,
        Some(raw) => match serde_json::from_str::<Ends>(&raw) {
            Ok(ends) => ends,
            Err(_) => {
                tracing::warn!(%id, "skipped schedule with unreadable ends JSON");
                return Ok(None);
            }
        },
    };
    let last_outcome = match last_outcome_raw {
        None => None,
        Some(raw) => Some(LastOutcome::parse(&raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                16,
                rusqlite::types::Type::Text,
                format!("unknown schedule outcome: {raw}").into(),
            )
        })?),
    };
    let paused_reason = match paused_raw {
        None => None,
        Some(raw) => Some(PausedReason::parse(&raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                19,
                rusqlite::types::Type::Text,
                format!("unknown pause reason: {raw}").into(),
            )
        })?),
    };
    Ok(Some(ScheduleRecord {
        id,
        created_at,
        updated_at,
        target,
        prompt,
        rule,
        missed_policy,
        ends,
        next_due,
        last_due,
        last_outcome,
        run_count: u32::try_from(run_count).unwrap_or(u32::MAX),
        unattended_runs: u32::try_from(unattended_runs).unwrap_or(u32::MAX),
        paused_reason,
    }))
}

fn validate_and_serialize(
    prompt: &str,
    rule: &Rule,
    ends: Ends,
) -> Result<(String, String, Option<String>)> {
    validate_prompt(prompt).map_err(|why| StoreError::Failed(why.to_owned()))?;
    validate_rule(rule).map_err(|why| StoreError::Failed(why.to_owned()))?;
    validate_ends(ends).map_err(|why| StoreError::Failed(why.to_owned()))?;
    let safe_prompt = redact(prompt);
    let rule_json = serde_json::to_string(rule)?;
    let ends_json = match ends {
        Ends::Never => None,
        other => Some(serde_json::to_string(&other)?),
    };
    Ok((safe_prompt, rule_json, ends_json))
}

impl Store {
    /// Creates a new schedule and returns its generated id.
    pub fn create_schedule(&self, new: &NewSchedule) -> Result<String> {
        let id = self.new_id()?;
        self.create_schedule_with_id(&id, new)?;
        Ok(id)
    }

    /// Creates a new schedule under an id the caller supplied ([`Store::new_id`]).
    pub fn create_schedule_with_id(&self, id: &str, new: &NewSchedule) -> Result<()> {
        let (safe_prompt, rule_json, ends_json) =
            validate_and_serialize(&new.prompt, &new.rule, new.ends)?;
        let (thread_id, folder, driver, profile_id, mode, model, worktree) =
            target_columns(&new.target)?;
        let now = self.now();
        let tx = self.write_txn()?;
        self.conn.execute(
            "INSERT INTO schedules (
                id, created_at, updated_at, thread_id, target_folder, driver, profile_id,
                mode, model, worktree, prompt, rule, missed_policy, ends, next_due
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                id,
                now,
                now,
                thread_id,
                folder,
                driver,
                profile_id,
                mode,
                model,
                worktree,
                safe_prompt,
                rule_json,
                new.missed_policy.as_str(),
                ends_json,
                new.next_due,
            ],
        )?;
        tx.commit()
    }

    /// Fetches one schedule by id.
    pub fn get_schedule(&self, id: &str) -> Result<Option<ScheduleRecord>> {
        let sql = format!("SELECT {SELECT_COLUMNS} FROM schedules WHERE id = ?1");
        let rec = self
            .conn
            .query_row(&sql, params![id], row_to_record)
            .optional()?
            .flatten();
        Ok(rec)
    }

    /// All schedules, ordered by next due time (active first), then creation order.
    pub fn list_schedules(&self) -> Result<Vec<ScheduleRecord>> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM schedules
             ORDER BY paused_reason IS NOT NULL, next_due IS NULL, next_due ASC, created_at ASC"
        );
        self.query_schedules(&sql, params![])
    }

    /// Schedules attached to `thread_id`.
    pub fn schedules_for_thread(&self, thread_id: &str) -> Result<Vec<ScheduleRecord>> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM schedules
             WHERE thread_id = ?1
             ORDER BY paused_reason IS NOT NULL, next_due IS NULL, next_due ASC, created_at ASC"
        );
        self.query_schedules(&sql, params![thread_id])
    }

    /// Active (unpaused) schedules whose `next_due <= now`, oldest due first.
    pub fn due_schedules(&self, now: i64) -> Result<Vec<ScheduleRecord>> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM schedules
             WHERE paused_reason IS NULL AND next_due IS NOT NULL AND next_due <= ?1
             ORDER BY next_due ASC, created_at ASC"
        );
        self.query_schedules(&sql, params![now])
    }

    fn query_schedules<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<ScheduleRecord>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_record)?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(rec) = row? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Updates a schedule's prompt, rule, missed policy, end condition and next due timestamp,
    /// resetting `unattended_runs` to 0 and clearing any pause.
    pub fn update_schedule(
        &self,
        id: &str,
        prompt: &str,
        rule: &Rule,
        missed_policy: MissedPolicy,
        ends: Ends,
        next_due: Option<i64>,
    ) -> Result<()> {
        let (safe_prompt, rule_json, ends_json) = validate_and_serialize(prompt, rule, ends)?;
        let now = self.now();
        let tx = self.write_txn()?;
        self.conn.execute(
            "UPDATE schedules
             SET updated_at = ?2, prompt = ?3, rule = ?4, missed_policy = ?5, ends = ?6,
                 next_due = ?7, unattended_runs = 0, paused_reason = NULL
             WHERE id = ?1",
            params![
                id,
                now,
                safe_prompt,
                rule_json,
                missed_policy.as_str(),
                ends_json,
                next_due,
            ],
        )?;
        tx.commit()
    }

    /// Records the outcome of checking or firing a schedule and advances `next_due` in one write.
    pub fn record_schedule_run(&self, update: &ScheduleRunUpdate) -> Result<()> {
        let now = self.now();
        let tx = self.write_txn()?;
        self.conn.execute(
            "UPDATE schedules
             SET updated_at = ?2,
                 next_due = ?3,
                 last_due = COALESCE(?4, last_due),
                 last_outcome = COALESCE(?5, last_outcome),
                 run_count = run_count + ?6,
                 unattended_runs = unattended_runs + ?7,
                 paused_reason = ?8
             WHERE id = ?1",
            params![
                update.id,
                now,
                update.next_due,
                update.last_due,
                update.last_outcome.map(LastOutcome::as_str),
                i64::from(update.increment_run_count),
                i64::from(update.increment_unattended),
                update.paused_reason.map(PausedReason::as_str),
            ],
        )?;
        tx.commit()
    }

    /// Pauses a schedule with `reason`, or resumes it (`None`) while resetting `unattended_runs`.
    pub fn set_schedule_paused(
        &self,
        id: &str,
        paused: Option<PausedReason>,
        next_due: Option<i64>,
    ) -> Result<()> {
        let now = self.now();
        let tx = self.write_txn()?;
        match paused {
            Some(reason) => {
                self.conn.execute(
                    "UPDATE schedules SET updated_at = ?2, paused_reason = ?3 WHERE id = ?1",
                    params![id, now, reason.as_str()],
                )?;
            }
            None => {
                self.conn.execute(
                    "UPDATE schedules
                     SET updated_at = ?2, paused_reason = NULL, unattended_runs = 0, next_due = ?3
                     WHERE id = ?1",
                    params![id, now, next_due],
                )?;
            }
        }
        tx.commit()
    }

    /// Resets `unattended_runs` for every schedule targeting `thread_id` after the user spoke.
    pub fn reset_schedule_unattended_for_thread(&self, thread_id: &str) -> Result<()> {
        let now = self.now();
        let tx = self.write_txn()?;
        self.conn.execute(
            "UPDATE schedules
             SET updated_at = ?2, unattended_runs = 0
             WHERE thread_id = ?1 AND unattended_runs > 0",
            params![thread_id, now],
        )?;
        tx.commit()
    }

    /// Deletes a schedule; returns whether a row was removed.
    pub fn delete_schedule(&self, id: &str) -> Result<bool> {
        let tx = self.write_txn()?;
        let changed = self
            .conn
            .execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

    fn mem() -> Store {
        Store::open_in_memory().expect("store")
    }

    fn interval_rule(anchor: i64) -> Rule {
        Rule::Interval {
            every_secs: 3600,
            anchor,
        }
    }

    #[test]
    fn schedules_migrate_v10_to_v11_and_cascade_on_thread_delete() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.pragma_update(None, "foreign_keys", "OFF")
                .expect("fk off");
            conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL)")
                .expect("version table");
            super::super::migrate_locked_to(&conn, 0, 10).expect("v10");
            conn.pragma_update(None, "foreign_keys", "ON")
                .expect("fk on");
            conn.execute(
                "INSERT INTO threads (id, title, cwd, created_at, updated_at)
                 VALUES ('t1', 'Thread', '/w', 1, 1)",
                [],
            )
            .expect("insert thread");
        }
        let s = Store::open(&path).expect("v11 open");
        let id = s
            .create_schedule(&NewSchedule {
                target: Target::ExistingThread {
                    thread_id: "t1".into(),
                },
                prompt: "Check CI".into(),
                rule: interval_rule(1_000_000),
                missed_policy: MissedPolicy::Skip,
                ends: Ends::Never,
                next_due: Some(1_000_000),
            })
            .expect("create");
        assert_eq!(s.schedules_for_thread("t1").expect("list").len(), 1);
        s.delete_thread("t1").expect("delete");
        assert!(s.get_schedule(&id).expect("get").is_none());
    }

    #[test]
    fn schedule_prompt_is_redacted_on_write() {
        let s = mem();
        let t = s.create_thread("/w", None).expect("thread");
        let id = s
            .create_schedule(&NewSchedule {
                target: Target::ExistingThread { thread_id: t },
                prompt: format!("Check token {SECRET}"),
                rule: interval_rule(1_000_000),
                missed_policy: MissedPolicy::Skip,
                ends: Ends::Never,
                next_due: Some(1_000_000),
            })
            .expect("create");
        let saved = s.get_schedule(&id).expect("get").expect("present");
        assert!(!saved.prompt.contains(SECRET), "{}", saved.prompt);
    }

    #[test]
    fn schedule_refuses_bypass_mode_and_agy_driver() {
        let s = mem();
        let bypass = s.create_schedule(&NewSchedule {
            target: Target::NewThread {
                folder: "/w".into(),
                driver: Driver::Claude,
                profile_id: None,
                mode: Mode::Bypass,
                model: None,
                worktree: true,
            },
            prompt: "Run".into(),
            rule: interval_rule(1_000_000),
            missed_policy: MissedPolicy::Skip,
            ends: Ends::Never,
            next_due: Some(1_000_000),
        });
        assert!(bypass.is_err());

        let agy_no_wt = s.create_schedule(&NewSchedule {
            target: Target::NewThread {
                folder: "/w".into(),
                driver: Driver::Agy,
                profile_id: None,
                mode: Mode::Plan,
                model: None,
                worktree: false,
            },
            prompt: "Run".into(),
            rule: interval_rule(1_000_000),
            missed_policy: MissedPolicy::Skip,
            ends: Ends::Never,
            next_due: Some(1_000_000),
        });
        assert!(agy_no_wt.is_err());

        let agy_wt = s.create_schedule(&NewSchedule {
            target: Target::NewThread {
                folder: "/w".into(),
                driver: Driver::Agy,
                profile_id: None,
                mode: Mode::AcceptEdits,
                model: None,
                worktree: true,
            },
            prompt: "Run".into(),
            rule: interval_rule(1_000_000),
            missed_policy: MissedPolicy::Skip,
            ends: Ends::Never,
            next_due: Some(1_000_000),
        });
        assert!(agy_wt.is_ok());
    }

    #[test]
    fn schedule_due_ordering_record_run_and_user_reset() {
        let s = mem();
        let t = s.create_thread("/w", None).expect("thread");
        let early = s
            .create_schedule(&NewSchedule {
                target: Target::ExistingThread {
                    thread_id: t.clone(),
                },
                prompt: "Early".into(),
                rule: interval_rule(1_000),
                missed_policy: MissedPolicy::RunOnce,
                ends: Ends::AfterRuns { max: 5 },
                next_due: Some(1_000),
            })
            .expect("early");
        let later = s
            .create_schedule(&NewSchedule {
                target: Target::ExistingThread {
                    thread_id: t.clone(),
                },
                prompt: "Later".into(),
                rule: interval_rule(2_000),
                missed_policy: MissedPolicy::Skip,
                ends: Ends::Never,
                next_due: Some(2_000),
            })
            .expect("later");

        let due = s.due_schedules(1_500).expect("due");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, early);

        s.record_schedule_run(&ScheduleRunUpdate {
            id: early.clone(),
            next_due: Some(4_600),
            last_due: Some(1_000),
            last_outcome: Some(LastOutcome::Ran),
            increment_run_count: true,
            increment_unattended: true,
            paused_reason: None,
        })
        .expect("record");

        let rec = s.get_schedule(&early).expect("get").expect("present");
        assert_eq!(rec.run_count, 1);
        assert_eq!(rec.unattended_runs, 1);
        assert_eq!(rec.last_outcome, Some(LastOutcome::Ran));
        assert_eq!(rec.next_due, Some(4_600));

        s.reset_schedule_unattended_for_thread(&t).expect("reset");
        let rec = s.get_schedule(&early).expect("get").expect("present");
        assert_eq!(rec.unattended_runs, 0);

        s.set_schedule_paused(&later, Some(PausedReason::User), None)
            .expect("pause");
        assert!(s.due_schedules(3_000).expect("due before early").is_empty());
        let due_at_5000 = s.due_schedules(5_000).expect("due at 5000");
        assert_eq!(due_at_5000.len(), 1);
        assert_eq!(due_at_5000[0].id, early);
        assert!(s.delete_schedule(&early).expect("delete"));
        assert!(!s.delete_schedule(&early).expect("delete again"));
    }
}
