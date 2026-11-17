//! Two to four transcripts read side by side, with no GTK and no I/O: what the compare window
//! draws. It knows nothing of how the transcripts came to be (two forks of one thread, or one
//! prompt sent to several agents): each column is a list of [`Entry`]s, already sliced to the
//! part worth comparing and already scrubbed by the store.
//!
//! Columns are lined up by turn ([`table`]): a turn is a prompt and everything up to the next
//! one, so row *k* holds the *k*-th turn of every column. Entries before a column's first
//! prompt (a turn that was still running where the comparison starts) make an [`RowKind::Earlier`]
//! row of their own, first.

use std::borrow::Cow;

use crate::text::truncate_bytes;

/// Fewest columns a comparison has.
pub const MIN_COLUMNS: usize = 2;
/// Most columns a comparison has.
pub const MAX_COLUMNS: usize = 4;
/// Most rows [`table`] lays out (the first ones); the rest are counted in [`Table::more`].
pub const MAX_ROWS: usize = 100;
/// Most bytes of one entry's text shown ([`clip`]).
pub const MAX_ENTRY_BYTES: usize = 16 * 1024;

/// One column of a comparison, as read from the store: a thread and the sequence number after
/// which its items are shown (0: all of them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub thread: String,
    pub after: i64,
}

/// What an [`Entry`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The user's message: it starts a turn.
    Prompt,
    /// The agent's reply text.
    Reply,
    /// A tool call, by its title (`Run cargo test`, `Edit src/lib.rs`).
    Tool,
}

impl Role {
    /// The role of a folded transcript item (`user`, `assistant`, `tool`); `None` for anything
    /// else (inherited context, notices), which a comparison leaves out.
    pub fn of(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::Prompt),
            "assistant" => Some(Self::Reply),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// One item of a column: a prompt, a reply or a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: Role,
    /// The message's text; for a tool, its title.
    pub text: String,
    /// `open`, `completed`, `failed`, `declined` or `interrupted`.
    pub status: String,
    /// The sequence number of the stored event the item began at.
    pub seq: i64,
}

impl Entry {
    /// The item ended without doing what it set out to.
    pub fn failed(&self) -> bool {
        matches!(self.status.as_str(), "failed" | "declined" | "interrupted")
    }

    /// The item has not finished yet.
    pub fn open(&self) -> bool {
        self.status == "open"
    }
}

/// Whether `n` columns make a comparison.
pub fn valid_columns(n: usize) -> bool {
    (MIN_COLUMNS..=MAX_COLUMNS).contains(&n)
}

/// `entries` split into what came before the first prompt (empty when it starts with one) and
/// its turns, each starting at a prompt.
pub fn turns(entries: &[Entry]) -> (&[Entry], Vec<&[Entry]>) {
    let first = entries
        .iter()
        .position(|e| e.role == Role::Prompt)
        .unwrap_or(entries.len());
    let (lead, rest) = entries.split_at(first);
    let mut out = Vec::new();
    let mut start = 0;
    for (i, e) in rest.iter().enumerate().skip(1) {
        if e.role == Role::Prompt {
            out.push(&rest[start..i]);
            start = i;
        }
    }
    if !rest.is_empty() {
        out.push(&rest[start..]);
    }
    (lead, out)
}

/// What a [`Row`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// What came before the first prompt of a column.
    Earlier,
    /// The turn of that number in every column, from 1.
    Turn(usize),
}

impl RowKind {
    /// The row's heading.
    pub fn label(self) -> String {
        match self {
            Self::Earlier => "Before the first message".to_owned(),
            Self::Turn(n) => format!("Turn {n}"),
        }
    }
}

/// One row of a comparison: a cell per column, empty where that column has no such turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row<'a> {
    pub kind: RowKind,
    pub cells: Vec<&'a [Entry]>,
}

/// The rows of a comparison ([`table`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table<'a> {
    pub rows: Vec<Row<'a>>,
    /// Rows past [`MAX_ROWS`], not laid out.
    pub more: usize,
}

/// `columns` lined up by turn: an [`RowKind::Earlier`] row when any column has entries before
/// its first prompt, then one row per turn, as many as the longest column has. At most
/// [`MAX_ROWS`] rows; the others are counted.
pub fn table(columns: &[Vec<Entry>]) -> Table<'_> {
    let split: Vec<(&[Entry], Vec<&[Entry]>)> = columns.iter().map(|c| turns(c)).collect();
    let mut rows = Vec::new();
    if split.iter().any(|(lead, _)| !lead.is_empty()) {
        rows.push(Row {
            kind: RowKind::Earlier,
            cells: split.iter().map(|(lead, _)| *lead).collect(),
        });
    }
    let most = split.iter().map(|(_, t)| t.len()).max().unwrap_or(0);
    for n in 0..most {
        rows.push(Row {
            kind: RowKind::Turn(n + 1),
            cells: split
                .iter()
                .map(|(_, t)| t.get(n).copied().unwrap_or(&[]))
                .collect(),
        });
    }
    let more = rows.len().saturating_sub(MAX_ROWS);
    rows.truncate(MAX_ROWS);
    Table { rows, more }
}

/// What a column holds, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub prompts: usize,
    pub replies: usize,
    pub tools: usize,
    /// Tool calls that failed, were declined or were interrupted.
    pub failed_tools: usize,
    /// Something is still running (an item not finished).
    pub running: bool,
}

/// `entries`, counted.
pub fn totals(entries: &[Entry]) -> Totals {
    let mut t = Totals::default();
    for e in entries {
        match e.role {
            Role::Prompt => t.prompts += 1,
            Role::Reply => t.replies += 1,
            Role::Tool => {
                t.tools += 1;
                if e.failed() {
                    t.failed_tools += 1;
                }
            }
        }
        t.running |= e.open();
    }
    t
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A column's one-line summary: "2 messages · 3 replies · 5 tool calls (1 failed)", "still
/// running" when it is, and "Nothing yet" for an empty column.
pub fn summary(t: &Totals) -> String {
    if *t == Totals::default() {
        return "Nothing yet".to_owned();
    }
    let mut tools = count(t.tools, "tool call", "tool calls");
    if t.failed_tools > 0 {
        tools.push_str(&format!(" ({} failed)", t.failed_tools));
    }
    let mut out = [
        count(t.prompts, "message", "messages"),
        count(t.replies, "reply", "replies"),
        tools,
    ]
    .join(" · ");
    if t.running {
        out.push_str(" · still running");
    }
    out
}

/// `text` as shown: at most [`MAX_ENTRY_BYTES`], cut at a character boundary, saying how much
/// was left out.
pub fn clip(text: &str) -> Cow<'_, str> {
    if text.len() <= MAX_ENTRY_BYTES {
        return Cow::Borrowed(text);
    }
    let kept = truncate_bytes(text, MAX_ENTRY_BYTES);
    let left = text[kept.len()..].chars().count();
    Cow::Owned(format!(
        "{kept}…\n\n({} not shown)",
        count(left, "more character", "more characters")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(role: Role, text: &str, seq: i64) -> Entry {
        Entry {
            role,
            text: text.to_owned(),
            status: "completed".to_owned(),
            seq,
        }
    }

    fn texts(cell: &[Entry]) -> Vec<&str> {
        cell.iter().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn roles_come_from_the_fold_and_leave_out_the_rest() {
        assert_eq!(Role::of("user"), Some(Role::Prompt));
        assert_eq!(Role::of("assistant"), Some(Role::Reply));
        assert_eq!(Role::of("tool"), Some(Role::Tool));
        assert_eq!(Role::of("inherited"), None);
        assert_eq!(Role::of("notice"), None);
    }

    #[test]
    fn two_to_four_columns_make_a_comparison() {
        let valid: Vec<usize> = (0..8).filter(|n| valid_columns(*n)).collect();
        assert_eq!(valid, [2, 3, 4]);
    }

    #[test]
    fn turns_start_at_each_prompt_and_keep_what_came_before() {
        let entries = [
            e(Role::Reply, "tail", 1),
            e(Role::Prompt, "p1", 2),
            e(Role::Tool, "Run ls", 3),
            e(Role::Reply, "r1", 4),
            e(Role::Prompt, "p2", 5),
        ];
        let (lead, turns) = turns(&entries);
        assert_eq!(texts(lead), ["tail"]);
        assert_eq!(turns.len(), 2);
        assert_eq!(texts(turns[0]), ["p1", "Run ls", "r1"]);
        assert_eq!(texts(turns[1]), ["p2"]);
        let (lead, none) = super::turns(&[]);
        assert!(lead.is_empty() && none.is_empty());
    }

    #[test]
    fn columns_line_up_by_turn_and_a_short_one_leaves_empty_cells() {
        let a = vec![
            e(Role::Prompt, "a1", 1),
            e(Role::Reply, "ra1", 2),
            e(Role::Prompt, "a2", 3),
            e(Role::Reply, "ra2", 4),
        ];
        let b = vec![e(Role::Prompt, "b1", 1), e(Role::Reply, "rb1", 2)];
        let columns = vec![a, b];
        let t = table(&columns);
        assert_eq!(t.more, 0);
        let kinds: Vec<RowKind> = t.rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, [RowKind::Turn(1), RowKind::Turn(2)]);
        assert_eq!(texts(t.rows[0].cells[0]), ["a1", "ra1"]);
        assert_eq!(texts(t.rows[0].cells[1]), ["b1", "rb1"]);
        assert_eq!(texts(t.rows[1].cells[0]), ["a2", "ra2"]);
        assert!(t.rows[1].cells[1].is_empty(), "b has no second turn");
    }

    #[test]
    fn an_earlier_row_comes_first_only_when_a_column_needs_it() {
        let a = vec![e(Role::Tool, "Edit x", 1), e(Role::Prompt, "a1", 2)];
        let b = vec![e(Role::Prompt, "b1", 3)];
        let columns = vec![a, b];
        let t = table(&columns);
        assert_eq!(t.rows[0].kind, RowKind::Earlier);
        assert_eq!(texts(t.rows[0].cells[0]), ["Edit x"]);
        assert!(t.rows[0].cells[1].is_empty());
        assert_eq!(t.rows[1].kind, RowKind::Turn(1));
        let columns = vec![vec![e(Role::Prompt, "a", 1)], Vec::new(), Vec::new()];
        let t = table(&columns);
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0].cells.len(), 3, "a cell per column");
        assert!(table(&[Vec::new(), Vec::new()]).rows.is_empty());
    }

    #[test]
    fn rows_past_the_cap_are_counted_not_laid_out() {
        let long: Vec<Entry> = (0..(MAX_ROWS as i64 + 5))
            .map(|i| e(Role::Prompt, "p", i))
            .collect();
        let columns = vec![long, Vec::new()];
        let t = table(&columns);
        assert_eq!(t.rows.len(), MAX_ROWS);
        assert_eq!(t.more, 5);
        assert_eq!(t.rows[0].kind, RowKind::Turn(1), "the first turns are kept");
    }

    #[test]
    fn a_summary_counts_and_says_what_failed_or_still_runs() {
        assert_eq!(summary(&totals(&[])), "Nothing yet");
        let mut failed = e(Role::Tool, "Run x", 3);
        failed.status = "failed".into();
        let mut open = e(Role::Reply, "…", 5);
        open.status = "open".into();
        let entries = [
            e(Role::Prompt, "p", 1),
            e(Role::Tool, "Run y", 2),
            failed,
            e(Role::Reply, "r", 4),
            open,
        ];
        let t = totals(&entries);
        assert_eq!(
            (t.prompts, t.replies, t.tools, t.failed_tools, t.running),
            (1, 2, 2, 1, true)
        );
        assert_eq!(
            summary(&t),
            "1 message · 2 replies · 2 tool calls (1 failed) · still running"
        );
        assert_eq!(
            summary(&totals(&[e(Role::Reply, "r", 1)])),
            "0 messages · 1 reply · 0 tool calls"
        );
    }

    #[test]
    fn long_text_is_clipped_at_a_character_and_says_how_much() {
        assert!(matches!(clip("short"), Cow::Borrowed("short")));
        let text = "é".repeat(MAX_ENTRY_BYTES);
        let shown = clip(&text);
        let kept = shown.split('…').next().unwrap_or_default();
        assert_eq!(
            kept.len(),
            MAX_ENTRY_BYTES,
            "cut on a boundary of a two-byte character"
        );
        let note = format!("({} more characters not shown)", MAX_ENTRY_BYTES / 2);
        assert!(shown.ends_with(&note), "the note says how much is left out");
    }
}
