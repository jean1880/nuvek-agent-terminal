//! The per-provider launch mapping: bridge env, the Claude overlay and
//! arguments, the Codex thread config, and the child read-back checks.
//!
//! The token appears only under a server's `env`, never in argv, and a
//! [`DelegationBinding`] is never serialised. The Local runner's [`LocalBridge`] is serialised
//! once, into its startup frame on stdin, and nowhere else.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::authority::TaskMode;
use super::env::{ENV_APP_PID, ENV_CAP, ENV_ENDPOINT, ENV_INSTANCE};
use super::ids::{ProcessGeneration, WorkerId};
use super::token::{CapabilityToken, InstanceNonce};
use super::{BRIDGE_ARG, SERVER_NAME};

/// Maximum number of extra stdio MCP servers a profile may grant to a delegated Claude child.
pub const MAX_CHILD_MCP_SERVERS: usize = 8;
/// Maximum byte length of a child MCP server name.
pub const MAX_CHILD_MCP_SERVER_NAME_LEN: usize = 64;
/// Maximum byte length of a child MCP server command.
pub const MAX_CHILD_MCP_COMMAND_LEN: usize = 4096;
/// Maximum argument count for one child MCP server.
pub const MAX_CHILD_MCP_ARGS: usize = 64;
/// Maximum combined byte length of arguments for one child MCP server.
pub const MAX_CHILD_MCP_ARGS_BYTES: usize = 16 * 1024;
/// Maximum allowed tool names per child MCP server.
pub const MAX_CHILD_MCP_TOOLS_PER_SERVER: usize = 64;
/// Maximum total allowed tool names across all child MCP servers on a profile.
pub const MAX_CHILD_MCP_TOTAL_TOOLS: usize = 128;
/// Maximum byte length of one allowed tool name.
pub const MAX_CHILD_MCP_TOOL_NAME_LEN: usize = 128;

/// One explicit stdio MCP server and exact tool allowlist granted to a delegated Claude child.
///
/// Selected tools are operator-trusted and may carry external side effects even in plan mode.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildMcpServer {
    /// Server name in Claude's `--mcp-config` (`mcpServers.<name>`) and `mcp__<name>__<tool>`.
    pub name: String,
    /// Absolute executable path for stdio transport.
    pub command: String,
    /// Optional command arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Exact tool names on this server callable by the child. Non-empty, no wildcards.
    pub tools: Vec<String>,
}

impl std::fmt::Debug for ChildMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildMcpServer")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args_len", &self.args.len())
            .field("tools", &self.tools)
            .finish()
    }
}

impl ChildMcpServer {
    /// Validates server identity, command/args bounds, and exact non-empty tool names.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || self.name.len() > MAX_CHILD_MCP_SERVER_NAME_LEN
            || !self.name.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || self.name.contains("__")
        {
            return Err(format!(
                "invalid child MCP server name {:?}: use 1–{} ASCII letters, digits, '-' or '_' without '__'",
                self.name, MAX_CHILD_MCP_SERVER_NAME_LEN
            ));
        }
        if self.name.to_ascii_lowercase().replace('-', "_") == SERVER_NAME {
            return Err(format!(
                "child MCP server name {:?} is reserved for {SERVER_NAME}",
                self.name
            ));
        }
        if self.command.is_empty()
            || !std::path::Path::new(&self.command).is_absolute()
            || self.command.len() > MAX_CHILD_MCP_COMMAND_LEN
            || self.command.trim() != self.command
            || self.command.contains('\0')
            || self.command.chars().any(char::is_control)
        {
            return Err(format!(
                "invalid command for child MCP server {:?}",
                self.name
            ));
        }
        if self.args.len() > MAX_CHILD_MCP_ARGS
            || self.args.iter().map(String::len).sum::<usize>() > MAX_CHILD_MCP_ARGS_BYTES
            || self
                .args
                .iter()
                .any(|a| a.contains('\0') || a.chars().any(char::is_control))
            || self.args.iter().any(|a| a.trim() == BRIDGE_ARG)
        {
            return Err(format!(
                "invalid arguments for child MCP server {:?}",
                self.name
            ));
        }
        if self.tools.is_empty() || self.tools.len() > MAX_CHILD_MCP_TOOLS_PER_SERVER {
            return Err(format!(
                "child MCP server {:?} must list 1–{} exact tool names",
                self.name, MAX_CHILD_MCP_TOOLS_PER_SERVER
            ));
        }
        let mut seen_tools = BTreeSet::new();
        for tool in &self.tools {
            if tool.is_empty()
                || tool.len() > MAX_CHILD_MCP_TOOL_NAME_LEN
                || !(tool.as_bytes()[0].is_ascii_alphanumeric() || tool.as_bytes()[0] == b'_')
                || !tool
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || tool.starts_with("mcp__")
            {
                return Err(format!(
                    "invalid tool name {:?} for child MCP server {:?}: exact names only, no wildcards",
                    tool, self.name
                ));
            }
            if !seen_tools.insert(tool.as_str()) {
                return Err(format!(
                    "duplicate tool name {:?} for child MCP server {:?}",
                    tool, self.name
                ));
            }
        }
        Ok(())
    }
}

/// Validated, immutable snapshot of extra child MCP server grants for one process generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildMcpGrants {
    servers: Vec<ChildMcpServer>,
}

impl ChildMcpGrants {
    /// Validates and freezes a list of child MCP server definitions.
    pub fn try_from_servers(servers: Vec<ChildMcpServer>) -> Result<Self, String> {
        if servers.len() > MAX_CHILD_MCP_SERVERS {
            return Err(format!(
                "at most {MAX_CHILD_MCP_SERVERS} child MCP servers may be configured"
            ));
        }
        let mut seen_names = BTreeSet::new();
        let mut total_tools = 0usize;
        for server in &servers {
            server.validate()?;
            let key = server.name.to_ascii_lowercase();
            if !seen_names.insert(key) {
                return Err(format!("duplicate child MCP server name {:?}", server.name));
            }
            total_tools = total_tools.saturating_add(server.tools.len());
            if total_tools > MAX_CHILD_MCP_TOTAL_TOOLS {
                return Err(format!(
                    "at most {MAX_CHILD_MCP_TOTAL_TOOLS} total child MCP tools may be configured"
                ));
            }
        }
        Ok(Self { servers })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn servers(&self) -> &[ChildMcpServer] {
        &self.servers
    }

    pub fn has_server(&self, name: &str) -> bool {
        self.servers.iter().any(|s| s.name == name)
    }

    /// Whether exact `(server, tool)` is explicitly allowed by this snapshot.
    pub fn allows_tool(&self, server: &str, tool: &str) -> bool {
        self.servers
            .iter()
            .find(|s| s.name == server)
            .is_some_and(|s| s.tools.iter().any(|t| t == tool))
    }

    pub fn clear(&mut self) {
        self.servers.clear();
    }
}

/// What one process generation of a principal needs to reach the supervisor. `Debug` hides
/// the token; there is deliberately no `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationBinding {
    /// The absolute installed path of `agent-terminal`.
    pub command: String,
    /// The absolute socket path.
    pub endpoint: String,
    pub instance: InstanceNonce,
    pub token: CapabilityToken,
}

/// The longest socket path a `sockaddr_un` holds (108 bytes with its NUL).
pub const MAX_ENDPOINT_BYTES: usize = 107;

/// What the Local runner needs to reach the supervisor itself: its own delegation client
/// connects with it, so no bridge process (and no environment) ever holds the token. It travels
/// only in the runner's startup frame (`Command::ConfigureLocal`, on stdin). `Debug` hides the
/// token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalBridge {
    /// The absolute socket path.
    pub endpoint: String,
    pub instance: InstanceNonce,
    pub token: CapabilityToken,
}

impl LocalBridge {
    pub fn from_binding(binding: &DelegationBinding) -> Self {
        Self {
            endpoint: binding.endpoint.clone(),
            instance: binding.instance.clone(),
            token: binding.token.clone(),
        }
    }

    /// The endpoint is an absolute path a Unix socket address can hold.
    pub fn validate(&self) -> Result<(), String> {
        if !self.endpoint.starts_with('/')
            || self.endpoint.len() > MAX_ENDPOINT_BYTES
            || self.endpoint.contains('\0')
        {
            return Err("The delegation endpoint is invalid.".to_owned());
        }
        Ok(())
    }
}

/// Codex's tool timeout for our server, above both bridge timeouts.
pub const CODEX_TOOL_TIMEOUT_SEC: u64 = 60;

/// Built-in Codex servers seen in every fixture; a child disables them.
pub const CODEX_BUILTIN_SERVERS: &[&str] = &["codex_apps"];

/// Claude child `--tools` for `plan`.
pub const CLAUDE_CHILD_TOOLS_PLAN: &[&str] = &["Read", "Glob", "Grep"];
/// Claude child `--tools` for `ask` and `accept_edits` (G-C1 decides `Bash`).
pub const CLAUDE_CHILD_TOOLS_WRITE: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "Edit",
    "Write",
    "NotebookEdit",
    "Bash",
];

/// The prefix of our tools as Claude names them.
const CLAUDE_OWN_TOOL_PREFIX: &str = "mcp__agent_terminal__";

impl DelegationBinding {
    /// `ENDPOINT`, `INSTANCE`, `CAP` (the secret) and `APP_PID`, in that order.
    pub fn env(&self, app_pid: u32) -> [(&'static str, String); 4] {
        [
            (ENV_ENDPOINT, self.endpoint.clone()),
            (ENV_INSTANCE, self.instance.as_str().to_owned()),
            (ENV_CAP, self.token.expose_secret().to_owned()),
            (ENV_APP_PID, app_pid.to_string()),
        ]
    }

    fn env_object(&self, app_pid: u32) -> Value {
        Value::Object(
            self.env(app_pid)
                .into_iter()
                .map(|(k, v)| (k.to_owned(), Value::String(v)))
                .collect(),
        )
    }

    /// The Claude `--mcp-config` overlay:
    /// `{"mcpServers":{"agent_terminal":{"type":"stdio","command":…,"args":["--cross-agent-mcp"],"env":{…}}}}`.
    /// The bridge reads `APP_PID` from this `env` (the provider's own env never has it), so
    /// it takes the app pid like [`DelegationBinding::env`].
    pub fn claude_overlay(&self, app_pid: u32) -> Value {
        self.claude_child_overlay(app_pid, &ChildMcpGrants::default())
    }

    /// The Claude child `--mcp-config` overlay: `agent_terminal` (with the bridge `env`) plus
    /// any validated explicit stdio servers in `grants`. Extra servers never receive `env` or
    /// the delegation capability token.
    pub fn claude_child_overlay(&self, app_pid: u32, grants: &ChildMcpGrants) -> Value {
        let mut servers = Map::new();
        servers.insert(
            SERVER_NAME.to_owned(),
            json!({
                "type": "stdio",
                "command": self.command,
                "args": [BRIDGE_ARG],
                "env": self.env_object(app_pid),
            }),
        );
        for extra in grants.servers() {
            if extra.name == SERVER_NAME {
                continue;
            }
            servers.insert(
                extra.name.clone(),
                json!({
                    "type": "stdio",
                    "command": extra.command,
                    "args": extra.args,
                }),
            );
        }
        Value::Object(Map::from_iter([(
            "mcpServers".to_owned(),
            Value::Object(servers),
        )]))
    }

    /// The Codex `thread/start` / `thread/resume` config: exactly the dotted keys
    /// `mcp_servers.agent_terminal.{command,args,env,default_tools_approval_mode,tool_timeout_sec}`.
    pub fn codex_thread_config(&self, app_pid: u32) -> Map<String, Value> {
        let key = |field: &str| format!("mcp_servers.{SERVER_NAME}.{field}");
        let mut m = Map::new();
        m.insert(key("command"), Value::String(self.command.clone()));
        m.insert(key("args"), json!([BRIDGE_ARG]));
        m.insert(key("env"), self.env_object(app_pid));
        m.insert(key("default_tools_approval_mode"), json!("approve"));
        m.insert(key("tool_timeout_sec"), json!(CODEX_TOOL_TIMEOUT_SEC));
        m
    }

    /// A child: the above plus `mcp_servers.<name>.enabled = false` for every name in
    /// `disable` (G-X1's first mechanism). Our own server is never disabled.
    pub fn codex_child_config(&self, app_pid: u32, disable: &[String]) -> Map<String, Value> {
        let mut m = self.codex_thread_config(app_pid);
        for name in disable.iter().filter(|n| n.as_str() != SERVER_NAME) {
            m.insert(format!("mcp_servers.{name}.enabled"), Value::Bool(false));
        }
        m
    }
}

fn child_tools(mode: TaskMode) -> &'static [&'static str] {
    match mode {
        TaskMode::Plan => CLAUDE_CHILD_TOOLS_PLAN,
        TaskMode::Ask | TaskMode::AcceptEdits => CLAUDE_CHILD_TOOLS_WRITE,
    }
}

/// `--restricted --strict-mcp-config --permission-prompt-tool stdio --permission-mode default
/// --mcp-config <overlay> --tools <list>`.
pub fn claude_child_args(mode: TaskMode, overlay_path: &str) -> Vec<String> {
    [
        "--restricted",
        "--strict-mcp-config",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "default",
        "--mcp-config",
        overlay_path,
        "--tools",
        &child_tools(mode).join(","),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// What a root agent is told about the other agents: appended to Claude's system prompt, added
/// to Codex's developer instructions (agy has neither; its tool descriptions carry a line of
/// it). An agent otherwise reaches for its built-in sub-agents first: it is never told the
/// others exist, and their tools may be deferred until searched for. Kept short: it is in every
/// turn's context.
pub const ROOT_PROMPT: &str = "agent-terminal: other agents (Claude, Codex, Antigravity/Gemini, \
     other profiles of yours) can be your sub-agents through the agent_terminal MCP tools; load \
     them if deferred. Before using your built-in sub-agents for independent work or a review, \
     call cross_agent_capabilities and start the best fit (model tags, then lowest usage.load) \
     with cross_agent_spawn. Results come back to you.";

/// A root Codex's developer instructions: the user's own (`developer_instructions` in its
/// effective config, which the thread's would otherwise replace), then [`ROOT_PROMPT`].
pub fn codex_root_instructions(own: Option<&str>) -> String {
    match own.map(str::trim).filter(|own| !own.is_empty()) {
        Some(own) => format!("{own}\n\n{ROOT_PROMPT}"),
        None => ROOT_PROMPT.to_owned(),
    }
}

/// A root: `--mcp-config <overlay>` (no `--strict-mcp-config`), and [`ROOT_PROMPT`].
pub fn claude_parent_args(overlay_path: &str) -> Vec<String> {
    vec![
        "--mcp-config".to_owned(),
        overlay_path.to_owned(),
        "--append-system-prompt".to_owned(),
        ROOT_PROMPT.to_owned(),
    ]
}

/// Claude asks `can_use_tool` for our server with `source: "dynamic"`: the host allows it and
/// the supervisor is the gate.
pub fn is_own_bridge_call(server_name: &str, source: Option<&str>) -> bool {
    server_name == SERVER_NAME && source == Some("dynamic")
}

/// Claude child read-back: `system/init.tools` is exactly the `--tools` list plus our own
/// `mcp__agent_terminal__*`, and `mcp_servers` exactly `[agent_terminal]`. `false` → stop
/// the child before its first turn.
pub fn claude_child_init_ok(
    mode: TaskMode,
    init_tools: &[String],
    init_mcp_servers: &[String],
) -> bool {
    claude_child_init_ok_with_grants(
        mode,
        init_tools,
        init_mcp_servers,
        &ChildMcpGrants::default(),
    )
}

/// Claude child read-back with optional explicit child MCP grants: `system/init.tools` must
/// contain every expected built-in for `mode` and no unexpected built-ins, plus only MCP tools
/// from `agent_terminal` or an explicitly granted server (an allowed server may advertise all of
/// its tools; exact callable tool filtering is enforced by host authority on `can_use_tool`).
/// `init_mcp_servers` must match `{agent_terminal} ∪ grants` exactly, with no missing, extra,
/// or duplicate entries.
pub fn claude_child_init_ok_with_grants(
    mode: TaskMode,
    init_tools: &[String],
    init_mcp_servers: &[String],
    grants: &ChildMcpGrants,
) -> bool {
    let expected = child_tools(mode);
    let tool_allowed = |t: &str| {
        if expected.contains(&t) {
            return true;
        }
        if let Some(rest) = t.strip_prefix(CLAUDE_OWN_TOOL_PREFIX) {
            return !rest.is_empty();
        }
        if let Some((server, name)) = t.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
            return !server.is_empty() && !name.is_empty() && grants.has_server(server);
        }
        false
    };
    let tools_ok = init_tools.iter().all(|t| tool_allowed(t))
        && expected.iter().all(|e| init_tools.iter().any(|t| t == e));

    let expected_server_count = 1 + grants.servers().len();
    let unique_servers: BTreeSet<&str> = init_mcp_servers.iter().map(String::as_str).collect();
    let servers_ok = init_mcp_servers.len() == expected_server_count
        && unique_servers.len() == expected_server_count
        && unique_servers.contains(SERVER_NAME)
        && grants
            .servers()
            .iter()
            .all(|s| unique_servers.contains(s.name.as_str()));

    tools_ok && servers_ok
}

/// Codex child read-back: the started servers are exactly `[agent_terminal]`.
pub fn codex_child_servers_ok(started: &[String]) -> bool {
    started.len() == 1 && started[0] == SERVER_NAME
}

/// `mcp-<app-pid>-<worker>-<gen>-<attempt>.json`. The attempt makes each launch's file its own:
/// a launch that never started does not advance the generation, and the file of an abandoned
/// attempt may still be on its way out when the next attempt writes.
pub fn overlay_file_name(
    app_pid: u32,
    worker: &WorkerId,
    generation: ProcessGeneration,
    attempt: u64,
) -> String {
    format!("mcp-{app_pid}-{worker}-{}-{attempt}.json", generation.0)
}

/// The app pid of an overlay file name (this form, or the earlier one without an attempt), or
/// `None` for any other name.
pub fn overlay_sweep_candidate(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("mcp-")?.strip_suffix(".json")?;
    let (pid, rest) = rest.split_once('-')?;
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !number(pid) {
        return None;
    }
    let (head, last) = rest.rsplit_once('-')?;
    if !number(last) {
        return None;
    }
    // `<worker>-<gen>-<attempt>`, else the earlier `<worker>-<gen>`.
    let worker = match head.rsplit_once('-') {
        Some((worker, generation)) if number(generation) && WorkerId::parse(worker).is_ok() => {
            generation.parse::<u64>().ok()?;
            worker
        }
        _ => head,
    };
    WorkerId::parse(worker).ok()?;
    last.parse::<u64>().ok()?;
    pid.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> DelegationBinding {
        DelegationBinding {
            command: "/usr/bin/agent-terminal".into(),
            endpoint: "/run/user/1000/agent-terminal/delegation-42.sock".into(),
            instance: InstanceNonce::from_bytes([0xab; 16]),
            token: CapabilityToken::from_random([5; 32]),
        }
    }

    fn secret() -> String {
        binding().token.expose_secret().to_owned()
    }

    /// Where in `v` the token appears, as JSON pointer-ish paths.
    fn token_paths(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::String(s) if s.contains(&secret()) => out.push(path.to_owned()),
            Value::Object(m) => {
                for (k, child) in m {
                    if k.contains(&secret()) {
                        out.push(format!("{path}/<key>"));
                    }
                    token_paths(child, &format!("{path}/{k}"), out);
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    token_paths(child, &format!("{path}/{i}"), out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn env_has_four_pairs_and_the_strip_list_five_names() {
        let env = binding().env(4242);
        assert_eq!(env.len(), 4);
        assert_eq!(env[2], (ENV_CAP, secret()));
        assert_eq!(env[3], (ENV_APP_PID, "4242".to_owned()));
        assert_eq!(crate::delegation::env::STRIP_ON_SPAWN.len(), 5);
        assert!(crate::delegation::env::STRIP_ON_SPAWN.contains(&ENV_APP_PID));
    }

    #[test]
    fn claude_overlay_puts_the_token_only_under_env() {
        let o = binding().claude_overlay(7);
        let server = &o["mcpServers"]["agent_terminal"];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["args"], json!(["--cross-agent-mcp"]));
        assert_eq!(server["env"][ENV_APP_PID], "7");
        let mut paths = Vec::new();
        token_paths(&o, "", &mut paths);
        assert_eq!(
            paths,
            vec![format!("/mcpServers/agent_terminal/env/{ENV_CAP}")]
        );
    }

    #[test]
    fn codex_config_has_exactly_the_documented_keys() {
        let c = binding().codex_thread_config(7);
        let keys: Vec<&str> = c.keys().map(String::as_str).collect();
        let mut expected = vec![
            "mcp_servers.agent_terminal.args",
            "mcp_servers.agent_terminal.command",
            "mcp_servers.agent_terminal.default_tools_approval_mode",
            "mcp_servers.agent_terminal.env",
            "mcp_servers.agent_terminal.tool_timeout_sec",
        ];
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        expected.sort_unstable();
        assert_eq!(sorted, expected);
        assert_eq!(
            c["mcp_servers.agent_terminal.default_tools_approval_mode"],
            "approve"
        );
        assert_eq!(c["mcp_servers.agent_terminal.tool_timeout_sec"], 60);
        let mut paths = Vec::new();
        token_paths(&Value::Object(c.clone()), "", &mut paths);
        assert_eq!(
            paths,
            vec![format!("/mcp_servers.agent_terminal.env/{ENV_CAP}")]
        );
        let child = binding().codex_child_config(7, &["x".into(), "codex_apps".into()]);
        assert_eq!(child.len(), c.len() + 2);
        assert_eq!(child["mcp_servers.x.enabled"], false);
        assert_eq!(child["mcp_servers.codex_apps.enabled"], false);
        let own = binding().codex_child_config(7, &["agent_terminal".into()]);
        assert_eq!(own.len(), c.len());
    }

    #[test]
    fn binding_debug_is_redacted() {
        let dbg = format!("{:?}", binding());
        assert!(!dbg.contains(&secret()[5..]));
        assert!(dbg.contains("atdc_****"));
        let local = LocalBridge::from_binding(&binding());
        let dbg = format!("{local:?}");
        assert!(!dbg.contains(&secret()[5..]));
        assert!(dbg.contains("atdc_****"));
    }

    #[test]
    fn a_local_bridge_round_trips_and_refuses_a_bad_endpoint() {
        let local = LocalBridge::from_binding(&binding());
        assert!(local.validate().is_ok());
        let json = serde_json::to_string(&local).expect("encode");
        assert_eq!(
            serde_json::from_str::<LocalBridge>(&json).expect("decode"),
            local
        );
        assert!(serde_json::from_str::<LocalBridge>(&json.replace("}", ",\"x\":1}")).is_err());
        for endpoint in ["relative.sock".to_owned(), format!("/{}", "a".repeat(107))] {
            let bad = LocalBridge {
                endpoint,
                ..local.clone()
            };
            assert!(bad.validate().is_err());
        }
    }

    #[test]
    fn claude_child_args_follow_the_mode() {
        let plan = claude_child_args(TaskMode::Plan, "/run/o.json");
        assert!(plan.contains(&"--restricted".to_owned()));
        assert!(plan.contains(&"--strict-mcp-config".to_owned()));
        let tools = plan.last().cloned().unwrap_or_default();
        assert_eq!(tools, "Read,Glob,Grep");
        for forbidden in ["Edit", "Write", "Bash"] {
            assert!(!tools.split(',').any(|t| t == forbidden));
        }
        let write = claude_child_args(TaskMode::AcceptEdits, "/run/o.json");
        assert!(write.last().is_some_and(|t| t.contains("Bash")));
        assert_eq!(
            claude_parent_args("/run/o.json"),
            vec![
                "--mcp-config".to_owned(),
                "/run/o.json".to_owned(),
                "--append-system-prompt".to_owned(),
                ROOT_PROMPT.to_owned()
            ]
        );
        // In every turn's context: it stays short, and names the tools it points to.
        assert!(ROOT_PROMPT.len() < 400, "{}", ROOT_PROMPT.len());
        for tool in ["cross_agent_capabilities", "cross_agent_spawn"] {
            assert!(ROOT_PROMPT.contains(tool), "{tool}");
        }
        // A Codex root keeps the user's own developer instructions, ahead of the note.
        assert_eq!(codex_root_instructions(None), ROOT_PROMPT);
        assert_eq!(codex_root_instructions(Some("  ")), ROOT_PROMPT);
        assert_eq!(
            codex_root_instructions(Some("Be terse.\n")),
            format!("Be terse.\n\n{ROOT_PROMPT}")
        );
        for args in [plan, write] {
            assert!(!args.iter().any(|a| a.contains("atdc_")));
            assert!(!args
                .iter()
                .any(|a| a.contains("Task") || a.contains("Agent")));
        }
        assert!(is_own_bridge_call("agent_terminal", Some("dynamic")));
        assert!(!is_own_bridge_call("agent_terminal", None));
        assert!(!is_own_bridge_call("other", Some("dynamic")));
    }

    #[test]
    fn child_read_backs_refuse_extras() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let mut tools = s(CLAUDE_CHILD_TOOLS_PLAN);
        tools.push("mcp__agent_terminal__cross_agent_read".into());
        let servers = s(&["agent_terminal"]);
        assert!(claude_child_init_ok(TaskMode::Plan, &tools, &servers));
        for extra in ["Task", "Agent", "Bash"] {
            let mut t = tools.clone();
            t.push(extra.into());
            assert!(
                !claude_child_init_ok(TaskMode::Plan, &t, &servers),
                "{extra}"
            );
        }
        assert!(!claude_child_init_ok(
            TaskMode::Plan,
            &tools,
            &s(&["agent_terminal", "other"])
        ));
        // `--tools` names Claude's built-in tools only; every one of our own tools, `show_image`
        // among them, reaches a child of any mode through the overlay's server.
        for mode in TaskMode::ALL {
            let mut own = s(child_tools(mode));
            own.push(format!(
                "{CLAUDE_OWN_TOOL_PREFIX}{}",
                super::super::ToolName::ShowImage.as_str()
            ));
            assert!(claude_child_init_ok(mode, &own, &servers), "{mode:?}");
            let args = claude_child_args(mode, "/run/o.json");
            assert!(!args.iter().any(|a| a.contains("mcp__")), "{mode:?}");
        }
        assert!(!claude_child_init_ok(TaskMode::Plan, &tools[1..], &servers));
        assert!(!claude_child_init_ok(TaskMode::Plan, &tools, &[]));
        assert!(codex_child_servers_ok(&s(&["agent_terminal"])));
        assert!(!codex_child_servers_ok(&s(&[
            "agent_terminal",
            "codex_apps"
        ])));
        assert!(!codex_child_servers_ok(&s(&["codex_apps"])));
        assert!(!codex_child_servers_ok(&[]));
    }

    #[test]
    fn overlay_names_round_trip() {
        let w = WorkerId::parse("w_abcdefgh").expect("valid");
        let name = overlay_file_name(4242, &w, ProcessGeneration(3), 17);
        assert_eq!(name, "mcp-4242-w_abcdefgh-3-17.json");
        assert_eq!(overlay_sweep_candidate(&name), Some(4242));
        // An earlier version's name, without the attempt, is still swept.
        assert_eq!(
            overlay_sweep_candidate("mcp-4242-w_abcdefgh-3.json"),
            Some(4242)
        );
        for foreign in [
            "mcp.json",
            "mcp-x-w_abcdefgh-3-1.json",
            "mcp-4242-w_abc-3-1.json",
            "mcp-4242-w_abc-3.json",
            "mcp-4242-w_abcdefgh-.json",
            "mcp-4242-w_abcdefgh-3-.json",
            "mcp-4242-w_abcdefgh-3-1.txt",
            "other-4242-w_abcdefgh-3-1.json",
        ] {
            assert_eq!(overlay_sweep_candidate(foreign), None, "{foreign}");
        }
    }

    fn sample_server(name: &str, tools: &[&str]) -> ChildMcpServer {
        ChildMcpServer {
            name: name.to_owned(),
            command: "/usr/local/bin/example-mcp".to_owned(),
            args: vec!["--stdio".to_owned()],
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    #[test]
    fn child_mcp_grants_validation_rejects_reserved_duplicates_wildcards_and_bounds() {
        assert!(ChildMcpGrants::try_from_servers(Vec::new())
            .expect("empty")
            .is_empty());

        let valid = ChildMcpGrants::try_from_servers(vec![sample_server(
            "discussion-board",
            &["list_topics", "post_message"],
        )])
        .expect("valid");
        assert!(valid.has_server("discussion-board"));
        assert!(valid.allows_tool("discussion-board", "list_topics"));
        assert!(valid.allows_tool("discussion-board", "post_message"));
        assert!(!valid.allows_tool("discussion-board", "delete_topic"));
        assert!(!valid.allows_tool("other-server", "list_topics"));

        for bad in [
            // Reserved server name (exact, uppercase, hyphenated)
            vec![sample_server("agent_terminal", &["t1"])],
            vec![sample_server("AGENT-TERMINAL", &["t1"])],
            vec![sample_server("agent-terminal", &["t1"])],
            // Duplicate server names (exact and case-insensitive)
            vec![
                sample_server("board", &["t1"]),
                sample_server("board", &["t2"]),
            ],
            vec![
                sample_server("Board", &["t1"]),
                sample_server("board", &["t2"]),
            ],
            // Invalid server names (empty, double underscore, bad chars, leading dash, too long)
            vec![sample_server("", &["t1"])],
            vec![sample_server("bad__name", &["t1"])],
            vec![sample_server("bad name", &["t1"])],
            vec![sample_server("-leading", &["t1"])],
            vec![sample_server(
                &"a".repeat(MAX_CHILD_MCP_SERVER_NAME_LEN + 1),
                &["t1"],
            )],
            // Empty / wildcard / duplicate / prefixed / invalid tool names
            vec![sample_server("board", &[])],
            vec![sample_server("board", &["*"])],
            vec![sample_server("board", &["board_*"])],
            vec![sample_server("board", &["t1", "t1"])],
            vec![sample_server("board", &["mcp__board__t1"])],
            vec![sample_server("board", &[""])],
            vec![sample_server("board", &["bad tool"])],
            // Invalid command / bridge arg / control chars
            vec![ChildMcpServer {
                command: "./example-mcp".into(),
                ..sample_server("board", &["t1"])
            }],
            vec![ChildMcpServer {
                command: "example-mcp".into(),
                ..sample_server("board", &["t1"])
            }],
            vec![ChildMcpServer {
                command: "".into(),
                ..sample_server("board", &["t1"])
            }],
            vec![ChildMcpServer {
                command: " /usr/bin/mcp ".into(),
                ..sample_server("board", &["t1"])
            }],
            vec![ChildMcpServer {
                command: "/usr/bin/mcp\0".into(),
                ..sample_server("board", &["t1"])
            }],
            vec![ChildMcpServer {
                args: vec![BRIDGE_ARG.into()],
                ..sample_server("board", &["t1"])
            }],
            vec![ChildMcpServer {
                args: vec!["bad\narg".into()],
                ..sample_server("board", &["t1"])
            }],
        ] {
            assert!(
                ChildMcpGrants::try_from_servers(bad.clone()).is_err(),
                "should reject {bad:?}"
            );
        }

        // Server count bound
        let too_many_servers: Vec<_> = (0..=MAX_CHILD_MCP_SERVERS)
            .map(|i| sample_server(&format!("srv-{i}"), &["t1"]))
            .collect();
        assert!(ChildMcpGrants::try_from_servers(too_many_servers).is_err());

        // Total tool count bound
        let tools_40: Vec<String> = (0..40).map(|i| format!("tool_{i}")).collect();
        let too_many_total: Vec<_> = (0..4)
            .map(|i| ChildMcpServer {
                name: format!("srv-{i}"),
                command: "/usr/local/bin/example-mcp".into(),
                args: Vec::new(),
                tools: tools_40.clone(),
            })
            .collect();
        assert!(ChildMcpGrants::try_from_servers(too_many_total).is_err());
    }

    #[test]
    fn claude_child_overlay_adds_configured_server_and_keeps_token_only_under_bridge_env() {
        let grants = ChildMcpGrants::try_from_servers(vec![sample_server(
            "discussion",
            &["list_topics", "post_message"],
        )])
        .expect("valid");
        let overlay = binding().claude_child_overlay(7, &grants);
        assert_eq!(overlay["mcpServers"]["discussion"]["type"], "stdio");
        assert_eq!(
            overlay["mcpServers"]["discussion"]["command"],
            "/usr/local/bin/example-mcp"
        );
        assert_eq!(
            overlay["mcpServers"]["discussion"]["args"],
            json!(["--stdio"])
        );
        assert!(overlay["mcpServers"]["discussion"].get("env").is_none());
        let mut paths = Vec::new();
        token_paths(&overlay, "", &mut paths);
        assert_eq!(
            paths,
            vec![format!("/mcpServers/agent_terminal/env/{ENV_CAP}")]
        );
    }

    #[test]
    fn claude_child_readback_with_grants_verifies_exact_server_set_and_builtins() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let grants =
            ChildMcpGrants::try_from_servers(vec![sample_server("discussion", &["list_topics"])])
                .expect("valid");

        // Configured server may advertise more tools than the allowlist; host authority filters.
        let mut tools = s(CLAUDE_CHILD_TOOLS_PLAN);
        tools.push("mcp__agent_terminal__cross_agent_read".into());
        tools.push("mcp__discussion__list_topics".into());
        tools.push("mcp__discussion__admin_delete".into());
        let servers = s(&["agent_terminal", "discussion"]);
        assert!(claude_child_init_ok_with_grants(
            TaskMode::Plan,
            &tools,
            &servers,
            &grants
        ));
        // Order of mcp_servers does not matter.
        assert!(claude_child_init_ok_with_grants(
            TaskMode::Plan,
            &tools,
            &s(&["discussion", "agent_terminal"]),
            &grants
        ));

        // Missing configured server is refused.
        assert!(!claude_child_init_ok_with_grants(
            TaskMode::Plan,
            &tools,
            &s(&["agent_terminal"]),
            &grants
        ));
        // Unexpected extra server is refused.
        assert!(!claude_child_init_ok_with_grants(
            TaskMode::Plan,
            &tools,
            &s(&["agent_terminal", "discussion", "rogue"]),
            &grants
        ));
        // Duplicate server entry is refused.
        assert!(!claude_child_init_ok_with_grants(
            TaskMode::Plan,
            &tools,
            &s(&["agent_terminal", "discussion", "discussion"]),
            &grants
        ));
        // Tool from an unconfigured server is refused.
        let mut rogue_tool = tools.clone();
        rogue_tool.push("mcp__rogue__ping".into());
        assert!(!claude_child_init_ok_with_grants(
            TaskMode::Plan,
            &rogue_tool,
            &servers,
            &grants
        ));
        // Unexpected builtin (e.g. ToolSearch or Bash in plan mode) is refused.
        for extra in ["ToolSearch", "Bash", "Task"] {
            let mut bad_tools = tools.clone();
            bad_tools.push(extra.into());
            assert!(
                !claude_child_init_ok_with_grants(TaskMode::Plan, &bad_tools, &servers, &grants),
                "{extra}"
            );
        }
    }
}
