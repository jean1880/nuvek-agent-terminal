//! The bridge's connection: `agent_kit::delegation_client`, configured from the environment the
//! app gave this process, after its lineage check.
use std::path::PathBuf;

use agent_core::delegation::env::{ENV_APP_PID, ENV_CAP, ENV_ENDPOINT, ENV_INSTANCE};
use agent_core::delegation::{CapabilityToken, InstanceNonce, ToolError};

pub(super) use agent_kit::delegation_client::{Client, Config};

#[cfg(test)]
pub(super) use agent_kit::delegation_client::test_support;

use super::unavailable;

/// The client's configuration from this process's environment. Refused (degraded) when it was
/// not launched by the app as a provider's bridge.
pub(super) fn from_environment() -> Result<Config, ToolError> {
    let read = |key: &str| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let endpoint = read(ENV_ENDPOINT)
        .map(PathBuf::from)
        .ok_or_else(|| unavailable("delegation is available only inside agent-terminal"))?;
    if !endpoint.is_absolute() || endpoint.as_os_str().len() > 107 {
        return Err(unavailable("the delegation endpoint is invalid"));
    }
    let app_pid = read(ENV_APP_PID).and_then(|pid| pid.parse::<u32>().ok());
    let parent = parent_pid();
    let grandparent = parent.and_then(|pid| read_ppid(pid).ok());
    if app_pid.is_none() || app_pid == Some(0) || grandparent != app_pid {
        // Claude/Codex process chains are unverified, so their peer checks are advisory.
        // Unknown launchers fail closed, so they cannot turn agy's check into advisory.
        if !parent.is_some_and(known_advisory_parent) {
            return Err(unavailable("the delegation bridge lineage check failed"));
        }
        eprintln!("agent-terminal: delegation bridge lineage check was advisory");
    }
    let instance = read(ENV_INSTANCE)
        .and_then(|value| InstanceNonce::parse(&value).ok())
        .ok_or_else(|| unavailable("the delegation instance is missing or invalid"))?;
    let cap = read(ENV_CAP)
        .and_then(|value| CapabilityToken::parse(&value).ok())
        .ok_or_else(|| unavailable("the delegation capability is missing or invalid"))?;
    Ok(Config::new(endpoint, instance, cap, std::process::id()))
}

fn parent_pid() -> Option<u32> {
    // SAFETY: getppid has no preconditions.
    u32::try_from(unsafe { libc::getppid() }).ok()
}

pub(super) fn parse_ppid(stat: &str) -> Option<u32> {
    // comm can contain spaces and parentheses. The state and ppid follow the last ')'.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn read_ppid(pid: u32) -> Result<u32, ()> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| ())?;
    parse_ppid(&stat).ok_or(())
}

fn known_advisory_parent(pid: u32) -> bool {
    let Ok(executable) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    if executable
        .file_name()
        .is_some_and(|name| matches!(name.to_str(), Some("claude" | "codex")))
    {
        return true;
    }
    if !executable
        .file_name()
        .is_some_and(|name| matches!(name.to_str(), Some("node" | "nodejs")))
    {
        return false;
    }
    // Claude can be a JS entrypoint run by node. Check the executable/script position only,
    // never prompt or profile arguments (which could contain the word 'claude').
    let Ok(argv) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    argv.split(|byte| *byte == 0).nth(1).is_some_and(|arg| {
        let Ok(arg) = std::str::from_utf8(arg) else {
            return false;
        };
        let path = std::path::Path::new(arg);
        path.file_name().is_some_and(|name| name == "cli.js")
            && path
                .components()
                .any(|part| part.as_os_str() == "claude-code")
    })
}
