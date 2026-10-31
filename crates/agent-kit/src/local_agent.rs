//! The Local agent's runner: `agent-terminal --local-agent`, the agent loop the app runs itself
//! for a model on a local or self-hosted server (see `agent_core::local_agent`).
//!
//! It reads canonical [`Command`]s from stdin, one JSON line each, and writes canonical
//! [`Envelope`]s to stdout. A prompt starts a turn: the conversation goes to the server's
//! OpenAI-compatible `/v1/chat/completions` (streamed), each tool call the model makes is shown,
//! gated by the mode (asking the user over the same pipes when it must) and run in the thread's
//! folder, and its result goes back to the model until it answers without a tool call.
//!
//! The conversation is kept as a private file under the app's state folder, so a resumed
//! session continues it. Requests go only to the configured server; redirects are not followed.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use agent_core::adapter::{Command, Mode};
use agent_core::event::{
    Decision, Envelope, Event, ItemKind, ItemStatus, ReasoningLabel, ReasoningState,
    ResponseCapability, StreamKind, TurnState, Usage,
};
use agent_core::local_agent::{
    error_message, gate, refuses_tools, tools_for, ChatMessage, Gate, RunnerArgs, StreamEvent,
    StreamReader, Tool, ToolCall, APPROVAL_OPTIONS,
};
use agent_core::local_context::{exchange_starts, ContextBudget, RequestPlan};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most model requests one turn may make: a model that keeps calling tools is stopped.
const MAX_STEPS: usize = 60;
/// The longest a server may take to start answering (a model may have to load first).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The most a reply may hold in all, and in one line: a server cannot make the runner hold more.
const MAX_REPLY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
/// The longest a reply may go without a line before it counts as stalled.
const STALL: Duration = Duration::from_secs(600);
const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
/// The most of the conversation kept at all (memory and file), in bytes of text: a few requests'
/// worth, so the oldest exchanges go once they could never be sent again.
const RETAIN_BUDGET: usize = 1_600_000;

/// One line of a streamed reply, or why the reply failed. The channel closing is its end.
pub type ReplyLine = Result<String, String>;

/// What the runner needs from a server: one streamed chat request. The reply arrives on the
/// channel line by line, read elsewhere, so the runner can see a Stop while the server is silent.
/// [`ChatClient::cancel`] ends the request in flight, whatever it is waiting on.
pub trait ChatClient {
    fn start(&mut self, body: &Value) -> Receiver<ReplyLine>;
    /// Ends the request [`ChatClient::start`] began, if it is still going.
    fn cancel(&mut self) {}
}

/// The real client: `POST <server>/v1/chat/completions` with `ureq`, no redirects, read on a
/// thread of its own. Each request has an agent of its own, so cancelling one closes its
/// connection (`Agent::shutdown`), which wakes a read the server left waiting and tells the
/// server to stop generating.
pub struct HttpChat {
    url: String,
    key: Option<String>,
    user_agent: String,
    current: Option<ureq::Agent>,
}

impl HttpChat {
    pub fn new(server: &str, key: Option<String>, user_agent: &str) -> Self {
        Self {
            url: format!("{server}/v1/chat/completions"),
            key: key.filter(|k| !k.is_empty()),
            user_agent: user_agent.to_owned(),
            current: None,
        }
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            // A ceiling on a whole reply; a silent server is noticed sooner (`STALL`).
            .timeout_recv_body(Some(Duration::from_secs(3600)))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(&self.user_agent)
            .build()
            .into()
    }

    fn request(
        agent: &ureq::Agent,
        url: &str,
        key: Option<&str>,
        body: &Value,
        tx: &std::sync::mpsc::Sender<ReplyLine>,
    ) -> Result<(), String> {
        let mut request = agent.post(url).header("Accept", "text/event-stream");
        if let Some(key) = key {
            request = request.header("Authorization", &format!("Bearer {key}"));
        }
        let mut reply = request
            .send_json(body)
            .map_err(|e| format!("Could not reach the model server: {}", plain_error(&e)))?;
        let status = reply.status().as_u16();
        if !(200..300).contains(&status) {
            let text = reply
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| error_message(&v))
                .unwrap_or_else(|| text.trim().chars().take(300).collect());
            return Err(match status {
                401 | 403 => "The model server refused the API key.".to_owned(),
                _ if detail.is_empty() => format!("The model server answered with HTTP {status}."),
                _ => format!("The model server answered with HTTP {status}: {detail}"),
            });
        }
        let body = reply
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .reader();
        let mut reader = std::io::BufReader::new(body);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = std::io::Read::take(&mut reader, MAX_LINE_BYTES)
                .read_until(b'\n', &mut line)
                .map_err(|e| format!("The model server's reply broke off: {e}"))?;
            if read == 0 {
                return Ok(());
            }
            if line.last() != Some(&b'\n') && read as u64 == MAX_LINE_BYTES {
                return Err("The model server sent a line too long to read.".to_owned());
            }
            let text = String::from_utf8_lossy(&line).trim_end().to_owned();
            if tx.send(Ok(text)).is_err() {
                return Ok(()); // abandoned: a Stop
            }
        }
    }
}

impl ChatClient for HttpChat {
    fn start(&mut self, body: &Value) -> Receiver<ReplyLine> {
        self.cancel();
        let (tx, rx) = std::sync::mpsc::channel();
        let agent = self.agent();
        self.current = Some(agent.clone());
        let (url, key, body) = (self.url.clone(), self.key.clone(), body.clone());
        std::thread::spawn(move || {
            if let Err(why) = Self::request(&agent, &url, key.as_deref(), &body, &tx) {
                let _ = tx.send(Err(why));
            }
        });
        rx
    }

    fn cancel(&mut self) {
        if let Some(agent) = self.current.take() {
            agent.shutdown();
        }
    }
}

/// A transport error as the user reads it: never a URL with credentials (there are none, the
/// address is validated) and never the key (it is not in the error).
fn plain_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "it did not answer in time".to_owned(),
        ureq::Error::ConnectionFailed | ureq::Error::Io(_) => "the connection failed".to_owned(),
        ureq::Error::HostNotFound => "the host was not found".to_owned(),
        other => other.to_string(),
    }
}

/// Only an explicit input-context rejection warrants advice about the context budget.
fn provider_failure(why: &str, budget: &ContextBudget) -> String {
    let lower = why.to_ascii_lowercase();
    let context_rejection = !["http 401", "http 403"]
        .iter()
        .any(|status| lower.contains(status))
        && [
            "context_length_exceeded",
            "context_window_exceeded",
            "maximum context length",
            "context length exceeded",
            "context window exceeded",
            "exceeds the context",
            "exceeds context",
            "exceed context",
            "context is too long",
            "context too large",
            "prompt is too long",
            "prompt too long",
            "too many input tokens",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
    let hint = if context_rejection {
        format!(
            " {}",
            budget.failure(
                "the provider rejected the input context; check its configured context size"
            )
        )
    } else {
        String::new()
    };
    format!("{why}{hint} No tool calls from this response ran; the request was not automatically replayed.")
}

/// The conversation as the transcript file keeps it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Transcript {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    messages: Vec<ChatMessage>,
}

/// Where the Local agent keeps its conversations: `<state>/agent-terminal/local`, inside the
/// folder the app's uninstall removes.
pub fn sessions_dir(xdg_state: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_state.filter(|s| Path::new(s).is_absolute()) {
        Some(state) => PathBuf::from(state),
        None => Path::new(home.filter(|h| Path::new(h).is_absolute())?).join(".local/state"),
    };
    Some(base.join("agent-terminal").join("local"))
}

/// `agent-terminal --local-agent …`: parses the arguments, reads the key from its variable, and
/// runs until stdin closes. The exit code is the process's.
pub fn main(args: &[String], user_agent: &str) -> i32 {
    let args = match RunnerArgs::parse(args) {
        Ok(args) => args,
        Err(why) => {
            eprintln!("agent-terminal --local-agent: {why}");
            return 2;
        }
    };
    let key = match &args.key_env {
        Some(name) => match std::env::var(name) {
            Ok(value) if !value.is_empty() => Some(value),
            _ => {
                eprintln!("agent-terminal --local-agent: {name} is not set");
                return 2;
            }
        },
        None => None,
    };
    let Some(dir) = sessions_dir(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    ) else {
        eprintln!("agent-terminal --local-agent: no home folder to keep conversations in");
        return 2;
    };
    let commands = read_commands(std::io::stdin());
    let client = HttpChat::new(&args.server, key, user_agent);
    let stdout = std::io::stdout();
    let mut runner = Runner::new(args, client, dir, stdout.lock());
    runner.run(&commands)
}

/// Local stdin transport, keeping malformed frames out of the canonical command protocol.
#[derive(Debug)]
pub enum RunnerInput {
    Command(Command),
    Closed,
    /// Fixed diagnostics only: rejected frames may contain credentials or instructions.
    Invalid(&'static str),
}

/// Reads stdin on its own thread: each line a command (`Closed` once stdin closes), so a turn can
/// look for an interrupt or an answer while it waits on the server. Invalid input is terminal.
pub fn read_commands(input: impl std::io::Read + Send + 'static) -> Receiver<RunnerInput> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(input);
        loop {
            let mut bytes = Vec::new();
            // Includes startup configuration; oversized or unterminated frames end the input.
            let count = match std::io::Read::take(
                &mut reader,
                agent_core::local_context::MAX_SETUP_BYTES as u64 + 1,
            )
            .read_until(b'\n', &mut bytes)
            {
                Ok(count) => count,
                Err(_) => {
                    let _ = tx.send(RunnerInput::Invalid("Could not read Local command input."));
                    return;
                }
            };
            if count == 0 {
                break;
            }
            if count > agent_core::local_context::MAX_SETUP_BYTES {
                let _ = tx.send(RunnerInput::Invalid(
                    "Local command exceeds the startup frame limit.",
                ));
                return;
            }
            if bytes.last() != Some(&b'\n') {
                let _ = tx.send(RunnerInput::Invalid(
                    "Local command frame is missing its newline.",
                ));
                return;
            }
            let Ok(line) = String::from_utf8(bytes) else {
                let _ = tx.send(RunnerInput::Invalid("Local command is not valid UTF-8."));
                return;
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Command>(&line) {
                Ok(command) => {
                    if tx.send(RunnerInput::Command(command)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    // App and runner ship in one binary: an unparsable command is a fatal
                    // protocol mismatch, never something to skip while awaiting more input.
                    let _ = tx.send(RunnerInput::Invalid("Malformed Local command JSON; check the startup configuration and command format."));
                    return;
                }
            }
        }
        let _ = tx.send(RunnerInput::Closed);
    });
    rx
}

/// How a turn ended early.
enum Stop {
    Interrupted,
    InvalidInput(&'static str),
    /// Stdin closed: the app is gone.
    Closed,
}

pub struct Runner<C: ChatClient, W: Write> {
    mcp_connections: std::collections::BTreeMap<String, crate::local_mcp::ConnectionLease>,
    registry: std::collections::BTreeMap<String, agent_core::local_mcp::RegistryTool>,
    masks: crate::local_mcp::CredentialMasks,
    mcp: Option<crate::local_mcp::Session>,
    budget_notice_sent: bool,
    turn_instructions: agent_core::local_context::InstructionSnapshot,
    context: agent_core::local_context::LocalLaunchContext,
    args: RunnerArgs,
    /// `None` only while a request is under way.
    client: Option<C>,
    dir: PathBuf,
    out: W,
    session: String,
    model: Option<String>,
    mode: Mode,
    messages: Vec<ChatMessage>,
    /// Commands that arrived during a turn and wait for its end.
    deferred: Vec<Command>,
    /// This process's part of every item and request id, so a resumed session (a new process)
    /// never reuses an id the thread already shows.
    generation: String,
    next_id: u64,
    /// The current model cannot take tools: it goes without them until the model changes.
    no_tools: bool,
    /// The thread's folder, opened when the session starts: every write and command is
    /// anchored to it.
    workspace: Option<std::sync::Arc<crate::local_tools::Workspace>>,
}

impl<C: ChatClient, W: Write> Runner<C, W> {
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_mcp_for_test(mut self, session: crate::local_mcp::Session) -> Self {
        self.masks = session.masks();
        self.mcp = Some(session);
        self
    }
    pub fn new(args: RunnerArgs, client: C, dir: PathBuf, out: W) -> Self {
        let session = args
            .resume
            .clone()
            .or_else(|| args.session_id.clone())
            .unwrap_or_else(new_session_id);
        Self {
            mcp_connections: Default::default(),
            registry: Default::default(),
            masks: Default::default(),
            mcp: None,
            model: args.model.clone(),
            context: Default::default(),
            turn_instructions: Default::default(),
            budget_notice_sent: false,
            mode: args.mode,
            args,
            client: Some(client),
            dir,
            out,
            session,
            messages: Vec::new(),
            deferred: Vec::new(),
            generation: new_session_id()[..8].to_owned(),
            next_id: 0,
            no_tools: false,
            workspace: None,
        }
    }

    fn transcript_path(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.session))
    }

    fn emit(&mut self, envelope: Envelope) {
        let Ok(mut value) = serde_json::to_value(&envelope) else {
            return;
        };
        self.masks.value_lossy(&mut value);
        match &envelope.event {
            Event::ContentDelta { text, .. } | Event::ContentSnapshot { text, .. } => {
                value["event"]["text"] = Value::String(self.masks.scrub(text));
            }
            Event::ItemCompleted { output, error, .. } => {
                if let Some(output) = output {
                    value["event"]["output"] = Value::String(self.masks.scrub(output));
                }
                if let Some(error) = error {
                    value["event"]["error"] = Value::String(self.masks.scrub(error));
                }
            }
            _ => {}
        }
        self.emit_value(value);
    }

    fn emit_value(&mut self, value: Value) {
        if let Ok(line) = serde_json::to_string(&value) {
            // A closed stdout means the app is gone; the next read of stdin ends the run.
            let _ = writeln!(self.out, "{line}");
            let _ = self.out.flush();
        }
    }

    fn emit_approval(&mut self, envelope: Envelope) -> bool {
        let Ok(mut value) = serde_json::to_value(&envelope) else {
            return false;
        };
        let masked = self.masks.value(&mut value);
        let intact = serde_json::from_value::<Envelope>(value.clone()).is_ok_and(|safe| {
            safe.request == envelope.request
                && safe.item == envelope.item
                && match (&envelope.event, &safe.event) {
                    (
                        Event::ApprovalRequested { tool, input, .. },
                        Event::ApprovalRequested {
                            tool: shown,
                            input: shown_input,
                            ..
                        },
                    ) => tool == shown && input.get("binding") == shown_input.get("binding"),
                    _ => false,
                }
        });
        if masked.is_err() || !intact {
            self.emit(Envelope::new(Event::Error {message:"The call was refused because its approval cannot be displayed faithfully after secret masking.".into()}));
            return false;
        }
        self.emit_value(value);
        true
    }

    fn id(&mut self, what: &str) -> String {
        self.next_id += 1;
        format!("{what}-{}-{}", self.generation, self.next_id)
    }

    /// Runs until stdin closes; 0 then, 1 for failed startup or invalid command input.
    pub fn run(&mut self, commands: &Receiver<RunnerInput>) -> i32 {
        let code = self.run_inner(commands);
        self.stop_mcp();
        code
    }

    fn stop_mcp(&mut self) {
        if let Some(mut mcp) = self.mcp.take() {
            mcp.shutdown();
        }
    }

    fn run_inner(&mut self, commands: &Receiver<RunnerInput>) -> i32 {
        match commands.recv() {
            Ok(RunnerInput::Invalid(why)) => {
                self.emit(Envelope::new(Event::Error {
                    message: why.into(),
                }));
                return 1;
            }
            Ok(RunnerInput::Command(Command::ConfigureLocal { context })) => {
                if let Err(why) = context.validate() {
                    self.emit(Envelope::new(Event::Error { message: why }));
                    return 1;
                }
                self.context = *context;
            }
            _ => {
                self.emit(Envelope::new(Event::Error {
                    message: "Local setup is required before any prompt.".into(),
                }));
                return 1;
            }
        }
        if let Err(why) = self.load() {
            self.emit(Envelope::new(Event::Error { message: why }));
            return 1;
        }
        match crate::local_tools::Workspace::open(Path::new(&self.args.cwd)) {
            Ok(ws) => self.workspace = Some(std::sync::Arc::new(ws)),
            Err(why) => {
                self.emit(Envelope::new(Event::Error { message: why }));
                return 1;
            }
        }
        self.emit(Envelope::new(Event::SessionStarted {
            native_id: self.session.clone(),
            model: self.model.clone(),
            cwd: Some(self.args.cwd.clone()),
        }));
        loop {
            let next = if self.deferred.is_empty() {
                match commands.recv() {
                    Ok(RunnerInput::Command(command)) => command,
                    Ok(RunnerInput::Closed) | Err(_) => return 0,
                    Ok(RunnerInput::Invalid(why)) => {
                        self.emit(Envelope::new(Event::Error {
                            message: why.into(),
                        }));
                        return 1;
                    }
                }
            } else {
                self.deferred.remove(0)
            };
            match self.handle(next, commands) {
                Err(Stop::Closed) => return 0,
                Err(Stop::InvalidInput(why)) => {
                    self.emit(Envelope::new(Event::Error {
                        message: why.into(),
                    }));
                    return 1;
                }
                Ok(()) | Err(Stop::Interrupted) => {}
            }
        }
    }

    /// Reads the conversation of a resumed session; a new one starts empty.
    fn load(&mut self) -> Result<(), String> {
        if self.args.resume.is_none() {
            return Ok(());
        }
        let path = self.transcript_path();
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_TRANSCRIPT_BYTES => {
                return Err("The saved conversation is too large to read back.".to_owned());
            }
            _ => {}
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // A session that never finished a turn left nothing: it starts over.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("Could not read the conversation: {e}")),
        };
        let transcript: Transcript = serde_json::from_str(&text)
            .map_err(|e| format!("The saved conversation is unreadable: {e}"))?;
        self.messages = transcript
            .messages
            .into_iter()
            .filter(|m| m.role != "system")
            .collect();
        if self.model.is_none() {
            self.model = transcript.model;
        }
        Ok(())
    }

    /// Writes the conversation, every text in it redacted as the thread store's events are: a
    /// secret read from a file or typed in a prompt is never kept in the clear. The live
    /// conversation is left as it is, so this turn's next request still has it.
    fn save(&mut self) {
        // What is kept is bounded, in whole exchanges, so it never grows past what can be sent.
        drop_oldest_until(&mut self.messages, |kept| {
            kept.iter().map(message_size).sum::<usize>() <= RETAIN_BUDGET
        });
        let mut messages: Vec<ChatMessage> = self
            .messages
            .iter()
            .map(|m| ChatMessage {
                content: m.content.clone(),
                tool_calls: m
                    .tool_calls
                    .iter()
                    .map(|c| ToolCall {
                        function: agent_core::local_agent::ToolFunction {
                            name: c.function.name.clone(),
                            arguments: self.safe_arguments(&c.function.arguments),
                        },
                        ..c.clone()
                    })
                    .collect(),
                // Reasoning is never kept: it only matters inside its own turn.
                reasoning_content: None,
                ..m.clone()
            })
            .collect();
        // Never a file the loader would refuse: the oldest exchanges go until it fits, and with
        // only the latest left and still too large, nothing is kept.
        let encode = |messages: &[ChatMessage]| {
            let mut value = serde_json::to_value(Transcript {
                model: self.model.clone(),
                messages: messages.to_vec(),
            })?;
            self.masks.value_lossy(&mut value);
            serde_json::to_vec(&value)
        };
        drop_oldest_until(&mut messages, |kept| {
            encode(kept).is_ok_and(|bytes| bytes.len() as u64 <= MAX_TRANSCRIPT_BYTES)
        });
        if encode(&messages).is_ok_and(|bytes| bytes.len() as u64 > MAX_TRANSCRIPT_BYTES) {
            messages.clear();
        }
        let result = encode(&messages)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                create_private_dir(&self.dir)?;
                crate::fsutil::write_private_atomic(&self.transcript_path(), &bytes)
            });
        if let Err(e) = result {
            self.emit(Envelope::new(Event::Notice {
                text: format!("The conversation could not be saved, so it cannot be resumed: {e}"),
            }));
        }
    }

    fn handle(&mut self, command: Command, commands: &Receiver<RunnerInput>) -> Result<(), Stop> {
        match command {
            Command::Prompt { text } => self.turn(text, commands),
            Command::Interrupt => {
                self.stop_mcp();
                Ok(())
            }
            Command::SetModel { model, .. } => {
                // Another model may take tools: it is offered them again.
                self.no_tools = false;
                self.model = Some(model.clone());
                self.emit(Envelope::new(Event::ModelChanged { model }));
                Ok(())
            }
            Command::SetMode { mode } => {
                self.mode = mode;
                self.emit(Envelope::new(Event::ModeChanged { mode }));
                Ok(())
            }
            // Nothing to interrupt or answer between turns.
            _ => Ok(()),
        }
    }

    /// One turn: the prompt, then model requests and tool calls until the model answers.
    fn turn(&mut self, text: String, commands: &Receiver<RunnerInput>) -> Result<(), Stop> {
        self.budget_notice_sent = false;
        let Some(model) = self.model.clone() else {
            self.emit(Envelope::new(Event::TurnStarted { model: None }));
            self.emit(Envelope::turn_completed(
                TurnState::Failed,
                Some("No model is chosen: pick one of the server's models."),
            ));
            return Ok(());
        };
        self.emit(Envelope::new(Event::TurnStarted {
            model: Some(model.clone()),
        }));
        self.turn_instructions = match crate::local_instructions::discover(
            &self.context.instructions,
            Path::new(&self.args.cwd),
            &crate::local_instructions::InstructionPaths::from_env(),
        ) {
            Ok(snapshot) => snapshot,
            Err(why) => {
                self.emit(Envelope::turn_completed(
                    TurnState::Failed,
                    Some(&agent_core::redact::redact(&why)),
                ));
                return Ok(());
            }
        };
        if let Err(error) = self.prepare_mcp(commands) {
            self.stop_mcp();
            let (state, message) = match &error {
                StepError::Stop(Stop::InvalidInput(why)) => (TurnState::Failed, Some(*why)),
                StepError::Stop(_) => (TurnState::Interrupted, None),
                StepError::Failed(why) => (TurnState::Failed, Some(why.as_str())),
            };
            self.emit(Envelope::turn_completed(state, message));
            return match error {
                StepError::Stop(stop) => {
                    if matches!(stop, Stop::Interrupted) {
                        Ok(())
                    } else {
                        Err(stop)
                    }
                }
                StepError::Failed(_) => Ok(()),
            };
        }
        // Earlier turns' reasoning is not sent again.
        agent_core::local_agent::forget_reasoning(&mut self.messages);
        let exchange_start = self.messages.len();
        self.messages.push(ChatMessage::new("user", text));
        let mut usage = Usage::default();
        let mut result = Ok(());
        let mut failure = None;
        for step in 0..MAX_STEPS {
            match self.step(&model, commands, &mut usage) {
                Ok(StepEnd::Answered) => break,
                Ok(StepEnd::Continue) if step + 1 == MAX_STEPS => {
                    failure = Some(format!(
                        "Stopped after {MAX_STEPS} model requests in one turn."
                    ));
                }
                Ok(StepEnd::Continue) => {}
                Err(StepError::Stop(stop)) => {
                    result = Err(stop);
                    break;
                }
                Err(StepError::Failed(why)) => {
                    if step == 0 && self.messages.len() == exchange_start + 1 {
                        self.messages.pop();
                    }
                    failure = Some(why);
                    break;
                }
            }
        }
        self.save();
        let usage = (usage.input_tokens + usage.output_tokens > 0).then_some(usage);
        let (state, error) = match (&result, failure) {
            (Err(Stop::InvalidInput(why)), _) => (TurnState::Failed, Some((*why).into())),
            (Err(_), _) => (TurnState::Interrupted, None),
            (Ok(()), Some(why)) => (TurnState::Failed, Some(why)),
            (Ok(()), None) => (TurnState::Completed, None),
        };
        self.emit(Envelope::new(Event::TurnCompleted {
            state,
            usage,
            cost_usd: None,
            error,
        }));
        if result.is_err() {
            self.stop_mcp();
        }
        match result {
            Err(Stop::Interrupted) => Ok(()),
            other => other,
        }
    }

    fn prepare_mcp(&mut self, commands: &Receiver<RunnerInput>) -> Result<(), StepError> {
        self.registry.clear();
        self.mcp_connections.clear();
        if self.no_tools || !self.context.mcp_servers.iter().any(|server| server.enabled) {
            return Ok(());
        }
        if self.mcp.is_none() {
            self.mcp = Some(
                crate::local_mcp::Session::new(
                    self.context.mcp_servers.clone(),
                    Path::new(&self.args.cwd),
                )
                .map_err(StepError::Failed)?,
            );
            if let Some(session) = &self.mcp {
                self.masks = session.masks();
            }
        }
        let Some(session) = &self.mcp else {
            return Ok(());
        };
        let discovery = session.discover(self.mode).map_err(StepError::Failed)?;
        loop {
            if let Some(stop) = poll(commands, &mut self.deferred) {
                return Err(StepError::Stop(stop));
            }
            match discovery.recv_timeout(Duration::from_millis(20)) {
                Ok(discovery) => {
                    let mut required_failed = false;
                    for server in discovery.servers {
                        if let Some(connection) = server.connection {
                            self.mcp_connections.insert(server.id.clone(), connection);
                        }
                        required_failed |= server.required_failed;
                        for text in server.notices {
                            self.emit(Envelope::new(Event::Notice { text }));
                        }
                        for entry in server.registry {
                            if self
                                .registry
                                .insert(entry.resolved.alias.clone(), entry)
                                .is_some()
                            {
                                return Err(StepError::Failed(
                                    "MCP alias collision; no calls were run.".into(),
                                ));
                            }
                        }
                    }
                    return if required_failed {
                        Err(StepError::Failed(
                            "A required MCP server is unavailable.".into(),
                        ))
                    } else {
                        Ok(())
                    };
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(StepError::Failed(
                        "MCP runtime ended during discovery.".into(),
                    ))
                }
            }
        }
    }

    /// One model request and the tool calls it makes.
    fn step(
        &mut self,
        model: &str,
        commands: &Receiver<RunnerInput>,
        usage: &mut Usage,
    ) -> Result<StepEnd, StepError> {
        use agent_core::local_context::{
            bound_result, compose_system, plan_request, reserve_batch, SchemaCandidate,
        };
        let mut candidates: Vec<_> = if self.no_tools {
            Vec::new()
        } else {
            tools_for(self.mode)
                .into_iter()
                .map(|tool| SchemaCandidate {
                    tool: agent_core::local_mcp::ResolvedTool::builtin(tool, tool.schema()),
                    priority: 0,
                })
                .collect()
        };
        if !self.no_tools {
            candidates.extend(self.registry.values().map(|entry| {
                SchemaCandidate {
                    priority: match &entry.resolved.identity {
                        agent_core::local_mcp::ToolIdentity::Mcp { server_id, .. } => self
                            .context
                            .mcp_servers
                            .iter()
                            .position(|s| &s.id == server_id)
                            .unwrap_or(usize::MAX),
                        _ => usize::MAX,
                    },
                    tool: entry.resolved.clone(),
                }
            }));
        }
        let system = compose_system(
            &self.args.cwd,
            self.mode,
            &self.turn_instructions,
            !self.no_tools,
        );
        let plan = plan_request(
            model,
            &system,
            &candidates,
            &self.messages,
            &self.context.budget,
        )
        .map_err(|why| {
            StepError::Failed(format!(
                "{why}\n{}",
                self.turn_instructions.sources_summary()
            ))
        })?;
        self.report_budget(&plan);
        let body = plan.body;
        let (reply, ended, stopped) = self.stream(model, &body, commands, usage)?;
        // A call runs only from a reply that ended as the server meant it to: one the output
        // limit cut off, or that broke off before its end, may hold a half-written call.
        let complete = ended && reply.finish_reason.as_deref() != Some("length");
        let runnable = stopped.is_none() && complete;
        // What arrived before an interrupt is kept, so the next turn knows it was said; calls
        // only when they will each get an answer below.
        if !reply.text.is_empty() || !reply.tool_calls.is_empty() {
            let calls = runnable && !reply.tool_calls.is_empty();
            self.messages.push(ChatMessage {
                tool_calls: if calls {
                    reply
                        .tool_calls
                        .iter()
                        .map(|call| {
                            let mut safe = call.clone();
                            safe.function.arguments = self.safe_arguments(&call.function.arguments);
                            safe
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                // Sent back with its calls while this turn runs (`forget_reasoning`).
                reasoning_content: (calls && !reply.reasoning.is_empty())
                    .then(|| self.safe_text(&reply.reasoning)),
                ..ChatMessage::new("assistant", self.safe_text(&reply.text))
            });
        }
        if let Some(stop) = stopped {
            return Err(StepError::Stop(stop));
        }
        if !complete && !reply.tool_calls.is_empty() {
            return Err(StepError::Failed(
                "The model's reply was cut off before it ended (its output limit, or the \
                 connection), in the middle of a tool call, so nothing was run."
                    .to_owned(),
            ));
        }
        if reply.tool_calls.is_empty() {
            return Ok(StepEnd::Answered);
        }
        let schemas = if self.no_tools {
            Vec::new()
        } else {
            plan.schemas
        };
        let system = compose_system(
            &self.args.cwd,
            self.mode,
            &self.turn_instructions,
            !self.no_tools,
        );
        let reservation = match reserve_batch(
            model,
            &system,
            &schemas,
            &self.messages,
            &self.context.budget,
        ) {
            Ok(bytes) => bytes,
            Err(why) => {
                // The call batch never became executable; keep the assistant text without
                // orphaned calls or reasoning that would require tool answers.
                if let Some(last) = self.messages.last_mut() {
                    last.tool_calls.clear();
                    last.reasoning_content = None;
                    if last.content.is_empty() {
                        self.messages.pop();
                    }
                }
                return Err(StepError::Failed(why));
            }
        };
        for (at, call) in reply.tool_calls.iter().enumerate() {
            let advertised = schemas.iter().any(|schema| {
                schema.pointer("/function/name").and_then(Value::as_str)
                    == Some(call.function.name.as_str())
            });
            match self.call(call, commands, reservation, advertised) {
                Ok(output) => self.messages.push(ChatMessage::tool_result(
                    &call.id,
                    bound_result(&self.safe_text(&output), reservation),
                )),
                Err(stop) => {
                    // Every call gets an answer. The interrupted MCP call may already have
                    // run; the remaining calls certainly did not.
                    let reason = if matches!(stop, StepError::Stop(Stop::InvalidInput(_))) {
                        "Not run: malformed input ended the turn."
                    } else {
                        "Not run: the user stopped the turn."
                    };
                    for (offset, skipped) in reply.tool_calls[at..].iter().enumerate() {
                        let output = if offset == 0
                            && self.registry.contains_key(&skipped.function.name)
                        {
                            // Cancellation can race with a server-side mutation. Never tell
                            // a later request that the interrupted call certainly did not run.
                            agent_core::local_mcp::result_text(
                                &serde_json::json!({"isError":true,"content":[{"type":"text","text":"MCP call interrupted without a confirmed result; its outcome may be unknown. Do not retry automatically."}]}),
                                reservation,
                            )
                        } else {
                            bound_result(reason, reservation)
                        };
                        self.messages
                            .push(ChatMessage::tool_result(&skipped.id, output));
                    }
                    return Err(stop);
                }
            }
        }
        Ok(StepEnd::Continue)
    }

    /// Streams one reply, showing its reasoning and text as they come. Also says whether the
    /// reply ended as the server meant it to (a finish reason or `[DONE]`), and whether a Stop
    /// (or stdin closing) ended it first: that is seen within a tenth of a second even while the
    /// server is silent.
    fn stream(
        &mut self,
        model: &str,
        body: &Value,
        commands: &Receiver<RunnerInput>,
        usage: &mut Usage,
    ) -> Result<(agent_core::local_agent::Reply, bool, Option<Stop>), StepError> {
        let mut reader = StreamReader::new();
        let mut shown = Shown::default();
        let mut stopped = None;
        let mut server_error = None;
        let mut failed = None;
        let Some(client) = self.client.as_mut() else {
            return Err(StepError::Failed(
                "The model server client is gone.".to_owned(),
            ));
        };
        let lines = client.start(body);
        let mut last_line = std::time::Instant::now();
        loop {
            if let Some(stop) = poll(commands, &mut self.deferred) {
                stopped = Some(stop);
                break; // the request is ended below
            }
            match lines.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(line)) => {
                    last_line = std::time::Instant::now();
                    for event in reader.feed(&line) {
                        match event {
                            StreamEvent::Error(why) => server_error = Some(why),
                            other => self.show(&mut shown, other, usage),
                        }
                    }
                    if reader.is_done() {
                        break;
                    }
                }
                Ok(Err(why)) => {
                    failed = Some(why);
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) if last_line.elapsed() > STALL => {
                    failed = Some("The model server stopped answering.".to_owned());
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        // Whatever ended the reading (a Stop, a stall, an error, the end), the request ends too:
        // its connection is closed, so no thread waits on a silent server and the server stops.
        drop(lines);
        if let Some(client) = self.client.as_mut() {
            client.cancel();
        }
        let done = reader.is_done();
        let (reply, rest) = reader.finish();
        let ended = done || reply.finish_reason.is_some();
        for event in rest {
            self.show(&mut shown, event, usage);
        }
        self.close_shown(&mut shown, stopped.is_some());
        if stopped.is_some() {
            return Ok((reply, ended, stopped));
        }
        let failure = failed.or(server_error);
        if let Some(why) = failure {
            if !self.no_tools && body.get("tools").is_some() && refuses_tools(&why) {
                self.no_tools = true;
                self.stop_mcp();
                self.emit(Envelope::new(Event::Notice {
                    text: "This model cannot use tools, so it answers without reading or \
                           changing anything. Pick a model that supports tools to let it work."
                        .to_owned(),
                }));
                let system = agent_core::local_context::compose_system(
                    &self.args.cwd,
                    self.mode,
                    &self.turn_instructions,
                    false,
                );
                let retry = agent_core::local_context::plan_request(
                    model,
                    &system,
                    &[],
                    &self.messages,
                    &self.context.budget,
                )
                .map_err(StepError::Failed)?;
                self.report_budget(&retry);
                return self.stream(model, &retry.body, commands, usage);
            }
            return Err(StepError::Failed(provider_failure(
                &why,
                &self.context.budget,
            )));
        }
        Ok((reply, ended, None))
    }

    fn report_budget(&mut self, plan: &RequestPlan) {
        if !self.budget_notice_sent {
            self.emit(Envelope::new(Event::Notice {
                text: plan.summary(),
            }));
            self.budget_notice_sent = true;
            for excluded in &plan.excluded {
                self.emit(Envelope::new(Event::Notice {
                    text: format!(
                        "Excluded tool {}: {} bytes; {}.",
                        excluded.alias, excluded.bytes, excluded.reason
                    ),
                }));
            }
        }
    }

    fn show(&mut self, shown: &mut Shown, event: StreamEvent, usage: &mut Usage) {
        match event {
            StreamEvent::Reasoning(text) => {
                let id = match &shown.reasoning {
                    Some((id, _)) => id.clone(),
                    None => {
                        let id = self.id("reasoning");
                        self.emit(Envelope::item_started(
                            &id,
                            ItemKind::Reasoning,
                            "reasoning",
                            None,
                            None,
                        ));
                        self.emit(
                            Envelope::new(Event::Reasoning {
                                state: ReasoningState::Streaming,
                                label: ReasoningLabel::Thinking,
                            })
                            .item(&id),
                        );
                        shown.reasoning = Some((id.clone(), String::new()));
                        id
                    }
                };
                if let Some((_, all)) = &mut shown.reasoning {
                    all.push_str(&text);
                }
                shown.reasoning_pending.push_str(&text);
                let safe = self.masks.drain_stream(&mut shown.reasoning_pending, false);
                if !safe.is_empty() {
                    self.emit(Envelope::delta(id, StreamKind::Reasoning, safe));
                }
            }
            StreamEvent::Text(text) => {
                // Reasoning ends where the answer starts.
                self.close_reasoning(shown);
                let id = match &shown.text {
                    Some((id, _)) => id.clone(),
                    None => {
                        let id = self.id("assistant");
                        self.emit(Envelope::item_started(
                            &id,
                            ItemKind::AssistantMessage,
                            "assistant",
                            None,
                            None,
                        ));
                        shown.text = Some((id.clone(), String::new()));
                        id
                    }
                };
                if let Some((_, all)) = &mut shown.text {
                    all.push_str(&text);
                }
                shown.text_pending.push_str(&text);
                let safe = self.masks.drain_stream(&mut shown.text_pending, false);
                if !safe.is_empty() {
                    self.emit(Envelope::delta(id, StreamKind::Assistant, safe));
                }
            }
            StreamEvent::Usage(prompt, completion) => {
                usage.input_tokens += prompt;
                usage.output_tokens += completion;
            }
            StreamEvent::Error(_) => {}
        }
    }

    fn close_reasoning(&mut self, shown: &mut Shown) {
        if let Some((id, all)) = shown.reasoning.take() {
            let safe = self.masks.drain_stream(&mut shown.reasoning_pending, true);
            if !safe.is_empty() {
                self.emit(Envelope::delta(&id, StreamKind::Reasoning, safe));
            }
            self.emit(Envelope::snapshot(&id, StreamKind::Reasoning, all));
            self.emit(
                Envelope::new(Event::Reasoning {
                    state: ReasoningState::Complete,
                    label: ReasoningLabel::Thinking,
                })
                .item(&id),
            );
            self.emit(Envelope::item_completed(
                id,
                ItemStatus::Completed,
                None,
                None,
            ));
        }
    }

    fn close_shown(&mut self, shown: &mut Shown, interrupted: bool) {
        self.close_reasoning(shown);
        if let Some((id, all)) = shown.text.take() {
            let safe = self.masks.drain_stream(&mut shown.text_pending, true);
            if !safe.is_empty() {
                self.emit(Envelope::delta(&id, StreamKind::Assistant, safe));
            }
            self.emit(Envelope::snapshot(&id, StreamKind::Assistant, all));
            let status = if interrupted {
                ItemStatus::Interrupted
            } else {
                ItemStatus::Completed
            };
            self.emit(Envelope::item_completed(id, status, None, None));
        }
    }

    /// Shows, gates and runs one tool call; what the model is told comes back. A Stop that came
    /// before it means it never starts; one that comes while it runs ends it.
    fn call(
        &mut self,
        call: &ToolCall,
        commands: &Receiver<RunnerInput>,
        reservation: usize,
        advertised: bool,
    ) -> Result<String, StepError> {
        if let Some(entry) = self.registry.get(&call.function.name).cloned() {
            return self.call_mcp(call, &entry, commands, reservation, advertised);
        }
        if let Some(stop) = poll(commands, &mut self.deferred) {
            return Err(StepError::Stop(stop));
        }
        let item = self.id("tool");
        let tool = Tool::from_name(&call.function.name);
        let input = call.input().and_then(|input| {
            if let Some(tool) = tool {
                tool.validate_arguments(&input).map_err(str::to_owned)?;
            }
            Ok(input)
        });
        let (kind, title) = match (tool, &input) {
            (Some(t), Ok(v)) => (t.item_kind(), t.title(v)),
            (Some(t), Err(_)) => (t.item_kind(), call.function.name.clone()),
            (None, _) => (ItemKind::Tool, call.function.name.clone()),
        };
        let shown_input = input.as_ref().ok().cloned();
        self.emit(Envelope::item_started(
            &item,
            kind,
            title.clone(),
            shown_input,
            None,
        ));
        let fail = |me: &mut Self, why: String| {
            let why = agent_core::local_context::bound_result(
                &agent_core::redact::redact(&why),
                reservation.saturating_sub(7),
            );
            me.emit(Envelope::item_completed(
                &item,
                ItemStatus::Failed,
                None,
                Some(&why),
            ));
            Ok(format!("Error: {why}"))
        };
        if !advertised {
            return fail(
                self,
                "This tool was not advertised for this request.".into(),
            );
        }
        let Some(tool) = tool else {
            return fail(
                self,
                format!("There is no tool named {}.", call.function.name),
            );
        };
        let input = match input {
            Ok(input) => input,
            Err(why) => return fail(self, why),
        };
        let mut displayed = input.clone();
        if let Err(why) = self.masks.value(&mut displayed) {
            return fail(self, why.into());
        }
        let Some(ws) = self.workspace.clone() else {
            return fail(self, "The thread's folder is not open.".to_owned());
        };
        let outside = tool != Tool::RunCommand
            && tool.writes()
            && input
                .get("file_path")
                .and_then(Value::as_str)
                .is_none_or(|p| !crate::local_tools::inside(&ws, p));
        match gate(tool, self.mode, outside) {
            Gate::Refuse(why) => {
                self.emit(Envelope::item_completed(
                    &item,
                    ItemStatus::Declined,
                    None,
                    Some(why),
                ));
                return Ok(format!("Refused: {why}"));
            }
            // Any allow runs this one call: what a session or Always answer keeps is the app's
            // rule, scoped as its card said, and the app answers the cards it covers itself.
            Gate::Ask => match self.ask(tool, &title, &input, commands)? {
                Decision::Allow | Decision::AllowForSession | Decision::AllowAlways => {}
                Decision::Deny | Decision::Cancel => {
                    self.emit(Envelope::item_completed(
                        &item,
                        ItemStatus::Declined,
                        None,
                        Some("The user declined it."),
                    ));
                    return Ok(
                        "The user declined this call. Do not retry it; ask or try another way."
                            .to_owned(),
                    );
                }
            },
            Gate::Run => {}
        }
        // Run on a thread of its own, so a Stop is seen (and ends a command's whole group)
        // while it works.
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = {
            let (cancel, input) = (cancel.clone(), input.clone());
            std::thread::spawn(move || crate::local_tools::run(tool, &input, &ws, outside, &cancel))
        };
        let mut stop = None;
        while !worker.is_finished() {
            if stop.is_none() {
                if let Some(s) = poll(commands, &mut self.deferred) {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    stop = Some(s);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let result = worker
            .join()
            .unwrap_or_else(|_| Err("The tool failed unexpectedly.".to_owned()));
        if let Some(stop) = stop {
            self.emit(Envelope::item_completed(
                &item,
                ItemStatus::Interrupted,
                None,
                None,
            ));
            return Err(StepError::Stop(stop));
        }
        match result {
            Ok(output) => {
                let output =
                    agent_core::local_context::bound_result(&self.safe_text(&output), reservation);
                self.emit(Envelope::item_completed(
                    &item,
                    ItemStatus::Completed,
                    Some(&output),
                    None,
                ));
                Ok(output)
            }
            Err(why) => fail(self, why),
        }
    }

    fn safe_arguments(&self, arguments: &str) -> String {
        match serde_json::from_str::<Value>(arguments) {
            Ok(mut value) => {
                self.masks.value_lossy(&mut value);
                value.to_string()
            }
            _ => "{\"_redacted\":\"Invalid or sensitive arguments withheld\"}".into(),
        }
    }

    fn safe_text(&self, text: &str) -> String {
        self.masks.scrub(text)
    }

    fn poll_mcp(&mut self, commands: &Receiver<RunnerInput>, active: bool) -> Option<Stop> {
        let stop = poll(commands, &mut self.deferred);
        let mut modes = Vec::new();
        self.deferred.retain(|command| {
            if let Command::SetMode { mode } = command {
                modes.push(*mode);
                false
            } else {
                true
            }
        });
        let mut narrowed = false;
        for mode in modes {
            narrowed |= self.mode == Mode::Bypass && mode != Mode::Bypass
                || mode == Mode::Plan && self.mode != Mode::Plan;
            self.mode = mode;
            self.emit(Envelope::new(Event::ModeChanged { mode }));
        }
        if active && narrowed && stop.is_none() {
            self.emit(Envelope::new(Event::Notice {
                text:
                    "MCP authority narrowed: the active call was stopped; its outcome is unknown."
                        .into(),
            }));
            Some(Stop::Interrupted)
        } else {
            stop
        }
    }

    fn call_mcp(
        &mut self,
        call: &ToolCall,
        entry: &agent_core::local_mcp::RegistryTool,
        commands: &Receiver<RunnerInput>,
        reservation: usize,
        advertised: bool,
    ) -> Result<String, StepError> {
        use agent_core::local_mcp::{
            gate as mcp_gate, result_text, ApprovalBinding, ApprovalCard, ApprovalStamp, Grant,
            ToolIdentity,
        };
        if let Some(stop) = self.poll_mcp(commands, false) {
            return Err(StepError::Stop(stop));
        }
        let ToolIdentity::Mcp { server_id, name } = &entry.resolved.identity else {
            return Err(StepError::Failed("Invalid MCP registry entry.".into()));
        };
        let item = self.id("mcp");
        let title = format!("{} ({}) / {}", entry.label, server_id, name);
        let grant = Grant {
            server_id: server_id.clone(),
            tool: name.clone(),
            server_revision: entry.resolved.server_revision.clone(),
            schema_revision: entry.resolved.schema_revision.clone(),
        };
        let connection = self.mcp_connections.get(server_id).cloned();
        let mut input = if call.function.arguments.len() <= 65536 {
            call.input()
        } else {
            Err("MCP arguments exceed 64 KiB.".into())
        };
        let shown = input
            .as_ref()
            .ok()
            .cloned()
            .and_then(|mut value| self.masks.value(&mut value).ok().map(|()| value));
        self.emit(Envelope::item_started(&item,ItemKind::McpTool,&title,
            Some(serde_json::json!({"server_id":server_id,"server_label":entry.label,"tool":name,"arguments":shown})),None));
        let failed = |me: &mut Self, why: &str, status: ItemStatus| {
            let reason = me.masks.scrub(why);
            let output = result_text(
                &serde_json::json!({"isError":true,"content":[{"type":"text","text":reason}]}),
                reservation,
            );
            me.emit(Envelope::item_completed(&item, status, None, Some(&output)));
            Ok(output)
        };
        // Identity came only from the frozen registry. Model-supplied fields are arguments,
        // never routing information, and do not weaken any eligibility check.
        if !advertised {
            return failed(
                self,
                "This MCP tool was not advertised for this request.",
                ItemStatus::Failed,
            );
        }
        let Some(server) = self
            .context
            .mcp_servers
            .iter()
            .find(|s| &s.id == server_id)
            .cloned()
        else {
            return failed(self, "This MCP server is not selected.", ItemStatus::Failed);
        };
        let Some(policy) = server.tools.iter().find(|t| &t.name == name).cloned() else {
            return failed(
                self,
                "This MCP tool is not allowlisted.",
                ItemStatus::Failed,
            );
        };
        loop {
            let arguments = match input {
                Ok(ref input) => input.clone(),
                Err(_) => {
                    return failed(
                        self,
                        "MCP arguments must be valid JSON and at most 64 KiB.",
                        ItemStatus::Failed,
                    )
                }
            };
            if let Err(why) = entry.validator.validate(&arguments) {
                return failed(self, why, ItemStatus::Failed);
            }
            let mut shown = arguments.clone();
            if let Err(why) = self.masks.value(&mut shown) {
                return failed(self, why, ItemStatus::Failed);
            }
            match mcp_gate(&server, &policy, self.mode, true) {
                Gate::Refuse(why) => return failed(self, why, ItemStatus::Declined),
                Gate::Run => break,
                Gate::Ask => {
                    let Some(connection) = &connection else {
                        return failed(self, "MCP connection is unavailable.", ItemStatus::Failed);
                    };
                    if !connection.is_open() {
                        return failed(
                            self,
                            "The MCP connection closed before the call was approved.",
                            ItemStatus::Failed,
                        );
                    }
                    let request = self.id("approval");
                    let stamp = ApprovalStamp {
                        request_id: request.clone(),
                        generation: self.generation.clone(),
                        call_id: self.id("binding"),
                        grant: grant.clone(),
                    };
                    let binding = ApprovalBinding {
                        stamp: stamp.clone(),
                        arguments: arguments.clone(),
                    };
                    let card = ApprovalCard {
                        binding: stamp,
                        server_label: entry.label.clone(),
                        // Emit from the original once: preflight masking must not feed its
                        // inserted markers into another masking pass.
                        arguments: arguments.clone(),
                    };
                    if !self.emit_approval(Envelope::new(Event::ApprovalRequested {
                        tool:agent_core::local_mcp::APPROVAL_TOOL.into(),title:Some(title.clone()),
                        input:serde_json::to_value(card).map_err(|_| StepError::Failed("MCP approval encoding failed.".into()))?,
                        reason:Some("Read-only status is separate from approval. Arbitrary MCP calls have no filesystem containment exemption.".into()),
                        options:agent_core::local_mcp::APPROVAL_OPTIONS.to_vec(),response:ResponseCapability::Live,
                        remembers:Some("Allow for session covers this session; Always allow also covers future sessions. Both cover all arguments for this reviewed tool and its server/schema content revisions. Changed definitions or schemas invalidate the grant.".into()),
                    }).request(&request)) {return failed(self, "MCP approval could not be displayed safely.", ItemStatus::Declined);}
                    let answer = self.wait_mcp_approval(&binding, connection, commands)?;
                    match answer {
                        McpAnswer::Disconnected => {
                            return failed(
                                self,
                                "The MCP connection closed before the call was approved.",
                                ItemStatus::Failed,
                            )
                        }
                        McpAnswer::Edited(edited) => {
                            input = Ok(edited);
                            continue;
                        }
                        McpAnswer::Declined => {
                            return failed(
                                self,
                                "The user declined this MCP call. Do not retry it.",
                                ItemStatus::Declined,
                            )
                        }
                        McpAnswer::Allowed => {}
                    }
                    // Recheck after waiting: a restrictive mode change cannot be approved away.
                    if matches!(mcp_gate(&server, &policy, self.mode, true), Gate::Refuse(_)) {
                        return failed(
                            self,
                            "The current mode refuses this MCP call.",
                            ItemStatus::Declined,
                        );
                    }
                    break;
                }
            }
        }
        let arguments = match input {
            Ok(Value::Object(arguments)) => arguments,
            _ => return failed(self, "Invalid MCP arguments.", ItemStatus::Failed),
        };
        let Some(session) = &self.mcp else {
            return failed(
                self,
                "The MCP connection is unavailable.",
                ItemStatus::Failed,
            );
        };
        let Some(connection) = &connection else {
            return failed(self, "MCP connection is unavailable.", ItemStatus::Failed);
        };
        let response =
            match session.call_frozen(server_id.clone(), name.clone(), arguments, connection) {
                Ok(response) => response,
                Err(why) => return failed(self, &why, ItemStatus::Failed),
            };
        loop {
            if let Some(stop) = self.poll_mcp(commands, true) {
                self.stop_mcp();
                self.emit(Envelope::item_completed(
                    &item,
                    ItemStatus::Interrupted,
                    None,
                    Some("MCP call interrupted; its outcome is unknown."),
                ));
                return Err(StepError::Stop(stop));
            }
            match response.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(mut value)) => {
                    if self.masks.value(&mut value).is_err() {
                        return failed(
                            self,
                            "Sensitive MCP result keys were withheld.",
                            ItemStatus::Failed,
                        );
                    }
                    let is_error = value
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let output = result_text(&value, reservation);
                    self.emit(Envelope::item_completed(
                        &item,
                        if is_error {
                            ItemStatus::Failed
                        } else {
                            ItemStatus::Completed
                        },
                        Some(&output),
                        None,
                    ));
                    return Ok(output);
                }
                Ok(Err(why)) => return failed(self, &why, ItemStatus::Failed),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return failed(
                        self,
                        "MCP connection lost; the outcome is unknown and the call was not retried.",
                        ItemStatus::Failed,
                    )
                }
            }
        }
    }

    fn wait_mcp_approval(
        &mut self,
        binding: &agent_core::local_mcp::ApprovalBinding,
        connection: &crate::local_mcp::ConnectionLease,
        commands: &Receiver<RunnerInput>,
    ) -> Result<McpAnswer, StepError> {
        use agent_core::local_mcp::ApprovalAnswer;
        let request = &binding.stamp.request_id;
        loop {
            if !connection.is_open() {
                self.emit(Envelope::new(Event::ApprovalExpired).request(request));
                return Ok(McpAnswer::Disconnected);
            }
            match commands.recv_timeout(Duration::from_millis(20)) {
                Ok(RunnerInput::Command(Command::Approve {
                    request: answered,
                    decision,
                    updated_input,
                    ..
                })) if &answered == request => {
                    if !connection.is_open() {
                        self.emit(Envelope::new(Event::ApprovalExpired).request(request));
                        return Ok(McpAnswer::Disconnected);
                    }
                    let answer = updated_input
                        .and_then(|value| serde_json::from_value::<ApprovalAnswer>(value).ok());
                    let Some(answer) = answer.filter(|answer| answer.binding == binding.stamp)
                    else {
                        continue;
                    };
                    if let Some(edited) = answer.edited_arguments {
                        self.emit(Envelope::new(Event::ApprovalExpired).request(request));
                        return Ok(McpAnswer::Edited(edited));
                    }
                    self.emit(
                        Envelope::new(Event::ApprovalResolved { decision })
                            .request(request)
                            .raw(serde_json::to_value(&binding.stamp).unwrap_or(Value::Null)),
                    );
                    return Ok(
                        if matches!(
                            decision,
                            Decision::Allow | Decision::AllowAlways | Decision::AllowForSession
                        ) {
                            McpAnswer::Allowed
                        } else {
                            McpAnswer::Declined
                        },
                    );
                }
                Ok(RunnerInput::Command(Command::SetMode { mode })) => {
                    self.mode = mode;
                    self.emit(Envelope::new(Event::ModeChanged { mode }));
                    self.emit(Envelope::new(Event::ApprovalExpired).request(request));
                    return Ok(McpAnswer::Edited(binding.arguments.clone()));
                }
                Ok(RunnerInput::Command(Command::Interrupt)) => {
                    self.emit(Envelope::new(Event::ApprovalExpired).request(request));
                    return Err(StepError::Stop(Stop::Interrupted));
                }
                Ok(RunnerInput::Invalid(why)) => {
                    self.emit(Envelope::new(Event::ApprovalExpired).request(request));
                    return Err(StepError::Stop(Stop::InvalidInput(why)));
                }
                Ok(RunnerInput::Closed) | Err(RecvTimeoutError::Disconnected) => {
                    self.emit(Envelope::new(Event::ApprovalExpired).request(request));
                    return Err(StepError::Stop(Stop::Closed));
                }
                Ok(RunnerInput::Command(Command::Approve { .. })) => {}
                Ok(RunnerInput::Command(other)) => self.deferred.push(other),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Asks the user, and waits for the answer (or an interrupt).
    fn ask(
        &mut self,
        tool: Tool,
        title: &str,
        input: &Value,
        commands: &Receiver<RunnerInput>,
    ) -> Result<Decision, StepError> {
        let request = self.id("approval");
        if !self.emit_approval(
            Envelope::new(Event::ApprovalRequested {
                tool: tool.name().to_owned(),
                title: Some(title.to_owned()),
                input: input.clone(),
                reason: None,
                options: APPROVAL_OPTIONS.to_vec(),
                response: ResponseCapability::Live,
                remembers: None,
            })
            .request(&request),
        ) {
            return Ok(Decision::Deny);
        }
        loop {
            match commands.recv_timeout(Duration::from_secs(3600)) {
                Ok(RunnerInput::Command(Command::Approve {
                    request: answered,
                    decision,
                    ..
                })) if answered == request => {
                    self.emit(
                        Envelope::new(Event::ApprovalResolved { decision }).request(&request),
                    );
                    return Ok(decision);
                }
                Ok(RunnerInput::Invalid(why)) => {
                    self.emit(Envelope::new(Event::ApprovalExpired).request(&request));
                    return Err(StepError::Stop(Stop::InvalidInput(why)));
                }
                Ok(RunnerInput::Command(Command::Interrupt)) => {
                    self.emit(Envelope::new(Event::ApprovalExpired).request(&request));
                    return Err(StepError::Stop(Stop::Interrupted));
                }
                Ok(RunnerInput::Command(other)) => self.deferred.push(other),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(RunnerInput::Closed) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(StepError::Stop(Stop::Closed))
                }
            }
        }
    }
}

enum StepEnd {
    Answered,
    Continue,
}
enum McpAnswer {
    Disconnected,
    Allowed,
    Declined,
    Edited(Value),
}

enum StepError {
    Stop(Stop),
    Failed(String),
}

/// The reasoning and text items open in the current reply, with their whole text so far.
#[derive(Default)]
struct Shown {
    reasoning: Option<(String, String)>,
    text: Option<(String, String)>,
    reasoning_pending: String,
    text_pending: String,
}

/// Looks at what arrived without waiting: an interrupt (or a closed stdin) stops the reply;
/// anything else waits for the turn's end.
fn poll(commands: &Receiver<RunnerInput>, deferred: &mut Vec<Command>) -> Option<Stop> {
    loop {
        match commands.try_recv() {
            Ok(RunnerInput::Command(Command::Interrupt)) => return Some(Stop::Interrupted),
            Ok(RunnerInput::Invalid(why)) => return Some(Stop::InvalidInput(why)),
            // A stale answer (its card is gone) is dropped.
            Ok(RunnerInput::Command(Command::Approve { .. })) => {}
            Ok(RunnerInput::Command(other)) => deferred.push(other),
            Ok(RunnerInput::Closed) | Err(TryRecvError::Disconnected) => return Some(Stop::Closed),
            Err(TryRecvError::Empty) => return None,
        }
    }
}

/// The text a message carries, in bytes: its content, its calls and its reasoning.
fn message_size(m: &ChatMessage) -> usize {
    m.content.len()
        + m.reasoning_content.as_ref().map_or(0, String::len)
        + m.tool_calls
            .iter()
            .map(|c| c.function.arguments.len() + c.function.name.len() + c.id.len())
            .sum::<usize>()
}

/// Drops the oldest whole exchanges of `messages` until `fits` accepts them, keeping at least the
/// latest one.
fn drop_oldest_until(messages: &mut Vec<ChatMessage>, fits: impl Fn(&[ChatMessage]) -> bool) {
    while !fits(messages) {
        let starts = exchange_starts(messages);
        match starts.get(1) {
            Some(&next) => {
                messages.drain(..next);
            }
            None => return,
        }
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// A fresh session id: 32 hex characters from the process's random hasher state, the clock and a
/// counter (the crate has no RNG dependency; only uniqueness matters).
pub(crate) fn new_session_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn setup_is_required_and_invalid_setup_never_acknowledged() {
        for command in [
            Command::Prompt {
                text: "too early".into(),
            },
            Command::ConfigureLocal {
                context: Box::new(agent_core::local_context::LocalLaunchContext {
                    budget: agent_core::local_context::ContextBudget {
                        context_tokens: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                }),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tx, rx) = channel();
            tx.send(RunnerInput::Command(command)).unwrap();
            let mut out = Vec::new();
            let code = Runner::new(
                args(dir.path(), Mode::Ask),
                Script::new(vec![]),
                dir.path().join("state"),
                &mut out,
            )
            .run(&rx);
            assert_eq!(code, 1);
            let text = String::from_utf8(out).unwrap();
            assert!(!text.contains("session_started"));
            assert!(text.contains("error"));
        }
    }
    use super::*;
    use agent_core::event::Event;
    use serde_json::json;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};

    /// One scripted reply: its SSE lines, or the error the request fails with.
    type Reply = Result<Vec<String>, String>;

    /// A server that answers each request with the next scripted reply, and keeps every request
    /// body.
    #[derive(Clone)]
    struct Script {
        replies: Arc<Mutex<Vec<Reply>>>,
        bodies: Arc<Mutex<Vec<Value>>>,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Script {
        fn new(replies: Vec<Reply>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies)),
                bodies: Arc::default(),
                cancelled: Arc::default(),
            }
        }
        fn bodies(&self) -> Vec<Value> {
            self.bodies.lock().unwrap().clone()
        }
    }

    /// A line that makes the scripted server go silent: the reply never ends.
    const STALL_MARK: &str = "<stall>";

    impl ChatClient for Script {
        fn cancel(&mut self) {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        fn start(&mut self, body: &Value) -> Receiver<ReplyLine> {
            self.bodies.lock().unwrap().push(body.clone());
            let (tx, rx) = channel();
            let next = {
                let mut replies = self.replies.lock().unwrap();
                if replies.is_empty() {
                    Err("no more scripted replies".to_owned())
                } else {
                    replies.remove(0)
                }
            };
            match next {
                Err(why) => {
                    let _ = tx.send(Err(why));
                }
                Ok(lines) => {
                    for line in lines {
                        if line == STALL_MARK {
                            // Silent from here on, with the reply still open.
                            std::mem::forget(tx);
                            return rx;
                        }
                        let _ = tx.send(Ok(line));
                    }
                }
            }
            rx
        }
    }

    fn text(words: &[&str]) -> Vec<String> {
        let mut lines: Vec<String> = words
            .iter()
            .map(|w| {
                format!(
                    r#"data: {{"choices":[{{"delta":{{"content":{}}}}}]}}"#,
                    json!(w)
                )
            })
            .collect();
        lines.push(
            r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#.into(),
        );
        lines.push("data: [DONE]".into());
        lines
    }

    fn tool_call(id: &str, name: &str, args: Value) -> Vec<String> {
        vec![
            format!(
                r#"data: {{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"{id}","type":"function","function":{{"name":"{name}","arguments":{}}}}}]}}}}]}}"#,
                json!(args.to_string())
            ),
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.into(),
            "data: [DONE]".into(),
        ]
    }

    /// Stdout as a channel of envelopes, so a test can answer what the runner asks.
    struct Lines(Sender<Envelope>, Vec<u8>);

    impl Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.1.extend_from_slice(buf);
            while let Some(end) = self.1.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.1.drain(..=end).collect();
                let envelope = serde_json::from_slice(&line).expect("an envelope per line");
                let _ = self.0.send(envelope);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Run {
        commands: Sender<RunnerInput>,
        events: Receiver<Envelope>,
        done: std::thread::JoinHandle<i32>,
    }

    impl Run {
        /// Waits for the first event `pick` accepts, and returns what it made of it.
        fn wait_for<T>(&self, mut pick: impl FnMut(&Envelope) -> Option<T>) -> T {
            loop {
                let env = self
                    .events
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the runner went quiet");
                if let Some(found) = pick(&env) {
                    return found;
                }
            }
        }
        fn send(&self, command: Command) {
            self.commands.send(RunnerInput::Command(command)).unwrap();
        }
        /// Closes stdin and returns every event left, and the exit code.
        fn finish(self) -> (Vec<Envelope>, i32) {
            self.commands.send(RunnerInput::Closed).unwrap();
            let code = self.done.join().unwrap();
            (self.events.try_iter().collect(), code)
        }

        fn invalid_input(self) -> (Vec<Envelope>, i32) {
            self.commands
                .send(RunnerInput::Invalid("Malformed Local command JSON."))
                .unwrap();
            let code = self.done.join().unwrap();
            (self.events.try_iter().collect(), code)
        }
    }

    fn start(args: RunnerArgs, script: Script, dir: PathBuf) -> Run {
        start_with_context(args, script, dir, Default::default())
    }

    fn start_with_context(
        args: RunnerArgs,
        script: Script,
        dir: PathBuf,
        context: agent_core::local_context::LocalLaunchContext,
    ) -> Run {
        let (commands, rx) = channel();
        commands
            .send(RunnerInput::Command(Command::ConfigureLocal {
                context: Box::new(context),
            }))
            .unwrap();
        let (tx, events) = channel();
        let done = std::thread::spawn(move || {
            Runner::new(args, script, dir, Lines(tx, Vec::new())).run(&rx)
        });
        Run {
            commands,
            events,
            done,
        }
    }

    fn args(cwd: &Path, mode: Mode) -> RunnerArgs {
        RunnerArgs {
            server: "http://192.0.2.1:11434".into(),
            cwd: cwd.display().to_string(),
            model: Some("qwen3.5:9b".into()),
            mode,
            session_id: Some("s-1".into()),
            ..RunnerArgs::default()
        }
    }

    #[test]
    fn builtin_spoof_fields_and_wrong_types_never_reach_approval_or_execution() {
        for mode in [Mode::Ask, Mode::AcceptEdits, Mode::Bypass] {
            for tool in Tool::ALL {
                let schema = tool.schema();
                let fields = &schema["function"]["parameters"];
                let mut base = json!({});
                for name in fields["required"].as_array().unwrap() {
                    base[name.as_str().unwrap()] = json!("marker");
                }
                if tool == Tool::RunCommand {
                    base["command"] = json!("touch marker");
                }
                let mut invalid = Vec::new();
                for key in [
                    "CommandLine",
                    "cmd",
                    "cwd",
                    "file_path",
                    "content",
                    "changes",
                    "edits",
                    "old_string",
                ] {
                    if fields["properties"].get(key).is_none() {
                        let mut input = base.clone();
                        input[key] = json!("git status");
                        invalid.push(input);
                    }
                }
                for key in fields["properties"].as_object().unwrap().keys() {
                    let mut input = base.clone();
                    input[key] = Value::Null;
                    invalid.push(input);
                }
                for input in invalid {
                    let folder = tempfile::tempdir().unwrap();
                    let script = Script::new(vec![
                        Ok(tool_call("c1", tool.name(), input.clone())),
                        Ok(text(&["done"])),
                    ]);
                    let run = start(
                        args(folder.path(), mode),
                        script,
                        folder.path().join("state"),
                    );
                    run.send(Command::Prompt {
                        text: "test".into(),
                    });
                    let mut failed = false;
                    loop {
                        let env = run.events.recv_timeout(Duration::from_secs(3)).unwrap();
                        // The app's saved rules are consulted only for ApprovalRequested.
                        assert!(
                            !matches!(env.event, Event::ApprovalRequested { .. }),
                            "{} {input}",
                            tool.name()
                        );
                        failed |= matches!(
                            env.event,
                            Event::ItemCompleted {
                                status: ItemStatus::Failed,
                                ..
                            }
                        );
                        if matches!(env.event, Event::TurnCompleted { .. }) {
                            break;
                        }
                    }
                    assert!(failed);
                    assert!(!folder.path().join("marker").exists());
                    assert!(crate::local_tools::run(
                        tool,
                        &input,
                        &crate::local_tools::Workspace::open(folder.path()).unwrap(),
                        false,
                        &std::sync::atomic::AtomicBool::new(false)
                    )
                    .is_err());
                    assert_eq!(run.finish().1, 0);
                }
            }
        }
    }

    #[test]
    fn unsafe_builtin_argument_keys_fail_without_waiting_and_history_remains_resumable() {
        let work = tempfile::tempdir().unwrap();
        let key = format!("ghp_{}", "x".repeat(32));
        let script = Script::new(vec![
            Ok(tool_call(
                "c1",
                "run_command",
                json!({"command":"touch marker",key.clone():"ordinary"}),
            )),
            Ok(text(&["done"])),
        ]);
        let run = start(
            args(work.path(), Mode::Ask),
            script,
            work.path().join("state"),
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let mut seen = Vec::new();
        loop {
            let env = run.events.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(!matches!(env.event, Event::ApprovalRequested { .. }));
            let done = matches!(env.event, Event::TurnCompleted { .. });
            seen.push(env);
            if done {
                break;
            }
        }
        let (_, code) = run.finish();
        assert_eq!(code, 0);
        assert!(!work.path().join("marker").exists());
        let saved = std::fs::read_dir(work.path().join("state"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let saved = std::fs::read_to_string(saved).unwrap();
        assert!(!saved.contains(&key));
        let transcript: Transcript = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            transcript
                .messages
                .iter()
                .filter(|m| m.role == "tool")
                .count(),
            1
        );
        assert!(transcript.messages.iter().any(|m| m.content == "test"));
        assert!(!serde_json::to_string(&seen).unwrap().contains(&key));
    }

    #[test]
    fn unsafe_approval_envelopes_are_refused_instead_of_waited_on() {
        let work = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut runner = Runner::new(
            args(work.path(), Mode::Ask),
            Script::new(vec![]),
            work.path().join("state"),
            &mut out,
        );
        let key = format!("ghp_{}", "a".repeat(32));
        let frame = Envelope::new(Event::ApprovalRequested {
            tool: "run_command".into(),
            title: None,
            input: json!({key:"value"}),
            reason: None,
            options: APPROVAL_OPTIONS.to_vec(),
            response: ResponseCapability::Live,
            remembers: None,
        })
        .request("r1");
        assert!(!runner.emit_approval(frame));
        let events: Vec<Envelope> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(matches!(events[0].event, Event::Error { .. }));
    }

    #[test]
    fn a_missing_mcp_lease_fails_without_emitting_an_approval_card() {
        use agent_core::local_mcp::{CompiledSchema, RegistryTool, ResolvedTool, ToolIdentity};
        let work = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut runner = Runner::new(
            args(work.path(), Mode::Ask),
            Script::new(vec![]),
            work.path().join("state"),
            &mut out,
        );
        runner.context.mcp_servers.push(serde_json::from_value(json!({"id":"test","label":"Test","enabled":true,"command":"fixture","args":[],"cwd":"/","env":{},"tools":[{"name":"read","read_only":true}],"approval":"ask","startup_timeout_secs":1,"call_timeout_secs":1})).unwrap());
        let entry = RegistryTool {
            resolved: ResolvedTool {
                identity: ToolIdentity::Mcp {
                    server_id: "test".into(),
                    name: "read".into(),
                },
                alias: "mcp_test".into(),
                schema: json!({}),
                server_revision: "a".repeat(64),
                schema_revision: "b".repeat(64),
            },
            label: "Test".into(),
            validator: CompiledSchema::compile(&json!({"type":"object"})).unwrap(),
        };
        let call: ToolCall = serde_json::from_value(json!({"id":"call-1","type":"function","function":{"name":"mcp_test","arguments":"{}"}})).unwrap();
        let (_keep, commands) = channel();
        assert!(runner
            .call_mcp(&call, &entry, &commands, 1000, true)
            .is_ok());
        let events: Vec<Envelope> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!events
            .iter()
            .any(|e| matches!(e.event, Event::ApprovalRequested { .. })));
        assert!(events.iter().any(|e| matches!(
            e.event,
            Event::ItemCompleted {
                status: ItemStatus::Failed,
                ..
            }
        )));
    }

    fn prompt(text: &str) -> Command {
        Command::Prompt { text: text.into() }
    }

    fn completed(env: &Envelope) -> Option<(TurnState, Option<Usage>, Option<String>)> {
        match &env.event {
            Event::TurnCompleted {
                state,
                usage,
                error,
                ..
            } => Some((*state, usage.clone(), error.clone())),
            _ => None,
        }
    }

    #[test]
    fn malformed_startup_frames_emit_safe_errors_and_never_acknowledge_setup() {
        let work = tempfile::tempdir().unwrap();
        let canary = "private-input-canary";
        for input in [
            format!("{{\"type\":\"configure_local\",\"context\":{{\"budget\":{{\"context_tokens\":\"{canary}\"}}}}}}\n").into_bytes(),
            b"{\n".to_vec(),
            vec![0xff, b'\n'],
            vec![b'x'; agent_core::local_context::MAX_SETUP_BYTES + 1],
            format!("{{\"type\":\"configure_local\",\"context\":{{\"extra\":\"{canary}\"}}}}").into_bytes(),
        ] {
            let commands = read_commands(std::io::Cursor::new(input));
            let mut out = Vec::new();
            assert_eq!(Runner::new(args(work.path(), Mode::Ask), Script::new(vec![]), work.path().join("state"), &mut out).run(&commands), 1);
            let output = String::from_utf8(out).unwrap();
            assert!(!output.contains(canary));
            let events: Vec<Envelope> = output.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0].event, Event::Error { message } if message.starts_with("Local command") || message.starts_with("Malformed Local command")));
            assert!(commands.recv_timeout(Duration::from_secs(1)).is_err(), "the reader exits instead of sending another frame");
        }
    }

    #[test]
    fn malformed_setup_exits_even_when_stdin_remains_open() {
        use std::os::fd::FromRawFd;
        let mut fds = [-1; 2];
        // SAFETY: pipe receives storage for two file descriptors, owned below on success.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned two new, distinct descriptors, each transferred exactly once.
        let (reader, mut writer) = unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        };
        let commands = read_commands(reader);
        let work = tempfile::tempdir().unwrap();
        let run_args = args(work.path(), Mode::Ask);
        let state = work.path().join("state");
        let (tx, events) = channel();
        let done = std::thread::spawn(move || {
            Runner::new(run_args, Script::new(vec![]), state, Lines(tx, vec![])).run(&commands)
        });
        writer
            .write_all(b"{\"type\":\"configure_local\",\"context\":false}\n")
            .unwrap();
        let event = events
            .recv_timeout(Duration::from_secs(2))
            .expect("malformed startup must not wait for EOF");
        assert!(
            matches!(&event.event, Event::Error { message } if message.contains("Malformed Local command"))
        );
        assert_eq!(done.join().unwrap(), 1);
        drop(writer);
    }

    #[test]
    fn malformed_input_after_setup_is_terminal() {
        let work = tempfile::tempdir().unwrap();
        let (tx, commands) = channel();
        tx.send(RunnerInput::Command(Command::ConfigureLocal {
            context: Box::default(),
        }))
        .unwrap();
        tx.send(RunnerInput::Invalid("Malformed Local command JSON."))
            .unwrap();
        let mut out = Vec::new();
        assert_eq!(
            Runner::new(
                args(work.path(), Mode::Ask),
                Script::new(vec![]),
                work.path().join("state"),
                &mut out
            )
            .run(&commands),
            1
        );
        let events: Vec<Envelope> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(matches!(events[0].event, Event::SessionStarted { .. }));
        assert!(matches!(events[1].event, Event::Error { .. }));
    }

    #[test]
    fn malformed_input_cancels_a_silent_stream_and_never_dispatches_deferred_prompts() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut lines = text(&["partial answer"]);
        lines.pop();
        lines.push(STALL_MARK.into());
        let script = Script::new(vec![Ok(lines)]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("start"));
        run.wait_for(|e| matches!(e.event, Event::ContentDelta { .. }).then_some(()));
        run.send(prompt("must stay deferred"));
        let (events, code) = run.invalid_input();
        assert_eq!(code, 1);
        assert!(script.cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(script.bodies().len(), 1);
        assert!(events
            .iter()
            .any(|e| matches!(completed(e), Some((TurnState::Failed, _, _)))));
        assert!(events
            .iter()
            .any(|e| matches!(e.event, Event::Error { .. })));
    }

    #[test]
    fn malformed_input_expires_approval_and_records_unexecuted_call_results() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Ok(tool_call(
            "write",
            "write_file",
            json!({"file_path":"untouched", "content":"no"}),
        ))]);
        let run = start(args(work.path(), Mode::Ask), script, state.path().into());
        run.send(prompt("write"));
        run.wait_for(|e| matches!(e.event, Event::ApprovalRequested { .. }).then_some(()));
        let (events, code) = run.invalid_input();
        assert_eq!(code, 1);
        assert!(!work.path().join("untouched").exists());
        assert!(events
            .iter()
            .any(|e| matches!(e.event, Event::ApprovalExpired)));
        assert!(events
            .iter()
            .any(|e| matches!(completed(e), Some((TurnState::Failed, _, _)))));
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(state.path().join("s-1.json")).unwrap()).unwrap();
        assert_eq!(
            saved.messages.last().unwrap().tool_call_id.as_deref(),
            Some("write")
        );
        assert!(saved
            .messages
            .last()
            .unwrap()
            .content
            .contains("malformed input"));
    }

    #[test]
    fn malformed_input_cancels_an_approved_command() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Ok(tool_call(
            "sleep",
            "run_command",
            json!({"command":"sleep 30"}),
        ))]);
        let run = start(args(work.path(), Mode::Ask), script, state.path().into());
        run.send(prompt("run"));
        let request = run.wait_for(|e| {
            matches!(e.event, Event::ApprovalRequested { .. }).then(|| e.request.clone().unwrap())
        });
        run.send(Command::Approve {
            request,
            decision: Decision::Allow,
            updated_input: None,
            message: None,
        });
        std::thread::sleep(Duration::from_millis(100));
        let started = std::time::Instant::now();
        let (events, code) = run.invalid_input();
        assert_eq!(code, 1);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(events
            .iter()
            .any(|e| matches!(completed(e), Some((TurnState::Failed, _, _)))));
    }

    #[test]
    fn an_oversize_prompt_is_not_saved_or_sent_with_the_next_prompt() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Ok(text(&["normal answer"]))]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        let rejected = format!("rejected-oversize-marker{}", "x".repeat(100_000));
        run.send(prompt(&rejected));
        assert_eq!(run.wait_for(completed).0, TurnState::Failed);
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(state.path().join("s-1.json")).unwrap()).unwrap();
        assert!(saved.messages.is_empty());
        run.send(prompt("normal prompt"));
        assert_eq!(run.wait_for(completed).0, TurnState::Completed);
        run.finish();
        let bodies = script.bodies();
        assert_eq!(bodies.len(), 1);
        assert!(!bodies[0].to_string().contains("rejected-oversize-marker"));
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(state.path().join("s-1.json")).unwrap()).unwrap();
        assert_eq!(
            saved
                .messages
                .iter()
                .map(|m| m.role.as_str())
                .collect::<Vec<_>>(),
            ["user", "assistant"]
        );
        assert_eq!(saved.messages[0].content, "normal prompt");
    }

    #[test]
    fn provider_failures_only_suggest_a_budget_change_for_context_rejection() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        for why in [
            "HTTP 401: authentication failed",
            "HTTP 403: permission denied",
            "Could not reach the model server: the connection failed",
            "The model server stopped answering.",
            "HTTP 500: internal error",
            "HTTP 400: context_length_exceeded",
            "maximum context length is 8192 tokens",
            "Input length exceeds the context window",
        ] {
            let script = Script::new(vec![Err(why.into())]);
            let run = start(
                args(work.path(), Mode::Ask),
                script.clone(),
                state.path().into(),
            );
            run.send(prompt("test error"));
            let (status, _, error) = run.wait_for(completed);
            assert_eq!(status, TurnState::Failed);
            let error = error.unwrap();
            assert!(error.contains(why));
            assert_eq!(
                error.contains("check its configured context size"),
                why.contains("context")
            );
            assert_eq!(
                error.contains("Estimated input allowance"),
                why.contains("context")
            );
            run.finish();
            assert_eq!(script.bodies().len(), 1, "no automatic retry");
        }
    }

    #[test]
    fn budget_notices_are_once_per_turn_across_multiple_requests_and_fallback() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Ok(tool_call("read", "list_dir", json!({"path":"."}))),
            Err("does not support tools".into()),
            Ok(text(&["first answer"])),
            Ok(text(&["second answer"])),
        ]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        let mut notices = 0;
        for prompt_text in ["first", "second"] {
            run.send(prompt(prompt_text));
            assert_eq!(run.wait_for(|e| {
                if matches!(&e.event, Event::Notice { text } if text.starts_with("Local request:")) { notices += 1; }
                completed(e)
            }).0, TurnState::Completed);
        }
        run.finish();
        assert_eq!(script.bodies().len(), 4);
        assert_eq!(notices, 2);
    }

    #[test]
    fn budget_and_each_exclusion_are_reported_only_once_per_turn() {
        use agent_core::local_context::{plan_request, SchemaCandidate};
        use agent_core::local_mcp::{ResolvedTool, ToolIdentity};
        let work = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut runner = Runner::new(
            args(work.path(), Mode::Ask),
            Script::new(vec![]),
            work.path().join("state"),
            &mut out,
        );
        let plan = plan_request("m", "policy", &[], &[], &Default::default()).unwrap();
        let excluded = plan_request(
            "m",
            "policy",
            &[SchemaCandidate {
                priority: 0,
                tool: ResolvedTool {
                    identity: ToolIdentity::Mcp {
                        server_id: "docs".into(),
                        name: "large".into(),
                    },
                    alias: "large".into(),
                    schema: json!({"description":"x".repeat(70_000)}),
                    server_revision: "1".into(),
                    schema_revision: "1".into(),
                },
            }],
            &[],
            &Default::default(),
        )
        .unwrap();
        runner.report_budget(&excluded);
        runner.report_budget(&excluded);
        runner.report_budget(&plan);
        let events: Vec<Envelope> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.iter().filter(|e| matches!(&e.event, Event::Notice { text } if text.starts_with("Local request:"))).count(), 1);
        assert_eq!(events.iter().filter(|e| matches!(&e.event, Event::Notice { text } if text.starts_with("Excluded tool large:"))).count(), 1);
    }

    #[test]
    fn fallback_uses_the_explicit_model_instead_of_reading_it_from_json() {
        let work = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Err("does not support tools".into()),
            Ok(text(&["fallback answer"])),
        ]);
        let mut runner = Runner::new(
            args(work.path(), Mode::Ask),
            script.clone(),
            work.path().join("state"),
            Vec::new(),
        );
        runner.messages.push(ChatMessage::new("user", "hello"));
        let (_tx, commands) = channel();
        let result = runner.stream(
            "chosen-model",
            &json!({"tools":[Tool::ReadFile.schema()]}),
            &commands,
            &mut Usage::default(),
        );
        assert!(result.is_ok());
        let bodies = script.bodies();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1]["model"], "chosen-model");
    }

    #[test]
    fn instructions_are_frozen_across_calls_reloaded_each_turn_and_never_saved() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let file = work.path().join("AGENTS.md");
        std::fs::write(&file, "first convention\npassword=canary-value-1234").unwrap();
        let script = Script::new(vec![
            Ok(tool_call(
                "write",
                "write_file",
                json!({"file_path":"AGENTS.md", "content":"second convention"}),
            )),
            Ok(text(&["done"])),
            Ok(text(&["next"])),
        ]);
        let mut context = agent_core::local_context::LocalLaunchContext::default();
        context.instructions.global_files = vec![file.display().to_string()];
        context.instructions.discover_ancestors = false;
        let run = start_with_context(
            args(work.path(), Mode::AcceptEdits),
            script.clone(),
            state.path().into(),
            context,
        );
        run.send(prompt("change instructions"));
        assert_eq!(run.wait_for(completed).0, TurnState::Completed);
        run.send(prompt("next turn"));
        assert_eq!(run.wait_for(completed).0, TurnState::Completed);
        run.finish();
        let bodies = script.bodies();
        assert_eq!(bodies.len(), 3);
        for body in &bodies[..2] {
            let system = body["messages"][0]["content"].as_str().unwrap();
            assert!(system.contains("first convention"));
            assert!(!system.contains("canary-value-1234"));
            assert!(!system.contains("second convention"));
        }
        assert!(bodies[2]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("second convention"));
        let saved = std::fs::read_to_string(state.path().join("s-1.json")).unwrap();
        assert!(!saved.contains("first convention"));
        assert!(!saved.contains("canary-value-1234"));
    }

    #[test]
    fn missing_explicit_instructions_and_current_exchange_overflow_make_no_request() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut context = agent_core::local_context::LocalLaunchContext::default();
        context.instructions.global_files =
            vec![work.path().join("missing.md").display().to_string()];
        for (context, prompt_text, expected) in [
            (context, "hi".into(), "missing.md"),
            (Default::default(), "x".repeat(100_000), "current exchange"),
        ] {
            let script = Script::new(vec![]);
            let run = start_with_context(
                args(work.path(), Mode::Ask),
                script.clone(),
                state.path().into(),
                context,
            );
            run.send(prompt(&prompt_text));
            let (status, _, error) = run.wait_for(completed);
            assert_eq!(status, TurnState::Failed);
            assert!(error.unwrap().contains(expected));
            run.finish();
            assert!(script.bodies().is_empty());
        }
    }

    #[test]
    fn oversized_call_batch_runs_nothing_and_leaves_no_orphaned_history() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let calls = json!([{"index":0,"id":"one","type":"function","function":{"name":"write_file","arguments":json!({"file_path":"one", "content":"x".repeat(30_000)}).to_string()}}, {"index":1,"id":"two","type":"function","function":{"name":"write_file","arguments":json!({"file_path":"two", "content":"x".repeat(30_000)}).to_string()}}]);
        let script = Script::new(vec![
            Ok(vec![
                format!(
                    "data: {}",
                    json!({"choices":[{"delta":{"tool_calls":calls}}]})
                ),
                "data: [DONE]".into(),
            ]),
            Ok(text(&["recover"])),
        ]);
        let run = start(
            args(work.path(), Mode::AcceptEdits),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("write both"));
        let (status, _, error) = run.wait_for(completed);
        assert_eq!(status, TurnState::Failed);
        assert!(error.unwrap().contains("none of the calls ran"));
        assert!(!work.path().join("one").exists());
        assert!(!work.path().join("two").exists());
        let rejected: Transcript =
            serde_json::from_slice(&std::fs::read(state.path().join("s-1.json")).unwrap()).unwrap();
        assert!(
            rejected.messages.is_empty(),
            "an empty rejected assistant and its unanswered prompt are both removed"
        );
        run.send(prompt("recover"));
        assert_eq!(run.wait_for(completed).0, TurnState::Completed);
        run.finish();
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(state.path().join("s-1.json")).unwrap()).unwrap();
        assert!(saved
            .messages
            .iter()
            .all(|m| m.tool_calls.is_empty() && m.tool_call_id.is_none()));
        assert!(saved
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .all(|m| !m.content.is_empty()));
    }

    #[test]
    fn reserved_results_are_bounded_and_next_request_fits() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("large"), "é".repeat(20_000)).unwrap();
        let script = Script::new(vec![
            Ok(tool_call("one", "read_file", json!({"file_path":"large"}))),
            Ok(text(&["done"])),
        ]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("read it"));
        assert_eq!(run.wait_for(completed).0, TurnState::Completed);
        run.finish();
        let bodies = script.bodies();
        let results: Vec<_> = bodies[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        let result = results[0]["content"].as_str().unwrap();
        assert!(result.len() <= agent_core::local_context::MAX_RESULT_BYTES);
        assert!(result.contains("truncated"));
        assert!(
            agent_core::local_context::estimated_tokens(
                serde_json::to_vec(&bodies[1]).unwrap().len(),
                bodies[1]["messages"].as_array().unwrap().len()
            ) <= agent_core::local_context::ContextBudget::default().input_allowance()
        );
    }

    #[test]
    fn a_tool_call_after_toolless_fallback_is_shown_but_never_executed() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Err("does not support tools".into()),
            Ok(tool_call(
                "hidden",
                "write_file",
                json!({"file_path":"hidden", "content":"must not run"}),
            )),
            Ok(text(&["done"])),
        ]);
        let run = start(
            args(work.path(), Mode::AcceptEdits),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("try tools"));
        let mut refusal_shown = false;
        assert_eq!(run.wait_for(|e| {
            if matches!(&e.event, Event::ItemCompleted { error: Some(why), .. } if why.contains("not advertised")) { refusal_shown = true; }
            completed(e)
        }).0, TurnState::Completed);
        run.finish();
        assert!(refusal_shown);
        assert!(!work.path().join("hidden").exists());
        let bodies = script.bodies();
        assert!(bodies[1].get("tools").is_none());
        assert!(bodies[2].get("tools").is_none());
    }

    #[test]
    fn a_turn_streams_its_answer_and_keeps_the_conversation_for_a_resume() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Ok(text(&["Hel", "lo"])), Ok(text(&["Again"]))]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        let started = run.wait_for(|e| match &e.event {
            Event::SessionStarted {
                native_id, model, ..
            } => Some((native_id.clone(), model.clone())),
            _ => None,
        });
        assert_eq!(started, ("s-1".into(), Some("qwen3.5:9b".into())));
        run.send(prompt("hi"));
        let mut deltas = Vec::new();
        let (state_, usage, _) = run.wait_for(|e| {
            if let Event::ContentDelta {
                stream: StreamKind::Assistant,
                text,
            } = &e.event
            {
                deltas.push(text.clone());
            }
            completed(e)
        });
        assert_eq!(state_, TurnState::Completed);
        assert_eq!(deltas, ["Hel", "lo"]);
        assert_eq!(
            usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((10, 2))
        );
        let (_, code) = run.finish();
        assert_eq!(code, 0);
        let saved = state.path().join("s-1.json");
        let mode = std::fs::metadata(&saved).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);

        // Resumed: the earlier messages go with the next request.
        let resumed = RunnerArgs {
            resume: Some("s-1".into()),
            session_id: None,
            model: None,
            ..args(work.path(), Mode::Ask)
        };
        let run = start(resumed, script.clone(), state.path().into());
        run.send(prompt("more"));
        run.wait_for(completed);
        run.finish();
        let last = script.bodies().pop().unwrap();
        assert_eq!(last["model"], "qwen3.5:9b", "the saved model");
        let roles: Vec<_> = last["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert_eq!(last["messages"][2]["content"], "Hello");
    }

    #[test]
    fn an_edit_in_accept_edits_runs_at_once_and_its_result_goes_back() {
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.rs"), "old").unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Ok(tool_call(
                "c1",
                "edit_file",
                json!({"file_path": "a.rs", "old_string": "old", "new_string": "new"}),
            )),
            Ok(text(&["Done"])),
        ]);
        let run = start(
            args(work.path(), Mode::AcceptEdits),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("change it"));
        let mut asked = false;
        let mut card = None;
        run.wait_for(|e| {
            match &e.event {
                Event::ApprovalRequested { .. } => asked = true,
                Event::ItemStarted {
                    kind: ItemKind::FileChange,
                    title,
                    ..
                } => card = Some(title.clone()),
                _ => {}
            }
            completed(e)
        });
        run.finish();
        assert!(!asked);
        assert_eq!(card.as_deref(), Some("Edit a.rs"));
        assert_eq!(
            std::fs::read_to_string(work.path().join("a.rs")).unwrap(),
            "new"
        );
        let second = &script.bodies()[1]["messages"];
        let answer = second.as_array().unwrap().last().unwrap();
        assert_eq!(answer["role"], "tool");
        assert_eq!(answer["tool_call_id"], "c1");
        assert!(answer["content"].as_str().unwrap().starts_with("Edited"));
    }

    #[test]
    fn a_command_in_ask_mode_waits_for_the_user_and_a_denial_is_told_to_the_model() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Ok(tool_call(
                "c1",
                "run_command",
                json!({"command": "touch made"}),
            )),
            Ok(text(&["Ok"])),
        ]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("make it"));
        let (request, options) = run.wait_for(|e| match &e.event {
            Event::ApprovalRequested { options, .. } => {
                Some((e.request.clone().unwrap(), options.clone()))
            }
            _ => None,
        });
        assert_eq!(options, APPROVAL_OPTIONS);
        run.send(Command::Approve {
            request: request.clone(),
            decision: Decision::Deny,
            updated_input: None,
            message: None,
        });
        let mut declined = false;
        run.wait_for(|e| {
            if matches!(
                e.event,
                Event::ItemCompleted {
                    status: ItemStatus::Declined,
                    ..
                }
            ) {
                declined = true;
            }
            completed(e)
        });
        run.finish();
        assert!(declined);
        assert!(!work.path().join("made").exists());
        let told = script.bodies()[1]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert!(told["content"].as_str().unwrap().contains("declined"));
    }

    #[test]
    /// The runner keeps no answer: "Allow for session" runs the one call, and the next asks again
    /// (the app's own rule, scoped as its card said, is what answers it there).
    fn allow_for_session_runs_the_one_call_and_the_runner_remembers_nothing() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let call = || Ok(tool_call("c", "run_command", json!({"command": "true"})));
        let script = Script::new(vec![call(), call(), Ok(text(&["Ok"]))]);
        let run = start(args(work.path(), Mode::Ask), script, state.path().into());
        run.send(prompt("go"));
        for decision in [Decision::AllowForSession, Decision::Allow] {
            let request = run.wait_for(|e| match &e.event {
                Event::ApprovalRequested { .. } => e.request.clone(),
                _ => None,
            });
            run.send(Command::Approve {
                request,
                decision,
                updated_input: None,
                message: None,
            });
        }
        let (state_, _, _) = run.wait_for(completed);
        run.finish();
        assert_eq!(state_, TurnState::Completed);
    }

    #[test]
    fn a_stop_while_the_server_is_silent_ends_the_turn_at_once() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let silent = vec![
            r#"data: {"choices":[{"delta":{"content":"thinking"}}]}"#.to_owned(),
            STALL_MARK.to_owned(),
        ];
        let run = start(
            args(work.path(), Mode::Ask),
            Script::new(vec![Ok(silent)]),
            state.path().into(),
        );
        run.send(prompt("go"));
        run.wait_for(|e| matches!(e.event, Event::ContentDelta { .. }).then_some(()));
        let asked = std::time::Instant::now();
        run.send(Command::Interrupt);
        let (state_, _, _) = run.wait_for(completed);
        assert_eq!(state_, TurnState::Interrupted);
        assert!(asked.elapsed() < Duration::from_secs(5));
        run.finish();
    }

    #[test]
    fn a_stop_at_an_approval_answers_every_call_so_the_history_stays_valid() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let two = vec![
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"run_command","arguments":"{\"command\":\"true\"}"}},{"index":1,"id":"b","function":{"name":"run_command","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#.to_owned(),
            "data: [DONE]".to_owned(),
        ];
        let script = Script::new(vec![Ok(two), Ok(text(&["Next"]))]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("go"));
        run.wait_for(|e| matches!(e.event, Event::ApprovalRequested { .. }).then_some(()));
        run.send(Command::Interrupt);
        run.wait_for(completed);
        run.send(prompt("again"));
        run.wait_for(completed);
        run.finish();
        let sent = &script.bodies()[1]["messages"];
        let answered: Vec<_> = sent
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["tool_call_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            answered,
            ["a", "b"],
            "every call of the reply has its answer"
        );
    }

    #[test]
    fn a_reply_that_breaks_off_before_its_end_runs_no_call() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        // A whole, valid call, but the stream closes with no finish reason and no [DONE].
        let broken = vec![
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"run_command","arguments":"{\"command\":\"touch made\"}"}}]}}]}"#.to_owned(),
        ];
        let run = start(
            args(work.path(), Mode::Bypass),
            Script::new(vec![Ok(broken)]),
            state.path().into(),
        );
        run.send(prompt("go"));
        let (state_, _, error) = run.wait_for(completed);
        run.finish();
        assert_eq!(state_, TurnState::Failed);
        assert!(error.unwrap().contains("before it ended"));
        assert!(!work.path().join("made").exists());
    }

    #[test]
    fn the_saved_conversation_is_redacted_and_ids_differ_per_process() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let token = "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB";
        let script = Script::new(vec![Ok(text(&["Noted"])), Ok(text(&["Again"]))]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt(&format!("my token is {token}")));
        let first = run.wait_for(|e| match &e.event {
            Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                ..
            } => e.item.clone(),
            _ => None,
        });
        run.wait_for(completed);
        run.finish();
        let saved = std::fs::read_to_string(state.path().join("s-1.json")).unwrap();
        assert!(!saved.contains(token), "{saved}");
        // The live request still had it: only what is kept is masked.
        assert!(script.bodies()[0].to_string().contains(token));
        let resumed = RunnerArgs {
            resume: Some("s-1".into()),
            session_id: None,
            ..args(work.path(), Mode::Ask)
        };
        let run = start(resumed, script, state.path().into());
        run.send(prompt("more"));
        let second = run.wait_for(|e| match &e.event {
            Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                ..
            } => e.item.clone(),
            _ => None,
        });
        run.wait_for(completed);
        run.finish();
        assert_ne!(first, second, "a resumed session never reuses a card's id");
    }

    #[test]
    fn reasoning_goes_back_with_its_calls_in_the_turn_and_never_after() {
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.txt"), "x").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut call =
            vec![r#"data: {"choices":[{"delta":{"reasoning":"I should read it"}}]}"#.to_owned()];
        call.extend(tool_call("c1", "read_file", json!({"file_path": "a.txt"})));
        let script = Script::new(vec![Ok(call), Ok(text(&["Read"])), Ok(text(&["Later"]))]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("read a.txt"));
        run.wait_for(completed);
        run.send(prompt("and now?"));
        run.wait_for(completed);
        run.finish();
        let bodies = script.bodies();
        let with_calls = |body: &Value| {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["tool_calls"].is_array())
                .cloned()
                .unwrap()
        };
        assert_eq!(
            with_calls(&bodies[1])["reasoning_content"],
            "I should read it",
            "sent back while its turn runs"
        );
        assert!(with_calls(&bodies[2]).get("reasoning_content").is_none());
        let saved = std::fs::read_to_string(state.path().join("s-1.json")).unwrap();
        assert!(!saved.contains("I should read it"), "never kept on disk");
    }

    #[test]
    fn a_new_model_is_offered_tools_again() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Err("The model server answered with HTTP 400: m does not support tools".into()),
            Ok(text(&["Plain"])),
            Ok(text(&["With tools"])),
        ]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("hi"));
        run.wait_for(completed);
        run.send(Command::SetModel {
            model: "other".into(),
            effort: None,
        });
        run.send(prompt("hi"));
        run.wait_for(completed);
        run.finish();
        let bodies = script.bodies();
        assert!(bodies[1].get("tools").is_none());
        assert!(bodies[2].get("tools").is_some());
    }

    #[test]
    fn a_stop_closes_the_connection_of_a_silent_server() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (closed_tx, closed) = channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                seen.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&seen).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; length];
            let _ = stream.read_exact(&mut body);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                  data: {\"choices\":[{\"delta\":{\"content\":\"thinking\"}}]}\n\n",
            );
            // Silent from here on; a read ends only when the client closes the connection.
            let mut rest = [0u8; 64];
            while matches!(stream.read(&mut rest), Ok(n) if n > 0) {}
            let _ = closed_tx.send(());
        });
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (commands, rx) = channel();
        let (tx, events) = channel();
        let run_args = RunnerArgs {
            server: base.clone(),
            ..args(work.path(), Mode::Ask)
        };
        let dir = state.path().to_path_buf();
        let done = std::thread::spawn(move || {
            Runner::new(
                run_args,
                HttpChat::new(&base, None, "t"),
                dir,
                Lines(tx, Vec::new()),
            )
            .run(&rx)
        });
        let run = Run {
            commands,
            events,
            done,
        };
        run.send(Command::ConfigureLocal {
            context: Box::default(),
        });
        run.send(prompt("go"));
        run.wait_for(|e| matches!(e.event, Event::ContentDelta { .. }).then_some(()));
        run.send(Command::Interrupt);
        let (state_, _, _) = run.wait_for(completed);
        assert_eq!(state_, TurnState::Interrupted);
        closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the connection is closed, not left waiting on the server");
        run.finish();
    }

    #[test]
    fn plan_mode_offers_no_writing_tool_and_refuses_one_called_anyway() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Ok(tool_call(
                "c1",
                "write_file",
                json!({"file_path": "x", "content": "y"}),
            )),
            Ok(text(&["Plan"])),
        ]);
        let run = start(
            args(work.path(), Mode::Plan),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("plan it"));
        run.wait_for(completed);
        run.finish();
        assert!(!work.path().join("x").exists());
        let offered: Vec<_> = script.bodies()[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(offered, ["read_file", "list_dir", "search"]);
    }

    #[test]
    fn a_model_without_tools_is_asked_again_without_them_and_the_user_is_told() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Err("The model server answered with HTTP 400: gemma does not support tools".into()),
            Ok(text(&["Plain answer"])),
        ]);
        let run = start(
            args(work.path(), Mode::Ask),
            script.clone(),
            state.path().into(),
        );
        run.send(prompt("hi"));
        let mut notice = false;
        let (state_, _, _) = run.wait_for(|e| {
            if matches!(e.event, Event::Notice { .. }) {
                notice = true;
            }
            completed(e)
        });
        run.finish();
        assert_eq!(state_, TurnState::Completed);
        assert!(notice);
        assert!(script.bodies()[1].get("tools").is_none());
    }

    #[test]
    fn a_server_error_fails_the_turn_with_its_reason_and_no_model_fails_at_once() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Err(
            "The model server answered with HTTP 404: model not found".into(),
        )]);
        let run = start(args(work.path(), Mode::Ask), script, state.path().into());
        run.send(prompt("hi"));
        let (state_, _, error) = run.wait_for(completed);
        assert_eq!(state_, TurnState::Failed);
        assert!(error.unwrap().contains("model not found"));
        run.finish();

        let none = RunnerArgs {
            model: None,
            ..args(work.path(), Mode::Ask)
        };
        let run = start(none, Script::new(vec![]), state.path().into());
        run.send(prompt("hi"));
        let (state_, _, error) = run.wait_for(completed);
        assert_eq!(state_, TurnState::Failed);
        assert!(error.unwrap().contains("No model"));
        run.finish();
    }

    #[test]
    fn an_interrupt_ends_the_turn_and_model_and_mode_changes_are_acknowledged() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let run = start(
            args(work.path(), Mode::Ask),
            Script::new(vec![Ok(tool_call(
                "c1",
                "run_command",
                json!({"command": "true"}),
            ))]),
            state.path().into(),
        );
        run.send(prompt("go"));
        run.wait_for(|e| matches!(e.event, Event::ApprovalRequested { .. }).then_some(()));
        run.send(Command::Interrupt);
        let (state_, _, _) = run.wait_for(completed);
        assert_eq!(state_, TurnState::Interrupted);
        run.send(Command::SetModel {
            model: "qwen3.5:4b".into(),
            effort: None,
        });
        let model = run.wait_for(|e| match &e.event {
            Event::ModelChanged { model } => Some(model.clone()),
            _ => None,
        });
        assert_eq!(model, "qwen3.5:4b");
        run.send(Command::SetMode { mode: Mode::Plan });
        run.wait_for(|e| matches!(e.event, Event::ModeChanged { mode: Mode::Plan }).then_some(()));
        let (_, code) = run.finish();
        assert_eq!(code, 0);
    }

    #[test]
    fn a_tool_call_cut_off_by_the_output_limit_never_runs() {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let cut = vec![
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"run_command","arguments":"{\"command\":\"touch made\"}"}}]}}]}"#.to_owned(),
            r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#.to_owned(),
            "data: [DONE]".to_owned(),
        ];
        let run = start(
            args(work.path(), Mode::Bypass),
            Script::new(vec![Ok(cut)]),
            state.path().into(),
        );
        run.send(prompt("go"));
        let (state_, _, error) = run.wait_for(completed);
        run.finish();
        assert_eq!(state_, TurnState::Failed);
        assert!(error.unwrap().contains("cut off"));
        assert!(!work.path().join("made").exists());
    }

    #[test]
    fn sessions_live_in_the_apps_state_folder() {
        assert_eq!(
            sessions_dir(Some("/s"), Some("/h")),
            Some(PathBuf::from("/s/agent-terminal/local"))
        );
        assert_eq!(
            sessions_dir(Some("relative"), Some("/h")),
            Some(PathBuf::from("/h/.local/state/agent-terminal/local"))
        );
        assert_eq!(sessions_dir(None, None), None);
        assert!(agent_core::local_agent::valid_session_id(&new_session_id()));
    }
}
#[test]
fn command_eof_is_clean_only_on_a_frame_boundary() {
    let mut frame = serde_json::to_vec(&Command::Interrupt).unwrap();
    let input = read_commands(std::io::Cursor::new(frame.clone()));
    assert!(matches!(input.recv().unwrap(), RunnerInput::Invalid(_)));
    assert!(input.recv().is_err());
    frame.push(b'\n');
    let input = read_commands(std::io::Cursor::new(frame));
    assert!(matches!(
        input.recv().unwrap(),
        RunnerInput::Command(Command::Interrupt)
    ));
    assert!(matches!(input.recv().unwrap(), RunnerInput::Closed));
    let input = read_commands(std::io::Cursor::new(Vec::<u8>::new()));
    assert!(matches!(input.recv().unwrap(), RunnerInput::Closed));
}
