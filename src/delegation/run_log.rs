//! One structured journal record per delegated run, written when the run ends (completed,
//! failed or interrupted), for tracing delegation in the journal: which agent, profile and model
//! ran it, in which task mode, how it ended, how long it took, its tokens when known, and how
//! large its result was. Never the task, the result's text, a path or a credential: only names
//! the app chose, sizes and counts. A model name passes through the redaction every persisted
//! text takes, and is cut to a bounded length.
//!
//! The record is written when the supervisor ends the run in memory, before the store commits
//! that end, and the first ending seen wins. In a rare race (two endings, the store keeping the
//! one written first by its writer, or a commit that fails) the record's outcome can disagree
//! with the store's; the store is the authority.

use std::time::Duration;

use agent_core::delegation::identity::Identity;
use agent_core::delegation::lifecycle::RunState;
use agent_core::event::Usage;
use agent_kit::store::delegation::WorkerRow;

/// The record's message: one fixed text, so the journal can be searched for it.
pub(super) const MESSAGE: &str = "delegated run ended";
/// The longest model name the record carries.
const MAX_MODEL_BYTES: usize = 128;

/// The fields of one run's record.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RunEndFields {
    pub driver: String,
    pub profile_id: String,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub task_mode: Option<String>,
    /// `completed`, `failed` or `interrupted`.
    pub outcome: String,
    pub duration_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub result_bytes: usize,
    /// No result, or one of whitespace only.
    pub result_empty: bool,
}

fn model_field(value: Option<&String>) -> Option<String> {
    let value = value.map(|v| v.trim()).filter(|v| !v.is_empty())?;
    let safe = agent_core::redact::redact(value);
    Some(agent_core::delegation::error::truncate_bytes(&safe, MAX_MODEL_BYTES).to_owned())
}

/// The record for a run of the worker `row` that ended as `outcome` after `elapsed`, with
/// `usage` and `result` as the run's end reported them. Pure: everything is passed in.
pub(super) fn fields(
    row: &WorkerRow,
    outcome: RunState,
    elapsed: Option<Duration>,
    usage: Option<&Usage>,
    result: Option<&str>,
) -> RunEndFields {
    let identity = row
        .identity_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<Identity>(json).ok());
    let model = identity.as_ref().map(|i| &i.model);
    let result = result.unwrap_or("");
    RunEndFields {
        driver: row.driver.clone(),
        profile_id: row.profile_id.clone(),
        requested_model: model_field(model.and_then(|m| m.requested.as_ref())),
        effective_model: model_field(model.and_then(|m| m.effective.as_ref())),
        task_mode: row.task_mode.clone(),
        outcome: super::supervisor::enum_value(&outcome),
        duration_ms: elapsed.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        input_tokens: usage.map(|u| u.input_tokens),
        output_tokens: usage.map(|u| u.output_tokens),
        result_bytes: result.len(),
        result_empty: result.trim().is_empty(),
    }
}

/// Writes the record: the journal's identifier is the app's, each field its own (`F_DRIVER`,
/// `F_OUTCOME`, …), and a field with no value is left out.
pub(super) fn log(f: &RunEndFields) {
    tracing::info!(
        driver = %f.driver,
        profile_id = %f.profile_id,
        requested_model = f.requested_model.as_deref(),
        effective_model = f.effective_model.as_deref(),
        task_mode = f.task_mode.as_deref(),
        outcome = %f.outcome,
        duration_ms = f.duration_ms,
        input_tokens = f.input_tokens,
        output_tokens = f.output_tokens,
        result_bytes = f.result_bytes,
        result_empty = f.result_empty,
        "{MESSAGE}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(identity_json: Option<String>) -> WorkerRow {
        WorkerRow {
            id: "w_child".into(),
            root_id: "w_root".into(),
            parent_id: Some("w_root".into()),
            origin: "supervised".into(),
            depth: 1,
            name: "Reviewer of /srv/example/secret-project".into(),
            driver: "local".into(),
            profile_id: "p-0123456789abcdef".into(),
            launch_json: r#"{"task":"fix the bug in /srv/app"}"#.into(),
            backing_thread_id: Some("t-1".into()),
            state: "running".into(),
            task_mode: Some("plan".into()),
            workspace_json: r#"{"root":"/srv/app"}"#.into(),
            identity_json,
            delegation_status: "ok".into(),
            delegation_reason: None,
            delegation_paths: "[]".into(),
            last_activity: Some("read /srv/app/main.rs".into()),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn identity(requested: Option<&str>, effective: Option<&str>) -> String {
        serde_json::json!({
            "driver": "local",
            "profile_id": "p-0123456789abcdef",
            "model": {"requested": requested, "effective": effective,
                      "source": "provider_reported"},
            "effort": {"requested": null, "effective": null, "source": "unknown"}
        })
        .to_string()
    }

    #[test]
    fn a_completed_run_carries_its_agent_model_mode_time_tokens_and_result_size() {
        let usage = Usage {
            input_tokens: 1200,
            output_tokens: 340,
            ..Usage::default()
        };
        let f = fields(
            &row(Some(identity(Some("model-a"), Some("model-a:9b")))),
            RunState::Completed,
            Some(Duration::from_millis(4321)),
            Some(&usage),
            Some("All done."),
        );
        assert_eq!(
            f,
            RunEndFields {
                driver: "local".into(),
                profile_id: "p-0123456789abcdef".into(),
                requested_model: Some("model-a".into()),
                effective_model: Some("model-a:9b".into()),
                task_mode: Some("plan".into()),
                outcome: "completed".into(),
                duration_ms: Some(4321),
                input_tokens: Some(1200),
                output_tokens: Some(340),
                result_bytes: 9,
                result_empty: false,
            }
        );
    }

    #[test]
    fn an_empty_or_failed_run_says_so_and_unknowns_stay_absent() {
        let f = fields(&row(None), RunState::Failed, None, None, None);
        assert_eq!(f.outcome, "failed");
        assert_eq!((f.result_bytes, f.result_empty), (0, true));
        assert_eq!((f.requested_model, f.effective_model), (None, None));
        assert_eq!(
            (f.duration_ms, f.input_tokens, f.output_tokens),
            (None, None, None)
        );
        let blank = fields(&row(None), RunState::Interrupted, None, None, Some("\n  "));
        assert_eq!(blank.outcome, "interrupted");
        assert_eq!((blank.result_bytes, blank.result_empty), (3, true));
        let unreadable = fields(
            &row(Some("{not json".into())),
            RunState::Completed,
            None,
            None,
            None,
        );
        assert_eq!(unreadable.requested_model, None);
    }

    #[test]
    fn the_record_never_carries_the_task_result_paths_or_a_secret() {
        let secret = format!("sk-{}", "z".repeat(48));
        let f = fields(
            &row(Some(identity(Some(&secret), Some(&"m".repeat(400))))),
            RunState::Completed,
            Some(Duration::from_secs(1)),
            None,
            Some("The answer mentions /srv/app/main.rs and a token."),
        );
        let shown = format!("{f:?}");
        for leaked in [
            secret.as_str(),
            "/srv/app",
            "secret-project",
            "fix the bug",
            "The answer",
            "/srv/example",
        ] {
            assert!(!shown.contains(leaked), "{leaked} in {shown}");
        }
        assert!(f
            .effective_model
            .is_some_and(|m| m.len() <= MAX_MODEL_BYTES));
    }
}
