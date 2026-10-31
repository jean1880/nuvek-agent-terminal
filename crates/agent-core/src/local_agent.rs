//! The Local agent: a model on a local or self-hosted server (Ollama, LM Studio, vLLM,
//! llama.cpp, any OpenAI-compatible one). No CLI drives such a model through a conversation with
//! tools, so the app runs the agent loop itself: `agent-terminal --local-agent` (agent-kit's
//! `local_agent`) talks to the server's OpenAI-compatible `/v1/chat/completions` and works in the
//! thread's folder with the tools below.
//!
//! The runner speaks this crate's canonical types on its pipes: every stdout line is an
//! [`Envelope`], every stdin line a [`Command`]. [`LocalAdapter`] is therefore a codec plus the
//! bookkeeping [`Adapter::on_exit`] needs. Also here, pure and shared with the runner: its
//! arguments ([`RunnerArgs`]), its tools, the mode gate ([`gate`]), the request body
//! ([`chat_request`]) and the streamed-reply reader ([`StreamReader`]).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adapter::{Action, Adapter, AdapterError, Command, Driver, Mode, OpenSession};
use crate::caps::Capabilities;
use crate::event::{Decision, Envelope, Event, ItemKind, ItemStatus, TurnState};

/// The app's own flag that runs the Local agent instead of the window.
pub const RUNNER_FLAG: &str = "--local-agent";

/// The longest native session id the runner accepts: it names a file.
const MAX_SESSION_ID: usize = 64;

/// Whether `id` can name a Local session (and so a transcript file): `[A-Za-z0-9-]{1,64}`.
pub fn valid_session_id(id: &str) -> bool {
    (1..=MAX_SESSION_ID).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// What the runner is started with. [`RunnerArgs::to_args`] and [`RunnerArgs::parse`] are each
/// other's inverse, so the adapter and the runner never disagree on a flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerArgs {
    /// The server's base URL (`http://host:11434`), normalized.
    pub server: String,
    /// The environment variable holding the server's API key, if it needs one. Only the name
    /// is passed: the key never appears on a command line.
    pub key_env: Option<String>,
    pub cwd: String,
    pub model: Option<String>,
    pub mode: Mode,
    /// Continue this session's conversation.
    pub resume: Option<String>,
    /// Start a new session under this id.
    pub session_id: Option<String>,
}

impl RunnerArgs {
    /// The server's own flags: what a profile's provider contributes ([`crate::provider`]).
    pub fn server_args(server: &str, key_env: Option<&str>) -> Vec<String> {
        let mut out = vec!["--server".to_owned(), server.to_owned()];
        if let Some(key) = key_env {
            out.extend(["--key-env".to_owned(), key.to_owned()]);
        }
        out
    }

    /// The session's flags: everything but the server.
    pub fn session_args(&self) -> Vec<String> {
        let mut out = vec![
            "--cwd".to_owned(),
            self.cwd.clone(),
            "--mode".to_owned(),
            mode_key(self.mode).to_owned(),
        ];
        if let Some(model) = &self.model {
            out.extend(["--model".to_owned(), model.clone()]);
        }
        if let Some(id) = &self.resume {
            out.extend(["--resume".to_owned(), id.clone()]);
        } else if let Some(id) = &self.session_id {
            out.extend(["--session-id".to_owned(), id.clone()]);
        }
        out
    }

    /// The arguments after [`RUNNER_FLAG`].
    pub fn to_args(&self) -> Vec<String> {
        let mut out = Self::server_args(&self.server, self.key_env.as_deref());
        out.extend(self.session_args());
        out
    }

    /// Reads the arguments after [`RUNNER_FLAG`]. Every value is checked here, so the runner
    /// never acts on a malformed one; the error says which.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = RunnerArgs::default();
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut value = || {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--server" => out.server = crate::provider::normalize_base_url(&value()?)?,
                "--key-env" => {
                    let name = value()?;
                    if !crate::provider::valid_env_name(&name) {
                        return Err(format!("\"{name}\" is not an environment variable name"));
                    }
                    out.key_env = Some(name);
                }
                "--cwd" => out.cwd = value()?,
                "--mode" => {
                    let key = value()?;
                    out.mode = mode_from_key(&key).ok_or_else(|| format!("unknown mode {key}"))?;
                }
                "--model" => out.model = Some(value()?).filter(|m| !m.trim().is_empty()),
                "--resume" | "--session-id" => {
                    let id = value()?;
                    if !valid_session_id(&id) {
                        return Err(format!("{flag} takes a session id, not \"{id}\""));
                    }
                    if flag == "--resume" {
                        out.resume = Some(id);
                    } else {
                        out.session_id = Some(id);
                    }
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        if out.server.is_empty() {
            return Err("--server is required".to_owned());
        }
        if out.cwd.is_empty() {
            return Err("--cwd is required".to_owned());
        }
        Ok(out)
    }
}

fn mode_key(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask",
        Mode::AcceptEdits => "accept_edits",
        Mode::Plan => "plan",
        Mode::Bypass => "bypass",
    }
}

fn mode_from_key(key: &str) -> Option<Mode> {
    [Mode::Ask, Mode::AcceptEdits, Mode::Plan, Mode::Bypass]
        .into_iter()
        .find(|m| mode_key(*m) == key)
}

/// The runner's tools. Inputs use the field names Claude's tools do (`file_path`, `old_string`,
/// `new_string`, `content`, `command`), so cards, titles and edit diffs read them unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    ReadFile,
    ListDir,
    Search,
    WriteFile,
    EditFile,
    RunCommand,
}

impl Tool {
    pub const ALL: [Tool; 6] = [
        Tool::ReadFile,
        Tool::ListDir,
        Tool::Search,
        Tool::WriteFile,
        Tool::EditFile,
        Tool::RunCommand,
    ];

    /// The name the model calls it by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::ReadFile => "read_file",
            Tool::ListDir => "list_dir",
            Tool::Search => "search",
            Tool::WriteFile => "write_file",
            Tool::EditFile => "edit_file",
            Tool::RunCommand => "run_command",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The card it is shown as.
    pub fn item_kind(self) -> ItemKind {
        match self {
            Tool::ReadFile | Tool::ListDir => ItemKind::FileRead,
            Tool::Search => ItemKind::Tool,
            Tool::WriteFile | Tool::EditFile => ItemKind::FileChange,
            Tool::RunCommand => ItemKind::Command,
        }
    }

    /// Whether it changes anything: a file or, through a command, anything at all.
    pub fn writes(self) -> bool {
        matches!(self, Tool::WriteFile | Tool::EditFile | Tool::RunCommand)
    }

    /// The card's title for a call with `input`.
    pub fn title(self, input: &Value) -> String {
        let field = |name: &str| input.get(name).and_then(Value::as_str).unwrap_or("");
        match self {
            Tool::ReadFile => format!("Read {}", field("file_path")),
            Tool::ListDir => format!("List {}", field("path")),
            Tool::Search => format!("Search for {}", field("pattern")),
            Tool::WriteFile => format!("Write {}", field("file_path")),
            Tool::EditFile => format!("Edit {}", field("file_path")),
            Tool::RunCommand => field("command").to_owned(),
        }
        .trim()
        .to_owned()
    }

    pub fn schema(self) -> Value {
        let (description, properties, required): (&str, Value, &[&str]) = match self {
            Tool::ReadFile => (
                "Read a text file. Paths are relative to the working folder unless absolute.",
                json!({
                    "file_path": {"type": "string", "description": "The file to read"},
                    "offset": {"type": "integer", "minimum": 0, "description": "First line to read, from 1"},
                    "limit": {"type": "integer", "minimum": 0, "description": "How many lines to read"}
                }),
                &["file_path"],
            ),
            Tool::ListDir => (
                "List the entries of a folder.",
                json!({"path": {"type": "string", "description": "The folder; . for the working folder"}}),
                &["path"],
            ),
            Tool::Search => (
                "Find lines containing some text in the files under a folder (plain text, case-sensitive).",
                json!({
                    "pattern": {"type": "string", "description": "The text to look for"},
                    "path": {"type": "string", "description": "Where to look; the working folder when left out"}
                }),
                &["pattern"],
            ),
            Tool::WriteFile => (
                "Create a file, or replace a file's whole content.",
                json!({
                    "file_path": {"type": "string", "description": "The file to write"},
                    "content": {"type": "string", "description": "Its full new content"}
                }),
                &["file_path", "content"],
            ),
            Tool::EditFile => (
                "Replace an exact piece of text in a file. old_string must occur exactly once unless replace_all is true.",
                json!({
                    "file_path": {"type": "string", "description": "The file to edit"},
                    "old_string": {"type": "string", "description": "The exact text to replace"},
                    "new_string": {"type": "string", "description": "What to put in its place"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"}
                }),
                &["file_path", "old_string", "new_string"],
            ),
            Tool::RunCommand => (
                "Run a shell command in the working folder and return its output.",
                json!({
                    "command": {"type": "string", "description": "The command line, run by sh -c"},
                    "timeout_secs": {"type": "integer", "minimum": 0, "description": "How long it may run (default 120, at most 600)"}
                }),
                &["command"],
            ),
        };
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": description,
                "parameters": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}
            }
        })
    }

    /// Built-ins accept exactly their advertised fields; card aliases for other agents must
    /// never become a second interpretation of the Local executor's arguments.
    pub fn validate_arguments(self, input: &Value) -> Result<(), &'static str> {
        let object = input
            .as_object()
            .ok_or("Tool arguments must be an object.")?;
        let schema = self.schema();
        let parameters = &schema["function"]["parameters"];
        let properties = &parameters["properties"];
        if parameters["required"].as_array().is_none_or(|required| {
            required
                .iter()
                .any(|name| name.as_str().is_none_or(|name| !object.contains_key(name)))
        }) {
            return Err("A required tool argument is missing.");
        }
        for (key, value) in object {
            let Some(field) = properties.get(key) else {
                return Err("Tool arguments contain an unknown field.");
            };
            let valid = match field["type"].as_str() {
                Some("string") => value.is_string(),
                Some("boolean") => value.is_boolean(),
                // These fields are read as u64 by the executor, including their bounds.
                Some("integer") => value.as_u64().is_some(),
                _ => false,
            };
            if !valid {
                return Err("A tool argument has the wrong type or range.");
            }
        }
        Ok(())
    }
}

/// The tools offered in `mode`: a plan is read-only, so the model is not offered what it may not
/// use.
pub fn tools_for(mode: Mode) -> Vec<Tool> {
    Tool::ALL
        .into_iter()
        .filter(|t| mode != Mode::Plan || !t.writes())
        .collect()
}

/// What happens to a tool call before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Run,
    /// Ask the user (an approval card).
    Ask,
    /// Refused with this reason, which the model is told.
    Refuse(&'static str),
}

/// The gate for `tool` in `mode`. `outside` says the call writes a path outside the working
/// folder (or one that cannot be resolved): that always asks unless every request is approved
/// (Bypass). The runner remembers no answer: "Allow for session" and "Always allow" run the one
/// call, and the app's own rules (scoped as each card says) answer later cards it covers.
pub fn gate(tool: Tool, mode: Mode, outside: bool) -> Gate {
    if !tool.writes() {
        return Gate::Run;
    }
    match mode {
        Mode::Plan => Gate::Refuse("Plan mode is read-only: describe the change instead."),
        Mode::Bypass => Gate::Run,
        Mode::AcceptEdits if tool != Tool::RunCommand && !outside => Gate::Run,
        Mode::Ask | Mode::AcceptEdits => Gate::Ask,
    }
}

/// One message of the conversation, in the OpenAI chat shape every target server takes, and as
/// the transcript keeps it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The call a `tool` message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The reasoning that led to an assistant message's tool calls, sent back with it while its
    /// turn runs: some reasoning models' templates refuse, or go astray on, a tool-call turn
    /// without it. Dropped once the turn ends ([`forget_reasoning`]), and never kept on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// Drops the reasoning of earlier turns: it is sent back only within the turn that made it.
pub fn forget_reasoning(messages: &mut [ChatMessage]) {
    for message in messages {
        message.reasoning_content = None;
    }
}

impl ChatMessage {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            content: content.into(),
            ..Self::default()
        }
    }

    /// The answer to the call `id`.
    pub fn tool_result(id: &str, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(id.to_owned()),
            ..Self::new("tool", content)
        }
    }
}

/// A tool call as the OpenAI shape carries it: `arguments` is a JSON *string*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: ToolFunction,
}

fn function_type() -> String {
    "function".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

impl ToolCall {
    /// The call's arguments as JSON. Models write them, so they may be malformed: the error is
    /// what the model is told.
    pub fn input(&self) -> Result<Value, String> {
        let text = self.function.arguments.trim();
        if text.is_empty() {
            return Ok(json!({}));
        }
        match serde_json::from_str::<Value>(text) {
            Ok(value @ Value::Object(_)) => Ok(value),
            Ok(_) => Err("The tool arguments must be a JSON object.".to_owned()),
            Err(e) => Err(format!("The tool arguments are not valid JSON: {e}")),
        }
    }
}

/// The body of a streamed `/v1/chat/completions` request: usage asked for in the last chunk,
/// tools offered when there are any (a server may refuse an empty list).
pub fn chat_request(model: &str, messages: &[ChatMessage], tools: &[Tool]) -> Value {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|t| t.schema()).collect();
    }
    body
}

/// Request construction with already selected complete schemas and an explicit output cap.
pub fn schema_request(
    model: &str,
    messages: &[ChatMessage],
    schemas: &[Value],
    response_tokens: usize,
) -> Value {
    let mut body = chat_request(model, messages, &[]);
    body["max_tokens"] = json!(response_tokens);
    if !schemas.is_empty() {
        body["tools"] = json!(schemas);
    }
    body
}

/// What one piece of a streamed reply says.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    /// Prompt and completion tokens.
    Usage(u64, u64),
    /// The server reported an error inside the stream.
    Error(String),
}

/// What a finished streamed reply amounts to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub text: String,
    /// Its reasoning, from whichever field or `<think>` tags the server used.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// The server's `finish_reason`, if it sent one.
    pub finish_reason: Option<String>,
}

/// Reads a streamed `/v1/chat/completions` reply line by line (SSE `data:` lines). Text and
/// reasoning come out as they arrive. Tool calls are put together from their fragments by
/// `index`, since some servers send `arguments` in pieces and others whole (Ollama). Reasoning is
/// read from `reasoning` (Ollama) or `reasoning_content` (vLLM, llama.cpp, LM Studio), and from
/// `<think>` tags a server leaves in the text.
#[derive(Debug, Default)]
pub struct StreamReader {
    reply: Reply,
    calls: Vec<PartialCall>,
    think: ThinkSplitter,
    done: bool,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the server said the reply is over (`data: [DONE]`).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// One line of the stream. Blank lines, comments and `event:` lines are ignored.
    pub fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.trim_end_matches('\r').strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            let rest = self.think.flush();
            self.keep(&rest);
            return rest;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return vec![StreamEvent::Error(
                "The server sent a stream line that is not JSON.".to_owned(),
            )];
        };
        if let Some(error) = error_message(&chunk) {
            return vec![StreamEvent::Error(error)];
        }
        let mut out = Vec::new();
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            let delta = &choice["delta"];
            for key in ["reasoning_content", "reasoning"] {
                if let Some(text) = delta[key].as_str().filter(|t| !t.is_empty()) {
                    out.push(StreamEvent::Reasoning(text.to_owned()));
                }
            }
            if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                out.extend(self.think.feed(text));
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                self.add_call_fragment(call);
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.reply.finish_reason = Some(reason.to_owned());
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            let count = |key: &str| usage[key].as_u64().unwrap_or(0);
            out.push(StreamEvent::Usage(
                count("prompt_tokens"),
                count("completion_tokens"),
            ));
        }
        self.keep(&out);
        out
    }

    /// Adds what `events` say to the reply's text and reasoning.
    fn keep(&mut self, events: &[StreamEvent]) {
        for event in events {
            match event {
                StreamEvent::Text(text) => self.reply.text.push_str(text),
                StreamEvent::Reasoning(text) => self.reply.reasoning.push_str(text),
                _ => {}
            }
        }
    }

    fn add_call_fragment(&mut self, call: &Value) {
        let index = call["index"]
            .as_u64()
            .map(|i| i as usize)
            .unwrap_or(self.calls.len());
        // A bound on what a model can make the runner hold.
        if index >= 64 {
            return;
        }
        while self.calls.len() <= index {
            self.calls.push(PartialCall::default());
        }
        let slot = &mut self.calls[index];
        if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty()) {
            slot.id = id.to_owned();
        }
        let function = &call["function"];
        if let Some(name) = function["name"].as_str() {
            slot.name.push_str(name);
        }
        match &function["arguments"] {
            Value::String(fragment) => slot.arguments.push_str(fragment),
            // A server that sends the arguments as an object.
            Value::Object(_) => slot.arguments = function["arguments"].to_string(),
            _ => {}
        }
    }

    /// The reply, once the stream ended. Calls without a name are dropped; calls without an id
    /// get one, since the answer must name it.
    pub fn finish(mut self) -> (Reply, Vec<StreamEvent>) {
        let rest = self.think.flush();
        self.keep(&rest);
        self.reply.tool_calls = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.name.trim().is_empty())
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{i}")
                } else {
                    c.id
                },
                kind: function_type(),
                function: ToolFunction {
                    name: c.name.trim().to_owned(),
                    arguments: c.arguments,
                },
            })
            .collect();
        (self.reply, rest)
    }
}

/// The message of an error body: OpenAI's `{"error": {"message": …}}`, or `{"error": "…"}`
/// (Ollama, llama.cpp), or `{"detail": …}` (some proxies).
pub fn error_message(body: &Value) -> Option<String> {
    match body.get("error") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Object(error)) => error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(Value::Object(error.clone()).to_string())),
        _ => body.get("detail").map(|d| {
            d.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| d.to_string())
        }),
    }
}

/// Whether a server error says the model cannot use tools.
pub fn refuses_tools(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("does not support tools")
        || error.contains("tool choice")
        || error.contains("tools are not supported")
}

/// Splits `<think>…</think>` out of streamed text, including a tag cut across two pieces.
#[derive(Debug, Default)]
struct ThinkSplitter {
    inside: bool,
    /// The end of the last piece, held back because it may be the start of a tag.
    held: String,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

impl ThinkSplitter {
    fn feed(&mut self, text: &str) -> Vec<StreamEvent> {
        let mut text = std::mem::take(&mut self.held) + text;
        let mut out = Vec::new();
        loop {
            let tag = if self.inside { THINK_CLOSE } else { THINK_OPEN };
            match text.find(tag) {
                Some(at) => {
                    self.emit(&text[..at], &mut out);
                    text = text[at + tag.len()..].to_owned();
                    self.inside = !self.inside;
                }
                None => {
                    // Hold back the longest end of `text` that starts the tag.
                    let keep = (1..tag.len())
                        .rev()
                        .find(|n| {
                            text.len() >= *n
                                && text.is_char_boundary(text.len() - n)
                                && tag.starts_with(&text[text.len() - n..])
                        })
                        .unwrap_or(0);
                    let cut = text.len() - keep;
                    self.emit(&text[..cut], &mut out);
                    self.held = text[cut..].to_owned();
                    return out;
                }
            }
        }
    }

    fn flush(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let held = std::mem::take(&mut self.held);
        self.emit(&held, &mut out);
        out
    }

    fn emit(&self, text: &str, out: &mut Vec<StreamEvent>) {
        if text.is_empty() {
            return;
        }
        out.push(if self.inside {
            StreamEvent::Reasoning(text.to_owned())
        } else {
            StreamEvent::Text(text.to_owned())
        });
    }
}

/// The adapter: a codec over the runner's canonical pipes, keeping what `on_exit` must close.
#[derive(Debug)]
pub struct LocalAdapter {
    context: crate::local_context::LocalLaunchContext,
    caps: Capabilities,
    started: bool,
    turn_open: bool,
    open_items: BTreeSet<String>,
    pending: BTreeSet<String>,
    mcp_pending: std::collections::BTreeMap<String, crate::local_mcp::ApprovalStamp>,
    mcp_answered: BTreeSet<String>,
}

impl Default for LocalAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalAdapter {
    pub fn new() -> Self {
        Self {
            context: Default::default(),
            caps: Capabilities::local(),
            started: false,
            turn_open: false,
            open_items: BTreeSet::new(),
            pending: BTreeSet::new(),
            mcp_pending: Default::default(),
            mcp_answered: Default::default(),
        }
    }

    fn write(command: &Command) -> Result<Vec<Action>, AdapterError> {
        serde_json::to_string(command)
            .map(|line| vec![Action::Write(vec![line])])
            .map_err(|e| AdapterError::Invalid(e.to_string()))
    }

    pub fn with_context(mut self, context: crate::local_context::LocalLaunchContext) -> Self {
        self.context = context;
        self
    }

    /// Tracks what `envelope` opens and closes.
    fn observe(&mut self, envelope: &Envelope) {
        match &envelope.event {
            Event::SessionStarted { .. } => self.started = true,
            Event::TurnStarted { .. } => self.turn_open = true,
            Event::TurnCompleted { .. } => self.turn_open = false,
            Event::ItemStarted { .. } => {
                if let Some(item) = &envelope.item {
                    self.open_items.insert(item.clone());
                }
            }
            Event::ItemCompleted { .. } => {
                if let Some(item) = &envelope.item {
                    self.open_items.remove(item);
                }
            }
            Event::ApprovalRequested { tool, input, .. } => {
                if let Some(request) = &envelope.request {
                    self.pending.insert(request.clone());
                    if tool == crate::local_mcp::APPROVAL_TOOL {
                        if let Ok(card) =
                            serde_json::from_value::<crate::local_mcp::ApprovalCard>(input.clone())
                        {
                            if card.binding.valid() && &card.binding.request_id == request {
                                self.mcp_pending.insert(request.clone(), card.binding);
                            }
                        }
                    }
                }
            }
            Event::ApprovalResolved { .. } | Event::ApprovalExpired => {
                if let Some(request) = &envelope.request {
                    self.pending.remove(request);
                    self.mcp_pending.remove(request);
                    self.mcp_answered.remove(request);
                }
            }
            _ => {}
        }
    }
}

impl Adapter for LocalAdapter {
    fn local_mcp_approval(&self, request: &str) -> Option<crate::local_mcp::ApprovalStamp> {
        if self.mcp_answered.contains(request) {
            None
        } else {
            self.mcp_pending.get(request).cloned()
        }
    }
    fn local_context(&self) -> Option<&crate::local_context::LocalLaunchContext> {
        Some(&self.context)
    }
    fn driver(&self) -> Driver {
        Driver::Local
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// `extra_args` carries the server ([`RunnerArgs::server_args`], from the profile's
    /// provider); the rest comes from the session.
    fn argv(&self, session: &OpenSession) -> Vec<String> {
        let mut argv = vec![session.program.clone(), RUNNER_FLAG.to_owned()];
        argv.extend(session.extra_args.iter().cloned());
        argv.extend(
            RunnerArgs {
                cwd: session.cwd.clone(),
                model: session.model.clone(),
                mode: session.mode,
                resume: session.resume.clone(),
                session_id: session.new_session_id.clone(),
                ..RunnerArgs::default()
            }
            .session_args(),
        );
        argv
    }

    fn handshake(&mut self) -> Vec<String> {
        match serde_json::to_string(&Command::ConfigureLocal {
            context: Box::new(self.context.clone()),
        }) {
            Ok(line) => vec![line],
            Err(_) => Vec::new(),
        }
    }

    fn handshake_answered(&self) -> bool {
        self.started
    }

    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        match &command {
            Command::ConfigureLocal { .. } => {
                Err(AdapterError::Unsupported("Local setup after handshake"))
            }
            Command::Prompt { .. }
            | Command::Interrupt
            | Command::SetModel { .. }
            | Command::SetMode { .. } => Self::write(&command),
            Command::Approve { request, .. } => {
                if !self.pending.contains(request) {
                    return Err(AdapterError::Invalid(format!(
                        "no pending request {request}"
                    )));
                }
                if let Command::Approve {
                    request,
                    decision,
                    updated_input,
                    message,
                } = command
                {
                    if let Some(binding) = self.mcp_pending.get(&request) {
                        if self.mcp_answered.contains(&request) {
                            return Err(AdapterError::Invalid(
                                "MCP approval was already answered.".into(),
                            ));
                        }
                        let encoded = Self::write(&Command::Approve {
                            request: request.clone(),
                            decision,
                            message,
                            updated_input: Some(
                                serde_json::to_value(crate::local_mcp::ApprovalAnswer {
                                    binding: binding.clone(),
                                    edited_arguments: updated_input,
                                })
                                .map_err(|_| {
                                    AdapterError::Invalid(
                                        "MCP approval binding could not be encoded.".into(),
                                    )
                                })?,
                            ),
                        });
                        if encoded.is_ok() {
                            self.mcp_answered.insert(request);
                        }
                        return encoded;
                    }
                    return Self::write(&Command::Approve {
                        request,
                        decision,
                        updated_input,
                        message,
                    });
                }
                Err(AdapterError::Invalid(
                    "Invalid Local approval command.".into(),
                ))
            }
            Command::Steer { .. } => Err(AdapterError::Unsupported("messages during a turn")),
            Command::DelegatedSteer { .. } => Err(AdapterError::Unsupported("delegated steering")),
            Command::Answer { .. } => Err(AdapterError::Unsupported("questions")),
            Command::SubmitForm { .. } => Err(AdapterError::Unsupported("forms")),
            Command::Control { .. } => Err(AdapterError::Unsupported("this control")),
        }
    }

    fn feed(&mut self, line: &str) -> Vec<Envelope> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        match serde_json::from_str::<Envelope>(line) {
            Ok(envelope) => {
                self.observe(&envelope);
                vec![envelope]
            }
            Err(_) => vec![Envelope::new(Event::Unknown).raw(Value::String(line.to_owned()))],
        }
    }

    /// The runner logs to stderr; what matters reaches stdout as events.
    fn feed_stderr(&mut self, _line: &str) -> Vec<Envelope> {
        Vec::new()
    }

    fn feed_side(&mut self, _id: &str, _stdout: &str, _success: bool) -> Vec<Envelope> {
        Vec::new()
    }

    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
        self.mcp_pending.clear();
        self.mcp_answered.clear();
        let mut out = Vec::new();
        for item in std::mem::take(&mut self.open_items) {
            out.push(Envelope::item_completed(
                item,
                ItemStatus::Interrupted,
                None,
                None,
            ));
        }
        for request in std::mem::take(&mut self.pending) {
            out.push(Envelope::new(Event::ApprovalExpired).request(request));
        }
        let expected = code == Some(0);
        if std::mem::take(&mut self.turn_open) {
            out.push(Envelope::turn_completed(
                TurnState::Failed,
                Some(&format!(
                    "The Local agent exited unexpectedly (code {code:?})"
                )),
            ));
        }
        self.started = false;
        out.push(Envelope::new(Event::SessionExited { code, expected }));
        out
    }

    fn pending_request_ids(&self) -> Vec<String> {
        self.pending.iter().cloned().collect()
    }
}

/// The decisions a Local approval card offers.
pub const APPROVAL_OPTIONS: [Decision; 3] =
    [Decision::Allow, Decision::AllowForSession, Decision::Deny];

#[cfg(test)]
mod tests {
    #[test]
    fn builtin_arguments_match_exact_declared_fields_and_types() {
        for tool in Tool::ALL {
            let schema = tool.schema();
            let parameters = &schema["function"]["parameters"];
            assert_eq!(parameters["additionalProperties"], false);
            let mut input = json!({});
            for name in parameters["required"].as_array().unwrap() {
                input[name.as_str().unwrap()] = json!("value");
            }
            assert!(tool.validate_arguments(&input).is_ok());
            for key in [
                "CommandLine",
                "cwd",
                "cmd",
                "file_path",
                "content",
                "changes",
                "edits",
                "old_string",
            ] {
                if parameters["properties"].get(key).is_none() {
                    let mut invalid = input.clone();
                    invalid[key] = json!("spoof");
                    assert!(
                        tool.validate_arguments(&invalid).is_err(),
                        "{} {key}",
                        tool.name()
                    );
                }
            }
            for (key, field) in parameters["properties"].as_object().unwrap() {
                for wrong in [Value::Null, json!([]), json!({}), json!(1.5)] {
                    let mut invalid = input.clone();
                    invalid[key] = wrong;
                    assert!(
                        tool.validate_arguments(&invalid).is_err(),
                        "{} {key}",
                        tool.name()
                    );
                }
                let mut valid = input.clone();
                valid[key] = match field["type"].as_str().unwrap() {
                    "string" => json!("value"),
                    "integer" => json!(1),
                    "boolean" => json!(false),
                    _ => unreachable!(),
                };
                assert!(tool.validate_arguments(&valid).is_ok());
            }
            for name in parameters["required"].as_array().unwrap() {
                let mut invalid = input.clone();
                invalid
                    .as_object_mut()
                    .unwrap()
                    .remove(name.as_str().unwrap());
                assert!(tool.validate_arguments(&invalid).is_err());
            }
            for invalid in [Value::Null, json!([]), json!("value")] {
                assert!(tool.validate_arguments(&invalid).is_err());
            }
        }
    }
    #[test]
    fn setup_is_typed_and_never_part_of_argv() {
        use super::*;
        let context = crate::local_context::LocalLaunchContext::default();
        let mut adapter = LocalAdapter::new().with_context(context.clone());
        let lines = adapter.handshake();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            serde_json::from_str::<Command>(&lines[0]).unwrap(),
            Command::ConfigureLocal {
                context: Box::new(context)
            }
        );
        assert!(!adapter.handshake_answered());
        assert!(!adapter
            .argv(&session())
            .iter()
            .any(|a| a.contains("context_tokens")));
        assert!(adapter
            .encode(Command::ConfigureLocal {
                context: Box::default()
            })
            .is_err());
    }
    use super::*;

    fn session() -> OpenSession {
        OpenSession {
            program: "/usr/bin/agent-terminal".into(),
            extra_args: RunnerArgs::server_args("http://192.0.2.7:11434", None),
            cwd: "/work".into(),
            model: Some("qwen3.5:9b".into()),
            effort: None,
            mode: Mode::AcceptEdits,
            resume: None,
            new_session_id: Some("s-1".into()),
            approval_hook: false,
            delegation: None,
        }
    }

    #[test]
    fn argv_runs_the_app_as_the_runner_and_parses_back() {
        let argv = LocalAdapter::new().argv(&session());
        assert_eq!(argv[..2], ["/usr/bin/agent-terminal", RUNNER_FLAG]);
        let parsed = RunnerArgs::parse(&argv[2..]).expect("parse");
        assert_eq!(
            parsed,
            RunnerArgs {
                server: "http://192.0.2.7:11434".into(),
                key_env: None,
                cwd: "/work".into(),
                model: Some("qwen3.5:9b".into()),
                mode: Mode::AcceptEdits,
                resume: None,
                session_id: Some("s-1".into()),
            }
        );
        // A resume wins over a new id: only one is passed.
        let full = RunnerArgs {
            key_env: Some("LOCAL_KEY".into()),
            resume: Some("s-2".into()),
            session_id: None,
            ..parsed
        };
        assert_eq!(RunnerArgs::parse(&full.to_args()), Ok(full));
    }

    #[test]
    fn runner_arguments_refuse_bad_values() {
        let bad = |args: &[&str]| {
            RunnerArgs::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).is_err()
        };
        assert!(bad(&["--cwd", "/w"]), "no server");
        assert!(bad(&[
            "--server", "http://h", "--resume", "../x", "--cwd", "/w"
        ]));
        assert!(bad(&["--server", "h:1", "--cwd", "/w"]));
        assert!(bad(&[
            "--server",
            "http://h",
            "--cwd",
            "/w",
            "--key-env",
            "A B"
        ]));
        assert!(bad(&[
            "--server", "http://h", "--cwd", "/w", "--mode", "yolo"
        ]));
        assert!(bad(&["--server", "http://h", "--cwd"]));
        assert!(bad(&["--server", "http://h", "--cwd", "/w", "--what"]));
    }

    #[test]
    fn every_mode_has_a_key_that_reads_back() {
        for mode in [Mode::Ask, Mode::AcceptEdits, Mode::Plan, Mode::Bypass] {
            assert_eq!(mode_from_key(mode_key(mode)), Some(mode));
        }
    }

    #[test]
    fn the_gate_follows_the_mode() {
        use Tool::*;
        for tool in [ReadFile, ListDir, Search] {
            for mode in [Mode::Ask, Mode::Plan, Mode::AcceptEdits] {
                assert_eq!(gate(tool, mode, true), Gate::Run);
            }
        }
        assert!(matches!(gate(EditFile, Mode::Plan, false), Gate::Refuse(_)));
        assert_eq!(gate(EditFile, Mode::Ask, false), Gate::Ask);
        assert_eq!(gate(EditFile, Mode::AcceptEdits, false), Gate::Run);
        assert_eq!(
            gate(EditFile, Mode::AcceptEdits, true),
            Gate::Ask,
            "outside"
        );
        assert_eq!(gate(RunCommand, Mode::AcceptEdits, false), Gate::Ask);
        // Every command asks: no answer is remembered by the runner.
        assert_eq!(gate(RunCommand, Mode::Ask, false), Gate::Ask);
        assert_eq!(gate(RunCommand, Mode::Bypass, true), Gate::Run);
        assert!(tools_for(Mode::Plan).iter().all(|t| !t.writes()));
        assert_eq!(tools_for(Mode::Ask).len(), Tool::ALL.len());
    }

    #[test]
    fn every_tool_is_named_once_and_found_by_name() {
        for tool in Tool::ALL {
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
            assert_eq!(tool.schema()["function"]["name"], tool.name());
        }
        assert_eq!(Tool::from_name("rm_rf"), None);
    }

    #[test]
    fn the_request_streams_with_usage_and_offers_tools_only_when_there_are_some() {
        let messages = [ChatMessage::new("user", "hi")];
        let body = chat_request("qwen3.5:9b", &messages, &tools_for(Mode::Ask));
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["content"], "hi");
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(6));
        assert!(chat_request("m", &messages, &[]).get("tools").is_none());
        let answer = serde_json::to_value(ChatMessage::tool_result("call_1", "ok")).unwrap();
        assert_eq!(
            answer,
            json!({"role": "tool", "content": "ok", "tool_call_id": "call_1"})
        );
    }

    /// Recorded from Ollama 0.40.13 (qwen3.5:4b, one tool), trimmed.
    const OLLAMA_STREAM: &str = r#"data: {"id":"chatcmpl-143","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":"The"},"finish_reason":null}]}

data: {"id":"chatcmpl-143","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"","reasoning":" user"},"finish_reason":null}]}

data: {"id":"chatcmpl-143","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"","tool_calls":[{"id":"call_b3lxrmml","index":0,"type":"function","function":{"name":"read_file","arguments":"{\"file_path\":\"src/main.rs\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-143","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl-143","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":277,"completion_tokens":78,"total_tokens":355}}

data: [DONE]"#;

    #[test]
    fn an_ollama_stream_reads_as_reasoning_one_tool_call_and_usage() {
        let mut reader = StreamReader::new();
        let events: Vec<_> = OLLAMA_STREAM.lines().flat_map(|l| reader.feed(l)).collect();
        assert_eq!(
            events,
            [
                StreamEvent::Reasoning("The".into()),
                StreamEvent::Reasoning(" user".into()),
                StreamEvent::Usage(277, 78),
            ]
        );
        assert!(reader.is_done());
        let (reply, rest) = reader.finish();
        assert!(rest.is_empty());
        assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_b3lxrmml");
        assert_eq!(
            reply.tool_calls[0].input(),
            Ok(json!({"file_path": "src/main.rs"}))
        );
    }

    #[test]
    fn fragmented_tool_calls_are_put_together_by_index() {
        let mut reader = StreamReader::new();
        for line in [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"edit_file","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"list_dir","arguments":"{\"path\""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"file_path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":":\".\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ] {
            reader.feed(line);
        }
        let (reply, _) = reader.finish();
        let inputs: Vec<_> = reply
            .tool_calls
            .iter()
            .map(|c| c.input().unwrap())
            .collect();
        assert_eq!(inputs, [json!({"file_path": "a.rs"}), json!({"path": "."})]);
        assert_eq!(reply.tool_calls[1].id, "b");
    }

    #[test]
    fn a_call_without_an_id_gets_one_and_bad_arguments_say_why() {
        let mut reader = StreamReader::new();
        reader.feed(r#"data: {"choices":[{"delta":{"tool_calls":[{"function":{"name":"read_file","arguments":{"file_path":"x"}}}]}}]}"#);
        reader.feed(r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"search","arguments":"{nope"}}]}}]}"#);
        let (reply, _) = reader.finish();
        assert_eq!(reply.tool_calls[0].id, "call_0");
        assert_eq!(reply.tool_calls[0].input(), Ok(json!({"file_path": "x"})));
        assert!(reply.tool_calls[1]
            .input()
            .unwrap_err()
            .contains("not valid JSON"));
    }

    #[test]
    fn reasoning_content_and_think_tags_are_reasoning_even_split_across_pieces() {
        let mut reader = StreamReader::new();
        let mut events = Vec::new();
        for line in [
            r#"data: {"choices":[{"delta":{"reasoning_content":"plan"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"<thi"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"nk>hm</th"}}]}"#,
            // `<` may start a tag, so it waits for the next piece.
            r#"data: {"choices":[{"delta":{"content":"ink>Done <"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"b>!"}}]}"#,
            "data: [DONE]",
        ] {
            events.extend(reader.feed(line));
        }
        let (reply, rest) = reader.finish();
        events.extend(rest);
        assert_eq!(
            events,
            [
                StreamEvent::Reasoning("plan".into()),
                StreamEvent::Reasoning("hm".into()),
                StreamEvent::Text("Done ".into()),
                StreamEvent::Text("<b>!".into()),
            ]
        );
        assert_eq!(reply.text, "Done <b>!");
    }

    #[test]
    fn errors_are_read_from_every_shape() {
        assert_eq!(
            error_message(&json!({"error": {"message": "model 'x' not found"}})).as_deref(),
            Some("model 'x' not found")
        );
        assert_eq!(
            error_message(&json!({"error": "boom"})).as_deref(),
            Some("boom")
        );
        assert_eq!(
            error_message(&json!({"detail": "nope"})).as_deref(),
            Some("nope")
        );
        assert_eq!(error_message(&json!({"ok": 1})), None);
        let mut reader = StreamReader::new();
        assert_eq!(
            reader.feed(r#"data: {"error":{"message":"x does not support tools"}}"#),
            [StreamEvent::Error("x does not support tools".into())]
        );
        assert!(refuses_tools(
            "registry.ollama.ai/library/gemma does not support tools"
        ));
        assert!(matches!(
            reader.feed("data: {nope")[0],
            StreamEvent::Error(_)
        ));
        assert!(reader.feed(": keep-alive").is_empty());
    }

    #[test]
    fn the_adapter_passes_events_through_and_closes_what_is_open_on_exit() {
        let mut a = LocalAdapter::new();
        assert!(!a.handshake_answered());
        let started = Envelope::new(Event::SessionStarted {
            native_id: "s-1".into(),
            model: Some("m".into()),
            cwd: None,
        });
        let line = serde_json::to_string(&started).unwrap();
        assert_eq!(a.feed(&line), vec![started]);
        assert!(a.handshake_answered());
        for env in [
            Envelope::new(Event::TurnStarted { model: None }),
            Envelope::item_started("t1", ItemKind::Command, "ls", None, None),
            Envelope::new(Event::ApprovalRequested {
                tool: "run_command".into(),
                title: None,
                input: json!({"command": "ls"}),
                reason: None,
                options: APPROVAL_OPTIONS.to_vec(),
                response: crate::event::ResponseCapability::Live,
                remembers: None,
            })
            .request("r1"),
        ] {
            a.feed(&serde_json::to_string(&env).unwrap());
        }
        assert_eq!(a.pending_request_ids(), ["r1"]);
        let approve = |request: &str| Command::Approve {
            request: request.into(),
            decision: Decision::Allow,
            updated_input: None,
            message: None,
        };
        assert!(a.encode(approve("r1")).is_ok());
        assert!(a.encode(approve("r9")).is_err());
        let out = a.on_exit(Some(1));
        assert!(matches!(
            out[0].event,
            Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                ..
            }
        ));
        assert!(matches!(out[1].event, Event::ApprovalExpired));
        assert!(matches!(
            out[2].event,
            Event::TurnCompleted {
                state: TurnState::Failed,
                ..
            }
        ));
        assert!(matches!(
            out[3].event,
            Event::SessionExited {
                expected: false,
                ..
            }
        ));
        assert!(matches!(a.feed("garbage")[0].event, Event::Unknown));
    }

    #[test]
    fn commands_go_out_as_one_json_line_and_unsupported_ones_are_refused() {
        let mut a = LocalAdapter::new();
        let actions = a.encode(Command::Prompt { text: "hi".into() }).unwrap();
        let Action::Write(lines) = &actions[0] else {
            panic!("a write");
        };
        assert_eq!(
            serde_json::from_str::<Command>(&lines[0]).unwrap(),
            Command::Prompt { text: "hi".into() }
        );
        assert!(a.encode(Command::Steer { text: "x".into() }).is_err());
        assert!(a
            .encode(Command::Control {
                id: "c".into(),
                control: crate::adapter::Control::McpStatus
            })
            .is_err());
    }

    #[test]
    fn mcp_answers_bind_the_displayed_revision_and_only_encode_once() {
        use crate::local_mcp::{ApprovalAnswer, ApprovalCard, ApprovalStamp, Grant};
        let stamp = ApprovalStamp {
            request_id: "r1".into(),
            generation: "g1".into(),
            call_id: "c1".into(),
            grant: Grant {
                server_id: "example".into(),
                tool: "read".into(),
                server_revision: "a".repeat(64),
                schema_revision: "b".repeat(64),
            },
        };
        let card = ApprovalCard {
            binding: stamp.clone(),
            server_label: "Example".into(),
            arguments: json!({"value":"masked"}),
        };
        let frame = Envelope::new(Event::ApprovalRequested {
            tool: crate::local_mcp::APPROVAL_TOOL.into(),
            title: None,
            input: serde_json::to_value(card).unwrap(),
            reason: None,
            options: crate::local_mcp::APPROVAL_OPTIONS.to_vec(),
            response: crate::event::ResponseCapability::Live,
            remembers: None,
        })
        .request("r1");
        let mut adapter = LocalAdapter::new();
        adapter.feed(&serde_json::to_string(&frame).unwrap());
        assert_eq!(adapter.local_mcp_approval("r1"), Some(stamp.clone()));
        let answer = Command::Approve {
            request: "r1".into(),
            decision: Decision::AllowAlways,
            updated_input: Some(json!({"value":"edited"})),
            message: None,
        };
        let encoded = adapter.encode(answer.clone()).unwrap();
        let Action::Write(lines) = &encoded[0] else {
            panic!("write");
        };
        let Command::Approve {
            updated_input: Some(value),
            ..
        } = serde_json::from_str::<Command>(&lines[0]).unwrap()
        else {
            panic!("approval");
        };
        assert_eq!(
            serde_json::from_value::<ApprovalAnswer>(value).unwrap(),
            ApprovalAnswer {
                binding: stamp,
                edited_arguments: Some(json!({"value":"edited"}))
            }
        );
        assert!(adapter.local_mcp_approval("r1").is_none());
        assert!(adapter.encode(answer.clone()).is_err());
        adapter.feed(
            &serde_json::to_string(&Envelope::new(Event::ApprovalExpired).request("r1")).unwrap(),
        );
        assert!(adapter.encode(answer).is_err());
        adapter.feed(&serde_json::to_string(&frame).unwrap());
        adapter.on_exit(Some(1));
        assert!(adapter.local_mcp_approval("r1").is_none());
    }
}
