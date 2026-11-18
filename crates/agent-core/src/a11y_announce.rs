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

/// Announcements made while one was waiting to be shown, joined: each urgency's texts in
/// order, a repeat dropped. The polite text first, then the assertive.
pub fn coalesce(pending: &[Announcement]) -> (Option<String>, Option<String>) {
    let join = |urgency: Urgency| {
        let mut texts: Vec<&str> = Vec::new();
        for a in pending.iter().filter(|a| a.urgency == urgency) {
            if !texts.contains(&a.text.as_str()) {
                texts.push(&a.text);
            }
        }
        (!texts.is_empty()).then(|| texts.join(" "))
    };
    (join(Urgency::Polite), join(Urgency::Assertive))
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
    if plain.chars().count() <= MAX_PART {
        return plain;
    }
    let cut: String = plain.chars().take(MAX_PART - 1).collect();
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
}
