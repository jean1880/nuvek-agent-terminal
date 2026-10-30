//! Configuration and authority for optional Local MCP tools. No transport or spawning.

use crate::adapter::Mode;
use crate::local_agent::{Gate, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    Inherit,
    Ask,
    Allow,
    Deny,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum EnvironmentValue {
    Reference { from_env: String },
    Literal(String),
}

impl std::fmt::Debug for EnvironmentValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reference { from_env } => f
                .debug_struct("Reference")
                .field("from_env", from_env)
                .finish(),
            Self::Literal(_) => f.write_str("Literal(<redacted>)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolPolicy {
    pub name: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub approval: ApprovalPolicy,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    #[serde(default)]
    pub required: bool,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, EnvironmentValue>,
    pub tools: Vec<McpToolPolicy>,
    pub approval: ApprovalPolicy,
    pub startup_timeout_secs: u64,
    pub call_timeout_secs: u64,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=64).contains(&self.id.len())
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(
                "MCP server ID must contain 1–64 lowercase letters, digits or hyphens.".into(),
            );
        }
        if self.label.trim().is_empty()
            || self.label.len() > 256
            || self.command.len() > 4096
            || self.cwd.len() > 4096
            || self.command.trim().is_empty()
            || self.cwd.trim().is_empty()
            || self.command.contains('\0')
            || self.cwd.contains('\0')
            || self.args.iter().any(|a| a.contains('\0'))
            || self.args.len() > 128
            || self.args.iter().map(String::len).sum::<usize>() > 65536
            || self.env.len() > 128
            || self.tools.len() > 256
            || !(1..=300).contains(&self.startup_timeout_secs)
            || !(1..=600).contains(&self.call_timeout_secs)
            || self.approval == ApprovalPolicy::Inherit
        {
            return Err(format!("Invalid MCP definition or bounds for {}.", self.id));
        }
        for (name, value) in &self.env {
            if name.len() > 256 || !crate::provider::valid_env_name(name) {
                return Err(format!("Invalid MCP environment name: {name}"));
            }
            match value {
                EnvironmentValue::Reference { from_env }
                    if from_env.len() > 256 || !crate::provider::valid_env_name(from_env) =>
                {
                    return Err(format!("Invalid environment reference for {name}"))
                }
                EnvironmentValue::Literal(value) => {
                    let key = name.to_ascii_lowercase();
                    if value.contains('\0')
                        || value.len() > 65536
                        || [
                            "token",
                            "secret",
                            "password",
                            "credential",
                            "api_key",
                            "apikey",
                        ]
                        .iter()
                        .any(|part| key.contains(part))
                        || crate::redact::redact(value) != *value
                    {
                        return Err(format!("MCP environment {name} must use a reference for credentials or a bounded non-secret literal."));
                    }
                }
                _ => {}
            }
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if tool.name.is_empty()
                || tool.name.len() > 256
                || tool.name.chars().any(char::is_control)
                || !names.insert(&tool.name)
            {
                return Err(format!("Invalid or duplicate tool name on {}.", self.id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolIdentity {
    Builtin { name: String },
    Mcp { server_id: String, name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedTool {
    pub identity: ToolIdentity,
    pub alias: String,
    pub schema: Value,
    pub server_revision: String,
    pub schema_revision: String,
}

impl ResolvedTool {
    pub fn builtin(tool: Tool, schema: Value) -> Self {
        Self {
            identity: ToolIdentity::Builtin {
                name: tool.name().into(),
            },
            alias: tool.name().into(),
            schema,
            server_revision: String::new(),
            schema_revision: String::new(),
        }
    }
}

/// A grant is valid for exactly the call that was shown, including its generation and schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalBinding {
    pub request_id: String,
    pub generation: String,
    pub server_revision: String,
    pub schema_revision: String,
    pub identity: ToolIdentity,
    pub arguments: Value,
}

/// Eligibility has already established selection, advertisement and valid arguments.
pub fn gate(server: &McpServerConfig, tool: &McpToolPolicy, mode: Mode, eligible: bool) -> Gate {
    if !eligible
        || !server.enabled
        || !server.tools.contains(tool)
        || server.approval == ApprovalPolicy::Deny
        || tool.approval == ApprovalPolicy::Deny
    {
        return Gate::Refuse("The MCP tool is disabled, disallowed or unavailable this turn.");
    }
    if mode == Mode::Plan && !tool.read_only {
        return Gate::Refuse("Plan requires an explicitly read-only MCP tool.");
    }
    let policy = match tool.approval {
        ApprovalPolicy::Inherit => server.approval,
        // A tool may narrow its server's policy, never widen it.
        ApprovalPolicy::Allow if server.approval != ApprovalPolicy::Allow => ApprovalPolicy::Ask,
        other => other,
    };
    if mode == Mode::Bypass || policy == ApprovalPolicy::Allow {
        Gate::Run
    } else {
        Gate::Ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn server() -> McpServerConfig {
        serde_json::from_value(serde_json::json!({"id":"docs","label":"Docs","enabled":true,"command":"server","args":[],"cwd":"$WORKSPACE","env":{},"tools":[{"name":"read","read_only":true}],"approval":"ask","startup_timeout_secs":15,"call_timeout_secs":60})).unwrap()
    }
    #[test]
    fn configuration_round_trip_and_names_only_debug() {
        let mut s = server();
        s.env.insert(
            "SERVICE_TOKEN".into(),
            EnvironmentValue::Reference {
                from_env: "SERVICE_TOKEN".into(),
            },
        );
        s.env.insert(
            "LANG".into(),
            EnvironmentValue::Literal("private-canary".into()),
        );
        s.validate().unwrap();
        assert!(!format!("{s:?}").contains("private-canary"));
        assert_eq!(
            serde_json::from_str::<McpServerConfig>(&serde_json::to_string(&s).unwrap()).unwrap(),
            s
        );
        s.env.insert(
            "SERVICE_TOKEN".into(),
            EnvironmentValue::Literal("private-canary".into()),
        );
        assert!(s.validate().is_err());
    }
    #[test]
    fn every_mode_preserves_eligibility_and_deny() {
        let mut s = server();
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits, Mode::Bypass] {
            assert!(matches!(
                gate(&s, &s.tools[0], mode, false),
                Gate::Refuse(_)
            ));
            assert_eq!(
                gate(&s, &s.tools[0], mode, true),
                if mode == Mode::Bypass {
                    Gate::Run
                } else {
                    Gate::Ask
                }
            );
            let mut write = s.tools[0].clone();
            write.read_only = false;
            s.tools[0] = write.clone();
            assert_eq!(
                matches!(gate(&s, &write, mode, true), Gate::Refuse(_)),
                mode == Mode::Plan
            );
            s.tools[0].read_only = true;
        }
        s.approval = ApprovalPolicy::Deny;
        assert!(matches!(
            gate(&s, &s.tools[0], Mode::Bypass, true),
            Gate::Refuse(_)
        ));
    }
}
