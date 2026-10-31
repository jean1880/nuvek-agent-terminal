//! One-shot import of explicitly selected Codex stdio definitions. No discovery or spawning.
//!
//! Raw TOML and credential literals never become part of an import preview. The caller reads
//! only on an explicit import action, previews this result, then consumes selected rows.

use agent_core::local_mcp::{ApprovalPolicy, EnvironmentValue, McpServerConfig, McpToolPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const MAX_IMPORT_BYTES: usize = 1024 * 1024;
pub type ImportError = &'static str;

/// All fields are names or fixed explanations; never source values or parser diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportIssue {
    pub server_id: Option<String>,
    pub reason: &'static str,
    pub fields: Vec<String>,
}

pub struct ImportPreview {
    pub servers: Vec<ServerPreview>,
    pub issues: Vec<ImportIssue>,
}

/// A row contains a private launch candidate, but its public surface and Debug are names only.
/// Sensitive environment literals have already been discarded, not merely masked.
#[derive(Clone)]
pub struct ServerPreview {
    pub id: String,
    pub label: String,
    pub command_name: String,
    pub tools: Vec<String>,
    pub environment_names: Vec<String>,
    pub required_env_refs: Vec<String>,
    pub enabled: bool,
    pub reviewed: bool,
    candidate: McpServerConfig,
    mapping_required: BTreeSet<String>,
    known_env_names: BTreeSet<String>,
}

impl std::fmt::Debug for ServerPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPreview")
            .field("id", &self.id)
            .field("command_name", &self.command_name)
            .field("tools", &self.tools)
            .field("environment_names", &self.environment_names)
            .field("required_env_refs", &self.required_env_refs)
            .field("enabled", &self.enabled)
            .field("reviewed", &self.reviewed)
            .finish()
    }
}

impl std::fmt::Debug for ImportPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportPreview")
            .field("servers", &self.servers)
            .field("issues", &self.issues)
            .finish()
    }
}

impl ImportPreview {
    /// Ready means reviewed and convertible without additional credential mapping, not connected.
    pub fn ready_ids(&self) -> Vec<&str> {
        self.servers
            .iter()
            .filter(|s| s.is_ready())
            .map(|s| s.id.as_str())
            .collect()
    }

    /// Suitable for a preview widget or log: no environment values, arguments or source text.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        for server in &self.servers {
            let state = if !server.reviewed {
                "disabled; select tools after discovery"
            } else if !server.required_env_refs.is_empty() {
                "environment reference mapping required"
            } else if !server.enabled {
                "disabled in source configuration"
            } else {
                "ready for explicit import"
            };
            lines.push(format!(
                "{}: {}; {} tools; environment names: {}; required mappings: {}",
                server.id,
                state,
                server.tools.len(),
                server.environment_names.join(", "),
                server.required_env_refs.join(", ")
            ));
        }
        for issue in &self.issues {
            let name = issue.server_id.as_deref().unwrap_or("Configuration");
            let fields = if issue.fields.is_empty() {
                String::new()
            } else {
                format!(" Fields: {}.", issue.fields.join(", "))
            };
            lines.push(format!("{name}: {}{fields}", issue.reason));
        }
        if lines.is_empty() {
            "No importable MCP definitions were found.".into()
        } else {
            lines.join("\n")
        }
    }
}

impl ServerPreview {
    pub fn is_ready(&self) -> bool {
        self.reviewed && self.required_env_refs.is_empty()
    }

    /// Consume only a row the user selected. Each map key is this server's environment name;
    /// its value is a variable NAME in the Local profile's effective environment, not a secret.
    /// Existing references may be remapped too. Unreviewed rows always remain disabled.
    pub fn into_config(
        mut self,
        mappings: &BTreeMap<String, String>,
    ) -> Result<McpServerConfig, ImportError> {
        if self
            .mapping_required
            .iter()
            .any(|name| !mappings.contains_key(name))
        {
            return Err("Every required environment name needs a source variable mapping.");
        }
        for (name, from_env) in mappings {
            if !self.known_env_names.contains(name)
                || !safe_env_name(from_env)
                || control_variable(from_env)
            {
                return Err("Invalid imported environment reference mapping.");
            }
            self.candidate.env.insert(
                name.clone(),
                EnvironmentValue::Reference {
                    from_env: from_env.clone(),
                },
            );
        }
        // Public preview fields are presentation, never an authority to enable the candidate.
        self.candidate
            .validate()
            .map_err(|_| "Imported MCP definition did not pass configuration validation.")?;
        Ok(self.candidate)
    }
}

/// The caller supplies its environment paths; this function reads no process environment.
pub fn default_codex_path(
    codex_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, ImportError> {
    match codex_home.filter(|p| !p.as_os_str().is_empty()) {
        Some(path) => Ok(path.join("config.toml")),
        None => home
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join(".codex/config.toml"))
            .ok_or("Choose a Codex configuration file; its default location is unknown."),
    }
}

/// Blocking I/O: call off the UI thread. The selected path may be a projection symlink, but
/// regular-file and length checks apply to the opened descriptor. Nonblocking open rejects
/// FIFOs/devices without waiting, and a capped descriptor read catches subsequent file growth.
pub fn read_codex(path: &Path, fallback_cwd: &Path) -> Result<ImportPreview, ImportError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(path)
        .map_err(|_| "Could not open the selected Codex configuration.")?;
    parse_codex(&read_selected(file)?, fallback_cwd)
}

fn read_selected(file: File) -> Result<String, ImportError> {
    let metadata = file
        .metadata()
        .map_err(|_| "Could not inspect the selected Codex configuration.")?;
    if !metadata.is_file() || metadata.len() > MAX_IMPORT_BYTES as u64 {
        return Err("The selected Codex configuration must be a regular file of at most 1 MiB.");
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMPORT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the selected Codex configuration.")?;
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err("The selected Codex configuration grew beyond 1 MiB.");
    }
    String::from_utf8(bytes).map_err(|_| "The selected Codex configuration is not UTF-8.")
}

/// Pure parser. It never follows includes, expands variables, sources shells or reads secrets.
/// Only the top-level mcp_servers table is considered; profiles and other harness sections
/// are not merged. Malformed TOML errors deliberately discard the parser's source excerpts.
pub fn parse_codex(text: &str, fallback_cwd: &Path) -> Result<ImportPreview, ImportError> {
    if text.len() > MAX_IMPORT_BYTES {
        return Err("The selected Codex configuration exceeds 1 MiB.");
    }
    let root: toml::Table =
        toml::from_str(text).map_err(|_| "The selected Codex configuration is not valid TOML.")?;
    for key in [
        "include",
        "includes",
        "import",
        "imports",
        "extends",
        "config_file",
        "config_files",
        "source",
        "sources",
    ] {
        if root.contains_key(key) {
            return Err("Codex configuration indirection is not supported by this importer.");
        }
    }
    let mut preview = ImportPreview {
        servers: Vec::new(),
        issues: Vec::new(),
    };
    if root.contains_key("profiles") {
        preview.issues.push(issue(
            None,
            "Codex profile settings were not merged; only top-level MCP definitions are imported.",
            vec!["profiles".into()],
        ));
    }
    let Some(servers) = root.get("mcp_servers") else {
        return Ok(preview);
    };
    let servers = servers
        .as_table()
        .ok_or("Codex MCP definitions must be a table.")?;
    if servers.len() > 64 {
        return Err("The Codex configuration contains more than 64 MCP definitions.");
    }
    // Parser-local only: catch opaque credentials copied into otherwise innocent fields.
    // The one-MiB source cap bounds this scan; no literal is carried into the preview.
    let mut known_literals: Vec<&str> = servers
        .values()
        .filter_map(toml::Value::as_table)
        .filter_map(|server| server.get("env"))
        .filter_map(toml::Value::as_table)
        .flat_map(|env| env.iter())
        .filter_map(|(name, value)| {
            value.as_str().filter(|value| {
                agent_core::redact::is_sensitive_name(name)
                    || agent_core::redact::redact(value) != *value
            })
        })
        .flat_map(|value| std::iter::once(value).chain(value.split(['\r', '\n'])))
        .filter(|value| !value.is_empty())
        .collect();
    known_literals.sort_unstable();
    known_literals.dedup();
    let mut ids = BTreeMap::<String, usize>::new();
    for name in servers.keys() {
        if let Some(id) = server_id(name) {
            *ids.entry(id).or_default() += 1;
        }
    }
    for (name, value) in servers {
        let Some(id) = server_id(name).filter(|_| !contains_literal(name, &known_literals)) else {
            preview.issues.push(issue(
                None,
                "An MCP server name is invalid or unsafe to display.",
                Vec::new(),
            ));
            continue;
        };
        if name.eq_ignore_ascii_case("agent_terminal") || id == "agent-terminal" {
            preview.issues.push(issue(
                Some(id),
                "The managed agent-terminal bridge was skipped.",
                Vec::new(),
            ));
            continue;
        }
        if ids.get(&id).copied().unwrap_or_default() != 1 {
            preview.issues.push(issue(
                Some(id),
                "Server names collide after conversion to app IDs.",
                Vec::new(),
            ));
            continue;
        }
        let result = parse_server(name, &id, value, fallback_cwd, &known_literals);
        match result {
            Ok((server, ignored)) => {
                if !ignored.is_empty() {
                    preview.issues.push(issue(
                        Some(id),
                        "Approval exemptions and trust declarations were not imported.",
                        ignored,
                    ));
                }
                preview.servers.push(server);
            }
            Err((reason, fields)) => preview.issues.push(issue(Some(id), reason, fields)),
        }
    }
    Ok(preview)
}

fn issue(server_id: Option<String>, reason: &'static str, fields: Vec<String>) -> ImportIssue {
    ImportIssue {
        server_id,
        reason,
        fields,
    }
}

type ServerError = (&'static str, Vec<String>);

fn parse_server(
    label: &str,
    id: &str,
    value: &toml::Value,
    fallback_cwd: &Path,
    known_literals: &[&str],
) -> Result<(ServerPreview, Vec<String>), ServerError> {
    const INVALID: &str = "The MCP stdio definition has invalid fields or bounds.";
    let fields = value.as_table().ok_or((INVALID, Vec::new()))?;
    if fields.len() > 64 {
        return Err((INVALID, Vec::new()));
    }
    let mut ignored = Vec::new();
    let mut unsupported = Vec::new();
    for name in fields.keys() {
        match name.as_str() {
            "command"
            | "args"
            | "cwd"
            | "env"
            | "env_vars"
            | "enabled"
            | "required"
            | "enabled_tools"
            | "disabled_tools"
            | "startup_timeout_sec"
            | "tool_timeout_sec"
            | "transport"
            | "type" => {}
            "default_tools_approval_mode"
            | "tools_approval_mode"
            | "tool_approvals"
            | "auto_approve"
            | "approval_policy"
            | "trust"
            | "read_only" => {
                ignored.push(safe_field_name(name, known_literals));
            }
            _ => unsupported.push(safe_field_name(name, known_literals)),
        }
    }
    if !unsupported.is_empty() {
        return Err((
            "Unsupported MCP transport, field or configuration indirection.",
            unsupported,
        ));
    }
    for name in ["transport", "type"] {
        if fields
            .get(name)
            .is_some_and(|v| v.as_str() != Some("stdio"))
        {
            return Err((
                "Only explicit stdio MCP definitions can be imported.",
                vec![name.into()],
            ));
        }
    }
    let command = fields
        .get("command")
        .and_then(toml::Value::as_str)
        .ok_or((INVALID, Vec::new()))?;
    let args =
        strings(fields.get("args"), 128, 65536).map_err(|_| (INVALID, vec!["args".into()]))?;
    if command.is_empty()
        || command.len() > 4096
        || !direct_text(command)
        || contains_literal(command, known_literals)
        || args
            .iter()
            .any(|arg| !direct_text(arg) || contains_literal(arg, known_literals))
    {
        return Err((
            "Interpolation, shell syntax or unsafe launch text is not supported.",
            Vec::new(),
        ));
    }
    let basename = Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or((INVALID, Vec::new()))?;
    if matches!(basename, "agent-terminal" | "agent_terminal") {
        return Err(("The managed agent-terminal bridge was skipped.", Vec::new()));
    }
    if matches!(
        basename,
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "fish"
            | "ksh"
            | "csh"
            | "tcsh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "env"
            | "sudo"
            | "su"
            | "doas"
            | "ssh"
            | "wsl"
            | "cross-agent"
            | "command"
            | "nohup"
            | "timeout"
            | "setsid"
            | "flatpak-spawn"
    ) || args.iter().any(|arg| {
        matches!(
            arg.split('=').next().unwrap_or_default(),
            "--env-file"
                | "--env_file"
                | "--rcfile"
                | "--init-file"
                | "--config"
                | "--config-file"
                | "--config_file"
                | "--configuration"
                | "--settings"
                | "--settings-file"
                | "--profile"
                | "--profile-file"
                | "--include"
                | "--source"
        ) || Path::new(arg)
            .file_name()
            .and_then(|p| p.to_str())
            .is_some_and(|p| {
                matches!(
                    p,
                    "sh" | "bash"
                        | "zsh"
                        | "dash"
                        | "fish"
                        | "ksh"
                        | "csh"
                        | "tcsh"
                        | "pwsh"
                        | "powershell"
                        | "cmd"
                )
            })
    }) || interpreter_code(basename, &args)
    {
        return Err((
            "Shell, source-file and indirect launch wrappers are not imported.",
            Vec::new(),
        ));
    }
    if package_runner_shell(basename, &args) {
        return Err((
            "A package runner's --call (-c) runs a shell string; it is not imported.",
            Vec::new(),
        ));
    }
    if args.iter().any(|arg| {
        let name = arg
            .split('=')
            .next()
            .unwrap_or_default()
            .trim_start_matches('-');
        arg.starts_with('-')
            && (agent_core::redact::is_sensitive_name(name)
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "auth" | "authorization" | "bearer" | "cookie"
                ))
            || [
                "authorization:",
                "proxy-authorization:",
                "cookie:",
                "set-cookie:",
            ]
            .iter()
            .any(|prefix| arg.to_ascii_lowercase().starts_with(prefix))
    }) {
        return Err((
            "Credential arguments must be moved to explicit environment references before import.",
            Vec::new(),
        ));
    }
    let cwd = match fields.get("cwd") {
        Some(v) => v.as_str().ok_or((INVALID, vec!["cwd".into()]))?,
        None => fallback_cwd.to_str().ok_or((INVALID, vec!["cwd".into()]))?,
    };
    if cwd.is_empty()
        || cwd.len() > 4096
        || !direct_text(cwd)
        || contains_literal(cwd, known_literals)
    {
        return Err((
            "The MCP working directory must be explicit and contain no interpolation.",
            vec!["cwd".into()],
        ));
    }
    let allowlist = fields.contains_key("enabled_tools");
    let enabled_tools = strings(fields.get("enabled_tools"), 256, 65536)
        .map_err(|_| (INVALID, vec!["enabled_tools".into()]))?;
    let disabled_tools = strings(fields.get("disabled_tools"), 256, 65536)
        .map_err(|_| (INVALID, vec!["disabled_tools".into()]))?;
    let valid_tool = |name: &str| {
        !name.is_empty()
            && name.len() <= 256
            && display_safe(name)
            && !contains_literal(name, known_literals)
    };
    if enabled_tools
        .iter()
        .chain(&disabled_tools)
        .any(|name| !valid_tool(name))
    {
        return Err((
            INVALID,
            vec!["enabled_tools".into(), "disabled_tools".into()],
        ));
    }
    let mut seen = BTreeSet::new();
    if enabled_tools.iter().any(|name| !seen.insert(name)) {
        return Err((INVALID, vec!["enabled_tools".into()]));
    }
    let disabled: BTreeSet<_> = disabled_tools.into_iter().collect();
    let tools: Vec<String> = enabled_tools
        .into_iter()
        .filter(|name| !disabled.contains(name))
        .collect();
    let mut env = BTreeMap::new();
    let mut names = BTreeSet::new();
    let mut required_env_refs = Vec::new();
    if let Some(values) = fields.get("env") {
        let values = values
            .as_table()
            .filter(|v| v.len() <= 128)
            .ok_or((INVALID, vec!["env".into()]))?;
        for (name, value) in values {
            let value = value.as_str().ok_or((INVALID, vec!["env".into()]))?;
            if !safe_env_name(name)
                || contains_literal(name, known_literals)
                || control_variable(name)
                || source_variable(name)
                || value.len() > 65536
                || value.contains('\0')
                || interpolated(value)
            {
                return Err((
                    "Invalid or indirect MCP environment definition.",
                    vec!["env".into()],
                ));
            }
            names.insert(name.clone());
            if agent_core::redact::is_sensitive_name(name)
                || agent_core::redact::redact(value) != value
                || contains_literal(value, known_literals)
            {
                // The value is intentionally neither retained nor included in an error.
                required_env_refs.push(name.clone());
            } else {
                env.insert(name.clone(), EnvironmentValue::Literal(value.into()));
            }
        }
    }
    for name in strings(fields.get("env_vars"), 128, 32768)
        .map_err(|_| (INVALID, vec!["env_vars".into()]))?
    {
        if !safe_env_name(&name)
            || contains_literal(&name, known_literals)
            || control_variable(&name)
            || source_variable(&name)
            || !names.insert(name.clone())
        {
            return Err((
                "Invalid or duplicate MCP environment reference.",
                vec!["env_vars".into()],
            ));
        }
        env.insert(name.clone(), EnvironmentValue::Reference { from_env: name });
    }
    if names.len() > 128 {
        return Err((INVALID, vec!["env".into(), "env_vars".into()]));
    }
    let source_enabled =
        boolean(fields, "enabled", true).map_err(|_| (INVALID, vec!["enabled".into()]))?;
    let enabled = source_enabled && allowlist && !tools.is_empty();
    let candidate = McpServerConfig {
        id: id.into(),
        label: label.into(),
        enabled,
        required: boolean(fields, "required", false)
            .map_err(|_| (INVALID, vec!["required".into()]))?,
        command: command.into(),
        args,
        cwd: cwd.into(),
        env,
        tools: tools
            .iter()
            .map(|name| McpToolPolicy {
                name: name.clone(),
                read_only: false,
                approval: ApprovalPolicy::Inherit,
            })
            .collect(),
        approval: ApprovalPolicy::Ask,
        startup_timeout_secs: seconds(fields, "startup_timeout_sec", 15, 300)
            .map_err(|_| (INVALID, vec!["startup_timeout_sec".into()]))?,
        call_timeout_secs: seconds(fields, "tool_timeout_sec", 60, 600)
            .map_err(|_| (INVALID, vec!["tool_timeout_sec".into()]))?,
    };
    candidate.validate().map_err(|_| (INVALID, Vec::new()))?;
    Ok((
        ServerPreview {
            id: id.into(),
            label: label.into(),
            command_name: basename.into(),
            tools,
            environment_names: names.iter().cloned().collect(),
            mapping_required: required_env_refs.iter().cloned().collect(),
            known_env_names: names,
            required_env_refs,
            enabled,
            reviewed: allowlist,
            candidate,
        },
        ignored,
    ))
}

fn strings(
    value: Option<&toml::Value>,
    max_count: usize,
    max_bytes: usize,
) -> Result<Vec<String>, ()> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .filter(|v| v.len() <= max_count)
        .ok_or(())?;
    let mut bytes = 0usize;
    values
        .iter()
        .map(|v| {
            let text = v.as_str().ok_or(())?;
            bytes = bytes.saturating_add(text.len());
            if bytes > max_bytes {
                return Err(());
            }
            Ok(text.to_owned())
        })
        .collect()
}

fn boolean(fields: &toml::Table, key: &str, default: bool) -> Result<bool, ()> {
    fields
        .get(key)
        .map_or(Ok(default), |v| v.as_bool().ok_or(()))
}

fn seconds(fields: &toml::Table, key: &str, default: u64, max: u64) -> Result<u64, ()> {
    let value = fields.get(key).map_or(Ok(default), |v| {
        v.as_integer().and_then(|v| u64::try_from(v).ok()).ok_or(())
    })?;
    if (1..=max).contains(&value) {
        Ok(value)
    } else {
        Err(())
    }
}

fn server_id(name: &str) -> Option<String> {
    if name.is_empty()
        || name.len() > 64
        || !display_safe(name)
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(name.to_ascii_lowercase().replace(['_', '.'], "-"))
}

fn safe_env_name(name: &str) -> bool {
    name.len() <= 256 && agent_core::provider::valid_env_name(name) && display_safe(name)
}

fn control_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.contains("APPROVAL")
        || name.contains("DELEGATION")
        || name.contains("CROSS_AGENT")
        || name.starts_with("AGENT_TERMINAL_")
}

/// These variables load code or source startup files before the imported executable runs.
fn source_variable(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "BASH_ENV"
            | "ENV"
            | "ZDOTDIR"
            | "NODE_OPTIONS"
            | "NODE_PATH"
            | "PYTHONSTARTUP"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PERL5OPT"
            | "PERL5LIB"
            | "RUBYOPT"
            | "RUBYLIB"
            | "LD_PRELOAD"
            | "LD_AUDIT"
            | "LD_LIBRARY_PATH"
            | "DYLD_INSERT_LIBRARIES"
            | "DYLD_LIBRARY_PATH"
    )
}

fn interpolated(text: &str) -> bool {
    text.contains(['$', '`', '%']) || text.starts_with('~')
}

fn display_safe(text: &str) -> bool {
    !text.chars().any(char::is_control) && agent_core::redact::redact(text) == text
}

fn direct_text(text: &str) -> bool {
    display_safe(text) && !interpolated(text) && !text.contains([';', '|', '&', '<', '>'])
}

fn safe_field_name(name: &str, known_literals: &[&str]) -> String {
    if name.len() <= 64
        && display_safe(name)
        && !contains_literal(name, known_literals)
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        name.into()
    } else {
        "[unsupported field]".into()
    }
}

fn contains_literal(text: &str, literals: &[&str]) -> bool {
    literals.iter().any(|literal| text.contains(literal))
}

fn interpreter_code(command: &str, args: &[String]) -> bool {
    let python = command.starts_with("python") || matches!(command, "pypy" | "pypy3");
    let js = matches!(command, "node" | "nodejs" | "deno" | "bun");
    args.iter().any(|arg| {
        python && arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c')
            || js
                && (arg.starts_with("-e")
                    || arg.starts_with("-p")
                    || matches!(arg.as_str(), "eval" | "--eval" | "--print")
                    || arg.starts_with("--eval=")
                    || arg.starts_with("--print=")
                    || arg.starts_with("-r")
                    || ["--require", "--import", "--loader", "--experimental-loader"]
                        .iter()
                        .any(|flag| arg == flag || arg.starts_with(&format!("{flag}="))))
    })
}

/// `npx -c`/`--call` (and the same flag on its bun and pnpm equivalents) hands the rest of the
/// line to a shell: indirection, not a direct executable.
fn package_runner_shell(command: &str, args: &[String]) -> bool {
    matches!(command, "npx" | "bunx" | "pnpx")
        && args.iter().any(|arg| {
            arg == "--call"
                || arg.starts_with("--call=")
                || arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(text: &str) -> ImportPreview {
        parse_codex(text, Path::new("/work/project")).unwrap()
    }
    fn only(text: &str) -> ServerPreview {
        let mut preview = parse(text);
        assert!(preview.issues.is_empty(), "{:?}", preview.issues);
        assert_eq!(preview.servers.len(), 1);
        preview.servers.remove(0)
    }
    #[test]
    fn stdio_allowlist_subtracts_disabled_without_importing_authority() {
        let preview = parse(
            r#"
[mcp_servers.docs]
command = "docs-server"
args = ["--mode", "stdio"]
enabled_tools = ["read", "write", "search"]
disabled_tools = ["write"]
default_tools_approval_mode = "never"
read_only = true
startup_timeout_sec = 30
tool_timeout_sec = 90
"#,
        );
        assert_eq!(preview.ready_ids(), vec!["docs"]);
        assert_eq!(preview.issues.len(), 1);
        let config = preview
            .servers
            .into_iter()
            .next()
            .unwrap()
            .into_config(&BTreeMap::new())
            .unwrap();
        assert!(config.enabled);
        assert_eq!(config.cwd, "/work/project");
        assert_eq!(config.approval, ApprovalPolicy::Ask);
        assert_eq!(
            config
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "search"]
        );
        assert!(config
            .tools
            .iter()
            .all(|t| !t.read_only && t.approval == ApprovalPolicy::Inherit));
        assert_eq!(
            (config.startup_timeout_secs, config.call_timeout_secs),
            (30, 90)
        );
    }
    #[test]
    fn absent_empty_or_fully_excluded_allowlist_never_enables() {
        for suffix in [
            "",
            "disabled_tools = ['read']",
            "enabled_tools = []",
            "enabled_tools = ['read']\ndisabled_tools = ['read']",
            "enabled_tools = ['read']\nenabled = false",
        ] {
            let server = only(&format!(
                "[mcp_servers.docs]\ncommand='docs-server'\n{suffix}"
            ));
            let config = server.into_config(&BTreeMap::new()).unwrap();
            assert!(!config.enabled);
        }
        let server = only("[mcp_servers.docs]\ncommand='docs-server'");
        assert!(!server.reviewed);
        assert!(server.tools.is_empty());
    }
    #[test]
    fn credential_literals_are_discarded_and_reference_names_are_explicit() {
        let server = only(
            r#"
[mcp_servers.docs]
command='docs-server'
enabled_tools=['read']
env_vars=['EXISTING_TOKEN']
[mcp_servers.docs.env]
API_TOKEN='opaque-private-canary'
PASSWORD='second-private-canary'
MODE='safe-mode'
"#,
        );
        assert_eq!(server.required_env_refs, vec!["API_TOKEN", "PASSWORD"]);
        assert!(!server.is_ready());
        let shown = format!("{server:?}");
        for text in ["opaque-private", "second-private", "safe-mode"] {
            assert!(!shown.contains(text));
        }
        assert!(!server.candidate.env.contains_key("API_TOKEN"));
        let mappings = BTreeMap::from([
            ("API_TOKEN".into(), "PROFILE_TOKEN".into()),
            ("PASSWORD".into(), "PROFILE_PASSWORD".into()),
        ]);
        let config = server.into_config(&mappings).unwrap();
        assert_eq!(
            config.env["API_TOKEN"],
            EnvironmentValue::Reference {
                from_env: "PROFILE_TOKEN".into()
            }
        );
        assert_eq!(
            config.env["EXISTING_TOKEN"],
            EnvironmentValue::Reference {
                from_env: "EXISTING_TOKEN".into()
            }
        );
        assert_eq!(
            config.env["MODE"],
            EnvironmentValue::Literal("safe-mode".into())
        );
        assert!(!format!("{config:?}").contains("private-canary"));
    }
    #[test]
    fn known_opaque_credentials_cannot_survive_in_other_launch_or_preview_fields() {
        let prefix = "[mcp_servers.docs]\ncommand='docs-server'\n[mcp_servers.docs.env]\nTOKEN='opaque-private-canary'\n";
        for extra in [
            "[mcp_servers.other]\ncommand='server'\nargs=['opaque-private-canary']",
            "[mcp_servers.other]\ncommand='/programs/opaque-private-canary/server'",
            "[mcp_servers.other]\ncommand='server'\ncwd='/work/opaque-private-canary'",
            "[mcp_servers.other]\ncommand='server'\nenabled_tools=['opaque-private-canary']",
            "[mcp_servers.opaque-private-canary]\ncommand='server'",
            "[mcp_servers.other]\ncommand='server'\nopaque_private_canary='ignored'",
        ] {
            let preview = parse(&format!("{prefix}{extra}"));
            assert_eq!(preview.servers.len(), 1);
            assert!(!preview.summary().contains("opaque-private-canary"));
            assert!(!format!("{preview:?}").contains("opaque-private-canary"));
        }
        let server = only("[mcp_servers.docs]\ncommand='docs-server'\n[mcp_servers.docs.env]\nTOKEN='opaque-private-canary'\nMODE='opaque-private-canary'");
        assert_eq!(server.required_env_refs, vec!["MODE", "TOKEN"]);
        assert!(server.candidate.env.is_empty());
    }
    #[test]
    fn required_mappings_and_mapping_names_cannot_be_bypassed() {
        let text = "[mcp_servers.docs]\ncommand='docs-server'\n[mcp_servers.docs.env]\nTOKEN='opaque-private-canary'";
        for mappings in [
            BTreeMap::new(),
            BTreeMap::from([("TOKEN".into(), "${TOKEN}".into())]),
            BTreeMap::from([("TOKEN".into(), "AGENT_TERMINAL_APPROVAL_SOCKET".into())]),
            BTreeMap::from([("OTHER".into(), "TOKEN".into())]),
        ] {
            assert!(only(text).into_config(&mappings).is_err());
        }
        let mut edited_preview = only(text);
        edited_preview.required_env_refs.clear();
        edited_preview.environment_names.push("OTHER".into());
        assert!(edited_preview.into_config(&BTreeMap::new()).is_err());
    }
    #[test]
    fn unsupported_transports_interpolation_indirection_and_code_wrappers_are_reported() {
        for definition in [
            "url='https://example.com/mcp'", "command='server'\ntransport='sse'",
            "command='server'\nenv_file='private.env'", "command='${SERVER}'",
            "command='server'\nargs=['$TOKEN']", "command='server'\ncwd='~/project'",
            "command='bash'\nargs=['-lc', 'source private.env; server']",
            "command='/bin/sh'\nargs=['-c','server']", "command='env'\nargs=['server']",
            "command='python3'\nargs=['-c','print(1)']", "command='node'\nargs=['--eval=console.log(1)']",
            "command='server'\nargs=['--token', 'opaque-private-canary']",
            "command='server'\nargs=['--env-file','private.env']",
            "command='server'\nargs=['--env-file=private.env']",
            "command='server'\nargs=['--config','private.toml']",
            "command='server'\nargs=['--config-file=private.toml']",
            "command='uv'\nargs=['run','bash','-c','server']",
            "command='node'\nargs=['-econsole.log(1)']",
            "command='deno'\nargs=['eval','console.log(1)']",
            "command='python3'\nargs=['-Ic','print(1)']",
            "command='node'\nargs=['-r','private.js','server.js']",
            "command='node'\nargs=['--import=private.js','server.js']",
            "command='server'\nargs=['--auth','opaque-private-canary']",
            "command='server'\nargs=['Cookie: opaque-private-canary']",
            "command='server'\n[mcp_servers.docs.env]\nMODE='${MODE}'",
            "command='server'\n[mcp_servers.docs.env]\nNODE_OPTIONS='--require private.js'",
            "command='server'\nenv_vars=['LD_PRELOAD']",
            "command='server'\n[mcp_servers.docs.env]\nAGENT_TERMINAL_DELEGATION_CAP='opaque-private-canary'",
            "command='npx'\nargs=['-c','opaque-private-canary']",
            "command='npx'\nargs=['-y','--call','opaque-private-canary']",
            "command='/usr/bin/npx'\nargs=['--call=opaque-private-canary']",
            "command='npx'\nargs=['-yc','opaque-private-canary']",
            "command='bunx'\nargs=['-c','opaque-private-canary']",
            "command='pnpx'\nargs=['--call','opaque-private-canary']",
        ] {
            let preview = parse(&format!("[mcp_servers.docs]\n{definition}"));
            assert!(preview.servers.is_empty(), "{definition}");
            assert_eq!(preview.issues.len(), 1);
            assert!(!preview.summary().contains("opaque-private-canary"));
        }
    }
    #[test]
    fn package_runners_without_a_shell_flag_still_import() {
        let server = only(
            "[mcp_servers.docs]\ncommand='npx'\nargs=['-y','--package=example','example-server']\nenabled_tools=['read']",
        );
        assert_eq!(server.command_name, "npx");
        assert!(package_runner_shell("npx", &["-c".into()]));
        assert!(package_runner_shell("pnpx", &["--call=".into()]));
        assert!(!package_runner_shell("npx", &["--package=example".into()]));
        assert!(!package_runner_shell("server", &["-c".into()]));
    }
    #[test]
    fn managed_bridge_is_skipped_and_unrelated_definitions_survive() {
        let preview = parse("[mcp_servers.agent_terminal]\ncommand='bridge'\n[mcp_servers.docs]\ncommand='docs-server'\nenabled_tools=['read']");
        assert_eq!(preview.servers.len(), 1);
        assert_eq!(preview.servers[0].id, "docs");
        assert_eq!(preview.issues.len(), 1);
        let preview = parse("[mcp_servers.other]\ncommand='/usr/bin/agent-terminal'");
        assert!(preview.servers.is_empty());
    }
    #[test]
    fn names_are_stable_safe_and_collision_checked() {
        assert_eq!(
            only("[mcp_servers.Docs_Server]\ncommand='server'").id,
            "docs-server"
        );
        let preview = parse("[mcp_servers.docs_server]\ncommand='server'\n[mcp_servers.docs-server]\ncommand='server'");
        assert!(preview.servers.is_empty());
        assert_eq!(preview.issues.len(), 2);
        let preview = parse("[mcp_servers.'unsafe name']\ncommand='server'");
        assert!(preview.servers.is_empty());
        assert!(!preview.summary().contains("unsafe name"));
    }
    #[test]
    fn collection_limits_duplicate_references_and_wrong_types_fail_closed() {
        let mut too_many = String::new();
        for index in 0..65 {
            too_many.push_str(&format!("[mcp_servers.s{index}]\ncommand='server'\n"));
        }
        assert!(parse_codex(&too_many, Path::new("/work")).is_err());
        for definition in [
            "command='server'\nenv_vars=['TOKEN','TOKEN']",
            "command='server'\nenv_vars=['MODE']\n[mcp_servers.docs.env]\nMODE='ordinary'",
            "command='server'\nargs=[1]",
            "command='server'\nenabled_tools=[1]",
            "command='server'\ntransport=42",
            "command='server'\nrequired='yes'",
        ] {
            assert!(parse(&format!("[mcp_servers.docs]\n{definition}"))
                .servers
                .is_empty());
        }
    }
    #[test]
    fn malformed_and_invalid_fields_do_not_echo_source_values() {
        let error = parse_codex(
            "[mcp_servers.docs]\ncommand='private-canary",
            Path::new("/work"),
        )
        .unwrap_err();
        assert!(!error.contains("private-canary"));
        for definition in [
            "command=42",
            "command='server'\nargs='private-canary'",
            "command='server'\nstartup_timeout_sec=0",
            "command='server'\ntool_timeout_sec=601",
            "command='server'\nenabled_tools=['read','read']",
            "command='server'\nenabled='private-canary'",
            "command='server'\n[mcp_servers.docs.env]\nMODE=42",
        ] {
            let preview = parse(&format!("[mcp_servers.docs]\n{definition}"));
            assert!(preview.servers.is_empty());
            assert!(!preview.summary().contains("private-canary"));
        }
        assert!(parse_codex("imports=['private-canary']", Path::new("/work")).is_err());
    }
    #[test]
    fn unsupported_field_names_and_unmerged_profiles_are_visible_without_values() {
        let preview = parse("[mcp_servers.docs]\ncommand='server'\nurl='private-canary'\n[profiles.other.mcp_servers.hidden]\ncommand='not-imported'");
        assert!(preview.servers.is_empty());
        let shown = preview.summary();
        assert!(shown.contains("profiles"));
        assert!(shown.contains("url"));
        assert!(!shown.contains("private-canary"));
        assert!(!shown.contains("not-imported"));
        assert_eq!(preview.issues.len(), 2);
    }
    #[test]
    fn bounded_descriptor_reader_accepts_regular_files_and_rejects_other_targets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[mcp_servers.docs]\ncommand='docs-server'").unwrap();
        assert_eq!(
            read_codex(&path, Path::new("/work")).unwrap().servers.len(),
            1
        );
        let link = dir.path().join("projection.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_codex(&link, Path::new("/work")).is_ok());
        assert!(read_codex(dir.path(), Path::new("/work")).is_err());
        let fifo = dir.path().join("fifo");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(read_codex(&fifo, Path::new("/work")).is_err());
        std::fs::write(&path, vec![b'x'; MAX_IMPORT_BYTES + 1]).unwrap();
        assert!(read_codex(&path, Path::new("/work")).is_err());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(read_codex(&path, Path::new("/work")).is_err());
        assert!(parse_codex(&"x".repeat(MAX_IMPORT_BYTES + 1), Path::new("/work")).is_err());
    }
    #[test]
    fn default_path_uses_only_caller_supplied_locations() {
        assert_eq!(
            default_codex_path(Some(Path::new("/custom")), Some(Path::new("/user"))).unwrap(),
            Path::new("/custom/config.toml")
        );
        assert_eq!(
            default_codex_path(None, Some(Path::new("/user"))).unwrap(),
            Path::new("/user/.codex/config.toml")
        );
        assert!(default_codex_path(None, None).is_err());
    }
}
