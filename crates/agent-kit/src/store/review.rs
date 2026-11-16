//! A thread's unsent review: the comments its user wrote on diff lines in the diff panel, kept
//! until they are sent or discarded (`review_drafts`).
//!
//! Every quoted line and every note is redacted when it is written, as every persisted text is,
//! so a secret in a diff line never reaches the disk. The panel writes the whole draft list at
//! once ([`Store::replace_review_drafts`]): on the single writer, in order, the last write is what
//! stays, and no row id is ever needed by the app.

use agent_core::review::Side;
use rusqlite::params;

use super::{redact, Result, Store, StoreError};

/// One unsent comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDraft {
    /// Repository-relative, as the diff named it.
    pub path: String,
    pub side: Side,
    /// 1-based, inclusive, on `side`.
    pub start_line: u32,
    pub end_line: u32,
    /// The diff lines chosen, prefix kept. Redacted when written.
    pub quoted: Vec<String>,
    /// What the user wrote. Redacted when written.
    pub note: String,
    /// The diff base it was made on (`DiffBase::key`).
    pub diff_base: String,
    /// Epoch milliseconds.
    pub created_at: i64,
}

impl ReviewDraft {
    /// The draft as it is stored: its quote redacted whole (a secret split over two lines is
    /// whole there), then split back into lines, and its note redacted.
    pub fn redacted(&self) -> ReviewDraft {
        ReviewDraft {
            quoted: split_lines(&redact(&self.quoted.join("\n"))),
            note: redact(&self.note),
            ..self.clone()
        }
    }
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').map(str::to_owned).collect()
    }
}

impl Store {
    /// `thread`'s unsent review comments, by path, then line, then the order they were made.
    pub fn review_drafts(&self, thread: &str) -> Result<Vec<ReviewDraft>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, side, start_line, end_line, quoted, note, diff_base, created_at
             FROM review_drafts WHERE thread_id = ?1
             ORDER BY path, start_line, created_at, id",
        )?;
        let rows = stmt.query_map(params![thread], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, side, start, end, quoted, note, diff_base, created_at) = row?;
            // The schema's checks keep these in range; a row that is not is skipped, not guessed.
            let (Some(side), Ok(start_line), Ok(end_line)) =
                (Side::parse(&side), u32::try_from(start), u32::try_from(end))
            else {
                tracing::warn!(thread, "skipped an unreadable review draft");
                continue;
            };
            out.push(ReviewDraft {
                path,
                side,
                start_line,
                end_line,
                quoted: split_lines(&quoted),
                note,
                diff_base,
                created_at,
            });
        }
        Ok(out)
    }

    /// Makes `drafts` the whole of `thread`'s unsent review, in one transaction: an empty list
    /// deletes it (a review sent or discarded). Each draft is redacted before it is written. A
    /// draft with no line (a start of 0, or an end before its start) refuses the whole write.
    pub fn replace_review_drafts(&self, thread: &str, drafts: &[ReviewDraft]) -> Result<()> {
        if let Some(bad) = drafts
            .iter()
            .find(|d| d.start_line == 0 || d.end_line < d.start_line)
        {
            return Err(StoreError::Failed(format!(
                "a review comment on {} names no line",
                redact(&bad.path)
            )));
        }
        let tx = self.write_txn()?;
        self.conn.execute(
            "DELETE FROM review_drafts WHERE thread_id = ?1",
            params![thread],
        )?;
        for draft in drafts {
            let kept = draft.redacted();
            self.conn.execute(
                "INSERT INTO review_drafts (thread_id, path, side, start_line, end_line, quoted,
                     note, diff_base, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    thread,
                    kept.path,
                    kept.side.as_str(),
                    i64::from(kept.start_line),
                    i64::from(kept.end_line),
                    kept.quoted.join("\n"),
                    kept.note,
                    kept.diff_base,
                    kept.created_at,
                ],
            )?;
        }
        tx.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

    fn draft(path: &str, start: u32, quoted: &[&str], note: &str) -> ReviewDraft {
        ReviewDraft {
            path: path.into(),
            side: Side::New,
            start_line: start,
            end_line: start + quoted.len().saturating_sub(1) as u32,
            quoted: quoted.iter().map(|s| (*s).to_owned()).collect(),
            note: note.into(),
            diff_base: "uncommitted".into(),
            created_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn drafts_are_kept_redacted_and_read_back_in_order() {
        let s = Store::open_in_memory().expect("store");
        let t = s.create_thread("/repo", None).expect("thread");
        let secret = draft(
            "b.rs",
            3,
            &[&format!("+let t = \"{TOKEN}\";"), " x"],
            &format!("rotate {TOKEN} now"),
        );
        let mut removed = draft("a.rs", 9, &["-gone"], "why");
        removed.side = Side::Old;
        s.replace_review_drafts(&t, &[secret.clone(), removed.clone()])
            .expect("write");

        let back = s.review_drafts(&t).expect("read");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], removed, "by path first");
        assert_eq!(back[1].path, "b.rs");
        assert_eq!(back[1].quoted.len(), 2, "still two lines");
        assert_eq!(back[1].quoted[1], " x");
        assert_eq!((back[1].start_line, back[1].end_line), (3, 4));
        // Nothing of the secret was written, in the quote or the note.
        let raw: String = s
            .conn
            .query_row(
                "SELECT group_concat(quoted || note, '|') FROM review_drafts",
                [],
                |r| r.get(0),
            )
            .expect("raw");
        assert!(!raw.contains(TOKEN), "{raw}");
        assert!(!raw.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        assert_eq!(
            back[1],
            secret.redacted(),
            "what was kept is the redacted draft"
        );

        // A second write replaces the list; an empty one deletes it.
        s.replace_review_drafts(&t, &[removed.clone()])
            .expect("write");
        assert_eq!(s.review_drafts(&t).expect("read"), vec![removed]);
        s.replace_review_drafts(&t, &[]).expect("clear");
        assert!(s.review_drafts(&t).expect("read").is_empty());
    }

    #[test]
    fn drafts_are_per_thread_and_go_with_it() {
        let s = Store::open_in_memory().expect("store");
        let a = s.create_thread("/repo", None).expect("a");
        let b = s.create_thread("/repo", None).expect("b");
        s.replace_review_drafts(&a, &[draft("x.rs", 1, &["+x"], "n")])
            .expect("a");
        s.replace_review_drafts(&b, &[draft("y.rs", 1, &["+y"], "n")])
            .expect("b");
        s.replace_review_drafts(&a, &[]).expect("clear a");
        assert_eq!(s.review_drafts(&b).expect("b").len(), 1, "b's are b's own");
        s.delete_thread(&b).expect("delete");
        let left: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM review_drafts", [], |r| r.get(0))
            .expect("count");
        assert_eq!(left, 0, "deleting a thread deletes its drafts");
    }

    #[test]
    fn a_draft_with_no_line_refuses_the_write_and_keeps_what_was_there() {
        let s = Store::open_in_memory().expect("store");
        let t = s.create_thread("/repo", None).expect("thread");
        let good = draft("a.rs", 1, &["+a"], "n");
        s.replace_review_drafts(&t, std::slice::from_ref(&good))
            .expect("write");
        let mut zero = draft("a.rs", 1, &["+a"], "n");
        zero.start_line = 0;
        let mut backwards = draft("a.rs", 5, &["+a"], "n");
        backwards.end_line = 4;
        for bad in [zero, backwards] {
            assert!(s.replace_review_drafts(&t, &[good.clone(), bad]).is_err());
            assert_eq!(s.review_drafts(&t).expect("read"), vec![good.clone()]);
        }
    }

    #[test]
    fn a_draft_for_a_missing_thread_is_refused() {
        let s = Store::open_in_memory().expect("store");
        assert!(s
            .replace_review_drafts("no-such-thread", &[draft("a.rs", 1, &["+a"], "n")])
            .is_err());
    }
}
