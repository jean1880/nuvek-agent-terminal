//! Schema v11: the tables for pins and tags, search, schedules, spending and review drafts.
//!
//! The step is strictly additive: it adds one nullable column to `threads` and creates new tables
//! and their indexes. It rebuilds no table and reads no event, so it costs the same on any store.
//! Every table starts empty; each is filled by its own feature, which redacts the text it writes.
//!
//! - `threads.pin_rank` and `thread_tags`: a pinned thread's place, and a thread's tags. Tag
//!   colours are not a table: a `meta` key holds them.
//! - `search_items` and `search_state`: one row per transcript item's searchable text, and how
//!   far each thread is indexed. The FTS5 table over `search_items` and its triggers are created
//!   after open ([`super::Store::ensure_search_fts`]), never here: a build whose SQLite lacks FTS5
//!   still opens the store.
//! - `schedules`: scheduled prompts. A schedule targets an existing thread or a new thread in a
//!   folder, never both; a new-thread target names its driver; an agy one runs in a worktree.
//!   For an existing thread the agy rule is checked when the schedule fires, since the thread
//!   can change agent after the schedule was made.
//! - `spend_ledger`: per-day totals of deleted threads' turns, numbers and ids only. Live
//!   threads are read from their events, through an index created after open
//!   ([`super::Store::ensure_spend_index`]), since building it parses every stored event.
//! - `review_drafts`: unsent comments on diff lines.
//!
//! Rows that belong to a thread go with it (`ON DELETE CASCADE`), and every foreign-key child
//! column leads an index. An older build refuses the upgraded store (`SchemaTooNew`).

/// The v11 migration step.
pub(crate) const SCHEMA_V11: &str = "
ALTER TABLE threads ADD COLUMN pin_rank INTEGER CHECK (pin_rank IS NULL OR pin_rank >= 0);
CREATE TABLE thread_tags (
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    tag TEXT NOT NULL CHECK (length(tag) BETWEEN 1 AND 32),
    PRIMARY KEY (thread_id, tag)
) WITHOUT ROWID;
CREATE INDEX thread_tags_by_tag ON thread_tags(tag);

CREATE TABLE search_items (
    id INTEGER PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    item_id TEXT NOT NULL,
    first_seq INTEGER NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'tool')),
    at INTEGER NOT NULL,
    text TEXT NOT NULL,
    UNIQUE (thread_id, item_id)
);
CREATE TABLE search_state (
    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    indexed_seq INTEGER NOT NULL DEFAULT 0 CHECK (indexed_seq >= 0)
) WITHOUT ROWID;

CREATE TABLE schedules (
    id TEXT PRIMARY KEY NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    thread_id TEXT REFERENCES threads(id) ON DELETE CASCADE,
    target_folder TEXT,
    driver TEXT REFERENCES drivers(key),
    profile_id TEXT,
    mode TEXT CHECK (mode IS NULL OR mode IN ('ask', 'accept_edits', 'plan')),
    model TEXT,
    worktree INTEGER NOT NULL DEFAULT 0 CHECK (worktree IN (0, 1)),
    prompt TEXT NOT NULL,
    rule TEXT NOT NULL,
    missed_policy TEXT NOT NULL CHECK (missed_policy IN ('skip', 'run_once')),
    ends TEXT,
    next_due INTEGER,
    last_due INTEGER,
    last_outcome TEXT CHECK (last_outcome IS NULL OR last_outcome IN ('ran', 'missed',
        'skipped_busy', 'skipped_approval', 'skipped_limit', 'skipped_mode',
        'skipped_agy_worktree', 'failed')),
    run_count INTEGER NOT NULL DEFAULT 0 CHECK (run_count >= 0),
    unattended_runs INTEGER NOT NULL DEFAULT 0 CHECK (unattended_runs >= 0),
    paused_reason TEXT CHECK (paused_reason IS NULL OR paused_reason IN ('user', 'stopped_run',
        'cap', 'budget', 'ended', 'target_gone', 'setting_off')),
    CHECK ((thread_id IS NULL) <> (target_folder IS NULL)),
    CHECK (thread_id IS NOT NULL OR driver IS NOT NULL),
    CHECK (driver IS NULL OR driver <> 'agy' OR worktree = 1)
);
CREATE INDEX schedules_thread ON schedules(thread_id);
CREATE INDEX schedules_due ON schedules(next_due) WHERE paused_reason IS NULL;

CREATE TABLE spend_ledger (
    day TEXT NOT NULL,
    driver TEXT NOT NULL REFERENCES drivers(key),
    profile_id TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    folder TEXT NOT NULL DEFAULT '',
    billing TEXT NOT NULL CHECK (billing IN ('metered', 'plan', 'free', 'unknown')),
    delegated INTEGER NOT NULL DEFAULT 0 CHECK (delegated IN (0, 1)),
    cost_usd REAL NOT NULL DEFAULT 0 CHECK (cost_usd >= 0),
    cost_turns INTEGER NOT NULL DEFAULT 0 CHECK (cost_turns >= 0),
    turns INTEGER NOT NULL DEFAULT 0 CHECK (turns >= 0),
    input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    cached_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cached_input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
    PRIMARY KEY (day, driver, profile_id, model, folder, billing, delegated)
) WITHOUT ROWID;

CREATE TABLE review_drafts (
    id INTEGER PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    side TEXT NOT NULL CHECK (side IN ('old', 'new')),
    start_line INTEGER NOT NULL CHECK (start_line >= 1),
    end_line INTEGER NOT NULL CHECK (end_line >= start_line),
    quoted TEXT NOT NULL,
    note TEXT NOT NULL,
    diff_base TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX review_drafts_thread ON review_drafts(thread_id, path, start_line);

INSERT INTO schema_version (version) VALUES (11);
";

/// The tables v11 creates, each empty after the step.
#[cfg(test)]
pub(crate) const V11_TABLES: [&str; 6] = [
    "thread_tags",
    "search_items",
    "search_state",
    "schedules",
    "spend_ledger",
    "review_drafts",
];

/// The FTS5 table over `search_items` ([`super::Store::ensure_search_fts`]).
pub(crate) const SEARCH_FTS: &str = "search_fts";
/// Its triggers, which keep it in step with `search_items`.
pub(crate) const SEARCH_FTS_TRIGGERS: [&str; 3] = [
    "search_items_fts_insert",
    "search_items_fts_delete",
    "search_items_fts_update",
];

/// The external-content FTS5 table over `search_items` and its three triggers, all
/// `IF NOT EXISTS`; `{module}` is `fts5` (a test names a module that does not exist). The index
/// is rebuilt from `search_items` at the end, so rows written while it was missing are found.
pub(crate) const SEARCH_FTS_SQL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING {module}(
    text, content='search_items', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER IF NOT EXISTS search_items_fts_insert AFTER INSERT ON search_items BEGIN
    INSERT INTO search_fts (rowid, text) VALUES (NEW.id, NEW.text);
END;
CREATE TRIGGER IF NOT EXISTS search_items_fts_delete AFTER DELETE ON search_items BEGIN
    INSERT INTO search_fts (search_fts, rowid, text) VALUES ('delete', OLD.id, OLD.text);
END;
CREATE TRIGGER IF NOT EXISTS search_items_fts_update AFTER UPDATE ON search_items BEGIN
    INSERT INTO search_fts (search_fts, rowid, text) VALUES ('delete', OLD.id, OLD.text);
    INSERT INTO search_fts (rowid, text) VALUES (NEW.id, NEW.text);
END;
INSERT INTO search_fts (search_fts) VALUES ('rebuild');
";

/// The partial index the spending history reads live threads' turns through
/// ([`super::Store::ensure_spend_index`]). A query repeats the WHERE term with the literal
/// `'turn_completed'`, so its plan uses the index whatever is bound.
pub(crate) const SPEND_INDEX: &str = "events_turns";
pub(crate) const SPEND_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS events_turns ON events(at)
     WHERE json_extract(envelope_json, '$.event.type') = 'turn_completed'";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{schema_version_within, SearchIndex, Store, StoreError, SCHEMA_VERSION};
    use agent_core::event::{Envelope, Event, TurnState};
    use rusqlite::{params, Connection};

    fn store() -> Store {
        Store::open_in_memory().expect("open")
    }

    fn count(s: &Store, sql: &str) -> i64 {
        s.conn.query_row(sql, [], |r| r.get(0)).expect(sql)
    }

    fn columns(s: &Store, table: &str) -> Vec<(String, bool)> {
        s.conn
            .prepare("SELECT name, \"notnull\" FROM pragma_table_info(?1) ORDER BY cid")
            .expect("prepare")
            .query_map(params![table], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("columns")
    }

    fn objects(s: &Store) -> Vec<(String, String)> {
        s.conn
            .prepare("SELECT type, name FROM sqlite_master ORDER BY type, name")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("objects")
    }

    fn has(s: &Store, name: &str) -> bool {
        s.has_objects(&[name]).expect("sqlite_master")
    }

    #[test]
    fn v11_creates_its_tables_empty_with_their_columns_and_indexes() {
        let s = store();
        for table in V11_TABLES {
            assert!(has(&s, table), "{table}");
            assert_eq!(
                count(&s, &format!("SELECT COUNT(*) FROM {table}")),
                0,
                "{table}"
            );
        }
        let threads = columns(&s, "threads");
        assert_eq!(
            threads.last(),
            Some(&("pin_rank".to_owned(), false)),
            "nullable, added last"
        );
        let names = |table: &str| -> Vec<String> {
            columns(&s, table).into_iter().map(|(n, _)| n).collect()
        };
        assert_eq!(names("thread_tags"), ["thread_id", "tag"]);
        assert_eq!(
            names("search_items"),
            [
                "id",
                "thread_id",
                "item_id",
                "first_seq",
                "role",
                "at",
                "text"
            ]
        );
        assert_eq!(names("search_state"), ["thread_id", "indexed_seq"]);
        assert_eq!(
            names("schedules"),
            [
                "id",
                "created_at",
                "updated_at",
                "thread_id",
                "target_folder",
                "driver",
                "profile_id",
                "mode",
                "model",
                "worktree",
                "prompt",
                "rule",
                "missed_policy",
                "ends",
                "next_due",
                "last_due",
                "last_outcome",
                "run_count",
                "unattended_runs",
                "paused_reason",
            ]
        );
        assert_eq!(
            names("spend_ledger"),
            [
                "day",
                "driver",
                "profile_id",
                "model",
                "folder",
                "billing",
                "delegated",
                "cost_usd",
                "cost_turns",
                "turns",
                "input_tokens",
                "cached_input_tokens",
                "output_tokens",
                "reasoning_tokens",
            ]
        );
        assert_eq!(
            names("review_drafts"),
            [
                "id",
                "thread_id",
                "path",
                "side",
                "start_line",
                "end_line",
                "quoted",
                "note",
                "diff_base",
                "created_at",
            ]
        );
        for index in [
            "thread_tags_by_tag",
            "schedules_thread",
            "schedules_due",
            "review_drafts_thread",
        ] {
            assert!(has(&s, index), "{index}");
        }
        // The after-open objects are not part of the migration.
        assert!(!has(&s, SEARCH_FTS));
        assert!(!has(&s, SPEND_INDEX));
    }

    #[test]
    fn a_v10_build_refuses_a_v11_store() {
        let s = store();
        assert_eq!(SCHEMA_VERSION, 11);
        assert!(matches!(
            schema_version_within(&s.conn, 10),
            Err(StoreError::SchemaTooNew {
                found: 11,
                supported: 10
            })
        ));
        assert_eq!(schema_version_within(&s.conn, 11).expect("v11"), 11);
    }

    /// Inserts one row of every thread-owned v11 table for `thread`, with `word` as its text.
    fn fill(s: &Store, thread: &str, word: &str) {
        s.conn
            .execute_batch(&format!(
                "INSERT INTO thread_tags (thread_id, tag) VALUES ('{thread}', 'work');
                 INSERT INTO search_items (thread_id, item_id, first_seq, role, at, text)
                     VALUES ('{thread}', 'i1', 1, 'user', 1, '{word}');
                 INSERT INTO search_state (thread_id, indexed_seq) VALUES ('{thread}', 1);
                 INSERT INTO schedules (id, created_at, updated_at, thread_id, prompt, rule,
                     missed_policy) VALUES ('s-{thread}', 1, 1, '{thread}', 'p', '{{}}', 'skip');
                 INSERT INTO review_drafts (thread_id, path, side, start_line, end_line, quoted,
                     note, diff_base, created_at)
                     VALUES ('{thread}', 'a.rs', 'new', 1, 2, 'q', 'n', 'head', 1);"
            ))
            .expect("rows");
    }

    #[test]
    fn deleting_a_thread_takes_its_v11_rows_and_their_search_text_with_it() {
        let s = store();
        assert_eq!(s.ensure_search_fts().expect("fts"), SearchIndex::Ready);
        let gone = s.create_thread("/w", None).expect("thread");
        let kept = s.create_thread("/w", None).expect("thread");
        fill(&s, &gone, "needle");
        fill(&s, &kept, "haystack");
        let matches = |word: &str| {
            count(
                &s,
                &format!("SELECT COUNT(*) FROM search_fts WHERE search_fts MATCH '{word}'"),
            )
        };
        assert_eq!((matches("needle"), matches("haystack")), (1, 1));

        s.delete_thread(&gone).expect("delete");

        for table in [
            "thread_tags",
            "search_items",
            "search_state",
            "schedules",
            "review_drafts",
        ] {
            let left = |t: &str| -> i64 {
                s.conn
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE thread_id = ?1"),
                        params![t],
                        |r| r.get(0),
                    )
                    .expect(table)
            };
            assert_eq!((left(&gone), left(&kept)), (0, 1), "{table}");
        }
        // The cascade fired the delete trigger: the index no longer holds the text.
        assert_eq!((matches("needle"), matches("haystack")), (0, 1));
        s.conn
            .execute_batch("INSERT INTO search_fts (search_fts) VALUES ('integrity-check')")
            .expect("the index matches its content");
    }

    #[test]
    fn updating_a_search_item_moves_its_text_in_the_index() {
        let s = store();
        assert_eq!(s.ensure_search_fts().expect("fts"), SearchIndex::Ready);
        let t = s.create_thread("/w", None).expect("thread");
        fill(&s, &t, "before");
        let matches = |word: &str| {
            count(
                &s,
                &format!("SELECT COUNT(*) FROM search_fts WHERE search_fts MATCH '{word}'"),
            )
        };
        assert_eq!((matches("before"), matches("after")), (1, 0));
        s.conn
            .execute(
                "UPDATE search_items SET text = 'after' WHERE thread_id = ?1",
                params![t],
            )
            .expect("update");
        assert_eq!(
            (matches("before"), matches("after")),
            (0, 1),
            "the old text is gone from the index, the new one found"
        );
        s.conn
            .execute_batch("INSERT INTO search_fts (search_fts) VALUES ('integrity-check')")
            .expect("the index matches its content");
    }

    #[test]
    fn every_v11_check_refuses_its_bad_row() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        let schedule = |id: &str, thread: &str, folder: &str, driver: &str, rest: &str| {
            format!(
                "INSERT INTO schedules (id, created_at, updated_at, thread_id, target_folder,
                     driver, worktree, mode, last_outcome, prompt, rule, missed_policy)
                 VALUES ('{id}', 1, 1, {thread}, {folder}, {driver}, {rest}, 'p', '{{}}',
                         'skip')"
            )
        };
        let draft = |start: i64, end: i64| {
            format!(
                "INSERT INTO review_drafts (thread_id, path, side, start_line, end_line, quoted,
                     note, diff_base, created_at)
                 VALUES ('{t}', 'a', 'old', {start}, {end}, '', '', 'h', 1)"
            )
        };
        let ledger = |day: &str, kind: &str, cost: &str| {
            format!(
                "INSERT INTO spend_ledger (day, driver, billing, cost_usd)
                 VALUES ('{day}', 'claude', '{kind}', {cost})"
            )
        };
        let thread = format!("'{t}'");
        // Good rows first, so each refusal below is its check's and not a typo's.
        for sql in [
            schedule("ok1", &thread, "NULL", "NULL", "0, NULL, NULL"),
            schedule("ok2", "NULL", "'/w'", "'claude'", "0, 'ask', 'ran'"),
            schedule("ok3", "NULL", "'/w'", "'agy'", "1, 'plan', NULL"),
            ledger("2026-11-15", "metered", "0.5"),
            draft(1, 1),
            format!("UPDATE threads SET pin_rank = 0 WHERE id = '{t}'"),
        ] {
            s.conn
                .execute_batch(&sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let refused = |what: &str, sql: String| match s.conn.execute_batch(&sql) {
            Err(rusqlite::Error::SqliteFailure(e, Some(msg)))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
                    && msg.contains("CHECK constraint failed") => {}
            other => panic!("{what}: {other:?}"),
        };
        let none = "0, NULL, NULL";
        refused(
            "both targets",
            schedule("b1", &thread, "'/w'", "'claude'", none),
        );
        refused(
            "neither target",
            schedule("b2", "NULL", "NULL", "'claude'", none),
        );
        refused(
            "a new-thread target without a driver",
            schedule("b3", "NULL", "'/w'", "NULL", none),
        );
        refused(
            "agy without a worktree",
            schedule("b4", "NULL", "'/w'", "'agy'", none),
        );
        refused(
            "bypass mode",
            schedule("b5", &thread, "NULL", "NULL", "0, 'bypass', NULL"),
        );
        refused(
            "an unknown outcome",
            schedule("b6", &thread, "NULL", "NULL", "0, NULL, 'exploded'"),
        );
        refused("a zero line", draft(0, 1));
        refused("an end before its start", draft(3, 2));
        refused("a negative cost", ledger("2026-11-16", "metered", "-0.01"));
        refused("an unknown billing kind", ledger("2026-11-16", "x", "0"));
        refused(
            "an empty tag",
            format!("INSERT INTO thread_tags (thread_id, tag) VALUES ('{t}', '')"),
        );
        refused(
            "a tag longer than 32 characters",
            format!(
                "INSERT INTO thread_tags (thread_id, tag) VALUES ('{t}', '{}')",
                "x".repeat(33)
            ),
        );
        refused(
            "a negative pin rank",
            format!("UPDATE threads SET pin_rank = -1 WHERE id = '{t}'"),
        );
        refused(
            "an unknown pause reason",
            "UPDATE schedules SET paused_reason = 'bored' WHERE id = 'ok1'".into(),
        );
        refused(
            "an unknown search role",
            format!(
                "INSERT INTO search_items (thread_id, item_id, first_seq, role, at, text)
                 VALUES ('{t}', 'i9', 1, 'system', 1, 'x')"
            ),
        );
        refused(
            "an unknown diff side",
            format!(
                "INSERT INTO review_drafts (thread_id, path, side, start_line, end_line, quoted,
                     note, diff_base, created_at)
                 VALUES ('{t}', 'a', 'middle', 1, 1, '', '', 'h', 1)"
            ),
        );
        refused(
            "a negative token count",
            "INSERT INTO spend_ledger (day, driver, billing, output_tokens)
                 VALUES ('2026-11-17', 'claude', 'metered', -1)"
                .into(),
        );
        // A driver the reference table does not list is refused by its foreign key.
        assert!(s
            .conn
            .execute_batch(&schedule("b7", "NULL", "'/w'", "'gpt'", none))
            .is_err());
    }

    #[test]
    fn the_bundled_sqlite_has_fts5() {
        let s = store();
        assert_eq!(
            count(&s, "SELECT sqlite_compileoption_used('ENABLE_FTS5')"),
            1
        );
        assert_eq!(s.ensure_search_fts().expect("fts"), SearchIndex::Ready);
    }

    #[test]
    fn without_fts5_search_is_unavailable_and_the_store_works_on() {
        let s = store();
        s.simulate_missing_fts5(true);
        let before = objects(&s);
        match s.ensure_search_fts().expect("not an error") {
            SearchIndex::Unavailable(why) => assert!(why.contains("no such module"), "{why}"),
            ready => panic!("{ready:?}"),
        }
        assert_eq!(objects(&s), before, "nothing is left half made");
        let t = s.create_thread("/w", None).expect("the store works on");
        fill(&s, &t, "written");

        // Once FTS5 is there, the index is made and finds what was written meanwhile.
        s.simulate_missing_fts5(false);
        assert_eq!(s.ensure_search_fts().expect("fts"), SearchIndex::Ready);
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM search_fts WHERE search_fts MATCH 'written'"
            ),
            1
        );
    }

    /// A file store whose write lock another connection holds, and that connection.
    fn locked(dir: &tempfile::TempDir) -> (Store, Connection) {
        let path = dir.path().join("threads.db");
        let s = Store::open(&path).expect("open");
        let other = Connection::open(&path).expect("other");
        other
            .execute_batch("BEGIN IMMEDIATE")
            .expect("the other connection takes the write lock");
        // Any attempt at the lock now fails at once, rather than after the busy timeout.
        s.conn
            .busy_timeout(std::time::Duration::ZERO)
            .expect("no wait");
        assert!(s.set_meta("probe", "x").is_err(), "the lock is held");
        (s, other)
    }

    #[test]
    fn ensuring_the_after_open_objects_is_idempotent_and_takes_no_lock_once_they_exist() {
        let dir = tempfile::tempdir().expect("tmp");
        {
            let s = Store::open(&dir.path().join("threads.db")).expect("open");
            assert_eq!(s.ensure_search_fts().expect("fts"), SearchIndex::Ready);
            s.ensure_spend_index().expect("index");
            let made = objects(&s);
            assert_eq!(s.ensure_search_fts().expect("again"), SearchIndex::Ready);
            s.ensure_spend_index().expect("again");
            assert_eq!(objects(&s), made, "a second run changes nothing");
        }
        let (s, _other) = locked(&dir);
        assert_eq!(
            s.ensure_search_fts().expect("a read only"),
            SearchIndex::Ready
        );
        s.ensure_spend_index().expect("a read only");
    }

    /// The control for the test above: making a missing object asks for the lock, so there the
    /// lock was never asked for.
    #[test]
    fn making_a_missing_after_open_object_needs_the_lock() {
        let dir = tempfile::tempdir().expect("tmp");
        let (s, _other) = locked(&dir);
        assert!(s.ensure_spend_index().is_err());
        assert!(
            s.ensure_search_fts().is_err(),
            "busy is an error, never unavailable"
        );
    }

    #[test]
    fn events_turns_serves_a_query_that_repeats_its_term_literally() {
        let s = store();
        s.ensure_spend_index().expect("index");
        let t = s.create_thread("/w", None).expect("thread");
        s.append_event(
            &t,
            None,
            &Envelope::turn_completed(TurnState::Completed, None),
        )
        .expect("a turn");
        s.append_event(&t, None, &Envelope::new(Event::Unknown))
            .expect("another event");
        // The indexed term matches the real envelope's shape.
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM events
                 WHERE json_extract(envelope_json, '$.event.type') = 'turn_completed'"
            ),
            1
        );
        let plan = |sql: &str, values: &[&dyn rusqlite::ToSql]| -> String {
            s.conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("prepare")
                .query_map(values, |r| r.get::<_, String>(3))
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("plan")
                .join("; ")
        };
        let used = plan(
            "SELECT at FROM events
             WHERE json_extract(envelope_json, '$.event.type') = 'turn_completed' AND at >= ?1",
            &[&0],
        );
        assert!(used.contains("events_turns"), "{used}");
        let other = plan(
            "SELECT at FROM events
             WHERE json_extract(envelope_json, '$.event.type') = 'turn_started' AND at >= ?1",
            &[&0],
        );
        assert!(!other.contains("events_turns"), "{other}");
        // With the type bound as a parameter, the index is not part of the statement's own plan:
        // SQLite picks it only by planning again for each bound value, so the plan follows the
        // value (another type, no index). Only the literal makes the index certain.
        let bound = |value: &str| {
            plan(
                "SELECT at FROM events
                 WHERE json_extract(envelope_json, '$.event.type') = ?2 AND at >= ?1",
                &[&0, &value],
            )
        };
        let other_bound = bound("turn_started");
        assert!(!other_bound.contains("events_turns"), "{other_bound}");
        let same_bound = bound("turn_completed");
        assert!(same_bound.contains("events_turns"), "{same_bound}");
    }
}
