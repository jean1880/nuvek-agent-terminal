//! Session-owned stdio MCP discovery and serial, non-replayed requests.
use super::process;

use agent_core::adapter::Mode;
use agent_core::local_mcp::{
    alias, revision, ApprovalPolicy, CompiledSchema, McpServerConfig, RegistryTool, ResolvedTool,
    ToolIdentity,
};
use rmcp::{
    model::{ClientConfig, PaginatedRequestParams, ProtocolVersion, Tool},
    service::{RoleClient, RunningService},
    ClientHandler, ClientLifecycleMode, ClientServiceExt,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

const MAX_SERVERS: usize = 16;
const STARTUP_CONCURRENCY: usize = 4;
const MAX_TOOLS: usize = 256;
const MAX_SESSION_TOOLS: usize = 1024;
const MAX_PAGES: usize = 32;
const MAX_DISCOVERY_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSION_BYTES: usize = 32 * 1024 * 1024;
const MAX_SCHEMA_BYTES: usize = 64 * 1024;

fn resolve_workspaces(
    mut configs: Vec<McpServerConfig>,
    workspace: &std::path::Path,
) -> Vec<McpServerConfig> {
    for config in &mut configs {
        let cwd = if config.cwd == "$WORKSPACE" {
            workspace.to_owned()
        } else if let Some(suffix) = config.cwd.strip_prefix("$WORKSPACE/") {
            workspace.join(suffix)
        } else if std::path::Path::new(&config.cwd).is_relative() {
            workspace.join(&config.cwd)
        } else {
            continue;
        };
        config.cwd = cwd.to_string_lossy().into_owned();
    }
    configs
}

/// Bounded discovery data, frozen at the turn boundary. Never contains resolved environment.
#[derive(Default)]
pub struct Discovery {
    pub servers: Vec<ServerDiscovery>,
}
pub struct ServerDiscovery {
    pub id: String,
    pub tools: Vec<Tool>,
    pub notices: Vec<String>,
    pub required_failed: bool,
    pub registry: Vec<RegistryTool>,
    pub connection: Option<ConnectionLease>,
}

/// Process-local lifetime identity, separate from rememberable tool content revisions.
#[derive(Clone)]
pub struct ConnectionLease {
    id: String,
    closed: Arc<AtomicBool>,
}
impl ConnectionLease {
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }
}

struct Handler;
impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}
type Client = RunningService<RoleClient, Handler>;

struct Server {
    config: McpServerConfig,
    client: Client,
    process: process::Process,
    stale: Arc<AtomicBool>,
    failure: Arc<std::sync::Mutex<Option<&'static str>>>,
    received_bytes: Arc<std::sync::atomic::AtomicUsize>,
    tools: Vec<Tool>,
    notices: Vec<String>,
    discovered_count: usize,
    discovered_bytes: usize,
    offered: BTreeSet<String>,
    validators: BTreeMap<String, CompiledSchema>,
    connection: ConnectionLease,
    revisions: BTreeMap<String, String>,
}
impl Server {
    async fn close(&mut self) {
        let _ = self
            .client
            .close_with_timeout(Duration::from_millis(200))
            .await;
        self.process.shutdown().await;
    }
    async fn discover(&mut self) -> Result<(), String> {
        let wire_start = self.received_bytes.load(Ordering::Relaxed);
        self.client.clear_response_cache().await;
        let mut cursor = None;
        let mut revisions = BTreeMap::new();
        let mut cursors = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut tools = Vec::new();
        let mut notices = Vec::new();
        let mut validators = BTreeMap::new();
        let mut count = 0usize;
        let mut bytes = 0usize;
        for _ in 0..MAX_PAGES {
            let page = self
                .client
                .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(|_| self.reason("MCP tool discovery failed."))?;
            bytes = bytes.saturating_add(
                serde_json::to_vec(&page)
                    .map_err(|_| "Invalid MCP tool discovery encoding.")?
                    .len(),
            );
            let wire_bytes = self
                .received_bytes
                .load(Ordering::Relaxed)
                .saturating_sub(wire_start);
            count = count.saturating_add(page.tools.len());
            if count > MAX_TOOLS || bytes.max(wire_bytes) > MAX_DISCOVERY_BYTES {
                return Err("MCP discovery exceeded its tool or byte bound.".into());
            }
            for tool in page.tools {
                if tool.name.is_empty()
                    || tool.name.len() > 256
                    || tool.name.chars().any(char::is_control)
                    || !names.insert(tool.name.to_string())
                {
                    return Err("MCP discovery returned invalid or duplicate tool names.".into());
                }
                if serde_json::to_vec(&tool)
                    .map_err(|_| "Invalid MCP schema encoding.")?
                    .len()
                    > MAX_SCHEMA_BYTES
                {
                    notices
                        .push("An MCP tool was excluded because its schema exceeds 64 KiB.".into());
                } else {
                    // Fingerprint original metadata before masking changes any displayed field.
                    let content_revision = revision(&tool).map_err(str::to_owned)?;
                    let validator = match CompiledSchema::compile(&serde_json::Value::Object(
                        (*tool.input_schema).clone(),
                    )) {
                        Ok(validator) => validator,
                        Err(why) => {
                            notices.push(
                                self.process
                                    .scrubber
                                    .scrub(&format!("MCP tool {} unavailable: {why}", tool.name)),
                            );
                            continue;
                        }
                    };
                    let mut safe =
                        serde_json::to_value(&tool).map_err(|_| "Invalid MCP schema encoding.")?;
                    self.process.scrubber.value(&mut safe)?;
                    if self.process.scrubber.scrub(&tool.name) != tool.name {
                        return Err("MCP tool identity contains sensitive text.".into());
                    }
                    if serde_json::to_vec(&safe)
                        .map_err(|_| "Invalid scrubbed MCP schema encoding.")?
                        .len()
                        > MAX_SCHEMA_BYTES
                    {
                        notices.push(
                            "An MCP tool was excluded because its scrubbed schema exceeds 64 KiB."
                                .into(),
                        );
                        continue;
                    }
                    tools.push(
                        serde_json::from_value(safe)
                            .map_err(|_| "MCP schema could not be scrubbed safely.")?,
                    );
                    validators.insert(tool.name.to_string(), validator);
                    revisions.insert(tool.name.to_string(), content_revision);
                }
            }
            cursor = page.next_cursor;
            match &cursor {
                Some(next) if next.len() > 4096 || !cursors.insert(next.clone()) => {
                    return Err("MCP discovery returned an oversized or repeated cursor.".into())
                }
                Some(_) => {}
                None => {
                    self.tools = tools;
                    self.notices = notices;
                    self.discovered_count = count;
                    self.discovered_bytes = bytes.max(wire_bytes);
                    self.validators = validators;
                    self.revisions = revisions;
                    return Ok(());
                }
            }
        }
        Err("MCP discovery exceeded its page bound.".into())
    }
    fn reason(&self, fallback: &str) -> String {
        let reason = self
            .failure
            .lock()
            .ok()
            .and_then(|f| *f)
            .unwrap_or(fallback);
        self.process.diagnostic(reason)
    }
    fn snapshot(&self, mode: Mode) -> ServerDiscovery {
        let tools: Vec<_> = self
            .tools
            .iter()
            .filter(|tool| {
                self.config.tools.iter().any(|policy| {
                    policy.name == tool.name
                        && policy.approval != ApprovalPolicy::Deny
                        && self.config.approval != ApprovalPolicy::Deny
                        && (mode != Mode::Plan || policy.read_only)
                })
            })
            .cloned()
            .collect();
        let registry = tools.iter().filter_map(|tool| {
            let alias = alias(&self.config.id, &tool.name).ok()?;
            let definition = revision(&self.config).ok()?;
            let schema_revision = self.revisions.get(tool.name.as_ref())?.clone();
            let schema = serde_json::json!({"type":"function","function":{
                "name":alias,
                "description":self.process.scrubber.scrub(&format!("{} ({}) / {}\n{}", self.config.label,self.config.id,tool.name,tool.description.as_deref().unwrap_or(""))),
                "parameters":tool.input_schema
            }});
            Some(RegistryTool { resolved: ResolvedTool {
                identity:ToolIdentity::Mcp {server_id:self.config.id.clone(),name:tool.name.to_string()},
                alias, schema, server_revision:definition, schema_revision,
            }, label:self.process.scrubber.scrub(&self.config.label), validator:self.validators.get(tool.name.as_ref())?.clone() })
        }).collect();
        ServerDiscovery {
            id: self.config.id.clone(),
            tools,
            notices: self.notices.clone(),
            required_failed: false,
            registry,
            connection: Some(self.connection.clone()),
        }
    }
}

fn failed(config: &McpServerConfig, reason: String) -> ServerDiscovery {
    ServerDiscovery {
        id: config.id.clone(),
        tools: Vec::new(),
        notices: vec![format!("MCP server {} unavailable: {reason}", config.id)],
        required_failed: config.required,
        registry: Vec::new(),
        connection: None,
    }
}

pub(super) async fn cancelled(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let _ = cancel.changed().await;
}

async fn start(
    config: McpServerConfig,
    effective: Arc<BTreeMap<String, String>>,
    executable: PathBuf,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Server, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.startup_timeout_secs);
    let (mut process, transport) =
        process::Process::spawn(&config, &effective, &executable, deadline, &mut cancel).await?;
    let stale = transport.stale.clone();
    let closed = transport.closed.clone();
    let failure = transport.failure.clone();
    let received_bytes = transport.received_bytes.clone();
    let startup = Handler.serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        },
    );
    let result = tokio::select! {
        _ = cancelled(&mut cancel) => Err("MCP startup cancelled."),
        result = tokio::time::timeout_at(deadline, startup) => match result {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(_)) => Err("MCP startup failed."),
            Err(_) => Err("MCP startup timed out."),
        }
    };
    let client = match result {
        Ok(client) => client,
        Err(reason) => {
            process.shutdown().await;
            return Err(process.diagnostic(reason));
        }
    };
    let mut server = Server {
        config,
        client,
        process,
        stale,
        failure,
        received_bytes,
        tools: Vec::new(),
        notices: Vec::new(),
        discovered_count: 0,
        discovered_bytes: 0,
        offered: BTreeSet::new(),
        validators: BTreeMap::new(),
        connection: ConnectionLease {
            id: crate::local_agent::new_session_id(),
            closed,
        },
        revisions: BTreeMap::new(),
    };
    let result = tokio::select! {
        _ = cancelled(&mut cancel) => Err("MCP discovery cancelled.".into()),
        result = tokio::time::timeout_at(deadline, server.discover()) => result.unwrap_or_else(|_| Err("MCP discovery timed out.".into())),
    };
    if let Err(reason) = result {
        server.close().await;
        return Err(reason);
    }
    // Do not reset stale here: a list-change notification during pagination must cause a
    // fresh discovery at the next boundary, even if this turn already has a snapshot.
    Ok(server)
}

enum Request {
    Discover {
        mode: Mode,
        reply: std::sync::mpsc::SyncSender<Discovery>,
    },
    Call {
        connection: Option<String>,
        server_id: String,
        name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
        reply: std::sync::mpsc::SyncSender<Result<serde_json::Value, String>>,
    },
}

/// One dedicated current-thread runtime per Local session. All cross-thread queues are
/// bounded; the blocking runner polls its own commands while a discovery is outstanding.
pub struct Session {
    masks: process::Scrubber,
    tx: tokio::sync::mpsc::Sender<Request>,
    cancel: tokio::sync::watch::Sender<bool>,
    done: std::sync::mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Session {
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        configs: Vec<McpServerConfig>,
        executable: PathBuf,
        effective: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        Self::with_executable(configs, executable, effective)
    }
    pub fn new(configs: Vec<McpServerConfig>, workspace: &std::path::Path) -> Result<Self, String> {
        let configs = resolve_workspaces(configs, workspace);
        Self::with_executable(
            configs,
            std::env::current_exe().map_err(|_| "Could not locate MCP guardian executable.")?,
            std::env::vars_os()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect(),
        )
    }
    fn with_executable(
        configs: Vec<McpServerConfig>,
        executable: PathBuf,
        effective: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        if configs.len() > MAX_SERVERS {
            return Err("MCP session exceeds 16 selected servers.".into());
        }
        let mut ids = BTreeSet::new();
        for config in &configs {
            config.validate()?;
            if !ids.insert(&config.id) {
                return Err("MCP session contains duplicate server IDs.".into());
            }
        }
        let masks = process::Scrubber::for_configs(&configs, &effective);
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let (done_tx, done) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("local-mcp".into())
            .spawn(move || {
                // SDK diagnostic events can contain complete peer messages. No SDK tracing
                // escapes this runtime; user diagnostics are constant errors and scrubbed tails.
                tracing::subscriber::with_default(
                    tracing::subscriber::NoSubscriber::default(),
                    || {
                        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                            .enable_io()
                            .enable_time()
                            .build()
                        {
                            runtime.block_on(worker(
                                configs,
                                executable,
                                Arc::new(effective),
                                rx,
                                cancelled,
                            ));
                        }
                    },
                );
                let _ = done_tx.send(());
            })
            .map_err(|_| "Could not start MCP runtime thread.")?;
        Ok(Self {
            masks,
            tx,
            cancel,
            done,
            thread: Some(thread),
        })
    }
    pub fn discover(&self, mode: Mode) -> Result<std::sync::mpsc::Receiver<Discovery>, String> {
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        self.tx
            .try_send(Request::Discover { mode, reply })
            .map_err(|_| "MCP runtime is busy or unavailable.")?;
        Ok(receiver)
    }
    /// Apply all resolved credential masks before arguments reach stdout or a transcript.
    pub fn scrub(&self, value: serde_json::Value) -> Result<serde_json::Value, String> {
        let mut value = value;
        self.masks.value(&mut value).map_err(str::to_owned)?;
        Ok(value)
    }
    pub fn masks(&self) -> process::Scrubber {
        self.masks.clone()
    }
    /// Low-level single request, for an already authorised frozen-registry entry. The Local
    /// model loop checks authority and converts results before and after this call.
    /// No MRTR follow-up, reconnect or replay is performed after an ambiguous outcome.
    pub fn call_discovered(
        &self,
        server_id: String,
        name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<std::sync::mpsc::Receiver<Result<serde_json::Value, String>>, String> {
        self.call_on_connection(server_id, name, arguments, None)
    }
    pub fn call_frozen(
        &self,
        server_id: String,
        name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
        connection: &ConnectionLease,
    ) -> Result<std::sync::mpsc::Receiver<Result<serde_json::Value, String>>, String> {
        if !connection.is_open() {
            return Err("The reviewed MCP connection has closed.".into());
        }
        self.call_on_connection(server_id, name, arguments, Some(connection.id.clone()))
    }
    fn call_on_connection(
        &self,
        server_id: String,
        name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
        connection: Option<String>,
    ) -> Result<std::sync::mpsc::Receiver<Result<serde_json::Value, String>>, String> {
        if server_id.len() > 64
            || name.len() > 256
            || serde_json::to_vec(&arguments)
                .map_err(|_| "Invalid MCP arguments.")?
                .len()
                > 64 * 1024
        {
            return Err("MCP call exceeds its identity or argument bound.".into());
        }
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        self.tx
            .try_send(Request::Call {
                connection,
                server_id,
                name,
                arguments,
                reply,
            })
            .map_err(|_| "MCP runtime is busy or unavailable.")?;
        Ok(receiver)
    }
    pub fn shutdown(&mut self) {
        let _ = self.cancel.send(true);
        // Startup concurrency is four, and cleanup runs concurrently for every process.
        if self.done.recv_timeout(Duration::from_secs(4)).is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.shutdown();
        }
    }
}

async fn worker(
    configs: Vec<McpServerConfig>,
    executable: PathBuf,
    effective: Arc<BTreeMap<String, String>>,
    mut requests: tokio::sync::mpsc::Receiver<Request>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let mut servers: Vec<Option<Server>> = (0..configs.len()).map(|_| None).collect();
    loop {
        let request = tokio::select! { _ = cancelled(&mut cancel) => break, request = requests.recv() => match request { Some(request) => request, None => break } };
        match request {
            Request::Call {
                connection,
                server_id,
                name,
                arguments,
                reply,
            } => {
                let index = servers.iter().position(|s| {
                    s.as_ref().is_some_and(|s| {
                        s.config.id == server_id
                            && s.connection.is_open()
                            && connection.as_ref().is_none_or(|id| id == &s.connection.id)
                            && s.offered.contains(&name)
                            && s.config.approval != ApprovalPolicy::Deny
                            && s.config
                                .tools
                                .iter()
                                .any(|t| t.name == name && t.approval != ApprovalPolicy::Deny)
                    })
                });
                let result = if let Some(index) = index {
                    let Some(server) = servers[index].as_mut() else {
                        continue;
                    };
                    if server.validators.get(&name).is_none_or(|v| {
                        v.validate(&serde_json::Value::Object(arguments.clone()))
                            .is_err()
                    }) {
                        let _ = reply.try_send(Err(
                            "MCP arguments do not match the advertised input schema.".into(),
                        ));
                        continue;
                    }
                    let call = server.client.call_tool_once(
                        rmcp::model::CallToolRequestParams::new(name).with_arguments(arguments),
                    );
                    let response = tokio::select! {
                        _ = cancelled(&mut cancel) => Err("MCP call cancelled; its outcome is unknown.".to_owned()),
                        response = tokio::time::timeout(Duration::from_secs(server.config.call_timeout_secs), call) => match response {
                            Ok(Ok(rmcp::model::CallToolResponse::Complete(response))) => serde_json::to_value(response).map_err(|_| "MCP result encoding failed.".into()),
                            Ok(Ok(_)) => Err("MCP server requested unsupported interactive input; no follow-up was sent.".into()),
                            Ok(Err(_)) => Err(server.reason("MCP call failed; its outcome is unknown.")),
                            Err(_) => Err("MCP call timed out; its outcome is unknown.".into()),
                        }
                    };
                    match response {
                        Ok(mut value) => server
                            .process
                            .scrubber
                            .value(&mut value)
                            .map(|()| value)
                            .map_err(str::to_owned),
                        Err(reason) => {
                            server.close().await;
                            servers[index] = None;
                            Err(reason)
                        }
                    }
                } else {
                    Err("MCP tool is unavailable or not in the reviewed allowlist.".into())
                };
                let _ = reply.try_send(result);
            }
            Request::Discover { mode, reply } => {
                let mut discovery = Discovery::default();
                let mut total = 0usize;
                let mut bytes = 0usize;
                for batch in (0..configs.len()).step_by(STARTUP_CONCURRENCY) {
                    if *cancel.borrow() {
                        break;
                    }
                    let mut jobs = tokio::task::JoinSet::new();
                    let mut results = BTreeMap::new();
                    for index in batch..(batch + STARTUP_CONCURRENCY).min(configs.len()) {
                        if !configs[index].enabled {
                            continue;
                        }
                        results.insert(
                            index,
                            Err("MCP discovery task ended unexpectedly.".to_owned()),
                        );
                        let config = configs[index].clone();
                        let previous = servers[index].take();
                        let effective = effective.clone();
                        let executable = executable.clone();
                        let mut cancel = cancel.clone();
                        jobs.spawn(async move {
                            let result = match previous {
                                Some(mut server) if !server.client.is_transport_closed() => {
                                    if server.stale.swap(false, Ordering::AcqRel) {
                                        let refreshed = tokio::select! {
                                            _ = cancelled(&mut cancel) => Err("MCP discovery cancelled.".into()),
                                            result = tokio::time::timeout(Duration::from_secs(config.startup_timeout_secs), server.discover()) => result.unwrap_or_else(|_| Err("MCP discovery timed out.".into())),
                                        };
                                        if let Err(reason) = refreshed { server.close().await; return (index, Err(reason)); }
                                    }
                                    Ok(server)
                                }
                                previous => { if let Some(mut server) = previous { server.close().await; } start(config, effective, executable, cancel).await }
                            };
                            (index, result)
                        });
                    }
                    while let Some(result) = jobs.join_next().await {
                        if let Ok((index, result)) = result {
                            results.insert(index, result);
                        }
                    }
                    // Apply session ceilings in configured priority order, independent of
                    // which server happened to respond first.
                    for (index, result) in results {
                        match result {
                            Ok(mut server) => {
                                if total + server.discovered_count > MAX_SESSION_TOOLS
                                    || bytes + server.discovered_bytes > MAX_SESSION_BYTES
                                {
                                    server.close().await;
                                    discovery.servers.push(failed(&configs[index], "MCP session discovery exceeded its total tool or byte bound.".into()));
                                } else {
                                    total += server.discovered_count;
                                    bytes += server.discovered_bytes;
                                    let snapshot = server.snapshot(mode);
                                    server.offered =
                                        snapshot.tools.iter().map(|t| t.name.to_string()).collect();
                                    discovery.servers.push(snapshot);
                                    servers[index] = Some(server);
                                }
                            }
                            Err(reason) => discovery.servers.push(failed(&configs[index], reason)),
                        }
                    }
                }
                let _ = reply.try_send(discovery);
            }
        }
        if *cancel.borrow() {
            break;
        }
    }
    let mut cleanup = tokio::task::JoinSet::new();
    for mut server in servers.into_iter().flatten() {
        cleanup.spawn(async move {
            server.close().await;
        });
    }
    while cleanup.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_cwd_uses_the_thread_directory_without_shell_expansion() {
        let mut config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "id":"test","label":"Test","enabled":true,"command":"fixture","args":["$WORKSPACE"],
            "cwd":"$WORKSPACE","env":{},"tools":[],"approval":"ask","startup_timeout_secs":15,"call_timeout_secs":60
        })).unwrap();
        for (input, expected) in [
            ("$WORKSPACE", "/work/project"),
            ("$WORKSPACE/sub", "/work/project/sub"),
            ("sub", "/work/project/sub"),
            ("/other", "/other"),
        ] {
            config.cwd = input.into();
            let resolved =
                resolve_workspaces(vec![config.clone()], std::path::Path::new("/work/project"));
            assert_eq!(resolved[0].cwd, expected);
            assert_eq!(resolved[0].args, ["$WORKSPACE"]);
        }
    }
}
