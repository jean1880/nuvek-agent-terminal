//! What a comparison of threads reads: a fork's link to its source ([`Store::fork_link`]), the
//! threads a thread can be compared with ([`Store::fork_pairings`]), the fork point taken with
//! the transcript a fork carries ([`Store::transcript_at_fork`]) and each column's items
//! ([`Store::compare_columns`]). All reads: the link is written with its thread by
//! [`Store::create_thread_record`] and goes with it in [`Store::delete_thread`].
//!
//! The items are the stored, scrubbed rows folded as a hand-off reads them
//! ([`super::TranscriptFold`], default options): no reasoning, no notices, and no inherited
//! context, which a fork brought with it and did not do.

use agent_core::compare::{Entry, Role, Side};
use agent_core::fork_compare::{pairings, ForkLink, Pairing};
use rusqlite::params;

use super::{
    thread_fork_key, FoldOptions, Result, Store, ThreadSummary, TranscriptMessage,
    THREAD_FORK_PREFIX,
};

/// One column of a comparison ([`Store::compare_columns`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareColumn {
    /// The thread's summary; `None` when it is gone.
    pub summary: Option<ThreadSummary>,
    /// Its items after the side's point, oldest first.
    pub entries: Vec<Entry>,
}

impl Store {
    /// Where `thread` was forked from, if it is a fork. A malformed value is ignored (with a
    /// warning): the thread is then compared with nothing through it.
    pub fn fork_link(&self, thread: &str) -> Result<Option<ForkLink>> {
        Ok(self.meta(&thread_fork_key(thread))?.and_then(|json| {
            let link = ForkLink::parse(&json);
            if link.is_none() {
                tracing::warn!(%thread, "ignoring an unreadable fork link");
            }
            link
        }))
    }

    /// Every readable fork link, by fork, in key order.
    pub fn fork_links(&self) -> Result<Vec<(String, ForkLink)>> {
        // The prefix ends in ':', so every key that starts with it sorts below the same prefix
        // ending in ';'.
        let upper = format!("{};", THREAD_FORK_PREFIX.trim_end_matches(':'));
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM meta WHERE key >= ?1 AND key < ?2 ORDER BY key")?;
        let rows = stmt.query_map(params![THREAD_FORK_PREFIX, upper], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, json) = row?;
            let Some(fork) = key.strip_prefix(THREAD_FORK_PREFIX) else {
                continue;
            };
            match ForkLink::parse(&json) {
                Some(link) => out.push((fork.to_owned(), link)),
                None => tracing::warn!(thread = %fork, "ignoring an unreadable fork link"),
            }
        }
        Ok(out)
    }

    /// The threads `thread` can be compared with ([`agent_core::fork_compare::pairings`]): its
    /// source, its forks and its source's other forks, archived or not, but only those that
    /// still exist and are not a delegated worker's hidden thread.
    pub fn fork_pairings(&self, thread: &str) -> Result<Vec<Pairing>> {
        let snapshot = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let own = self.fork_link(thread)?;
        let links = self.fork_links()?;
        let mut visible = std::collections::HashSet::new();
        let candidates = own
            .iter()
            .map(|l| l.source_thread.as_str())
            .chain(links.iter().map(|(fork, _)| fork.as_str()));
        for candidate in candidates {
            if self.thread_summary(candidate)?.is_some()
                && !self.is_hidden_child_thread(candidate)?
            {
                visible.insert(candidate.to_owned());
            }
        }
        snapshot.commit()?;
        Ok(pairings(thread, own.as_ref(), &links, |t| {
            visible.contains(t)
        }))
    }

    /// `thread`'s transcript (what a fork's hand-off is built from) and the sequence number of
    /// its last stored event, read together: the fork point is exactly where the transcript
    /// ends.
    pub fn transcript_at_fork(&self, thread: &str) -> Result<(Vec<TranscriptMessage>, i64)> {
        let snapshot = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let messages = self.transcript_messages(thread)?;
        let last: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
            params![thread],
            |r| r.get(0),
        )?;
        snapshot.commit()?;
        Ok((messages, last))
    }

    /// Each side's thread summary and its items after the side's point, all read in one
    /// snapshot. A thread that is gone has no summary and no items.
    pub fn compare_columns(&self, sides: &[Side]) -> Result<Vec<CompareColumn>> {
        let snapshot = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let mut out = Vec::with_capacity(sides.len());
        for side in sides {
            let summary = self.thread_summary(&side.thread)?;
            let entries = match summary {
                Some(_) => self.compare_entries(&side.thread, side.after)?,
                None => Vec::new(),
            };
            out.push(CompareColumn { summary, entries });
        }
        snapshot.commit()?;
        Ok(out)
    }

    /// `thread`'s prompts, replies and tool calls whose first event came after `after`.
    fn compare_entries(&self, thread: &str, after: i64) -> Result<Vec<Entry>> {
        Ok(self
            .transcript_items(thread, FoldOptions::default())?
            .into_iter()
            .filter(|item| item.first_seq > after)
            .filter_map(|item| {
                let role = Role::of(&item.message.role)?;
                let text = match role {
                    Role::Tool if !item.message.title.is_empty() => item.message.title,
                    Role::Tool => "Tool".to_owned(),
                    Role::Prompt | Role::Reply => item.message.text,
                };
                Some(Entry {
                    role,
                    text,
                    status: item.message.status,
                    seq: item.first_seq,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use agent_core::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind};
    use agent_core::fork_compare::Relation;

    use super::super::{thread_meta_keys, ThreadAttributes, ThreadRecord};
    use super::*;

    fn link(source: &str, seq: i64) -> ForkLink {
        ForkLink {
            source_thread: source.to_owned(),
            fork_seq: seq,
        }
    }

    /// A thread whose record carries `fork`.
    fn thread(s: &Store, id: &str, fork: Option<ForkLink>) {
        s.create_thread_record(&ThreadRecord {
            attributes: ThreadAttributes {
                fork,
                ..ThreadAttributes::default()
            },
            ..ThreadRecord::new(id, "/w")
        })
        .expect("thread");
    }

    /// A user message and a reply in `thread`.
    fn exchange(s: &Store, thread: &str, n: usize, prompt: &str, reply: &str) {
        s.append_user_message(thread, None, prompt).expect("prompt");
        let agent = format!("a{n}");
        for env in [
            Envelope::item_started(&agent, ItemKind::AssistantMessage, "", None, None),
            Envelope::delta(&agent, StreamKind::Assistant, reply),
            Envelope::item_completed(&agent, ItemStatus::Completed, None, None),
        ] {
            s.append_event(thread, None, &env).expect("event");
        }
    }

    fn texts(column: &CompareColumn) -> Vec<&str> {
        column.entries.iter().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn fork_link_round_trips_and_malformed_is_ignored() {
        let s = Store::open_in_memory().expect("store");
        thread(&s, "src", None);
        thread(&s, "fork", Some(link("src", 7)));
        assert_eq!(s.fork_link("fork").unwrap(), Some(link("src", 7)));
        assert_eq!(
            s.thread_attributes("fork").unwrap().fork,
            Some(link("src", 7))
        );
        assert_eq!(s.fork_link("src").unwrap(), None);
        s.set_meta(&thread_fork_key("src"), "not json").unwrap();
        assert_eq!(s.fork_link("src").unwrap(), None);
        assert_eq!(
            s.thread_attributes("src").unwrap(),
            ThreadAttributes::default()
        );
        assert_eq!(
            s.fork_links().unwrap(),
            [("fork".to_owned(), link("src", 7))],
            "an unreadable link is left out of the list too"
        );
    }

    #[test]
    fn the_link_lists_only_fork_keys() {
        let s = Store::open_in_memory().expect("store");
        thread(&s, "a", Some(link("src", 1)));
        // Keys beside the prefix in sort order are not links.
        s.set_meta("thread_fork", "x").unwrap();
        s.set_meta("thread_fork;", "x").unwrap();
        s.set_meta("thread_forks:b", &link("src", 2).to_json())
            .unwrap();
        s.set_meta("thread_profile:a", "p").unwrap();
        assert_eq!(s.fork_links().unwrap(), [("a".to_owned(), link("src", 1))]);
    }

    #[test]
    fn deleting_a_thread_removes_its_fork_key() {
        let s = Store::open_in_memory().expect("store");
        thread(&s, "src", None);
        thread(&s, "fork", Some(link("src", 3)));
        assert!(thread_meta_keys("fork").contains(&thread_fork_key("fork")));
        s.delete_thread("fork").unwrap();
        for key in thread_meta_keys("fork") {
            assert_eq!(s.meta(&key).unwrap(), None, "{key}");
        }
        assert!(s.fork_pairings("src").unwrap().is_empty());
    }

    #[test]
    fn a_fork_link_survives_a_restart_and_the_source_being_deleted() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("threads.db");
        {
            let s = Store::open(&path).expect("store");
            thread(&s, "src", None);
            thread(&s, "b", Some(link("src", 4)));
            thread(&s, "c", Some(link("src", 9)));
        }
        let s = Store::open(&path).expect("reopened");
        let relations = |t: &str| -> Vec<(String, Relation)> {
            s.fork_pairings(t)
                .unwrap()
                .into_iter()
                .map(|p| (p.other, p.relation))
                .collect()
        };
        assert_eq!(
            relations("b"),
            [
                ("src".into(), Relation::Source),
                ("c".into(), Relation::Sibling)
            ]
        );
        s.delete_thread("src").unwrap();
        assert_eq!(
            s.fork_link("b").unwrap(),
            Some(link("src", 4)),
            "the fork keeps it"
        );
        assert_eq!(
            relations("b"),
            [("c".into(), Relation::Sibling)],
            "a source that is gone is not offered; the forks still pair"
        );
        // Archived threads still pair; hidden ones never do.
        s.set_archived("c", true).unwrap();
        assert_eq!(relations("b").len(), 1);
    }

    #[test]
    fn the_fork_point_is_where_the_transcript_ends() {
        let s = Store::open_in_memory().expect("store");
        thread(&s, "src", None);
        assert_eq!(s.transcript_at_fork("src").unwrap(), (Vec::new(), 0));
        exchange(&s, "src", 1, "hello", "hi");
        let (messages, seq) = s.transcript_at_fork("src").unwrap();
        assert_eq!(messages.len(), 2);
        let last = s.events("src", None, 100).unwrap().last().map(|(q, _)| *q);
        assert_eq!(Some(seq), last);
    }

    #[test]
    fn columns_slice_the_source_after_the_fork_point_and_drop_what_the_fork_inherited() {
        let s = Store::open_in_memory().expect("store");
        thread(&s, "src", None);
        exchange(&s, "src", 1, "before the fork", "earlier reply");
        let (_, fork_seq) = s.transcript_at_fork("src").unwrap();
        thread(&s, "fork", Some(link("src", fork_seq)));
        // The fork's first events: its inherited context and a notice.
        s.append_event(
            "fork",
            None,
            &Envelope::new(Event::HandoffSeeded {
                summary: "inherited context".into(),
                fence: "f".into(),
                carried: 2,
                total: 2,
                acknowledged: false,
            }),
        )
        .unwrap();
        exchange(&s, "src", 2, "source goes on", "source answer");
        exchange(&s, "fork", 3, "fork tries this", "fork answer");
        let pairing = &s.fork_pairings("fork").unwrap()[0];
        let columns = s.compare_columns(&pairing.sides("fork")).unwrap();
        assert_eq!(columns.len(), 2);
        assert_eq!(texts(&columns[0]), ["source goes on", "source answer"]);
        assert_eq!(texts(&columns[1]), ["fork tries this", "fork answer"]);
        assert_eq!(
            columns[0].summary.as_ref().map(|t| t.id.as_str()),
            Some("src")
        );
        let entries = [columns[0].entries.clone(), columns[1].entries.clone()];
        let table = agent_core::compare::table(&entries);
        assert_eq!(table.rows.len(), 1, "one turn each, side by side");
        // A thread that is gone is an empty column, never an error.
        let gone = s
            .compare_columns(&[Side {
                thread: "nope".into(),
                after: 0,
            }])
            .unwrap();
        assert_eq!(
            gone,
            [CompareColumn {
                summary: None,
                entries: Vec::new()
            }]
        );
    }
}
