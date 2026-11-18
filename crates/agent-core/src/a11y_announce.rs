//! What the app tells a screen reader when a thread needs the user: a turn that finished or
//! failed, an approval, question or form waiting, an error. The window shows the text in an
//! off-screen live region (a polite status for most, an alert for requests), which is what the
//! screen reader reads; it is never stored, logged or sent anywhere.
//!
//! The text is short and plain: built from fields the store already scrubs (a thread's title,
//! a tool's title, an error message), each passed through [`crate::redact::redact`] again, its
//! hidden characters dropped, its whitespace collapsed and cut to [`MAX_PART`] characters.
//! Nothing is said for an event a desktop notification already announced, so a screen reader
//! never reads the same thing twice.

use crate::event::{Event, QuestionSource, TurnState};

/// The most characters any one part of an announcement (a title, an error) keeps.
pub const MAX_PART: usize = 120;

/// How urgently a screen reader should speak an announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// After what it is saying now (a status live region).
    Polite,
    /// At once, interrupting (an alert): only for a request waiting on the user.
    Assertive,
}

/// One thing to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub text: String,
    pub urgency: Urgency,
}

/// What the window knows about the event's thread when it arrives.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The thread's title, as its page shows it.
    pub thread: &'a str,
    /// The user is looking at the thread: it is shown in the focused window.
    pub looking: bool,
    /// A desktop notification was raised for this very event.
    pub notified: bool,
    /// A finished turn finishes the thread: no sub-agent or background command still runs.
    pub finished: bool,
}

/// What to announce for `event`, if anything.
pub fn announce(event: &Event, cx: &Context<'_>) -> Option<Announcement> {
    if cx.notified {
        return None;
    }
    let polite = |text: String| {
        Some(Announcement {
            text,
            urgency: Urgency::Polite,
        })
    };
    let assertive = |text: String| {
        Some(Announcement {
            text,
            urgency: Urgency::Assertive,
        })
    };
    // Every variant is named, so a new one has to decide here whether it is said.
    match event {
        Event::TurnCompleted { state, error, .. } => match state {
            TurnState::Completed if cx.finished => polite(if cx.looking {
                "Reply finished.".to_owned()
            } else {
                format!("{} finished.", quoted(cx.thread))
            }),
            TurnState::Completed => None,
            // Stopped on purpose: nothing to say.
            TurnState::Interrupted => None,
            TurnState::Failed => polite(with_detail(
                &if cx.looking {
                    "Turn failed".to_owned()
                } else {
                    format!("{} failed", quoted(cx.thread))
                },
                error.as_deref(),
            )),
        },
        Event::ApprovalRequested { tool, title, .. } => {
            let what = part(title.as_deref().unwrap_or(tool));
            assertive(match (cx.looking, what.is_empty()) {
                (true, true) => "Approval needed.".to_owned(),
                (true, false) => format!("Approval needed: {what}."),
                (false, _) => format!("{} needs approval.", quoted(cx.thread)),
            })
        }
        Event::QuestionRequested { source, .. } => match source {
            QuestionSource::Agent => assertive(if cx.looking {
                "The agent has a question.".to_owned()
            } else {
                format!("{} has a question.", quoted(cx.thread))
            }),
            // Found in a finished reply: the reply itself is the news.
            QuestionSource::Reply => None,
        },
        Event::FormRequested { server, .. } => {
            let server = part(server);
            assertive(match (cx.looking, server.is_empty()) {
                (true, true) => "A tool asks you to fill in a form.".to_owned(),
                (true, false) => format!("{server} asks you to fill in a form."),
                (false, _) => format!("{} needs your input.", quoted(cx.thread)),
            })
        }
        Event::Error { message } => polite(with_detail(
            &if cx.looking {
                "Error".to_owned()
            } else {
                format!("Error in {}", quoted(cx.thread))
            },
            Some(message),
        )),
        Event::RateLimited { .. } => polite(if cx.looking {
            "The agent hit its usage limit.".to_owned()
        } else {
            format!("{} hit its usage limit.", quoted(cx.thread))
        }),
        Event::SessionExited { expected, code } => {
            (!expected && *code != Some(0)).then_some(())?;
            polite(if cx.looking {
                "The agent stopped unexpectedly.".to_owned()
            } else {
                format!("The agent in {} stopped unexpectedly.", quoted(cx.thread))
            })
        }
        Event::SessionStarted { .. }
        | Event::CommandsChanged { .. }
        | Event::TurnStarted { .. }
        | Event::ItemStarted { .. }
        | Event::ContentDelta { .. }
        | Event::ContentSnapshot { .. }
        | Event::ItemCompleted { .. }
        | Event::Attachments { .. }
        | Event::ToolImage { .. }
        | Event::Reasoning { .. }
        | Event::PlanUpdated { .. }
        | Event::ApprovalResolved { .. }
        | Event::ApprovalExpired
        | Event::QuestionResolved { .. }
        | Event::ModeChanged { .. }
        | Event::ModeChangeDeferred { .. }
        | Event::PermissionsForced { .. }
        | Event::FormResolved { .. }
        | Event::UsageUpdated { .. }
        | Event::Compacted { .. }
        | Event::QuotaUpdated { .. }
        | Event::ExtraUsageUpdated { .. }
        | Event::ModelChanged { .. }
        | Event::ModelChangeRequested { .. }
        | Event::ModelChangeFailed { .. }
        | Event::SubagentModel { .. }
        | Event::BackgroundTasks { .. }
        | Event::WorkersUpdated { .. }
        | Event::Notice { .. }
        | Event::DelegatedMessage { .. }
        | Event::WorkerLinked { .. }
        | Event::ModeChosen { .. }
        | Event::HandoffSeeded { .. }
        | Event::ControlResult { .. }
        | Event::Unknown => None,
    }
}

/// The most announcements one live-region update reads out; the rest are counted ("And 2
/// more.").
pub const MAX_SAID: usize = 3;
/// The most characters one live-region update holds, its count of the rest included.
pub const MAX_TEXT: usize = 480;
/// The most distinct announcements of one urgency held while they wait; past it they are only
/// counted.
pub const MAX_HELD: usize = 16;

/// Announcements made while one was waiting to be shown, joined: each urgency's texts in
/// order, a repeat dropped, at most [`MAX_SAID`] within [`MAX_TEXT`] characters, the rest
/// counted. The polite text first, then the assertive.
pub fn coalesce(pending: &[Announcement]) -> (Option<String>, Option<String>) {
    let mut held = Pending::default();
    for a in pending {
        held.push(a.clone());
    }
    (held.take(Urgency::Polite), held.take(Urgency::Assertive))
}

/// What waits to be shown, bounded: up to [`MAX_HELD`] distinct texts of each urgency, and a
/// count of those past it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pending {
    polite: Held,
    assertive: Held,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Held {
    texts: Vec<String>,
    /// Past [`MAX_HELD`]: said only as a count.
    more: usize,
}

impl Pending {
    fn held(&mut self, urgency: Urgency) -> &mut Held {
        match urgency {
            Urgency::Polite => &mut self.polite,
            Urgency::Assertive => &mut self.assertive,
        }
    }

    /// Holds `announcement` (a repeat of one held is dropped).
    pub fn push(&mut self, announcement: Announcement) {
        let held = self.held(announcement.urgency);
        if held.texts.contains(&announcement.text) {
            return;
        }
        if held.texts.len() < MAX_HELD {
            held.texts.push(announcement.text);
        } else {
            held.more += 1;
        }
    }

    /// Whether anything of `urgency` waits.
    pub fn has(&self, urgency: Urgency) -> bool {
        let held = match urgency {
            Urgency::Polite => &self.polite,
            Urgency::Assertive => &self.assertive,
        };
        !held.texts.is_empty() || held.more > 0
    }

    /// The text to show for `urgency`, joined and bounded, and nothing of it held any more.
    pub fn take(&mut self, urgency: Urgency) -> Option<String> {
        let held = std::mem::take(self.held(urgency));
        join(&held.texts, held.more)
    }

    /// Drops everything held.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// `texts` read one after another: the first [`MAX_SAID`] that fit in [`MAX_TEXT`] characters,
/// and "And N more." for the rest and `more` besides.
fn join(texts: &[String], more: usize) -> Option<String> {
    if texts.is_empty() && more == 0 {
        return None;
    }
    let total = texts.len() + more;
    let tail = |left: usize| (left > 0).then(|| format!("And {left} more."));
    // Room for the count however many are left out; one alone is never counted.
    let room = if total > 1 {
        MAX_TEXT - format!(" And {total} more.").chars().count()
    } else {
        MAX_TEXT
    };
    let mut said: Vec<&str> = Vec::new();
    let mut length = 0;
    for text in texts.iter().take(MAX_SAID) {
        let add = text.chars().count() + usize::from(!said.is_empty());
        if length + add > room && !said.is_empty() {
            break;
        }
        said.push(text);
        length += add;
    }
    let mut out = said.join(" ");
    if out.chars().count() > room {
        // One text longer than the room (parts are capped, so only a very long sentence).
        let cut: String = out.chars().take(room.saturating_sub(1)).collect();
        out = format!("{}…", cut.trim_end());
    }
    match tail(total - said.len()) {
        Some(tail) if out.is_empty() => Some(tail),
        Some(tail) => Some(format!("{out} {tail}")),
        None => Some(out),
    }
}

/// `title` in quotation marks, or "A thread" when it has none.
fn quoted(title: &str) -> String {
    let title = part(title);
    if title.is_empty() {
        "A thread".to_owned()
    } else {
        format!("“{title}”")
    }
}

/// `head`, then the first line of `detail` when there is one: "Turn failed: …".
fn with_detail(head: &str, detail: Option<&str>) -> String {
    let detail = detail
        .and_then(|d| d.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(part)
        .unwrap_or_default();
    if detail.is_empty() {
        format!("{head}.")
    } else {
        let stop = if detail.ends_with(['.', '!', '?', '…']) {
            ""
        } else {
            "."
        };
        format!("{head}: {detail}{stop}")
    }
}

/// `text` redacted, without hidden characters, its whitespace collapsed, cut to [`MAX_PART`]
/// characters (with an ellipsis).
pub fn part(text: &str) -> String {
    plain(text, MAX_PART)
}

/// `text` as one plain line a screen reader may be given: redacted, without control, bidi,
/// zero-width or other hidden characters, its whitespace collapsed, cut to `max` characters
/// with an ellipsis. Announcements' parts and transcript rows' names both come from it.
pub fn plain(text: &str, max: usize) -> String {
    let redacted = crate::redact::redact(text);
    let words: Vec<&str> = redacted
        .split(|c: char| c.is_whitespace() || crate::text::is_hidden(c))
        .filter(|w| !w.is_empty())
        .collect();
    let plain: String = words
        .join(" ")
        .chars()
        .filter(|c| !crate::text::is_hidden(*c))
        .collect();
    if plain.chars().count() <= max {
        return plain;
    }
    let cut: String = plain.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Decision, ResponseCapability};

    fn cx(looking: bool) -> Context<'static> {
        Context {
            thread: "Fix the build",
            looking,
            notified: false,
            finished: true,
        }
    }

    fn done(state: TurnState, error: Option<&str>) -> Event {
        crate::event::Envelope::turn_completed(state, error).event
    }

    fn approval(title: Option<&str>) -> Event {
        Event::ApprovalRequested {
            tool: "Bash".into(),
            title: title.map(str::to_owned),
            input: serde_json::Value::Null,
            reason: None,
            options: vec![Decision::Allow, Decision::Deny],
            response: ResponseCapability::Live,
            remembers: None,
        }
    }

    fn text(event: &Event, cx: &Context<'_>) -> Option<String> {
        announce(event, cx).map(|a| a.text)
    }

    #[test]
    fn a_finished_turn_is_said_politely_naming_the_thread_when_out_of_sight() {
        let here = announce(&done(TurnState::Completed, None), &cx(true)).unwrap();
        assert_eq!(here.text, "Reply finished.");
        assert_eq!(here.urgency, Urgency::Polite);
        assert_eq!(
            text(&done(TurnState::Completed, None), &cx(false)).as_deref(),
            Some("“Fix the build” finished.")
        );
    }

    #[test]
    fn a_turn_still_waiting_on_background_work_or_stopped_says_nothing() {
        let held = Context {
            finished: false,
            ..cx(true)
        };
        assert_eq!(announce(&done(TurnState::Completed, None), &held), None);
        assert_eq!(
            announce(&done(TurnState::Interrupted, None), &cx(true)),
            None
        );
    }

    #[test]
    fn a_failed_turn_says_the_first_line_of_its_error() {
        assert_eq!(
            text(
                &done(TurnState::Failed, Some("\n  Connection reset\nmore detail")),
                &cx(true)
            )
            .as_deref(),
            Some("Turn failed: Connection reset.")
        );
        assert_eq!(
            text(&done(TurnState::Failed, None), &cx(false)).as_deref(),
            Some("“Fix the build” failed.")
        );
    }

    #[test]
    fn requests_are_alerts() {
        let a = announce(&approval(Some("Run cargo test")), &cx(true)).unwrap();
        assert_eq!(a.urgency, Urgency::Assertive);
        assert_eq!(a.text, "Approval needed: Run cargo test.");
        assert_eq!(
            text(&approval(None), &cx(false)).as_deref(),
            Some("“Fix the build” needs approval.")
        );
        let question = Event::QuestionRequested {
            questions: Vec::new(),
            source: QuestionSource::Agent,
        };
        assert_eq!(
            announce(&question, &cx(true)).unwrap().urgency,
            Urgency::Assertive
        );
        let found = Event::QuestionRequested {
            questions: Vec::new(),
            source: QuestionSource::Reply,
        };
        assert_eq!(announce(&found, &cx(true)), None);
    }

    #[test]
    fn nothing_is_said_twice_when_a_notification_said_it() {
        let notified = Context {
            notified: true,
            ..cx(false)
        };
        for event in [
            done(TurnState::Completed, None),
            approval(None),
            Event::Error {
                message: "boom".into(),
            },
        ] {
            assert_eq!(announce(&event, &notified), None, "{event:?}");
        }
    }

    #[test]
    fn secrets_and_hidden_characters_never_reach_the_text() {
        let key = "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnop0123456789";
        let said = text(
            &Event::Error {
                message: format!("auth failed with {key}\u{202e}"),
            },
            &cx(true),
        )
        .unwrap();
        assert!(!said.contains(key), "{said}");
        assert!(!said.contains('\u{202e}'), "{said}");
        assert!(said.starts_with("Error: auth failed with "), "{said}");
    }

    #[test]
    fn long_parts_are_cut() {
        let title = "word ".repeat(100);
        let said = text(&approval(Some(&title)), &cx(true)).unwrap();
        assert!(said.chars().count() <= MAX_PART + "Approval needed: .".len());
        assert!(said.ends_with("….") || said.ends_with('…') || said.ends_with('.'));
        assert_eq!(part("  a \n\t b  "), "a b");
        assert_eq!(quoted("   "), "A thread");
    }

    #[test]
    fn an_unexpected_exit_is_said_and_a_clean_one_is_not() {
        let crash = Event::SessionExited {
            code: Some(1),
            expected: false,
        };
        assert_eq!(
            text(&crash, &cx(true)).as_deref(),
            Some("The agent stopped unexpectedly.")
        );
        for quiet in [
            Event::SessionExited {
                code: Some(0),
                expected: false,
            },
            Event::SessionExited {
                code: Some(1),
                expected: true,
            },
        ] {
            assert_eq!(announce(&quiet, &cx(true)), None);
        }
    }

    #[test]
    fn coalescing_joins_each_urgency_and_drops_repeats() {
        let a = |text: &str, urgency| Announcement {
            text: text.into(),
            urgency,
        };
        let (polite, alert) = coalesce(&[
            a("“A” finished.", Urgency::Polite),
            a("“B” needs approval.", Urgency::Assertive),
            a("“A” finished.", Urgency::Polite),
            a("“C” finished.", Urgency::Polite),
        ]);
        assert_eq!(polite.as_deref(), Some("“A” finished. “C” finished."));
        assert_eq!(alert.as_deref(), Some("“B” needs approval."));
        assert_eq!(coalesce(&[]), (None, None));
    }

    #[test]
    fn a_burst_reads_three_and_counts_the_rest() {
        let polite = |text: String| Announcement {
            text,
            urgency: Urgency::Polite,
        };
        let burst: Vec<Announcement> = (0..10)
            .map(|i| polite(format!("“Thread {i}” finished.")))
            .collect();
        let (said, alert) = coalesce(&burst);
        assert_eq!(alert, None);
        assert_eq!(
            said.as_deref(),
            Some("“Thread 0” finished. “Thread 1” finished. “Thread 2” finished. And 7 more.")
        );

        // Long texts: fewer, within the ceiling, the rest counted.
        let long: Vec<Announcement> = (0..5)
            .map(|i| polite(format!("{i} {}.", "x".repeat(200))))
            .collect();
        let said = coalesce(&long).0.unwrap();
        assert!(said.chars().count() <= MAX_TEXT, "{}", said.chars().count());
        assert!(said.ends_with("And 3 more."), "{said}");
        // One alone longer than the ceiling is cut.
        let huge = coalesce(&[polite("y".repeat(2000))]).0.unwrap();
        assert!(huge.chars().count() <= MAX_TEXT);
        assert!(huge.ends_with('…'));
        // One alone that fits the ceiling is read whole: no room is kept for a count it
        // never has.
        let whole = "z".repeat(MAX_TEXT);
        assert_eq!(coalesce(&[polite(whole.clone())]).0, Some(whole));
        let near = "w".repeat(MAX_TEXT - 3);
        assert_eq!(coalesce(&[polite(near.clone())]).0, Some(near));
        // Two: the count's room is kept, so the whole stays within the ceiling.
        let said = coalesce(&[polite("a".repeat(MAX_TEXT)), polite("b".into())])
            .0
            .unwrap();
        assert!(said.chars().count() <= MAX_TEXT, "{}", said.chars().count());
        assert!(said.ends_with("… And 1 more."), "{said}");
    }

    #[test]
    fn what_waits_is_bounded_and_taken_per_urgency() {
        let mut held = Pending::default();
        for i in 0..1000 {
            held.push(Announcement {
                text: format!("“T{i}” finished."),
                urgency: Urgency::Polite,
            });
            held.push(Announcement {
                text: "“A” needs approval.".into(),
                urgency: Urgency::Assertive,
            });
        }
        assert_eq!(held.polite.texts.len(), MAX_HELD);
        assert_eq!(held.polite.more, 1000 - MAX_HELD);
        assert_eq!(held.assertive.texts.len(), 1, "repeats dropped");
        assert!(held.has(Urgency::Assertive));
        assert_eq!(
            held.take(Urgency::Assertive).as_deref(),
            Some("“A” needs approval.")
        );
        assert!(!held.has(Urgency::Assertive));
        assert!(held.has(Urgency::Polite), "the other urgency still waits");
        let said = held.take(Urgency::Polite).unwrap();
        assert!(said.ends_with("And 997 more."), "{said}");
        assert_eq!(held.take(Urgency::Polite), None);
        held.push(Announcement {
            text: "x".into(),
            urgency: Urgency::Polite,
        });
        held.clear();
        assert_eq!(held, Pending::default());
    }
}
