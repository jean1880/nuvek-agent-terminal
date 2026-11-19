//! What a comparison of threads reads: a fork's link to its source ([`Store::fork_link`]), the
//! threads a thread can be compared with ([`Store::fork_pairings`]), the fork point taken with
//! the transcript a fork carries ([`Store::transcript_at_fork`]) and each column's items
//! ([`Store::compare_columns`]). All reads: the link is written with its thread by
//! [`Store::create_thread_record`] and goes with it in [`Store::delete_thread`].
//!
//! The items are the stored, scrubbed rows folded as a hand-off reads them
//! ([`super::TranscriptFold`], default options): no reasoning, no notices, and no inherited
//! context, which a fork brought with it and did not do.

use std::path::Path;

use agent_core::compare::{Entry, Role, Side};
use agent_core::event::{Envelope, Event};
use agent_core::fork_compare::{pairings, settle, unsettle, ForkLink, Pairing};
use rusqlite::params;

use super::{
    thread_fork_key, FoldOptions, Result, Store, StoreError, ThreadSummary, TranscriptMessage,
    THREAD_FORK_PREFIX,
};

/// What [`Store::keep_fork`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptFork {
    /// The notice for the kept thread ("Kept over “…”").
    pub notice: String,
    /// The worktree the archived thread was opened in, if it was.
    pub worktree: Option<crate::worktree::WorktreeInfo>,
}

/// [`Store::keep_fork`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepOutcome {
    Kept(KeptFork),
    /// Nothing was written, and why (said to the user).
    Refused(String),
}

/// What [`Store::keep_compare_group`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptGroup {
    pub archived: Vec<String>,
    pub worktrees: Vec<(String, crate::worktree::WorktreeInfo)>,
}

/// [`Store::keep_compare_group`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepGroupOutcome {
    Kept(KeptGroup),
    Refused(String),
}

/// The notice Keep This One writes into the kept thread, naming the archived one by its stored
/// (already scrubbed) title.
pub fn kept_notice(other_title: &str) -> String {
    let title = other_title.trim();
    if title.is_empty() {
        "Kept over the other thread, which is archived".to_owned()
    } else {
        format!("Kept over “{title}”, which is archived")
    }
}

/// One column of a comparison ([`Store::compare_columns`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareColumn {
    /// The thread's summary; `None` when it is gone.
    pub summary: Option<ThreadSummary>,
    /// Its items after the side's point, oldest first.
    pub entries: Vec<Entry>,
}

/// A fork comparison's two columns and the baseline each is compared against
/// ([`Store::fork_columns`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkColumns {
    pub columns: Vec<CompareColumn>,
    /// The baseline tree of each column, when the fork has one.
    pub bases: [Option<String>; 2],
    /// The thread kept over the other, when the pair was settled with Keep This One.
    pub kept: Option<String>,
    /// The paths each column's changes leave out ([`ForkLink::not_compared`]).
    pub not_compared: [Vec<String>; 2],
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

    /// The sequence number of `thread`'s last stored event (`0` if empty).
    pub fn max_seq(&self, thread: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
            params![thread],
            |r| r.get(0),
        )?)
    }

    /// `thread`'s transcript (what a fork's hand-off is built from) and the sequence number of
    /// its last stored event, read together: the fork point is exactly where the transcript
    /// ends.
    pub fn transcript_at_fork(&self, thread: &str) -> Result<(Vec<TranscriptMessage>, i64)> {
        let snapshot = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let messages = self.transcript_messages(thread)?;
        let last = self.max_seq(thread)?;
        snapshot.commit()?;
        Ok((messages, last))
    }

    /// Each side's thread summary and its items after the side's point, all read in one
    /// snapshot. A thread that is gone or hidden has no summary and no items.
    pub fn compare_columns(&self, sides: &[Side]) -> Result<Vec<CompareColumn>> {
        let snapshot = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let mut out = Vec::with_capacity(sides.len());
        for side in sides {
            let summary = if self.is_hidden_child_thread(&side.thread)? {
                None
            } else {
                self.thread_summary(&side.thread)?
            };
            let entries = match summary {
                Some(_) => self.compare_entries(&side.thread, side.after)?,
                None => Vec::new(),
            };
            out.push(CompareColumn { summary, entries });
        }
        snapshot.commit()?;
        Ok(out)
    }

    /// What a comparison of `thread` with `pairing`'s other thread reads, in one snapshot: each
    /// side's column ([`Self::compare_columns`] of [`Pairing::sides`]) and the baseline tree it
    /// is compared against ([`Pairing::bases`]), in the same order, and which one was kept
    /// ([`Pairing::kept`]).
    pub fn fork_columns(&self, thread: &str, pairing: &Pairing) -> Result<ForkColumns> {
        let snapshot = self.scope(rusqlite::TransactionBehavior::Deferred)?;
        let columns = self.compare_columns(&pairing.sides(thread))?;
        let own = self.fork_link(thread)?;
        let other = self.fork_link(&pairing.other)?;
        snapshot.commit()?;
        Ok(ForkColumns {
            columns,
            bases: pairing.bases(own.as_ref(), other.as_ref()),
            kept: pairing.kept(thread, own.as_ref(), other.as_ref()),
            not_compared: pairing.not_compared(own.as_ref(), other.as_ref()),
        })
    }

    /// Keep This One: keeps `kept` over `other`, its fork, source or sibling fork, in one
    /// transaction. `other` is archived (never deleted), the pair's links record the choice
    /// ([`settle`]), and, with `write_notice`, the kept thread gets a notice saying so, through
    /// the store's scrubbing like any event (without it, the caller's live session writes
    /// [`KeptFork::notice`] itself, after this transaction). Everything is checked again inside
    /// the transaction, and [`KeepOutcome::Refused`], writing nothing, when the two are no pair,
    /// either is archived already, the pair is settled already, or a link would lose another
    /// pair's record. A thread that is gone (or hidden) is [`StoreError::NotFound`].
    pub fn keep_fork(&self, kept: &str, other: &str, write_notice: bool) -> Result<KeepOutcome> {
        self.atomically(|s| {
            let summary = |thread: &str| -> Result<ThreadSummary> {
                s.thread_summary(thread)?
                    .filter(|_| !s.is_hidden_child_thread(thread).unwrap_or(true))
                    .ok_or_else(|| StoreError::NotFound(format!("thread {thread}")))
            };
            let kept_summary = summary(kept)?;
            let other_summary = summary(other)?;
            if let Some(archived) = [&kept_summary, &other_summary]
                .into_iter()
                .find(|t| t.archived)
            {
                let title = archived.title.trim();
                return Ok(KeepOutcome::Refused(if title.is_empty() {
                    "One of these two threads is archived already".to_owned()
                } else {
                    format!("“{title}” is archived already")
                }));
            }
            let other_title = other_summary.title;
            let kept_link = s.fork_link(kept)?;
            let other_link = s.fork_link(other)?;
            let links = match settle(kept, kept_link.as_ref(), other, other_link.as_ref()) {
                Ok(links) => links,
                Err(refused) => return Ok(KeepOutcome::Refused(refused.reason().to_owned())),
            };
            s.set_archived(other, true)?;
            for (fork, link) in &links {
                s.set_meta(&thread_fork_key(fork), &link.to_json())?;
            }
            let notice = kept_notice(&other_title);
            if write_notice {
                let pt = s.active_provider_thread(kept)?;
                s.append_event(
                    kept,
                    pt.as_deref(),
                    &Envelope::new(Event::Notice {
                        text: notice.clone(),
                    }),
                )?;
            }
            // Who else works in the worktree is asked afterwards, never in here: it resolves
            // every thread's folder, and a hung mount would hold every store write behind it.
            let worktree = s.thread_attributes(other)?.worktree;
            Ok(KeepOutcome::Kept(KeptFork { notice, worktree }))
        })
    }

    /// Who, other than thread `except`, works in the folder `path` or below it: any thread,
    /// archived or not, and a delegated sub-agent's hidden one too, paths compared once links
    /// are resolved. Its title (or what it is, for one with none), or `None`.
    pub fn worktree_occupant(&self, path: &Path, except: &str) -> Result<Option<String>> {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let root = canonical(path);
        let mut stmt = self
            .conn
            .prepare("SELECT id, title, cwd FROM threads ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, title, cwd) = row?;
            if id == except || !canonical(Path::new(&cwd)).starts_with(&root) {
                continue;
            }
            let title = title.unwrap_or_default();
            let title = title.trim();
            return Ok(Some(if self.is_hidden_child_thread(&id)? {
                "a delegated sub-agent".to_owned()
            } else if title.is_empty() {
                "another thread".to_owned()
            } else {
                format!("“{title}”")
            }));
        }
        Ok(None)
    }

    /// Undoes [`Self::keep_fork`] of `kept` over `other`: brings `other` back and clears the
    /// record from the links that still hold it. The notice stays in the kept thread's history.
    pub fn undo_keep_fork(&self, kept: &str, other: &str) -> Result<()> {
        self.atomically(|s| {
            s.set_archived(other, false)?;
            let kept_link = s.fork_link(kept)?;
            let other_link = s.fork_link(other)?;
            for (fork, link) in unsettle(kept, kept_link.as_ref(), other, other_link.as_ref()) {
                s.set_meta(&thread_fork_key(&fork), &link.to_json())?;
            }
            Ok(())
        })
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

    /// Saves `group` under `compare_group:<id>` (redacted on write) and links every participant
    /// thread via `compare_of:<thread_id>` in a single transaction.
    pub fn save_compare_group(&self, group: &agent_core::ask_several::CompareGroup) -> Result<()> {
        let mut scrubbed = group.clone();
        scrubbed.prompt_preview = agent_core::redact::redact(&scrubbed.prompt_preview);
        for p in &mut scrubbed.participants {
            p.label = agent_core::redact::redact(&p.label);
        }
        let json = scrubbed.to_json();
        self.atomically(|s| {
            s.set_meta(
                &agent_core::ask_several::group_meta_key(&scrubbed.id),
                &json,
            )?;
            for p in &scrubbed.participants {
                s.set_meta(
                    &agent_core::ask_several::participant_meta_key(&p.thread_id),
                    &scrubbed.id,
                )?;
            }
            Ok(())
        })
    }

    /// Reads the [`agent_core::ask_several::CompareGroup`] recorded under `group_id`, if valid.
    pub fn compare_group(
        &self,
        group_id: &str,
    ) -> Result<Option<agent_core::ask_several::CompareGroup>> {
        Ok(self
            .meta(&agent_core::ask_several::group_meta_key(group_id))?
            .and_then(|json| agent_core::ask_several::CompareGroup::from_json(&json)))
    }

    /// Returns the compare group that participant `thread` belongs to, if any.
    pub fn compare_group_for_thread(
        &self,
        thread: &str,
    ) -> Result<Option<agent_core::ask_several::CompareGroup>> {
        let Some(group_id) = self.meta(&agent_core::ask_several::participant_meta_key(thread))?
        else {
            return Ok(None);
        };
        self.compare_group(&group_id)
    }

    /// Leave an answer as an ordinary thread. Historical comparisons keep their other sides.
    pub fn detach_compare_participant(&self, group_id: &str, thread: &str) -> Result<()> {
        self.atomically(|s| {
            let Some(mut group) = s.compare_group(group_id)? else {
                return Ok(());
            };
            group.participants.retain(|p| p.thread_id != thread);
            if group.kept_thread.as_deref() == Some(thread) {
                group.kept_thread = None;
            }
            s.delete_meta(&agent_core::ask_several::participant_meta_key(thread))?;
            if group.participants.is_empty() {
                s.delete_meta(&agent_core::ask_several::group_meta_key(group_id))
            } else {
                s.set_meta(
                    &agent_core::ask_several::group_meta_key(group_id),
                    &group.to_json(),
                )
            }
        })
    }

    /// Returns all compare groups whose `source_thread` matches `source`, newest first.
    pub fn compare_groups_for_source(
        &self,
        source: &str,
    ) -> Result<Vec<agent_core::ask_several::CompareGroup>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key LIKE 'compare_group:%'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut groups = Vec::new();
        for row in rows {
            if let Some(group) = agent_core::ask_several::CompareGroup::from_json(&row?) {
                if group.source_thread == source {
                    groups.push(group);
                }
            }
        }
        groups.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(groups)
    }

    /// Sidebar comparison parents in one bounded meta scan, with no query per thread.
    /// Ceiling: the newest 4096 stored group rows; malformed rows are ignored.
    pub fn compare_sources(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut statement = self.conn.prepare(
            "SELECT value FROM meta WHERE key LIKE 'compare_group:%' ORDER BY rowid DESC LIMIT 4096"
        )?;
        let mut groups = Vec::new();
        for row in statement.query_map([], |r| r.get::<_, String>(0))? {
            if let Some(group) = agent_core::ask_several::CompareGroup::from_json(&row?) {
                groups.push(group);
            }
        }
        groups.sort_by_key(|g| g.created_at);
        let mut parents = std::collections::HashMap::new();
        for group in groups {
            for participant in group.participants {
                parents.insert(participant.thread_id, group.source_thread.clone());
            }
        }
        Ok(parents)
    }

    /// Records `kept_thread` as the chosen participant of `group_id`. Refuses if already settled.
    pub fn mark_compare_group_kept(&self, group_id: &str, kept_thread: &str) -> Result<()> {
        let key = agent_core::ask_several::group_meta_key(group_id);
        self.atomically(|s| {
            let Some(mut group) = s
                .meta(&key)?
                .and_then(|json| agent_core::ask_several::CompareGroup::from_json(&json))
            else {
                return Err(StoreError::NotFound(format!("compare group {group_id}")));
            };
            if group.kept_thread.is_some() {
                return Err(StoreError::Failed(
                    "another participant has already been kept".to_owned(),
                ));
            }
            if !group
                .participants
                .iter()
                .any(|p| p.thread_id == kept_thread)
            {
                return Err(StoreError::Failed(format!(
                    "thread {kept_thread} is not a participant of {group_id}"
                )));
            }
            group.kept_thread = Some(kept_thread.to_owned());
            s.set_meta(&key, &group.to_json())
        })
    }

    /// Keep and persist the source's next-prompt handoff together. A source that changed
    /// during preparation, or still owes a handoff, leaves the entire group untouched.
    pub fn keep_compare_group_answer(
        &self,
        group_id: &str,
        kept_thread: &str,
        source_seq: i64,
        handoff_events: &[Envelope],
    ) -> Result<KeepGroupOutcome> {
        self.atomically(|s| {
            if !matches!(handoff_events.first().map(|e| &e.event),
                Some(Event::HandoffSeeded { carried, fence, .. }) if *carried > 0 && agent_core::handoff_budget::is_valid_fence(fence)) {
                return Ok(KeepGroupOutcome::Refused("The chosen answer has no valid handoff".into()));
            }
            let Some(group) = s.compare_group(group_id)? else {
                return Ok(KeepGroupOutcome::Refused("Comparison group no longer exists".into()));
            };
            if s.thread_summary(&group.source_thread)?.is_none()
                || s.is_hidden_child_thread(&group.source_thread)?
                || s.max_seq(&group.source_thread)? != source_seq {
                return Ok(KeepGroupOutcome::Refused("The source changed; try Keep again".into()));
            }
            let mut after = None;
            let mut owes = false;
            let mut legacy = false;
            loop {
                let batch = s.events(&group.source_thread, after, 1000)?;
                if batch.is_empty() { break; }
                for (seq, env) in batch {
                    match env.event {
                        Event::HandoffSeeded { carried, acknowledged, .. } => { owes = carried > 0; legacy = !acknowledged; }
                        Event::TurnCompleted { state: agent_core::event::TurnState::Completed, .. } if legacy => owes = false,
                        _ => {}
                    }
                    after = Some(seq);
                }
            }
            if owes {
                return Ok(KeepGroupOutcome::Refused("The source already holds an undelivered handoff".into()));
            }
            let outcome = s.keep_compare_group(group_id, kept_thread, "", false)?;
            if matches!(outcome, KeepGroupOutcome::Kept(_)) {
                let pt = s.active_provider_thread(&group.source_thread)?;
                for event in handoff_events { s.append_event(&group.source_thread, pt.as_deref(), event)?; }
            }
            Ok(outcome)
        })
    }

    /// Atomically keeps `kept_thread` in `group_id`: verifies the group is still unsettled, the
    /// winner exists and is not archived, archives every remaining live non-archived participant,
    /// records `kept_thread`, and appends `notice_text` to `source_thread` when
    /// `write_source_notice` is set.
    pub fn keep_compare_group(
        &self,
        group_id: &str,
        kept_thread: &str,
        notice_text: &str,
        write_source_notice: bool,
    ) -> Result<KeepGroupOutcome> {
        let key = agent_core::ask_several::group_meta_key(group_id);
        self.atomically(|s| {
            let Some(mut group) = s
                .meta(&key)?
                .and_then(|json| agent_core::ask_several::CompareGroup::from_json(&json))
            else {
                return Ok(KeepGroupOutcome::Refused(
                    "This comparison group no longer exists".to_owned(),
                ));
            };
            if group.kept_thread.is_some() {
                return Ok(KeepGroupOutcome::Refused(
                    "Another answer in this group was already kept".to_owned(),
                ));
            }
            if !group
                .participants
                .iter()
                .any(|p| p.thread_id == kept_thread)
            {
                return Ok(KeepGroupOutcome::Refused(
                    "That thread is not part of this comparison group".to_owned(),
                ));
            }
            let Some(kept_summary) = s
                .thread_summary(kept_thread)?
                .filter(|_| !s.is_hidden_child_thread(kept_thread).unwrap_or(true))
            else {
                return Ok(KeepGroupOutcome::Refused(
                    "The chosen participant thread no longer exists".to_owned(),
                ));
            };
            if kept_summary.archived {
                return Ok(KeepGroupOutcome::Refused(
                    "The chosen participant is already archived".to_owned(),
                ));
            }

            let mut archived = Vec::new();
            let mut worktrees = Vec::new();
            for p in &group.participants {
                if p.thread_id == kept_thread {
                    continue;
                }
                let Some(summary) = s
                    .thread_summary(&p.thread_id)?
                    .filter(|_| !s.is_hidden_child_thread(&p.thread_id).unwrap_or(true))
                else {
                    continue;
                };
                if summary.archived {
                    continue;
                }
                let wt = s.thread_attributes(&p.thread_id)?.worktree;
                s.set_archived(&p.thread_id, true)?;
                archived.push(p.thread_id.clone());
                if let Some(info) = wt {
                    worktrees.push((p.thread_id.clone(), info));
                }
            }

            group.kept_thread = Some(kept_thread.to_owned());
            s.set_meta(&key, &group.to_json())?;

            if write_source_notice && s.thread_summary(&group.source_thread)?.is_some() {
                let pt = s.active_provider_thread(&group.source_thread)?;
                s.append_event(
                    &group.source_thread,
                    pt.as_deref(),
                    &Envelope::new(Event::Notice {
                        text: notice_text.to_owned(),
                    }),
                )?;
            }

            Ok(KeepGroupOutcome::Kept(KeptGroup {
                archived,
                worktrees,
            }))
        })
    }

    /// Undo a specific Keep, withdrawing its handoff only if it still waits unsent under the
    /// same fence. A newer handoff, a prompt already sent, or a different settled choice wins.
    /// `withdraw` is false when a live source could not be reserved for withdrawal.
    pub fn undo_compare_group_answer(
        &self,
        group_id: &str,
        archived: &[String],
        kept: &str,
        source: &str,
        fence: &str,
        withdraw: bool,
    ) -> Result<bool> {
        self.atomically(|s| {
            if s.compare_group(group_id)?
                .and_then(|g| g.kept_thread)
                .as_deref()
                .is_some_and(|id| id != kept)
            {
                return Err(StoreError::Failed(
                    "A newer choice replaced this Keep".into(),
                ));
            }
            let mut pending = None;
            let mut legacy = false;
            let mut sent = false;
            let mut after = None;
            if withdraw {
                loop {
                    let events = s.events(source, after, 1000)?;
                    if events.is_empty() {
                        break;
                    }
                    for (seq, env) in events {
                        match env.event {
                            Event::HandoffSeeded {
                                carried,
                                fence,
                                acknowledged,
                                ..
                            } => {
                                pending = (carried > 0).then_some(fence);
                                legacy = !acknowledged;
                                sent = false;
                            }
                            Event::ItemStarted {
                                kind: agent_core::event::ItemKind::UserMessage,
                                ..
                            } => sent = true,
                            Event::TurnCompleted {
                                state: agent_core::event::TurnState::Completed,
                                ..
                            } if legacy => pending = None,
                            _ => {}
                        }
                        after = Some(seq);
                    }
                }
            }
            let cleared = withdraw && !sent && pending.as_deref() == Some(fence);
            if cleared {
                let pt = s.active_provider_thread(source)?;
                s.append_event(
                    source,
                    pt.as_deref(),
                    &Envelope::new(Event::HandoffSeeded {
                        summary: String::new(),
                        fence: String::new(),
                        carried: 0,
                        total: 0,
                        acknowledged: true,
                    }),
                )?;
                s.append_event(
                    source,
                    pt.as_deref(),
                    &Envelope::new(Event::Notice {
                        text:
                            "The chosen comparison context was withdrawn before delivery by Undo."
                                .into(),
                    }),
                )?;
            }
            s.undo_keep_compare_group(group_id, archived)?;
            Ok(cleared)
        })
    }

    /// Reverses [`Self::keep_compare_group`]: unarchives `archived` participants and clears
    /// `kept_thread` on `group_id`.
    pub fn undo_keep_compare_group(&self, group_id: &str, archived: &[String]) -> Result<()> {
        let key = agent_core::ask_several::group_meta_key(group_id);
        self.atomically(|s| {
            if let Some(mut group) = s
                .meta(&key)?
                .and_then(|json| agent_core::ask_several::CompareGroup::from_json(&json))
            {
                group.kept_thread = None;
                s.set_meta(&key, &group.to_json())?;
            }
            for id in archived {
                if s.thread_summary(id)?.is_some() {
                    s.set_archived(id, false)?;
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use agent_core::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind};
    use agent_core::fork_compare::Relation;

    use super::super::{thread_meta_keys, ThreadAttributes, ThreadRecord};
    use super::*;

    fn link(source: &str, seq: i64) -> ForkLink {
        ForkLink::new(source, seq)
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
    fn a_baseline_is_kept_with_the_link_and_an_earlier_builds_link_has_none() {
        let s = Store::open_in_memory().expect("store");
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let based = ForkLink {
            base_tree: Some(tree.into()),
            base_commit: Some(commit.into()),
            ..link("src", 2)
        };
        thread(&s, "src", None);
        thread(&s, "fork", Some(based.clone()));
        thread(&s, "old", None);
        // What a build before baselines wrote.
        s.set_meta(
            &thread_fork_key("old"),
            r#"{"source_thread":"src","fork_seq":2}"#,
        )
        .unwrap();
        assert_eq!(s.fork_link("fork").unwrap(), Some(based));
        assert_eq!(s.fork_link("old").unwrap(), Some(link("src", 2)));
        // Seen from the source, the fork's baseline is both sides'; a sibling with none has none.
        let pairings = s.fork_pairings("src").unwrap();
        let fork = pairings.iter().find(|p| p.other == "fork").expect("fork");
        let read = s.fork_columns("src", fork).unwrap();
        assert_eq!(read.bases, [Some(tree.into()), Some(tree.into())]);
        assert_eq!(read.columns.len(), 2);
        let sibling = s
            .fork_pairings("fork")
            .unwrap()
            .into_iter()
            .find(|p| p.other == "old")
            .expect("sibling");
        assert_eq!(
            s.fork_columns("fork", &sibling).unwrap().bases,
            [Some(tree.into()), None]
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

    /// A thread in `cwd`, titled `title`, carrying `fork` and `worktree`.
    fn placed(
        s: &Store,
        id: &str,
        cwd: &str,
        title: &str,
        fork: Option<ForkLink>,
        worktree: Option<crate::worktree::WorktreeInfo>,
    ) {
        s.create_thread_record(&ThreadRecord {
            title: Some(title),
            attributes: ThreadAttributes {
                fork,
                worktree,
                ..ThreadAttributes::default()
            },
            ..ThreadRecord::new(id, cwd)
        })
        .expect("thread");
    }

    fn notices(s: &Store, thread: &str) -> Vec<String> {
        s.events(thread, None, 1000)
            .expect("events")
            .into_iter()
            .filter_map(|(_, env)| match env.event {
                Event::Notice { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn keep_archives_the_other_and_never_removes_a_dirty_worktree() {
        use crate::test_support::{git as sh, git_or_skip, init_repo_at};
        use crate::worktree::{add_fork, check_new_branch, plan_fork, removal_blocker, remove};
        if !git_or_skip() {
            return;
        }
        let tmp = tempfile::tempdir().expect("dir");
        let main = tmp.path().join("work/main");
        std::fs::create_dir_all(&main).expect("dir");
        init_repo_at(&main, &[("a.txt", b"a\n")]);
        let main = main.canonicalize().expect("canonical");
        std::fs::write(main.join("a.txt"), "work in progress\n").expect("write");
        let made = add_fork(&main, &plan_fork(&main, "Plan", None).expect("plan")).expect("fork");
        let info = made.info.clone();

        let s = Store::open_in_memory().expect("store");
        let main_s = main.to_string_lossy().into_owned();
        let fork_s = made.dir.to_string_lossy().into_owned();
        placed(&s, "src", &main_s, "Plan", None, None);
        let link = ForkLink {
            base_tree: Some(made.baseline.tree.clone()),
            base_commit: Some(made.baseline.commit.clone()),
            ..link("src", 0)
        };
        placed(
            &s,
            "fork",
            &fork_s,
            "Plan (fork)",
            Some(link.clone()),
            Some(info.clone()),
        );
        placed(&s, "lone", &main_s, "Unrelated", None, None);

        let refused = |outcome: KeepOutcome| match outcome {
            KeepOutcome::Refused(why) => why,
            KeepOutcome::Kept(k) => panic!("kept: {k:?}"),
        };
        let took = |outcome: KeepOutcome| match outcome {
            KeepOutcome::Kept(k) => k,
            KeepOutcome::Refused(why) => panic!("refused: {why}"),
        };
        // Two threads that are no pair: refused, and nothing is written.
        assert!(refused(s.keep_fork("src", "lone", true).unwrap()).contains("not a fork"));
        refused(s.keep_fork("src", "src", true).unwrap());
        assert!(s.keep_fork("src", "missing", true).is_err());
        assert!(!s.thread_summary("lone").unwrap().unwrap().archived);
        assert!(notices(&s, "src").is_empty());

        // The fork's worktree holds the uncommitted work copied in: removal is not offered.
        assert!(removal_blocker(&info).is_some());

        // Keep the source: the fork is archived, never deleted, and the pair records it.
        let kept = took(s.keep_fork("src", "fork", true).expect("kept"));
        let fork = s.thread_summary("fork").unwrap().expect("never deleted");
        assert!(fork.archived);
        assert!(!s.thread_summary("src").unwrap().unwrap().archived);
        assert_eq!(kept.notice, "Kept over “Plan (fork)”, which is archived");
        assert_eq!(notices(&s, "src"), std::slice::from_ref(&kept.notice));
        assert!(notices(&s, "fork").is_empty());
        assert_eq!(kept.worktree.as_ref(), Some(&info));
        assert_eq!(s.worktree_occupant(&info.path, "fork").unwrap(), None);
        let settled = s.fork_link("fork").unwrap().expect("link");
        assert_eq!(
            settled.kept,
            Some(agent_core::fork_compare::Kept {
                thread: "src".into(),
                over: "fork".into()
            })
        );
        assert_eq!(settled.base_commit, link.base_commit, "the rest stays");
        // Seen from either side, the comparison knows who was kept.
        for (this, other) in [("src", "fork"), ("fork", "src")] {
            let pairing = s
                .fork_pairings(this)
                .unwrap()
                .into_iter()
                .find(|p| p.other == other)
                .expect("still paired: archived threads pair");
            assert_eq!(
                s.fork_columns(this, &pairing).unwrap().kept.as_deref(),
                Some("src")
            );
        }

        // Settled, or with one side archived: refused again inside the transaction, so a second
        // click (or a second window) writes nothing more.
        assert!(refused(s.keep_fork("src", "fork", true).unwrap()).contains("archived"));
        assert!(refused(s.keep_fork("fork", "src", true).unwrap()).contains("archived"));
        s.set_archived("fork", false).unwrap();
        assert!(refused(s.keep_fork("fork", "src", true).unwrap()).contains("already"));
        s.set_archived("fork", true).unwrap();
        assert_eq!(notices(&s, "src").len(), 1);

        // Worked in, its worktree is never removed: git refuses too, and the branch stays.
        std::fs::write(info.path.join("fork-only.txt"), "unsaved\n").expect("write");
        assert!(removal_blocker(&info).is_some());
        assert!(remove(&main, &info.path).is_err(), "never forced");
        assert!(info.path.join("fork-only.txt").exists());
        assert!(
            check_new_branch(&main, &info.branch).is_err(),
            "the branch stays"
        );
        assert_eq!(
            sh(&main, &["rev-parse", "HEAD"]),
            sh(&main, &["rev-parse", "main"])
        );

        // Undo brings it back and clears the record; the notice stays in the history.
        s.undo_keep_fork("src", "fork").expect("undo");
        assert!(!s.thread_summary("fork").unwrap().unwrap().archived);
        assert_eq!(s.fork_link("fork").unwrap(), Some(link.clone()));
        assert_eq!(notices(&s, "src").len(), 1);

        // Who else is in the archived one's worktree is named, archived or not, and a folder
        // beside it is no one: a sibling fork kept over it, working in it, blocks its removal.
        assert_eq!(s.worktree_occupant(&info.path, "fork").unwrap(), None);
        placed(
            &s,
            "beside",
            &format!("{}-other", info.path.display()),
            "Beside",
            None,
            None,
        );
        assert_eq!(s.worktree_occupant(&info.path, "fork").unwrap(), None);
        placed(&s, "src2", &fork_s, "Also here", Some(link.clone()), None);
        took(s.keep_fork("src2", "fork", false).expect("siblings"));
        assert_eq!(
            s.worktree_occupant(&info.path, "fork").unwrap().as_deref(),
            Some("“Also here”")
        );
        assert!(notices(&s, "src2").is_empty(), "the live session writes it");
        s.set_archived("src2", true).unwrap();
        assert_eq!(
            s.worktree_occupant(&info.path, "fork").unwrap().as_deref(),
            Some("“Also here”"),
            "an archived thread in it still counts"
        );
        // Through a link to the worktree, too.
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&info.path, &alias).expect("link");
        let other_store = Store::open_in_memory().expect("store");
        placed(
            &other_store,
            "linked",
            &alias.to_string_lossy(),
            "Linked",
            None,
            None,
        );
        assert_eq!(
            other_store
                .worktree_occupant(&info.path, "x")
                .unwrap()
                .as_deref(),
            Some("“Linked”")
        );
        s.set_archived("src2", false).unwrap();
        s.undo_keep_fork("src2", "fork").expect("undo");
        let kept = took(s.keep_fork("fork", "src", true).expect("fork kept"));
        assert!(s.thread_summary("src").unwrap().unwrap().archived);
        assert_eq!(kept.worktree, None, "the source has no worktree");
        assert_eq!(notices(&s, "fork"), ["Kept over “Plan”, which is archived"]);
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

    #[test]
    fn compare_group_persists_redacted_and_links_participants() {
        use agent_core::adapter::Driver;
        use agent_core::ask_several::{participant_meta_key, CompareGroup, CompareParticipant};
        let s = Store::open_in_memory().expect("store");
        thread(&s, "src", None);
        thread(&s, "p1", Some(link("src", 1)));
        thread(&s, "p2", Some(link("src", 1)));
        let group = CompareGroup {
            id: "g1".into(),
            source_thread: "src".into(),
            prompt_preview: "Inspect ghp_1234567890abcdefghijklmnopqrstuvwx".into(),
            created_at: 100,
            participants: vec![
                CompareParticipant {
                    thread_id: "p1".into(),
                    label: "Claude".into(),
                    driver: Driver::Claude,
                    profile_id: None,
                    model: None,
                    after_seq: 1,
                },
                CompareParticipant {
                    thread_id: "p2".into(),
                    label: "Codex".into(),
                    driver: Driver::Codex,
                    profile_id: None,
                    model: None,
                    after_seq: 1,
                },
            ],
            kept_thread: None,
        };
        s.save_compare_group(&group).expect("save");
        let by_thread = s
            .compare_group_for_thread("p1")
            .expect("lookup")
            .expect("present");
        assert!(!by_thread.prompt_preview.contains("ghp_"));
        assert_eq!(s.compare_groups_for_source("src").expect("list").len(), 1);
        let KeepGroupOutcome::Kept(done) = s
            .keep_compare_group("g1", "p2", "Kept Codex answer", true)
            .expect("keep")
        else {
            panic!("expected Kept");
        };
        assert_eq!(done.archived, vec!["p1".to_owned()]);
        assert_eq!(
            s.compare_group("g1")
                .expect("read")
                .and_then(|g| g.kept_thread),
            Some("p2".to_owned())
        );
        // Concurrent second keep is refused without mutating kept_thread or archiving p2.
        assert!(matches!(
            s.keep_compare_group("g1", "p1", "Other", true)
                .expect("second"),
            KeepGroupOutcome::Refused(_)
        ));
        assert!(
            !s.thread_summary("p2").unwrap().unwrap().archived,
            "winner must not be archived by a stale concurrent keep"
        );
        s.undo_keep_compare_group("g1", &done.archived)
            .expect("undo");
        assert!(
            s.compare_group("g1")
                .unwrap()
                .unwrap()
                .kept_thread
                .is_none(),
            "undo clears kept_thread"
        );
        assert!(
            !s.thread_summary("p1").unwrap().unwrap().archived,
            "undo restores archived participant"
        );

        s.delete_thread("p1").expect("delete participant");
        assert!(s.meta(&participant_meta_key("p1")).expect("meta").is_none());
    }

    #[test]
    fn ask_several_keep_is_atomic_persistent_and_refuses_pending_or_changed_source() {
        use agent_core::adapter::Driver;
        use agent_core::ask_several::{CompareGroup, CompareParticipant};
        let s = Store::open_in_memory().unwrap();
        for id in ["src", "p1", "p2"] {
            thread(&s, id, None);
        }
        let group = CompareGroup {
            id: "group".into(),
            source_thread: "src".into(),
            prompt_preview: "Question".into(),
            created_at: 1,
            kept_thread: None,
            participants: ["p1", "p2"]
                .into_iter()
                .map(|id| CompareParticipant {
                    thread_id: id.into(),
                    label: id.into(),
                    driver: Driver::Claude,
                    profile_id: None,
                    model: None,
                    after_seq: 0,
                })
                .collect(),
        };
        s.save_compare_group(&group).unwrap();
        let seed = Envelope::new(Event::HandoffSeeded {
            summary: "Chosen answer".into(),
            fence: "0123456789abcdef0123456789abcdef".into(),
            carried: 1,
            total: 1,
            acknowledged: true,
        });
        assert!(matches!(
            s.keep_compare_group_answer("group", "p1", -1, std::slice::from_ref(&seed))
                .unwrap(),
            KeepGroupOutcome::Refused(_)
        ));
        assert!(!s.thread_summary("p2").unwrap().unwrap().archived);
        s.append_event("src", None, &seed).unwrap();
        assert!(matches!(
            s.keep_compare_group_answer(
                "group",
                "p1",
                s.max_seq("src").unwrap(),
                std::slice::from_ref(&seed)
            )
            .unwrap(),
            KeepGroupOutcome::Refused(_)
        ));
        assert!(s
            .compare_group("group")
            .unwrap()
            .unwrap()
            .kept_thread
            .is_none());
        s.append_event(
            "src",
            None,
            &Envelope::new(Event::HandoffSeeded {
                summary: String::new(),
                fence: String::new(),
                carried: 0,
                total: 0,
                acknowledged: true,
            }),
        )
        .unwrap();
        let outcome = s
            .keep_compare_group_answer(
                "group",
                "p1",
                s.max_seq("src").unwrap(),
                std::slice::from_ref(&seed),
            )
            .unwrap();
        assert!(matches!(outcome, KeepGroupOutcome::Kept(_)));
        assert!(s.thread_summary("p2").unwrap().unwrap().archived);
        assert!(
            matches!(&s.events("src", None, 100).unwrap().last().unwrap().1.event, Event::HandoffSeeded { summary, carried: 1, .. } if summary == "Chosen answer")
        );
        assert_eq!(
            s.compare_sources().unwrap().get("p1").map(String::as_str),
            Some("src")
        );
        s.set_meta("compare_group:malformed", "not-json").unwrap();
        assert_eq!(s.compare_sources().unwrap().len(), 2);
        let fence = "0123456789abcdef0123456789abcdef";
        assert!(s
            .undo_compare_group_answer("group", &["p2".into()], "p1", "src", fence, true)
            .unwrap());
        assert!(!s.thread_summary("p2").unwrap().unwrap().archived);
        assert_eq!(
            s.events("src", None, 100)
                .unwrap()
                .iter()
                .rev()
                .find_map(|(_, e)| match e.event {
                    Event::HandoffSeeded { carried, .. } => Some(carried),
                    _ => None,
                }),
            Some(0)
        );
        assert!(matches!(
            s.keep_compare_group_answer(
                "group",
                "p1",
                s.max_seq("src").unwrap(),
                std::slice::from_ref(&seed)
            )
            .unwrap(),
            KeepGroupOutcome::Kept(_)
        ));
        s.append_user_message("src", None, "User has sent a message")
            .unwrap();
        assert!(
            !s.undo_compare_group_answer("group", &["p2".into()], "p1", "src", fence, true)
                .unwrap(),
            "cannot retract after prompt delivery"
        );
        assert!(
            !s.thread_summary("p2").unwrap().unwrap().archived,
            "sent context still permits restoring participants"
        );
        let spent = Envelope::new(Event::HandoffSeeded {
            summary: String::new(),
            fence: String::new(),
            carried: 0,
            total: 0,
            acknowledged: true,
        });
        s.append_event("src", None, &spent).unwrap();
        assert!(matches!(
            s.keep_compare_group_answer(
                "group",
                "p1",
                s.max_seq("src").unwrap(),
                std::slice::from_ref(&seed)
            )
            .unwrap(),
            KeepGroupOutcome::Kept(_)
        ));
        let newer_fence = "abcdef0123456789abcdef0123456789";
        let newer = Envelope::new(Event::HandoffSeeded {
            summary: "Newer unrelated handoff".into(),
            fence: newer_fence.into(),
            carried: 1,
            total: 1,
            acknowledged: true,
        });
        s.append_event("src", None, &newer).unwrap();
        assert!(!s
            .undo_compare_group_answer("group", &["p2".into()], "p1", "src", fence, true)
            .unwrap());
        assert!(
            matches!(&s.events("src", None, 100).unwrap().last().unwrap().1.event, Event::HandoffSeeded { fence, carried: 1, .. } if fence == newer_fence),
            "a newer handoff is untouched"
        );
        s.mark_compare_group_kept("group", "p1").unwrap();
        s.detach_compare_participant("group", "p1").unwrap();
        assert!(s.compare_group_for_thread("p1").unwrap().is_none());
        assert!(
            s.compare_group("group")
                .unwrap()
                .unwrap()
                .kept_thread
                .is_none(),
            "detaching the chosen thread clears its choice"
        );
        assert!(!s.compare_sources().unwrap().contains_key("p1"));
        assert_eq!(
            s.compare_group("group")
                .unwrap()
                .unwrap()
                .participants
                .len(),
            1
        );
        s.detach_compare_participant("group", "p2").unwrap();
        assert!(s.compare_group("group").unwrap().is_none());
    }
}
