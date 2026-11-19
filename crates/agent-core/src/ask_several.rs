//! Pure domain types and validation for asking 2 to 4 agents the same prompt side by side (F7).
//!
//! No GTK and no I/O: participant rules (`MIN_CHOICES`..=`MAX_CHOICES`, `agy` worktree
//! isolation, no remote threads), persistent comparison group metadata, and handoff redaction
//! when bringing a chosen answer back into the source thread.

use crate::adapter::Driver;
use crate::compare::{Side, MAX_COLUMNS, MIN_COLUMNS};
use crate::redact::redact;
use serde::{Deserialize, Serialize};

/// Fewest participants that can be compared side by side.
pub const MIN_CHOICES: usize = MIN_COLUMNS;
/// Most participants that can be compared side by side.
pub const MAX_CHOICES: usize = MAX_COLUMNS;

/// Live attention and work take precedence over the last persisted turn outcome.
pub fn participant_status(
    approval: bool,
    limited: bool,
    working: bool,
    last_turn: Option<crate::event::TurnState>,
) -> &'static str {
    use crate::event::TurnState;
    if approval {
        "Needs approval"
    } else if limited {
        "Waiting for limit"
    } else if working {
        "Working"
    } else {
        match last_turn {
            Some(TurnState::Completed) => "Done",
            Some(TurnState::Failed) => "Failed",
            Some(_) => "Stopped",
            None => "Waiting",
        }
    }
}
/// Maximum characters retained from a kept answer when building the return brief for the source thread.
const MAX_KEPT_BRIEF_CHARS: usize = 6_000;

/// Target agent configuration for one column in an Ask Several launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSpec {
    pub label: String,
    pub driver: Driver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Whether this participant runs in its own isolated git worktree.
    pub worktree: bool,
}

/// Validates a set of candidate participants for an Ask Several run.
pub fn validate_choices(choices: &[TargetSpec], remote: bool) -> Result<(), &'static str> {
    if remote {
        return Err("Ask several does not run on a remote thread");
    }
    if choices.len() < MIN_CHOICES {
        return Err("Choose at least two agents to compare");
    }
    if choices.len() > MAX_CHOICES {
        return Err("Choose at most four agents to compare");
    }
    if choices.iter().any(|c| c.label.trim().is_empty()) {
        return Err("Every participant must have a label");
    }
    if choices
        .iter()
        .any(|c| c.driver == Driver::Agy && !c.worktree)
    {
        return Err("Antigravity participants require isolated worktrees");
    }
    Ok(())
}

/// One forked participant thread belonging to a stored [`CompareGroup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareParticipant {
    pub thread_id: String,
    pub label: String,
    pub driver: Driver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Store sequence number in the participant thread at fork time; entries after it belong to this ask.
    pub after_seq: i64,
}

/// Stored metadata for one side-by-side multi-agent ask launched from `source_thread`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareGroup {
    pub id: String,
    pub source_thread: String,
    pub prompt_preview: String,
    pub created_at: i64,
    pub participants: Vec<CompareParticipant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_thread: Option<String>,
}

impl CompareGroup {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(raw: &str) -> Option<Self> {
        let mut parsed: Self = serde_json::from_str(raw).ok()?;
        if parsed.id.trim().is_empty()
            || parsed.source_thread.trim().is_empty()
            || parsed.participants.is_empty()
            || parsed.participants.len() > MAX_CHOICES
        {
            return None;
        }
        let mut participants = std::collections::HashSet::new();
        if parsed.participants.iter().any(|p| {
            p.thread_id.trim().is_empty()
                || p.thread_id == parsed.source_thread
                || !participants.insert(p.thread_id.clone())
        }) {
            return None;
        }
        if parsed
            .kept_thread
            .as_ref()
            .is_some_and(|kept| !parsed.participants.iter().any(|p| &p.thread_id == kept))
        {
            parsed.kept_thread = None;
        }
        parsed.prompt_preview = redact(&parsed.prompt_preview);
        for p in &mut parsed.participants {
            p.label = redact(&p.label);
        }
        Some(parsed)
    }

    /// Converts the group's participants into [`Side`] entries for [`crate::compare::table`].
    pub fn sides(&self) -> Vec<Side> {
        self.participants
            .iter()
            .map(|p| Side {
                thread: p.thread_id.clone(),
                after: p.after_seq,
            })
            .collect()
    }
}

/// Meta key under which a [`CompareGroup`] is stored.
pub fn group_meta_key(group_id: &str) -> String {
    format!("compare_group:{group_id}")
}

/// Meta key linking a participant fork thread back to its [`CompareGroup`] id.
pub fn participant_meta_key(thread_id: &str) -> String {
    format!("compare_of:{thread_id}")
}

/// Builds a redacted, length-bounded notice/brief returning a chosen comparison answer back into
/// the source thread.
pub fn keep_handoff_brief(kept_label: &str, kept_answer: &str, prompt_preview: &str) -> String {
    let safe_label = redact(kept_label.trim());
    let safe_prompt = redact(prompt_preview.trim());
    let safe_answer = redact(kept_answer.trim());
    let body: String = safe_answer.chars().take(MAX_KEPT_BRIEF_CHARS).collect();
    let truncated = safe_answer.chars().count() > MAX_KEPT_BRIEF_CHARS;
    let suffix = if truncated { "\n…(truncated)" } else { "" };
    if body.is_empty() {
        format!("Kept {safe_label}'s run for: {safe_prompt}")
    } else {
        format!("Kept {safe_label}'s answer for \"{safe_prompt}\":\n\n{body}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn participant_status_prefers_live_work_and_never_calls_unknown_done() {
        use crate::event::TurnState;
        assert_eq!(
            participant_status(true, true, true, Some(TurnState::Failed)),
            "Needs approval"
        );
        assert_eq!(
            participant_status(false, true, true, None),
            "Waiting for limit"
        );
        assert_eq!(
            participant_status(false, false, true, Some(TurnState::Completed)),
            "Working"
        );
        assert_eq!(
            participant_status(false, false, false, Some(TurnState::Failed)),
            "Failed"
        );
        assert_eq!(
            participant_status(false, false, false, Some(TurnState::Completed)),
            "Done"
        );
        assert_eq!(participant_status(false, false, false, None), "Waiting");
    }

    fn spec(driver: Driver, worktree: bool) -> TargetSpec {
        TargetSpec {
            label: driver.info().label.to_owned(),
            driver,
            profile_id: None,
            model: None,
            effort: None,
            worktree,
        }
    }

    #[test]
    fn validates_participant_count_remote_and_agy_worktree_requirement() {
        assert!(validate_choices(&[spec(Driver::Claude, false)], false).is_err());
        assert!(validate_choices(
            &[
                spec(Driver::Claude, false),
                spec(Driver::Codex, false),
                spec(Driver::Local, false),
                spec(Driver::Claude, false),
                spec(Driver::Codex, false),
            ],
            false
        )
        .is_err());
        assert!(validate_choices(
            &[spec(Driver::Claude, false), spec(Driver::Codex, false)],
            true
        )
        .is_err());
        assert!(validate_choices(
            &[spec(Driver::Claude, false), spec(Driver::Agy, false)],
            false
        )
        .is_err());
        assert!(validate_choices(
            &[spec(Driver::Claude, false), spec(Driver::Agy, true)],
            false
        )
        .is_ok());
    }

    #[test]
    fn compare_group_round_trips_and_redacts_secrets() {
        let g = CompareGroup {
            id: "grp-1".into(),
            source_thread: "t-src".into(),
            prompt_preview: "Use token ghp_1234567890abcdefghijklmnopqrstuvwx".into(),
            created_at: 1_795_000_000,
            participants: vec![
                CompareParticipant {
                    thread_id: "t-1".into(),
                    label: "Claude".into(),
                    driver: Driver::Claude,
                    profile_id: None,
                    model: None,
                    after_seq: 10,
                },
                CompareParticipant {
                    thread_id: "t-2".into(),
                    label: "Codex".into(),
                    driver: Driver::Codex,
                    profile_id: None,
                    model: None,
                    after_seq: 10,
                },
            ],
            kept_thread: None,
        };
        let raw = g.to_json();
        let parsed = CompareGroup::from_json(&raw).expect("valid group");
        assert!(!parsed.prompt_preview.contains("ghp_"));
        assert_eq!(parsed.sides().len(), 2);

        let brief = keep_handoff_brief(
            "Claude",
            "Answer with sk-proj-abcdefghijklmnopqrstuvwxyz012345",
            &parsed.prompt_preview,
        );
        assert!(!brief.contains("sk-proj-"));
        assert!(!brief.contains("ghp_"));
    }
}
