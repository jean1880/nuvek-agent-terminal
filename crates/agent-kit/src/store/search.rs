//! Search inside threads: the index of what was said, and the reads of it.
//!
//! The index holds, per transcript item, the text a search finds it by: a user message (with
//! `[image: name]` for each image it carried), an agent's reply, or a tool's title (`Run cargo
//! test`, `Edit src/lib.rs`). Never a tool's output, the model's reasoning, a notice, a `raw`
//! frame or an image. Its text is read from stored rows only (already scrubbed), and redacted
//! again whole before it is written: a secret split across two deltas is whole there.
//!
//! [`Store::index_thread`] is the only way in: live after a turn, a session's end and a user
//! message, and for every thread not yet indexed at start-up (`search_backfill`). It reads the
//! events after the thread's watermark (`search_state.indexed_seq`), folds them into items with
//! the transcript fold, upserts them, and moves the watermark on. The watermark never passes an
//! item that may still change, so the next pass reads it again from its start. Running it again
//! changes nothing, and stopping part-way loses nothing.
//!
//! [`Store::search`] is a read: the app runs it off the main thread, after the writer's barrier.

use std::collections::HashMap;

use agent_core::event::Event;
use rusqlite::{params, OptionalExtension};

use super::{
    redact, FoldOptions, Result, Store, StoredEvent, ThreadId, TranscriptFold, TranscriptItem,
    NOT_HIDDEN_CHILD,
};

/// Events [`Store::index_thread`] reads in one call before it stops, once it has moved the
/// watermark: what keeps one job on the writer short. A thread with more is finished by later
/// calls ([`IndexProgress::more`]).
pub const INDEX_BUDGET: usize = 5_000;
/// Most bytes of an item's text the index keeps.
pub const ITEM_CAP: usize = 32 * 1024;
/// Events read per query.
const BATCH: usize = 1_000;

/// Where a match begins in [`SearchHit::snippet`]. A private-use character, taken out of indexed
/// text, so it never appears there otherwise.
pub const SNIPPET_OPEN: char = '\u{E000}';
/// Where a match ends in [`SearchHit::snippet`].
pub const SNIPPET_CLOSE: char = '\u{E001}';

/// How far a call of [`Store::index_thread`] got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProgress {
    /// The thread's watermark after the call: every event up to it is indexed for good.
    pub indexed_seq: i64,
    /// The call stopped at its budget with events still unread: call again.
    pub more: bool,
}

/// One item a search found.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub thread_id: ThreadId,
    pub item_id: String,
    /// The sequence number of the item's first event.
    pub first_seq: i64,
    /// `user`, `assistant` or `tool`.
    pub role: String,
    /// When the item began (epoch milliseconds).
    pub at: i64,
    /// A short piece of the item's text around the match, each matched word between
    /// [`SNIPPET_OPEN`] and [`SNIPPET_CLOSE`].
    pub snippet: String,
    /// FTS5's rank (bm25): lower is a better match.
    pub rank: f64,
    pub archived: bool,
}

/// What [`Store::search`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchResults {
    /// The best matches first (none is no match).
    Hits(Vec<SearchHit>),
    /// The full-text index does not exist (this SQLite has no FTS5): never "no match".
    Unavailable,
}

/// The role and text the index keeps of `item`; `None` for an item it never keeps (reasoning,
/// a notice, the inherited context, an item whose start it did not see).
fn searchable(item: &TranscriptItem) -> Option<(&'static str, &str)> {
    let m = &item.message;
    match (m.role.as_str(), m.kind.as_str()) {
        ("user", "user_message") => Some(("user", &m.text)),
        ("assistant", "assistant_message") => Some(("assistant", &m.text)),
        // A tool by its title alone: its text is its output, never indexed.
        ("tool", kind) if kind != "unknown" => Some(("tool", &m.title)),
        _ => None,
    }
}

/// `text` as the index keeps it: redacted whole, without the snippet markers, cut to
/// [`ITEM_CAP`] bytes at a character boundary.
fn index_text(text: &str) -> String {
    let mut text: String = redact(text)
        .chars()
        .filter(|c| *c != SNIPPET_OPEN && *c != SNIPPET_CLOSE)
        .collect();
    if text.len() > ITEM_CAP {
        let mut end = ITEM_CAP;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Whether `item` may still change, so the watermark must not pass it: an item the index keeps,
/// still open, with no turn's or session's end after its start. A user message never holds it:
/// it is stored whole, in one write, and never completes.
fn still_open(item: &TranscriptItem, last_boundary: i64) -> bool {
    let m = &item.message;
    m.status == "open"
        && item.first_seq >= last_boundary
        && m.kind != "user_message"
        && searchable(item).is_some()
}

/// Ends what was open: after one, no earlier item gets more of its events.
fn is_boundary(event: &Event) -> bool {
    matches!(
        event,
        Event::TurnCompleted { .. } | Event::SessionExited { .. } | Event::SessionStarted { .. }
    )
}

impl Store {
    /// The thread's index watermark (0 when it was never indexed).
    fn indexed_seq(&self, thread: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT indexed_seq FROM search_state WHERE thread_id = ?1",
                params![thread],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Up to [`BATCH`] events of `thread` after `after`, when each was stored going into `ats`,
    /// with the last sequence number read and how many rows were. A row that no longer decodes
    /// is left out (it holds no text the index could read).
    fn index_batch(
        &self,
        thread: &str,
        after: i64,
        ats: &mut HashMap<i64, i64>,
    ) -> Result<(Vec<StoredEvent>, i64, usize)> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT e.seq, p.driver, e.at, e.envelope_json FROM events e
             LEFT JOIN provider_threads p ON p.id = e.provider_thread_id
             WHERE e.thread_id = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![thread, after, BATCH as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let (mut out, mut last, mut read) = (Vec::new(), after, 0);
        for row in rows {
            let (seq, driver, at, json) = row?;
            (last, read) = (seq, read + 1);
            match serde_json::from_str(&json) {
                Ok(envelope) => {
                    out.push(StoredEvent {
                        seq,
                        driver,
                        envelope,
                    });
                    ats.insert(seq, at);
                }
                Err(e) => tracing::debug!(seq, error = %e, "an event the index cannot read"),
            }
        }
        Ok((out, last, read))
    }

    /// Indexes `thread` for search from its watermark on, reading at most about `budget` events
    /// once the watermark can move (see the module docs). Idempotent: an item already indexed
    /// with the same text is not written again. A thread that is gone is no error: nothing is
    /// indexed.
    ///
    /// It opens a transaction of its own for its writes, so it must not run inside another
    /// (on the store writer it becomes a savepoint).
    pub fn index_thread(&self, thread: &str, budget: usize) -> Result<IndexProgress> {
        // A delegated worker's hidden backing thread is never searched, so never indexed.
        let exists: bool = self.conn.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM threads t WHERE t.id = ?1 AND {NOT_HIDDEN_CHILD})"
            ),
            params![thread],
            |r| r.get(0),
        )?;
        let start = self.indexed_seq(thread)?;
        if !exists {
            return Ok(IndexProgress {
                indexed_seq: start,
                more: false,
            });
        }
        let mut ats = HashMap::new();
        let mut fold = TranscriptFold::after(FoldOptions::default(), start);
        let (mut read, mut last_boundary, mut more) = (0usize, 0i64, false);
        let watermark = |fold: &TranscriptFold, last_boundary: i64| {
            fold.items_so_far()
                .iter()
                .filter(|item| still_open(item, last_boundary))
                .map(|item| item.first_seq - 1)
                .min()
                .unwrap_or(fold.after)
                .max(start)
        };
        loop {
            let (batch, last, n) = self.index_batch(thread, fold.after, &mut ats)?;
            if n == 0 {
                break;
            }
            read += n;
            if let Some(b) = batch.iter().rev().find(|e| is_boundary(&e.envelope.event)) {
                last_boundary = b.seq;
            }
            fold.feed(batch);
            // Rows that did not decode still count as read.
            fold.after = last;
            if read >= budget && watermark(&fold, last_boundary) > start {
                more = n == BATCH;
                break;
            }
        }
        let indexed_seq = watermark(&fold, last_boundary);
        let items = fold.finish(Vec::new());
        self.write_index(thread, start, indexed_seq, &items, &ats)?;
        Ok(IndexProgress { indexed_seq, more })
    }

    fn write_index(
        &self,
        thread: &str,
        start: i64,
        indexed_seq: i64,
        items: &[TranscriptItem],
        ats: &HashMap<i64, i64>,
    ) -> Result<()> {
        let tx = self.write_txn()?;
        {
            let mut known = self.conn.prepare_cached(
                "SELECT first_seq FROM search_items WHERE thread_id = ?1 AND item_id = ?2",
            )?;
            let mut upsert = self.conn.prepare_cached(
                "INSERT INTO search_items (thread_id, item_id, first_seq, role, at, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (thread_id, item_id) DO UPDATE SET
                     first_seq = excluded.first_seq, role = excluded.role, at = excluded.at,
                     text = excluded.text
                 WHERE search_items.text IS NOT excluded.text
                    OR search_items.role IS NOT excluded.role
                    OR search_items.first_seq IS NOT excluded.first_seq",
            )?;
            let mut remove = self
                .conn
                .prepare_cached("DELETE FROM search_items WHERE thread_id = ?1 AND item_id = ?2")?;
            for item in items {
                let Some((role, text)) = searchable(item) else {
                    continue;
                };
                let id = &item.message.item_id;
                let stored: Option<i64> = known
                    .query_row(params![thread, id], |r| r.get(0))
                    .optional()?;
                // Settled before this pass began: what came after its start was not read now.
                if stored.is_some_and(|first| first <= start) {
                    continue;
                }
                let text = index_text(text);
                if text.is_empty() {
                    if stored.is_some() {
                        remove.execute(params![thread, id])?;
                    }
                    continue;
                }
                let at = ats.get(&item.first_seq).copied().unwrap_or(0);
                upsert.execute(params![thread, id, item.first_seq, role, at, text])?;
            }
        }
        self.conn.execute(
            "INSERT INTO search_state (thread_id, indexed_seq) VALUES (?1, ?2)
             ON CONFLICT (thread_id) DO UPDATE SET
                 indexed_seq = MAX(search_state.indexed_seq, excluded.indexed_seq)",
            params![thread, indexed_seq],
        )?;
        tx.commit()
    }

    /// The threads with events the index has not taken for good yet, the most recently
    /// updated first: what the start-up backfill indexes. A thread whose last item never
    /// finished (its agent stopped mid-reply) stays here, so it is read again from that item.
    /// A delegated worker's hidden backing thread is never listed.
    pub fn search_pending(&self) -> Result<Vec<ThreadId>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT t.id FROM threads t
             LEFT JOIN search_state s ON s.thread_id = t.id
             WHERE COALESCE(s.indexed_seq, 0) <
                   COALESCE((SELECT MAX(e.seq) FROM events e WHERE e.thread_id = t.id), 0)
               AND {NOT_HIDDEN_CHILD}
             ORDER BY t.updated_at DESC, t.rowid DESC"
        ))?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Finds `query` (what the user typed: [`agent_core::search_query::compile`]) in the indexed
    /// items of every visible thread, archived ones only with `include_archived`, the best
    /// `limit` first. Its `#tag` terms are never searched for: they keep only the threads that
    /// hold every one of those tags. A read: it takes no lock, and runs on any connection.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        include_archived: bool,
    ) -> Result<SearchResults> {
        if !self.search_fts_present()? {
            return Ok(SearchResults::Unavailable);
        }
        let Some(matching) = agent_core::search_query::compile(query) else {
            return Ok(SearchResults::Hits(Vec::new()));
        };
        // `#tag` terms never reach the MATCH: they keep the hits of threads holding every one.
        let tags = agent_core::search_query::SearchQuery::parse(query).tags;
        if tags.len() > agent_core::thread_tag::MAX_TAGS {
            return Ok(SearchResults::Hits(Vec::new())); // no thread holds that many
        }
        let sql = format!(
            "SELECT i.thread_id, i.item_id, i.first_seq, i.role, i.at,
                    snippet(search_fts, 0, ?2, ?3, '…', 16), search_fts.rank, t.archived
             FROM search_fts
             JOIN search_items i ON i.id = search_fts.rowid
             JOIN threads t ON t.id = i.thread_id
             WHERE search_fts MATCH ?1 AND (?4 OR t.archived = 0) AND {NOT_HIDDEN_CHILD}
               AND (SELECT COUNT(*) FROM thread_tags g
                    WHERE g.thread_id = i.thread_id
                      AND g.tag IN (SELECT value FROM json_each(?6))) = ?7
             ORDER BY search_fts.rank, i.id LIMIT ?5"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            params![
                matching,
                SNIPPET_OPEN.to_string(),
                SNIPPET_CLOSE.to_string(),
                include_archived,
                limit.min(i64::MAX as usize) as i64,
                serde_json::to_string(&tags)?,
                tags.len() as i64
            ],
            |r| {
                Ok(SearchHit {
                    thread_id: r.get(0)?,
                    item_id: r.get(1)?,
                    first_seq: r.get(2)?,
                    role: r.get(3)?,
                    at: r.get(4)?,
                    snippet: r.get(5)?,
                    rank: r.get(6)?,
                    archived: r.get(7)?,
                })
            },
        )?;
        Ok(SearchResults::Hits(rows.collect::<rusqlite::Result<_>>()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SearchIndex;
    use crate::store_writer::{StoreHandle, StoreWriter};
    use agent_core::event::{Envelope, ItemKind, ItemStatus, StreamKind, TurnState};
    use serde_json::json;
    use std::rc::Rc;

    const TOKEN: &str = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123");

    fn store() -> Store {
        let s = Store::open_in_memory().expect("open");
        assert_eq!(s.ensure_search_fts().expect("fts"), SearchIndex::Ready);
        s
    }

    fn started(
        item: &str,
        kind: ItemKind,
        title: &str,
        input: Option<serde_json::Value>,
    ) -> Envelope {
        Envelope::new(Event::ItemStarted {
            kind,
            title: title.to_owned(),
            input,
            parent: None,
        })
        .item(item)
    }

    fn delta(item: &str, stream: StreamKind, text: &str) -> Envelope {
        Envelope::new(Event::ContentDelta {
            stream,
            text: text.to_owned(),
        })
        .item(item)
    }

    fn completed(item: &str, output: Option<&str>) -> Envelope {
        Envelope::new(Event::ItemCompleted {
            status: ItemStatus::Completed,
            output: output.map(str::to_owned),
            error: None,
        })
        .item(item)
    }

    fn turn_end() -> Envelope {
        Envelope::turn_completed(TurnState::Completed, None)
    }

    fn append(s: &Store, t: &str, events: impl IntoIterator<Item = Envelope>) {
        for e in events {
            s.append_event(t, None, &e).expect("append");
        }
    }

    /// A reply `item` saying `text`, complete.
    fn reply(item: &str, text: &str) -> Vec<Envelope> {
        vec![
            started(item, ItemKind::AssistantMessage, "", None),
            delta(item, StreamKind::Assistant, text),
            completed(item, None),
        ]
    }

    fn rows(s: &Store, t: &str) -> Vec<(String, i64, String, i64, String)> {
        s.conn
            .prepare(
                "SELECT item_id, first_seq, role, at, text FROM search_items
                 WHERE thread_id = ?1 ORDER BY first_seq, item_id",
            )
            .expect("prepare")
            .query_map(params![t], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows")
    }

    fn hits(s: &Store, query: &str) -> Vec<SearchHit> {
        match s.search(query, 50, true).expect("search") {
            SearchResults::Hits(hits) => hits,
            SearchResults::Unavailable => panic!("the index exists"),
        }
    }

    fn fts_matches(s: &Store, phrase: &str) -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM search_fts WHERE search_fts MATCH ?1",
                params![format!("\"{phrase}\"")],
                |r| r.get(0),
            )
            .expect("match")
    }

    fn integrity(s: &Store) {
        s.conn
            .execute_batch("INSERT INTO search_fts (search_fts) VALUES ('integrity-check')")
            .expect("the index matches its content");
    }

    fn index_all(s: &Store, t: &str) -> IndexProgress {
        s.index_thread(t, usize::MAX).expect("index")
    }

    #[test]
    fn user_messages_replies_and_tool_titles_are_indexed() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        let image = agent_core::attachment::AttachmentRef {
            id: "a".repeat(64),
            kind: Default::default(),
            mime: "image/png".to_owned(),
            width: 1,
            height: 1,
            bytes: 3,
            name: Some("chart.png".to_owned()),
            origin: Default::default(),
        };
        s.append_user_input(&t, None, "please look at the parser", &[image])
            .expect("message");
        append(&s, &t, reply("r1", "The parser trips on tabs."));
        append(
            &s,
            &t,
            [
                started(
                    "c1",
                    ItemKind::Command,
                    "Bash",
                    Some(json!({"command": "cargo test --workspace"})),
                ),
                completed("c1", None),
                turn_end(),
            ],
        );
        let progress = index_all(&s, &t);
        assert!(!progress.more);
        let got = rows(&s, &t);
        let roles: Vec<_> = got.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool"]);
        assert!(got[0].4.contains("parser") && got[0].4.contains("[image: chart.png]"));
        assert_eq!(got[1].4, "The parser trips on tabs.");
        assert!(got[2].4.contains("cargo test"), "{}", got[2].4);
        assert!(got.iter().all(|r| r.3 > 0), "each row has its time");
        assert_eq!(
            progress.indexed_seq,
            s.conn
                .query_row("SELECT MAX(seq) FROM events", [], |r| r.get::<_, i64>(0))
                .expect("max")
        );
        assert_eq!(hits(&s, "parser").len(), 2);
        assert_eq!(hits(&s, "workspace").len(), 1);
        integrity(&s);
    }

    #[test]
    fn tool_output_reasoning_notices_and_raw_frames_are_never_indexed() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        let mut raw = started(
            "c1",
            ItemKind::Command,
            "Bash",
            Some(json!({"command": "ls"})),
        );
        raw.raw = Some(json!({"frame": "rawframeword"}));
        append(
            &s,
            &t,
            [
                raw,
                delta("c1", StreamKind::ToolOutput, "streamedoutputword"),
                completed("c1", Some("finaloutputword")),
                started("th", ItemKind::Reasoning, "", None),
                delta("th", StreamKind::Reasoning, "privatethoughtword"),
                completed("th", None),
                Envelope::new(Event::Notice {
                    text: "noticeword".to_owned(),
                }),
                started("tool2", ItemKind::FileRead, "Read", None),
                completed("tool2", Some("fileoutputword")),
                turn_end(),
            ],
        );
        index_all(&s, &t);
        for word in [
            "streamedoutputword",
            "finaloutputword",
            "privatethoughtword",
            "noticeword",
            "rawframeword",
            "fileoutputword",
        ] {
            assert_eq!(fts_matches(&s, word), 0, "{word}");
            assert!(hits(&s, word).is_empty(), "{word}");
        }
        let texts: Vec<_> = rows(&s, &t).into_iter().map(|r| r.4).collect();
        assert!(
            texts.iter().all(|t| !t.contains("output")),
            "only titles: {texts:?}"
        );
        assert!(!texts.is_empty(), "the tool titles are there");
    }

    #[test]
    fn a_secret_split_across_deltas_is_never_indexed() {
        let s = store();
        // Off, the fragments are stored as they came, so only the index's own redaction of the
        // whole text stands between them and the index until the item completes.
        s.set_delta_gate(false);
        let t = s.create_thread("/w", None).expect("thread");
        let (a, b) = TOKEN.split_at(12);
        append(
            &s,
            &t,
            [
                started("r1", ItemKind::AssistantMessage, "", None),
                delta("r1", StreamKind::Assistant, &format!("the token is {a}")),
                delta("r1", StreamKind::Assistant, &format!("{b} keep it safe")),
            ],
        );
        // Indexed open: the text is joined, so the secret is whole there, and masked.
        index_all(&s, &t);
        let check = |s: &Store| {
            for (_, _, _, _, text) in rows(s, &t) {
                assert!(!text.contains(a) && !text.contains(b), "{text}");
                assert!(text.contains("keep it safe"), "{text}");
            }
            assert!(hits(s, TOKEN).is_empty());
            assert!(hits(s, a).is_empty());
            assert_eq!(fts_matches(s, &b[..8]), 0);
        };
        check(&s);
        // The item completes and its rows are healed: the next pass reads it again.
        append(&s, &t, [completed("r1", None), turn_end()]);
        index_all(&s, &t);
        check(&s);
        assert_eq!(rows(&s, &t).len(), 1);
        integrity(&s);
    }

    #[test]
    fn index_thread_is_idempotent_and_resumable() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        // Over two batches of events, so a small budget stops between them.
        for n in 0..400 {
            s.append_user_message(&t, None, &format!("question number{n}"))
                .expect("message");
            append(
                &s,
                &t,
                reply(&format!("r{n}"), &format!("answer number{n}")),
            );
            append(&s, &t, [turn_end()]);
        }
        // A tiny budget: many calls, each stopping as soon as it has moved the watermark.
        let mut calls = 0;
        let mut last = 0;
        loop {
            let progress = s.index_thread(&t, 1).expect("index");
            assert!(
                progress.indexed_seq >= last,
                "the watermark never goes back"
            );
            last = progress.indexed_seq;
            calls += 1;
            assert!(calls < 1000, "it always makes progress");
            if !progress.more {
                break;
            }
        }
        assert!(calls > 1, "it stopped part-way at least once");
        let resumed = rows(&s, &t);
        assert_eq!(resumed.len(), 800);

        // The same thread indexed from scratch in one call gives the same rows.
        s.conn
            .execute_batch("DELETE FROM search_items; DELETE FROM search_state;")
            .expect("reset");
        assert_eq!(rows(&s, &t), Vec::new());
        assert!(!index_all(&s, &t).more);
        assert_eq!(rows(&s, &t), resumed);

        // Once more changes nothing, and writes nothing to the index.
        let changes = |s: &Store| -> i64 {
            s.conn
                .query_row("SELECT total_changes()", [], |r| r.get(0))
                .expect("changes")
        };
        let before = changes(&s);
        index_all(&s, &t);
        assert_eq!(rows(&s, &t), resumed);
        assert_eq!(changes(&s) - before, 1, "only the watermark row is touched");
        integrity(&s);
        assert!(s.search_pending().expect("pending").is_empty());
    }

    #[test]
    fn an_open_reply_holds_the_watermark_until_it_is_finished() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        s.append_user_message(&t, None, "tell me a story")
            .expect("message");
        append(
            &s,
            &t,
            [
                started("r1", ItemKind::AssistantMessage, "", None),
                delta("r1", StreamKind::Assistant, "once upon "),
            ],
        );
        let first = index_all(&s, &t);
        assert_eq!(hits(&s, "upon").len(), 1, "open text is searchable already");
        assert!(s.search_pending().expect("pending").contains(&t));
        append(
            &s,
            &t,
            [
                delta("r1", StreamKind::Assistant, "a midnight dreary"),
                completed("r1", None),
                turn_end(),
            ],
        );
        let second = index_all(&s, &t);
        assert!(second.indexed_seq > first.indexed_seq);
        assert_eq!(hits(&s, "midnight").len(), 1);
        let texts: Vec<_> = rows(&s, &t).into_iter().map(|r| r.4).collect();
        assert_eq!(texts, ["tell me a story", "once upon a midnight dreary"]);
        assert!(s.search_pending().expect("pending").is_empty());
    }

    #[test]
    fn an_item_a_turn_end_left_open_does_not_hold_the_watermark() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        append(
            &s,
            &t,
            [
                started("r1", ItemKind::AssistantMessage, "", None),
                delta("r1", StreamKind::Assistant, "cut short by a crash "),
                Envelope::new(Event::SessionExited {
                    code: Some(1),
                    expected: false,
                }),
            ],
        );
        index_all(&s, &t);
        assert!(s.search_pending().expect("pending").is_empty());
        assert_eq!(hits(&s, "short").len(), 1);
    }

    #[test]
    fn deleting_a_thread_removes_its_index_rows() {
        let s = store();
        let gone = s.create_thread("/w", None).expect("thread");
        let kept = s.create_thread("/w", None).expect("thread");
        append(&s, &gone, reply("r", "needleword"));
        append(&s, &kept, reply("r", "haystackword"));
        index_all(&s, &gone);
        index_all(&s, &kept);
        s.delete_thread(&gone).expect("delete");
        assert_eq!(fts_matches(&s, "needleword"), 0);
        assert_eq!(fts_matches(&s, "haystackword"), 1);
        assert!(rows(&s, &gone).is_empty());
        integrity(&s);
        // A thread that is gone indexes as nothing, without an error.
        assert!(!s.index_thread(&gone, INDEX_BUDGET).expect("no error").more);
        assert!(rows(&s, &gone).is_empty());
    }

    #[test]
    fn search_ranks_snippets_and_respects_archived() {
        let s = store();
        let one = s.create_thread("/w", Some("one")).expect("thread");
        let two = s.create_thread("/w", Some("two")).expect("thread");
        append(
            &s,
            &one,
            reply("r", "a gentle mention of rust somewhere in here"),
        );
        append(&s, &two, reply("r", "rust rust rust, all about rust"));
        index_all(&s, &one);
        index_all(&s, &two);
        let found = hits(&s, "rust");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].thread_id, two, "the denser match ranks first");
        assert!(found[0].rank <= found[1].rank);
        assert!(
            found[1]
                .snippet
                .contains(&format!("{SNIPPET_OPEN}rust{SNIPPET_CLOSE}")),
            "{}",
            found[1].snippet
        );
        assert_eq!(found[0].role, "assistant");
        // A prefix of the last word matches as it is being typed.
        assert_eq!(hits(&s, "gent").len(), 1);

        s.set_archived(&two, true).expect("archive");
        let SearchResults::Hits(visible) = s.search("rust", 50, false).expect("search") else {
            panic!("ready");
        };
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].thread_id, one);
        let all = hits(&s, "rust");
        assert!(all.iter().any(|h| h.thread_id == two && h.archived));
        // Nothing to look for is no hit, never an error.
        assert!(hits(&s, "  \"*  ").is_empty());
    }

    #[test]
    fn tag_terms_filter_the_hits_and_are_never_searched_for() {
        let s = store();
        let one = s.create_thread("/w", Some("one")).expect("thread");
        let two = s.create_thread("/w", Some("two")).expect("thread");
        append(&s, &one, reply("r", "deploy the review build"));
        append(&s, &two, reply("r", "deploy notes"));
        index_all(&s, &one);
        index_all(&s, &two);
        s.set_tags(&one, &["review", "ops"]).expect("tag");
        s.set_tags(&two, &["ops"]).expect("tag");
        let threads = |query: &str| -> Vec<String> {
            let mut t: Vec<String> = hits(&s, query).into_iter().map(|h| h.thread_id).collect();
            t.sort();
            t
        };
        let mut both = vec![one.clone(), two.clone()];
        both.sort();
        assert_eq!(threads("deploy"), both);
        assert_eq!(threads("deploy #review"), std::slice::from_ref(&one));
        assert_eq!(threads("#OPS deploy"), both, "tags match lowercased");
        assert_eq!(
            threads("deploy #ops #review"),
            std::slice::from_ref(&one),
            "ANDed"
        );
        assert!(threads("deploy #nothing").is_empty());
        // `#review` is a filter, never a word: "review" alone finds the first thread's text,
        // but a tag term on its own searches no message.
        assert_eq!(threads("review"), std::slice::from_ref(&one));
        assert!(threads("#review").is_empty());
        assert!(threads("#ops #review #a #b #c #d #e #f #g deploy").is_empty());
    }

    fn image(name: &str) -> agent_core::attachment::AttachmentRef {
        agent_core::attachment::AttachmentRef {
            id: "b".repeat(64),
            kind: Default::default(),
            mime: "image/png".to_owned(),
            width: 1,
            height: 1,
            bytes: 3,
            name: Some(name.to_owned()),
            origin: Default::default(),
        }
    }

    fn max_seq(s: &Store) -> i64 {
        s.conn
            .query_row("SELECT MAX(seq) FROM events", [], |r| r.get(0))
            .expect("max")
    }

    #[test]
    fn an_image_only_message_never_holds_the_watermark() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        s.append_user_input(&t, None, "", &[image("diagram.png")])
            .expect("message");
        // No turn has ended: only the message itself says it is whole.
        let progress = index_all(&s, &t);
        assert_eq!(progress.indexed_seq, max_seq(&s));
        assert!(s.search_pending().expect("pending").is_empty());
        let texts: Vec<_> = rows(&s, &t).into_iter().map(|r| r.4).collect();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("[image: diagram.png]"), "{texts:?}");
    }

    #[test]
    fn the_inherited_hand_off_is_never_indexed() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        append(
            &s,
            &t,
            [Envelope::new(Event::HandoffSeeded {
                summary: "inheritedsummaryword".to_owned(),
                fence: "```".to_owned(),
                carried: 1,
                total: 1,
                acknowledged: true,
            })],
        );
        s.append_user_message(&t, None, "carry on")
            .expect("message");
        append(&s, &t, reply("r", "carrying on"));
        index_all(&s, &t);
        assert_eq!(fts_matches(&s, "inheritedsummaryword"), 0);
        let roles: Vec<_> = rows(&s, &t).into_iter().map(|r| r.2).collect();
        assert_eq!(roles, ["user", "assistant"]);
    }

    #[test]
    fn an_item_is_cut_at_the_cap_on_a_character_boundary() {
        let text = format!("{}é tail", "a".repeat(ITEM_CAP - 1));
        let kept = index_text(&text);
        assert_eq!(
            kept.len(),
            ITEM_CAP - 1,
            "the two-byte é does not fit whole"
        );
        assert!(kept.chars().all(|c| c == 'a'));
        let short = "short and whole";
        assert_eq!(index_text(short), short);
        assert_eq!(
            index_text(&format!("x{SNIPPET_OPEN}y{SNIPPET_CLOSE}z")),
            "xyz"
        );
    }

    #[test]
    fn a_secret_across_the_cap_is_masked_before_the_cut() {
        // The token starts a few bytes before the cap: cut first, its head would survive whole.
        let text = format!("{} {TOKEN} after", "a".repeat(ITEM_CAP - 12));
        let kept = index_text(&text);
        assert!(kept.len() <= ITEM_CAP);
        assert!(!kept.contains("ghp_"), "{}", &kept[kept.len() - 20..]);
        assert!(!kept.contains(&TOKEN[..8]));
        // And through the index itself.
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        s.append_user_message(&t, None, &text).expect("message");
        index_all(&s, &t);
        let stored = rows(&s, &t).remove(0).4;
        assert!(stored.len() <= ITEM_CAP && !stored.contains("ghp_"));
    }

    #[test]
    fn a_hidden_worker_thread_is_neither_indexed_nor_pending() {
        let s = store();
        let root = s.create_thread("/w", Some("root")).expect("root");
        let backing = s.create_thread("/w", Some("child")).expect("child");
        append(&s, &root, reply("r", "rootword"));
        append(&s, &backing, reply("r", "workerword"));
        s.conn
            .execute_batch(&format!(
                "BEGIN;
                 INSERT INTO delegation_roots (id, root_thread_id, root_worker_id, task_state,
                     policy_json, continuation_budget, created_at, updated_at)
                     VALUES ('r1', '{root}', 'w_root', 'active', '{{}}', 3, 1, 2);
                 INSERT INTO delegation_workers (id, root_id, parent_id, origin, depth, name,
                     driver, profile_id, launch_json, backing_thread_id, state, workspace_json,
                     delegation_status, created_at, updated_at)
                     VALUES ('w_root', 'r1', NULL, 'root', 0, 'root', 'local', 'p', '{{}}', NULL,
                             'running', '{{}}', 'available', 1, 2),
                            ('w_kid', 'r1', 'w_root', 'supervised', 1, 'kid', 'codex', 'p', '{{}}',
                             '{backing}', 'closed', '{{}}', 'available', 1, 2);
                 COMMIT;"
            ))
            .expect("a supervised child");
        assert_eq!(
            s.search_pending().expect("pending"),
            std::slice::from_ref(&root)
        );
        index_all(&s, &backing);
        index_all(&s, &root);
        assert!(rows(&s, &backing).is_empty());
        assert_eq!(fts_matches(&s, "workerword"), 0);
        assert!(hits(&s, "workerword").is_empty());
        assert_eq!(hits(&s, "rootword").len(), 1);
        assert!(s.search_pending().expect("pending").is_empty());
    }

    #[test]
    fn without_the_full_text_index_search_says_unavailable() {
        let s = Store::open_in_memory().expect("open");
        s.simulate_missing_fts5(true);
        assert!(matches!(
            s.ensure_search_fts().expect("no error"),
            SearchIndex::Unavailable(_)
        ));
        let t = s.create_thread("/w", None).expect("thread");
        append(&s, &t, reply("r", "words"));
        // Indexing still works: the rows are there for when the index is made.
        index_all(&s, &t);
        assert_eq!(rows(&s, &t).len(), 1);
        assert_eq!(
            s.search("words", 10, true).expect("no error"),
            SearchResults::Unavailable
        );
        s.simulate_missing_fts5(false);
        assert_eq!(s.ensure_search_fts().expect("fts"), SearchIndex::Ready);
        assert_eq!(hits(&s, "words").len(), 1);
    }

    #[test]
    fn no_query_text_makes_fts5_fail() {
        let s = store();
        let t = s.create_thread("/w", None).expect("thread");
        append(&s, &t, reply("r", "some ordinary text"));
        index_all(&s, &t);
        let alphabet: Vec<char> =
            "aZ09 \"*-^:()NEAROR AND NOT{}[],.;'\\/\t\u{e9}\u{6771}\u{2122}\u{E000}\u{301}_+\0"
                .chars()
                .collect();
        // A small deterministic generator: the same strings every run.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut inputs: Vec<String> = [
            "\"",
            "\"\"",
            "\0",
            "a\0b",
            "\"x\0y",
            "*",
            "-x",
            "NEAR",
            "NEAR(a b)",
            "a OR",
            "AND",
            "text:x",
            "^",
            "(",
            ")",
            "a\"b",
            "\u{301}",
            "\u{2122}x",
            "x*y",
            "\"a b\"*",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for _ in 0..2000 {
            let len = (next() % 24) as usize;
            inputs.push(
                (0..len)
                    .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                    .collect(),
            );
        }
        for input in inputs {
            assert!(
                s.search(&input, 5, true).is_ok(),
                "{input:?} -> {:?}",
                agent_core::search_query::compile(&input)
            );
        }
    }

    #[test]
    fn the_writer_indexes_after_a_turn_and_a_message_and_the_inline_store_in_place() {
        // Queued: a file store and its writer.
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let reader = Rc::new(Store::open(&path).expect("reader"));
        let writer = StoreWriter::spawn(&path).expect("writer");
        let queued = StoreHandle::Queued {
            reader: reader.clone(),
            writer: writer.clone(),
        };
        let inline = StoreHandle::Inline(Rc::new(store()));
        for handle in [&queued, &inline] {
            let t = handle.reader().create_thread("/w", None).expect("thread");
            handle
                .append_user_message(&t, None, "where is the config parsed")
                .expect("message");
            for e in reply("r1", "In config dot rs, early on.") {
                handle.append_event(&t, None, &e).expect("event");
            }
            let found = |query: &str| -> usize {
                let SearchResults::Hits(hits) = handle
                    .read(|s| s.search(query, 10, true))
                    .expect("caught up")
                    .expect("search")
                else {
                    panic!("ready");
                };
                hits.len()
            };
            assert_eq!(found("parsed"), 1, "a message is searchable at once");
            assert_eq!(found("early"), 0, "a reply, once its turn has ended");
            handle.append_event(&t, None, &turn_end()).expect("end");
            assert_eq!(found("early"), 1);
        }
        assert_eq!(writer.failures().0, 0);
    }
}
