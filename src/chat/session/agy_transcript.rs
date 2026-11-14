//! Antigravity's on-disk transcript, read for what its stdout omits.
//!
//! `agy --output-format stream-json` leaves out the raw thinking string; it writes it (each
//! tool call's `args`, and the images it gave its model) to
//! `~/.gemini/antigravity-cli/brain/<conv>/.system_generated/logs/transcript.jsonl`.
//! Everything here is plain file I/O: callers run [`read_step`] on a worker thread
//! (`gio::spawn_blocking`), never on the main thread. [`decode_args`] is pure.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// How much of the transcript's tail is read: recent steps live there.
const TAIL_BYTES: u64 = 65_536;

/// The most images one step shows.
const MAX_MEDIA: usize = 8;

/// What the transcript holds for one step.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StepRecord {
    /// The model's thinking text, trimmed; `None` when absent or blank.
    pub thinking: Option<String>,
    /// The step's tool calls with their `args` already decoded ([`decode_args`]).
    pub calls: Vec<ToolCall>,
    /// The images agy handed its model as the step's result: the copies it wrote in the
    /// conversation's `logs/media` folder, and only those ([`read_step`] drops any other path).
    pub media: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    /// An object of the call's arguments with JSON-encoded string values decoded.
    pub args: Value,
}

impl StepRecord {
    pub fn is_empty(&self) -> bool {
        self.thinking.is_none() && self.calls.is_empty() && self.media.is_empty()
    }
}

/// The transcript stores each arg value as a JSON-encoded string (`"\"/etc/hostname\""`, with
/// the quotes inside). This returns `args` with those values decoded to what the agent meant:
/// a string, object or array. Plain strings that are not JSON, numbers, booleans and
/// non-string values are left alone. Titles are built elsewhere; this only exposes the values.
pub fn decode_args(args: &Value) -> Value {
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    let decoded: Map<String, Value> = map
        .iter()
        .map(|(key, value)| (key.clone(), decode_value(value)))
        .collect();
    Value::Object(decoded)
}

fn decode_value(value: &Value) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    match serde_json::from_str::<Value>(text) {
        Ok(inner @ (Value::String(_) | Value::Object(_) | Value::Array(_))) => inner,
        _ => value.clone(),
    }
}

/// The transcript file for `conv_id` under `home`, newest layout first.
fn transcript_path(home: &Path, conv_id: &str) -> Option<std::path::PathBuf> {
    // The id comes from agy's output and is joined into a path: it must be one plain component.
    if conv_id.is_empty()
        || conv_id.contains(['/', '\\'])
        || conv_id.contains("..")
        || conv_id.contains('\0')
    {
        return None;
    }
    [".gemini/antigravity-cli/brain", ".gemini/brain"]
        .iter()
        .map(|base| {
            home.join(base)
                .join(conv_id)
                .join(".system_generated/logs/transcript.jsonl")
        })
        .find(|p| p.exists())
}

/// Reads what the transcript tail records for `target_index` (or the step before it, where the
/// planner response that carries the thinking sits). Blocking file I/O.
pub fn read_step(home: &Path, conv_id: &str, target_index: u64) -> StepRecord {
    let Some(path) = transcript_path(home, conv_id) else {
        return StepRecord::default();
    };
    let Some((mut record, uris)) = read_tail(&path).map(|text| find_step(&text, target_index))
    else {
        return StepRecord::default();
    };
    if let Some(folder) = path.parent().map(|logs| logs.join("media")) {
        record.media = uris
            .iter()
            .filter_map(|uri| media_path(uri, &folder))
            .take(MAX_MEDIA)
            .collect();
    }
    record
}

/// A `media` entry's file, when it is a plain `file://` path directly inside `folder` (the
/// conversation's own `logs/media`); anything else is refused. Pure.
fn media_path(uri: &str, folder: &Path) -> Option<PathBuf> {
    let path = Path::new(uri.strip_prefix("file://")?);
    // Percent-encoding is refused rather than decoded: agy names its copies plainly.
    if !path.is_absolute() || uri.contains('%') || path.parent()? != folder {
        return None;
    }
    path.file_name()?;
    Some(path.to_path_buf())
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let to_read = len.min(TAIL_BYTES) as usize;
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut buf = vec![0u8; to_read];
    file.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Pure: the newest thinking and the newest non-empty tool calls for `target_index`, and the
/// `media` URIs recorded for `target_index` itself (the step that is the call's result).
fn find_step(text: &str, target_index: u64) -> (StepRecord, Vec<String>) {
    let mut record = StepRecord::default();
    let mut media = Vec::new();
    for line in text.lines().rev() {
        if !line.contains("\"thinking\"")
            && !line.contains("\"tool_calls\"")
            && !line.contains("\"media\"")
        {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(idx) = val.get("step_index").and_then(Value::as_u64) else {
            continue;
        };
        if idx != target_index && !(idx + 1 == target_index && target_index > 0) {
            continue;
        }
        if idx == target_index && media.is_empty() {
            media = val
                .get("media")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|m| m.get("uri").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
        }
        if record.thinking.is_none() {
            record.thinking = val
                .get("thinking")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
        }
        if record.calls.is_empty() {
            record.calls = val
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| {
                            let name = call.get("name").and_then(Value::as_str)?;
                            let args = call.get("args").map(decode_args).unwrap_or(Value::Null);
                            Some(ToolCall {
                                name: name.to_owned(),
                                args,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        if record.thinking.is_some() && !record.calls.is_empty() {
            break;
        }
    }
    (record, media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decode_args_strips_the_extra_json_quotes() {
        let args = json!({
            "AbsolutePath": "\"/etc/hostname\"",
            "toolAction": "\"Viewing hostname file\"",
            "Lines": "[1,2]",
            "Opts": "{\"a\":1}",
            "Plain": "no json here",
            "Count": "42",
            "Flag": true
        });
        let out = decode_args(&args);
        assert_eq!(out["AbsolutePath"], "/etc/hostname");
        assert_eq!(out["toolAction"], "Viewing hostname file");
        assert_eq!(out["Lines"], json!([1, 2]));
        assert_eq!(out["Opts"], json!({"a": 1}));
        assert_eq!(out["Plain"], "no json here");
        // A bare number stays the string it was: it cannot be told from text.
        assert_eq!(out["Count"], "42");
        assert_eq!(out["Flag"], true);
        assert_eq!(decode_args(&json!("x")), json!("x"));
    }

    #[test]
    fn find_step_decodes_the_recorded_fixture_args() {
        let fixture = include_str!("../../../tests/fixtures/real/agy/transcript-view-file.jsonl");
        let record = find_step(fixture, 2).0;
        assert_eq!(record.thinking, None);
        assert_eq!(record.calls.len(), 1);
        assert_eq!(record.calls[0].name, "view_file");
        assert_eq!(record.calls[0].args["AbsolutePath"], "/etc/hostname");
        assert_eq!(record.calls[0].args["toolSummary"], "View /etc/hostname");
    }

    /// An image read, in the shape agy records it (live, 2026-11-14): the call is one step, its
    /// result (`GENERIC`, with `media`) the next, which is the index stdout reports for the tool.
    #[test]
    fn find_step_takes_an_image_reads_media_from_its_result_step() {
        const MEDIA: &str =
            "file:///home/user/.gemini/antigravity-cli/brain/c1/.system_generated/logs/media/step_7_media_0.jpg";
        let call = json!({"step_index": 6, "source": "MODEL", "type": "PLANNER_RESPONSE",
            "status": "DONE", "tool_calls": [{"name": "read_file",
            "args": {"Path": "\"/home/user/work/shot.png\""}}]});
        let result = json!({"step_index": 7, "source": "MODEL", "type": "GENERIC",
            "status": "DONE", "content": "Showing binary file of type image/png",
            "media": [{"mime_type": "image/jpeg", "uri": MEDIA}]});
        let text = format!("{call}\n{result}\n");
        let (record, media) = find_step(&text, 7);
        assert_eq!(record.calls.len(), 1);
        assert_eq!(record.calls[0].name, "read_file");
        assert_eq!(record.calls[0].args["Path"], "/home/user/work/shot.png");
        assert_eq!(media, [MEDIA]);
        // The call's own step has no media: only a result's is taken.
        assert!(find_step(&text, 6).1.is_empty());
    }

    /// Only a plain `file://` path directly inside the conversation's `logs/media` is kept.
    #[test]
    fn read_step_keeps_only_media_inside_the_conversation_logs() {
        let tmp = tempfile::tempdir().expect("tmp");
        let logs = tmp
            .path()
            .join(".gemini/antigravity-cli/brain/c1/.system_generated/logs");
        std::fs::create_dir_all(&logs).expect("mkdir");
        let folder = logs.join("media");
        let inside = folder.join("step_4_media_0.jpg");
        let uris = [
            format!("file://{}", inside.display()),
            format!("file://{}", folder.join("../transcript.jsonl").display()),
            format!("file://{}", folder.join("sub/x.jpg").display()),
            format!("file://{}", folder.join("a%2Fb.jpg").display()),
            inside.display().to_string(),
            "file://relative/step_4_media_0.jpg".to_owned(),
            "https://example.com/x.jpg".to_owned(),
            "file:///etc/passwd".to_owned(),
        ];
        let media: Vec<_> = uris.iter().map(|uri| json!({"uri": uri})).collect();
        let line = json!({"step_index": 4, "type": "GENERIC", "media": media});
        std::fs::write(logs.join("transcript.jsonl"), format!("{line}\n")).expect("write");
        let record = read_step(tmp.path(), "c1", 4);
        assert_eq!(record.media, [inside]);
        assert!(!record.is_empty());
    }

    #[test]
    fn find_step_takes_the_step_or_the_one_before() {
        let text = "{\"step_index\":4,\"thinking\":\" hmm \"}\n{\"step_index\":9,\"thinking\":\"later\"}\n";
        assert_eq!(find_step(text, 5).0.thinking.as_deref(), Some("hmm"));
        assert_eq!(find_step(text, 4).0.thinking.as_deref(), Some("hmm"));
        assert!(find_step(text, 7).0.is_empty());
    }

    #[test]
    fn a_conversation_id_that_is_not_one_path_component_reads_nothing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp
            .path()
            .join(".gemini/antigravity-cli/brain/c1/.system_generated/logs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("transcript.jsonl"),
            "{\"step_index\":1,\"thinking\":\"t\"}\n",
        )
        .expect("write");
        assert!(read_step(tmp.path(), "c1", 1).thinking.is_some());
        for bad in ["", "..", "../c1", "c1/..", "a/b", "c1\\x", "c1/", "x..y"] {
            assert!(read_step(tmp.path(), bad, 1).is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn read_step_reads_the_file_and_is_empty_without_one() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert!(read_step(tmp.path(), "none", 1).is_empty());
        let dir = tmp
            .path()
            .join(".gemini/antigravity-cli/brain/c1/.system_generated/logs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("transcript.jsonl"),
            "{\"step_index\":1,\"thinking\":\"t\"}\n",
        )
        .expect("write");
        assert_eq!(
            read_step(tmp.path(), "c1", 1).thinking.as_deref(),
            Some("t")
        );
    }
}
