use super::transport::ClientTransport;
use crate::mcp_guardian::{Launch, GUARDIAN_FLAG, LIFETIME_FD};
use agent_core::local_mcp::{EnvironmentValue, McpServerConfig};
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TAIL_BYTES: usize = 64 * 1024;
const STDERR_LINE_BYTES: usize = 128 * 1024;

// No Debug implementations for resolved credentials or raw process data.
#[derive(Clone, Default)]
pub(super) struct Scrubber(Vec<String>);
impl Scrubber {
    pub fn value(&self, value: &mut serde_json::Value) -> Result<(), &'static str> {
        match value {
            serde_json::Value::String(text) => *text = self.scrub(text),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.value(value)?;
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    if self.scrub(key) != *key {
                        return Err("MCP JSON contains a sensitive object key.");
                    }
                    self.value(value)?;
                    if let serde_json::Value::String(text) = value {
                        *text = agent_core::redact::redact_keyed(key, text);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn scrub(&self, text: &str) -> String {
        let mut safe = String::new();
        let mut at = 0;
        while at < text.len() {
            if let Some(secret) = self
                .0
                .iter()
                .find(|secret| !secret.is_empty() && text[at..].starts_with(secret.as_str()))
            {
                safe.push_str("[REDACTED]");
                at += secret.len();
            } else if let Some(ch) = text[at..].chars().next() {
                safe.push(ch);
                at += ch.len_utf8();
            }
        }
        agent_core::redact::redact(&safe)
    }
}

fn control_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.contains("APPROVAL")
        || name.contains("DELEGATION")
        || name.contains("CROSS_AGENT")
        || name.starts_with("AGENT_TERMINAL_")
}

fn environment(
    config: &McpServerConfig,
    effective: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, String>, Scrubber), String> {
    let mut env = BTreeMap::new();
    for name in [
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "SYSTEMROOT",
    ] {
        if let Some(value) = effective.get(name) {
            env.insert(name.into(), value.clone());
        }
    }
    let mut secrets = Vec::new();
    for (name, value) in &config.env {
        if control_variable(name) {
            return Err("MCP environment contains an application control variable.".into());
        }
        let value = match value {
            EnvironmentValue::Literal(value) => value.clone(),
            EnvironmentValue::Reference { from_env } => {
                if control_variable(from_env) {
                    return Err(
                        "MCP environment references an application control variable.".into(),
                    );
                }
                let value = effective
                    .get(from_env)
                    .filter(|s| !s.is_empty())
                    .ok_or("MCP environment reference is unset.")?
                    .clone();
                secrets.push(value.clone());
                // A multiline credential must also be hidden when stderr is drained by
                // complete bounded lines. Mask every nonempty fragment as well as the whole.
                secrets.extend(
                    value
                        .split(['\r', '\n'])
                        .filter(|part| !part.is_empty())
                        .map(str::to_owned),
                );
                value
            }
        };
        if value.len() > 65536 || value.contains('\0') {
            return Err("MCP environment value exceeds its bound or contains NUL.".into());
        }
        env.insert(name.clone(), value);
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    Ok((env, Scrubber(secrets)))
}

pub(super) struct Process {
    child: tokio::process::Child,
    lifetime: Option<File>,
    drain: tokio::task::JoinHandle<()>,
    pub tail: Arc<Mutex<String>>,
    pub scrubber: Scrubber,
}
impl Drop for Process {
    fn drop(&mut self) {
        // EOF survives runner crashes and task cancellation; never kill the guardian first.
        self.lifetime.take();
        self.drain.abort();
    }
}

impl Process {
    pub async fn spawn(
        config: &McpServerConfig,
        effective: &BTreeMap<String, String>,
        executable: &std::path::Path,
        deadline: tokio::time::Instant,
        cancel: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<
        (
            Self,
            ClientTransport<tokio::process::ChildStdout, tokio::process::ChildStdin>,
        ),
        String,
    > {
        if *cancel.borrow() {
            return Err("MCP startup cancelled.".into());
        }
        let (env, scrubber) = environment(config, effective)?;
        let setup = serde_json::to_vec(&Launch {
            command: config.command.clone(),
            args: config.args.clone(),
            cwd: config.cwd.clone(),
            env,
        })
        .map_err(|_| "Could not encode MCP launch configuration.")?;
        if setup.len() >= 2 * 1024 * 1024 {
            return Err("MCP launch configuration exceeds its frame bound.".into());
        }
        let mut descriptors = [-1; 2];
        if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err("Could not create MCP lifetime pipe.".into());
        }
        let read_end = unsafe { File::from_raw_fd(descriptors[0]) };
        let write_end = unsafe { File::from_raw_fd(descriptors[1]) };
        let lifetime_fd = read_end.as_raw_fd();
        let mut command = tokio::process::Command::new(executable);
        command
            .arg(GUARDIAN_FLAG)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        // SAFETY: only async-signal-safe descriptor operations run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(lifetime_fd, LIFETIME_FD) < 0
                    || libc::fcntl(LIFETIME_FD, libc::F_SETFD, 0) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|_| "Could not start MCP guardian.")?;
        drop(read_end);
        let mut stdin = child
            .stdin
            .take()
            .ok_or("MCP guardian has no input pipe.")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("MCP guardian has no output pipe.")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("MCP guardian has no diagnostic pipe.")?;
        let tail = Arc::new(Mutex::new(String::new()));
        let drain = tokio::spawn(drain_stderr(stderr, tail.clone(), scrubber.clone()));
        let mut process = Self {
            child,
            lifetime: Some(write_end),
            drain,
            tail,
            scrubber,
        };
        let written = tokio::time::timeout_at(
            deadline.min(tokio::time::Instant::now() + Duration::from_secs(5)),
            async {
                stdin.write_all(&setup).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await
            },
        );
        let written = tokio::select! {
            _ = super::client::cancelled(cancel) => None,
            written = written => Some(written),
        };
        if !matches!(written, Some(Ok(Ok(())))) {
            drop(stdin);
            process.shutdown().await;
            return Err("MCP guardian setup could not be written in time.".into());
        }
        Ok((process, ClientTransport::new(stdout, stdin)))
    }

    pub async fn shutdown(&mut self) {
        self.lifetime.take();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
        if tokio::time::timeout(Duration::from_millis(200), &mut self.drain)
            .await
            .is_err()
        {
            self.drain.abort();
        }
    }

    pub fn diagnostic(&self, reason: &str) -> String {
        let tail = self.tail.lock().map(|s| s.clone()).unwrap_or_default();
        if tail.is_empty() {
            reason.into()
        } else {
            format!("{reason}\n{tail}")
        }
    }
}

fn retain_tail(tail: &Arc<Mutex<String>>, text: &str, scrubber: &Scrubber) {
    // Complete bounded lines are scrubbed before retention. An overlong line is discarded
    // whole, so a boundary cannot expose a credential fragment.
    let text = scrubber.scrub(text);
    if let Ok(mut tail) = tail.lock() {
        tail.push_str(&text);
        if tail.len() > TAIL_BYTES {
            let mut start = tail.len() - TAIL_BYTES;
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            tail.drain(..start);
        }
    }
}

async fn drain_stderr(
    mut stderr: impl tokio::io::AsyncRead + Unpin,
    tail: Arc<Mutex<String>>,
    scrubber: Scrubber,
) {
    let mut chunk = [0; 4096];
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let count = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        for byte in &chunk[..count] {
            if !overflow {
                if line.len() == STDERR_LINE_BYTES {
                    line.clear();
                    overflow = true;
                } else {
                    line.push(*byte);
                }
            }
            if *byte == b'\n' {
                if overflow {
                    retain_tail(&tail, "[MCP stderr line exceeded its bound]\n", &scrubber);
                } else {
                    retain_tail(&tail, &String::from_utf8_lossy(&line), &scrubber);
                }
                overflow = false;
                line.clear();
            }
        }
    }
    if overflow {
        retain_tail(&tail, "[MCP stderr line exceeded its bound]\n", &scrubber);
    } else if !line.is_empty() {
        retain_tail(&tail, &String::from_utf8_lossy(&line), &scrubber);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn config() -> McpServerConfig {
        serde_json::from_value(json!({
            "id":"test","label":"Test","enabled":true,"command":"fixture","args":[],
            "cwd":"/","env":{},"tools":[],"approval":"ask","startup_timeout_secs":15,"call_timeout_secs":60
        })).unwrap()
    }
    #[test]
    fn environment_only_exports_basics_and_explicit_values_and_rejects_controls() {
        let mut config = config();
        let effective = BTreeMap::from([
            ("PATH".into(), "/bin".into()),
            ("PRIVATE".into(), "opaque-value".into()),
            ("IGNORED".into(), "private".into()),
        ]);
        config.env.insert(
            "KEY".into(),
            EnvironmentValue::Reference {
                from_env: "PRIVATE".into(),
            },
        );
        let (env, _) = environment(&config, &effective).unwrap();
        assert_eq!(env.len(), 2);
        assert!(!env.contains_key("PRIVATE"));
        config.env.insert(
            "KEY".into(),
            EnvironmentValue::Reference {
                from_env: "MISSING".into(),
            },
        );
        assert!(environment(&config, &effective).is_err());
        config.env.insert(
            "KEY".into(),
            EnvironmentValue::Reference {
                from_env: "AGENT_TERMINAL_DELEGATION_CAP".into(),
            },
        );
        assert!(environment(&config, &effective).is_err());
        config.env.clear();
        config.env.insert(
            "AGENT_TERMINAL_APPROVAL_SOCKET".into(),
            EnvironmentValue::Literal("path".into()),
        );
        assert!(environment(&config, &effective).is_err());
    }
    #[test]
    fn stderr_is_drained_bounded_and_scrubbed_across_chunk_and_unicode_boundaries() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let secret = "opaque-雪-\"quoted\"-credential";
                let scrubber = Scrubber(vec![secret.into()]);
                let tail = Arc::new(Mutex::new(String::new()));
                let (mut writer, reader) = tokio::io::duplex(64);
                let task = tokio::spawn(drain_stderr(reader, tail.clone(), scrubber.clone()));
                writer
                    .write_all("雪".repeat(50_000).as_bytes())
                    .await
                    .unwrap();
                writer.write_all(b"\n").await.unwrap();
                for byte in secret.as_bytes() {
                    writer.write_all(&[*byte]).await.unwrap();
                }
                writer.write_all(b"\n").await.unwrap();
                drop(writer);
                task.await.unwrap();
                let tail = tail.lock().unwrap();
                assert!(tail.len() <= TAIL_BYTES);
                assert!(tail.contains("exceeded its bound"));
                assert!(tail.contains("[REDACTED]"));
                assert!(!tail.contains(secret));
                drop(tail);
                let mut value =
                    json!({"nested":[{"text":secret,"api_key":"unpatterned-private-12345"}]});
                scrubber.value(&mut value).unwrap();
                let text = value.to_string();
                assert!(!text.contains("opaque"));
                assert!(!text.contains("unpatterned-private"));
                let mut keyed = json!({secret: "safe"});
                assert!(scrubber.value(&mut keyed).is_err());
            });
    }

    #[test]
    fn exact_masks_never_rescan_inserted_markers() {
        let scrubber = Scrubber("[]ACDERT".chars().map(|ch| ch.to_string()).collect());
        let input = "A".repeat(100);
        assert!(scrubber.scrub(&input).len() <= 100 * "[REDACTED]".len());
    }

    #[test]
    fn multiline_references_mask_each_stderr_fragment() {
        let mut config = config();
        config.env.insert(
            "KEY".into(),
            EnvironmentValue::Reference {
                from_env: "SOURCE".into(),
            },
        );
        let effective = BTreeMap::from([(
            "SOURCE".into(),
            "first-private-line\nsecond-private-line".into(),
        )]);
        let (_, scrubber) = environment(&config, &effective).unwrap();
        assert!(!scrubber
            .scrub("first-private-line\n")
            .contains("first-private-line"));
        assert!(!scrubber
            .scrub("second-private-line\n")
            .contains("second-private-line"));
    }
}
