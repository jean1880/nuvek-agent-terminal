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
    chat_request, error_message, gate, refuses_tools, system_prompt, tools_for, ChatMessage, Gate,
    RunnerArgs, StreamEvent, StreamReader, Tool, ToolCall, APPROVAL_OPTIONS,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most model requests one turn may make: a model that keeps calling tools is stopped.
const MAX_STEPS: usize = 60;
/// The most of a tool's output the model is given back.
const MAX_TOOL_OUTPUT: usize = 30_000;
/// The longest a server may take to start answering (a model may have to load first).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The most a reply may hold in all, and in one line: a server cannot make the runner hold more.
const MAX_REPLY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
/// The longest a reply may go without a line before it counts as stalled.
const STALL: Duration = Duration::from_secs(600);
/// The most of the conversation (in characters of text) sent with a request: the oldest
/// exchanges are left out past it, and a transcript larger than [`MAX_TRANSCRIPT_BYTES`] is not
/// read back.
const CONTEXT_BUDGET: usize = 400_000;
const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
/// The most of the conversation kept at all (memory and file), in bytes of text: a few requests'
/// worth, so the oldest exchanges go once they could never be sent again.
const RETAIN_BUDGET: usize = 4 * CONTEXT_BUDGET;

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

/// Reads stdin on its own thread: each line a command (`None` once stdin closes), so a turn can
/// look for an interrupt or an answer while it waits on the server.
pub fn read_commands(input: impl std::io::Read + Send + 'static) -> Receiver<Option<Command>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(input);
        loop {
            let mut bytes = Vec::new();
            // Includes startup configuration; oversized or unterminated frames end the input.
            let Ok(count) = std::io::Read::take(
                &mut reader,
                agent_core::local_context::MAX_SETUP_BYTES as u64 + 1,
            )
            .read_until(b'\n', &mut bytes) else {
                break;
            };
            if count == 0 {
                break;
            }
            if count > agent_core::local_context::MAX_SETUP_BYTES || bytes.last() != Some(&b'\n') {
                break;
            }
            let Ok(line) = String::from_utf8(bytes) else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Command>(&line) {
                Ok(command) => {
                    if tx.send(Some(command)).is_err() {
                        return;
                    }
                }
                Err(_) => eprintln!("agent-terminal --local-agent: unreadable command"),
            }
        }
        let _ = tx.send(None);
    });
    rx
}

/// How a turn ended early.
enum Stop {
    Interrupted,
    /// Stdin closed: the app is gone.
    Closed,
}

pub struct Runner<C: ChatClient, W: Write> {
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
    pub fn new(args: RunnerArgs, client: C, dir: PathBuf, out: W) -> Self {
        let session = args
            .resume
            .clone()
            .or_else(|| args.session_id.clone())
            .unwrap_or_else(new_session_id);
        Self {
            model: args.model.clone(),
            context: Default::default(),
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
        if let Ok(line) = serde_json::to_string(&envelope) {
            // A closed stdout means the app is gone; the next read of stdin ends the run.
            let _ = writeln!(self.out, "{line}");
            let _ = self.out.flush();
        }
    }

    fn id(&mut self, what: &str) -> String {
        self.next_id += 1;
        format!("{what}-{}-{}", self.generation, self.next_id)
    }

    /// Runs until stdin closes; 0 then, 1 if the session could not start.
    pub fn run(&mut self, commands: &Receiver<Option<Command>>) -> i32 {
        match commands.recv() {
            Ok(Some(Command::ConfigureLocal { context })) => {
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
                    Ok(Some(command)) => command,
                    Ok(None) | Err(_) => return 0,
                }
            } else {
                self.deferred.remove(0)
            };
            if let Err(Stop::Closed) = self.handle(next, commands) {
                return 0;
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
        self.messages = transcript.messages;
        if self.model.is_none() {
            self.model = transcript.model;
        }
        Ok(())
    }

    /// Writes the conversation, every text in it redacted as the thread store's events are: a
    /// secret read from a file or typed in a prompt is never kept in the clear. The live
    /// conversation is left as it is, so this turn's next request still has it.
    fn save(&mut self) {
        use agent_core::redact::redact;
        // What is kept is bounded, in whole exchanges, so it never grows past what can be sent.
        drop_oldest_until(&mut self.messages, |kept| {
            kept.iter().map(message_size).sum::<usize>() <= RETAIN_BUDGET
        });
        let mut messages: Vec<ChatMessage> = self
            .messages
            .iter()
            .map(|m| ChatMessage {
                content: redact(&m.content),
                tool_calls: m
                    .tool_calls
                    .iter()
                    .map(|c| ToolCall {
                        function: agent_core::local_agent::ToolFunction {
                            name: c.function.name.clone(),
                            arguments: redact(&c.function.arguments),
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
            serde_json::to_vec(&Transcript {
                model: self.model.clone(),
                messages: messages.to_vec(),
            })
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

    fn handle(
        &mut self,
        command: Command,
        commands: &Receiver<Option<Command>>,
    ) -> Result<(), Stop> {
        match command {
            Command::Prompt { text } => self.turn(text, commands),
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
    fn turn(&mut self, text: String, commands: &Receiver<Option<Command>>) -> Result<(), Stop> {
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
        // Earlier turns' reasoning is not sent again.
        agent_core::local_agent::forget_reasoning(&mut self.messages);
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
                    failure = Some(why);
                    break;
                }
            }
        }
        self.save();
        let usage = (usage.input_tokens + usage.output_tokens > 0).then_some(usage);
        let (state, error) = match (&result, failure) {
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
        match result {
            Err(Stop::Interrupted) => Ok(()),
            other => other,
        }
    }

    /// One model request and the tool calls it makes.
    fn step(
        &mut self,
        model: &str,
        commands: &Receiver<Option<Command>>,
        usage: &mut Usage,
    ) -> Result<StepEnd, StepError> {
        let tools = if self.no_tools {
            Vec::new()
        } else {
            tools_for(self.mode)
        };
        let mut messages = vec![ChatMessage::new(
            "system",
            system_prompt(&self.args.cwd, self.mode),
        )];
        messages.extend(
            within_budget(&self.messages, CONTEXT_BUDGET)
                .iter()
                .cloned(),
        );
        let body = chat_request(model, &messages, &tools);
        let (reply, ended, stopped) = self.stream(&body, commands, usage)?;
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
                    reply.tool_calls.clone()
                } else {
                    Vec::new()
                },
                // Sent back with its calls while this turn runs (`forget_reasoning`).
                reasoning_content: (calls && !reply.reasoning.is_empty())
                    .then(|| reply.reasoning.clone()),
                ..ChatMessage::new("assistant", reply.text.clone())
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
        for (at, call) in reply.tool_calls.iter().enumerate() {
            match self.call(call, commands) {
                Ok(output) => self
                    .messages
                    .push(ChatMessage::tool_result(&call.id, output)),
                Err(stop) => {
                    // Every call the reply made gets an answer, so the history stays one a
                    // server accepts: this one and the rest did not run.
                    for skipped in &reply.tool_calls[at..] {
                        self.messages.push(ChatMessage::tool_result(
                            &skipped.id,
                            "Not run: the user stopped the turn.",
                        ));
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
        body: &Value,
        commands: &Receiver<Option<Command>>,
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
                self.emit(Envelope::new(Event::Notice {
                    text: "This model cannot use tools, so it answers without reading or \
                           changing anything. Pick a model that supports tools to let it work."
                        .to_owned(),
                }));
                let mut retry = body.clone();
                if let Some(map) = retry.as_object_mut() {
                    map.remove("tools");
                }
                return self.stream(&retry, commands, usage);
            }
            return Err(StepError::Failed(why));
        }
        Ok((reply, ended, None))
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
                self.emit(Envelope::delta(id, StreamKind::Reasoning, text));
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
                self.emit(Envelope::delta(id, StreamKind::Assistant, text));
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
        commands: &Receiver<Option<Command>>,
    ) -> Result<String, StepError> {
        if let Some(stop) = poll(commands, &mut self.deferred) {
            return Err(StepError::Stop(stop));
        }
        let item = self.id("tool");
        let tool = Tool::from_name(&call.function.name);
        let input = call.input();
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
            me.emit(Envelope::item_completed(
                &item,
                ItemStatus::Failed,
                None,
                Some(&why),
            ));
            Ok(format!("Error: {why}"))
        };
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
                let output = truncate(&output, MAX_TOOL_OUTPUT);
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

    /// Asks the user, and waits for the answer (or an interrupt).
    fn ask(
        &mut self,
        tool: Tool,
        title: &str,
        input: &Value,
        commands: &Receiver<Option<Command>>,
    ) -> Result<Decision, StepError> {
        let request = self.id("approval");
        self.emit(
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
        );
        loop {
            match commands.recv_timeout(Duration::from_secs(3600)) {
                Ok(Some(Command::Approve {
                    request: answered,
                    decision,
                    ..
                })) if answered == request => {
                    self.emit(
                        Envelope::new(Event::ApprovalResolved { decision }).request(&request),
                    );
                    return Ok(decision);
                }
                Ok(Some(Command::Interrupt)) => {
                    self.emit(Envelope::new(Event::ApprovalExpired).request(&request));
                    return Err(StepError::Stop(Stop::Interrupted));
                }
                Ok(Some(other)) => self.deferred.push(other),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
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

enum StepError {
    Stop(Stop),
    Failed(String),
}

/// The reasoning and text items open in the current reply, with their whole text so far.
#[derive(Default)]
struct Shown {
    reasoning: Option<(String, String)>,
    text: Option<(String, String)>,
}

/// Looks at what arrived without waiting: an interrupt (or a closed stdin) stops the reply;
/// anything else waits for the turn's end.
fn poll(commands: &Receiver<Option<Command>>, deferred: &mut Vec<Command>) -> Option<Stop> {
    loop {
        match commands.try_recv() {
            Ok(Some(Command::Interrupt)) => return Some(Stop::Interrupted),
            // A stale answer (its card is gone) is dropped.
            Ok(Some(Command::Approve { .. })) => {}
            Ok(Some(other)) => deferred.push(other),
            Ok(None) | Err(TryRecvError::Disconnected) => return Some(Stop::Closed),
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

/// Where each exchange of `messages` starts: at every user message (and at 0, for anything
/// before the first). An exchange is a question and everything that answers it, so cutting only
/// at these never parts a tool call from its answer.
fn exchange_starts(messages: &[ChatMessage]) -> Vec<usize> {
    let mut starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user")
        .map(|(at, _)| at)
        .collect();
    if starts.first() != Some(&0) && !messages.is_empty() {
        starts.insert(0, 0);
    }
    starts
}

/// The newest whole exchanges of `messages` whose text fits `budget` bytes. The latest exchange
/// is kept whole even past the budget: the server then says if it is too long, and nothing is
/// ever sent without what it answers.
fn within_budget(messages: &[ChatMessage], budget: usize) -> &[ChatMessage] {
    let starts = exchange_starts(messages);
    let mut keep_from = messages.len();
    let mut total = 0;
    let mut end = messages.len();
    for &start in starts.iter().rev() {
        total += messages[start..end].iter().map(message_size).sum::<usize>();
        if total > budget && keep_from < messages.len() {
            break;
        }
        keep_from = start;
        end = start;
    }
    &messages[keep_from..]
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

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… ({} more bytes not shown)",
        &text[..cut],
        text.len() - cut
    )
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
fn new_session_id() -> String {
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
            tx.send(Some(command)).unwrap();
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
    }

    impl Script {
        fn new(replies: Vec<Reply>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies)),
                bodies: Arc::default(),
            }
        }
        fn bodies(&self) -> Vec<Value> {
            self.bodies.lock().unwrap().clone()
        }
    }

    /// A line that makes the scripted server go silent: the reply never ends.
    const STALL_MARK: &str = "<stall>";

    impl ChatClient for Script {
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
        commands: Sender<Option<Command>>,
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
            self.commands.send(Some(command)).unwrap();
        }
        /// Closes stdin and returns every event left, and the exit code.
        fn finish(self) -> (Vec<Envelope>, i32) {
            self.commands.send(None).unwrap();
            let code = self.done.join().unwrap();
            (self.events.try_iter().collect(), code)
        }
    }

    fn start(args: RunnerArgs, script: Script, dir: PathBuf) -> Run {
        let (commands, rx) = channel();
        commands
            .send(Some(Command::ConfigureLocal {
                context: Box::default(),
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
    fn the_history_sent_is_whole_exchanges_and_never_parts_a_call_from_its_answer() {
        let msg = |role: &str, n: usize| ChatMessage::new(role, "x".repeat(n));
        let call = |n: usize| ChatMessage {
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: agent_core::local_agent::ToolFunction {
                    name: "write_file".into(),
                    arguments: "a".repeat(n),
                },
            }],
            ..ChatMessage::new("assistant", "")
        };
        let all = vec![
            msg("user", 50),
            msg("assistant", 50),
            msg("tool", 50),
            msg("user", 10),
            msg("assistant", 10),
        ];
        assert_eq!(within_budget(&all, 1000).len(), 5);
        let kept = within_budget(&all, 100);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].role, "user");
        // The latest exchange is kept whole, even alone past the budget.
        assert_eq!(within_budget(&all, 1).len(), 2);
        // A huge call then a short answer: the call, its answer and their question go together.
        let big = vec![
            msg("user", 10),
            msg("user", 5),
            call(450_000),
            msg("tool", 5),
        ];
        let kept = within_budget(&big, 400_000);
        assert_eq!(
            kept.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            ["user", "assistant", "tool"]
        );
        // Reasoning counts.
        let thought = ChatMessage {
            reasoning_content: Some("r".repeat(500)),
            ..msg("assistant", 1)
        };
        let reasoned = vec![msg("user", 1), msg("user", 1), thought];
        assert_eq!(within_budget(&reasoned, 100).len(), 2);
        // Kept history drops whole exchanges from the oldest.
        let mut kept = all.clone();
        drop_oldest_until(&mut kept, |m| m.len() <= 2);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].role, "user");
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
