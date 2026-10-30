//! Pure configuration and request policy for the Local runner.

use serde::{Deserialize, Serialize};

use crate::local_mcp::McpServerConfig;

/// Maximum stdin command frame, including its newline.
pub const MAX_SETUP_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstructionSettings {
    /// Additional files, in priority order. The native XDG file is always considered first.
    pub global_files: Vec<String>,
    pub discover_ancestors: bool,
    pub claude_fallback: bool,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for InstructionSettings {
    fn default() -> Self {
        Self {
            global_files: Vec::new(),
            discover_ancestors: true,
            claude_fallback: false,
            max_file_bytes: 32_768,
            max_total_bytes: 131_072,
        }
    }
}

impl InstructionSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_file_bytes == 0
            || self.max_file_bytes > self.max_total_bytes
            || self.max_total_bytes > 4 * 1024 * 1024
            || self.global_files.len() > 128
        {
            return Err("Instruction caps must satisfy 0 < file ≤ total ≤ 4194304 bytes (at most 128 global files).".into());
        }
        if self
            .global_files
            .iter()
            .any(|p| p.trim().is_empty() || p.contains('\0') || p.len() > 4096)
        {
            return Err("An instruction path is empty or contains a NUL.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextBudget {
    pub context_tokens: usize,
    pub response_tokens: usize,
    pub safety_tokens: usize,
    pub static_context_bytes: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            context_tokens: 32_768,
            response_tokens: 4096,
            safety_tokens: 4096,
            static_context_bytes: 49_152,
        }
    }
}

impl ContextBudget {
    pub fn validate(&self) -> Result<(), String> {
        if !(1024..=4_194_304).contains(&self.context_tokens)
            || self.response_tokens == 0
            || self.safety_tokens == 0
            || self.response_tokens.saturating_add(self.safety_tokens) >= self.context_tokens
            || !(1024..=4_194_304).contains(&self.static_context_bytes)
        {
            return Err("Invalid Local budget: context must exceed positive response and safety reserves; context/static caps are at most 4194304.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalDefaults {
    pub instructions: InstructionSettings,
    pub budget: ContextBudget,
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalProfileOptions {
    pub mcp_server_ids: Vec<String>,
    pub instructions: Option<InstructionSettings>,
    pub budget: Option<ContextBudget>,
}

impl LocalDefaults {
    pub fn validate(&self) -> Result<(), String> {
        self.instructions.validate()?;
        self.budget.validate()?;
        if self.mcp_servers.len() > 64 {
            return Err("At most 64 MCP servers can be configured.".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut labels = std::collections::BTreeSet::new();
        for server in &self.mcp_servers {
            server.validate()?;
            if !ids.insert(&server.id) || !labels.insert(&server.label) {
                return Err("MCP server IDs and labels must be unique.".into());
            }
        }
        Ok(())
    }

    pub fn launch(&self, options: &LocalProfileOptions) -> Result<LocalLaunchContext, String> {
        self.validate()?;
        if options.mcp_server_ids.len() > 16 {
            return Err("At most 16 MCP servers may be selected per session.".into());
        }
        let mut selected = std::collections::BTreeSet::new();
        for id in &options.mcp_server_ids {
            if !selected.insert(id) || !self.mcp_servers.iter().any(|s| &s.id == id) {
                return Err(format!(
                    "Duplicate or unresolved Local MCP server reference: {id}"
                ));
            }
        }
        let context = LocalLaunchContext {
            instructions: options
                .instructions
                .clone()
                .unwrap_or_else(|| self.instructions.clone()),
            budget: options
                .budget
                .clone()
                .unwrap_or_else(|| self.budget.clone()),
            // Selection order is user priority order, independent of definition order.
            mcp_servers: options
                .mcp_server_ids
                .iter()
                .filter_map(|id| self.mcp_servers.iter().find(|s| &s.id == id).cloned())
                .collect(),
        };
        context.validate()?;
        Ok(context)
    }
}

/// An immutable generation snapshot delivered on stdin, never on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalLaunchContext {
    pub instructions: InstructionSettings,
    pub budget: ContextBudget,
    pub mcp_servers: Vec<McpServerConfig>,
}

impl LocalLaunchContext {
    pub fn validate(&self) -> Result<(), String> {
        if self.mcp_servers.len() > 16
            || self
                .mcp_servers
                .iter()
                .map(|s| s.tools.len())
                .sum::<usize>()
                > 1024
        {
            return Err("Local sessions allow at most 16 MCP servers and 1024 tools.".into());
        }
        if serde_json::to_vec(self)
            .map_err(|_| "Cannot encode Local setup.")?
            .len()
            > MAX_SETUP_BYTES - 1024
        {
            return Err("Local setup exceeds the 2 MiB startup frame limit.".into());
        }
        LocalDefaults {
            instructions: self.instructions.clone(),
            budget: self.budget.clone(),
            mcp_servers: self.mcp_servers.clone(),
        }
        .validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_overrides_validate() {
        let defaults = LocalDefaults::default();
        let encoded = serde_json::to_string(&defaults).unwrap();
        assert_eq!(
            serde_json::from_str::<LocalDefaults>(&encoded).unwrap(),
            defaults
        );
        assert_eq!(defaults.budget.context_tokens, 32768);
        assert!(defaults
            .launch(&LocalProfileOptions {
                mcp_server_ids: vec!["missing".into()],
                ..Default::default()
            })
            .is_err());
        let mut options = LocalProfileOptions {
            budget: Some(ContextBudget {
                context_tokens: 65536,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            defaults.launch(&options).unwrap().budget.context_tokens,
            65536
        );
        options.budget.as_mut().unwrap().response_tokens = usize::MAX;
        assert!(defaults.launch(&options).is_err());
    }
}
