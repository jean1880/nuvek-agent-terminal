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
pub struct Scrubber(Vec<String>);
impl Scrubber {
    pub(super) fn for_configs(
        configs: &[McpServerConfig],
        effective: &BTreeMap<String, String>,
    ) -> Self {
        let mut values: Vec<String> = configs
            .iter()
            .filter(|c| c.enabled)
            .flat_map(|c| c.env.values())
            .filter_map(|v| match v {
                EnvironmentValue::Reference { from_env } => effective.get(from_env),
                _ => None,
            })
            .filter(|v| !v.is_empty())
            .flat_map(|v| {
                std::iter::once(v.clone()).chain(
                    v.split(['\r', '\n'])
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned),
                )
            })
            .collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
        values.dedup();
        Self(values)
    }
    /// Strict hand-off: changed object keys cannot preserve call identity or schema meaning.
    pub fn value(&self, value: &mut serde_json::Value) -> Result<(), &'static str> {
        if self.walk_value(None, value) {
            Err("Arguments cannot be shown faithfully after secret masking: JSON keys, structure or escape spelling would change.")
        } else {
            Ok(())
        }
    }
    /// Persistence must retain the surrounding record even when a key needs masking.
    pub fn value_lossy(&self, value: &mut serde_json::Value) {
        self.walk_value(None, value);
    }
    fn walk_value(&self, parent: Option<&str>, value: &mut serde_json::Value) -> bool {
        self.walk_value_at(parent, value, 0, &mut (32 * 1024 * 1024))
    }
    fn walk_value_at(
        &self,
        parent: Option<&str>,
        value: &mut serde_json::Value,
        depth: usize,
        remaining: &mut usize,
    ) -> bool {
        let cost = value.as_str().map_or(1, str::len);
        if depth > 128 || cost > *remaining {
            *value = serde_json::Value::String("[JSON masking limit exceeded.]".into());
            return true;
        }
        *remaining -= cost;
        match value {
            serde_json::Value::String(text) => {
                let mut nested_changed = false;
                let masked = agent_core::redact::structured::mask_json(
                    text,
                    parent,
                    |key, decoded, is_key| {
                        if is_key {
                            return self.scrub(decoded);
                        }
                        let mut value = serde_json::Value::String(decoded.into());
                        nested_changed |= self.walk_value_at(key, &mut value, depth + 1, remaining);
                        value
                            .as_str()
                            .unwrap_or("[Sensitive text withheld.]")
                            .to_owned()
                    },
                );
                match masked {
                    Ok(Some(masked)) => {
                        // Check original source provenance, never the generated mask markers.
                        // Every decoded string was scrubbed by the callback; credentials in
                        // syntax or spanning tokens cannot be masked as JSON string values.
                        let residual = self.unmasked_json_secret(text, &masked.strings);
                        *text = if residual {
                            "[Sensitive JSON text withheld.]".into()
                        } else {
                            masked.text
                        };
                        masked.keys_changed || masked.spelling_changed || nested_changed || residual
                    }
                    Ok(None) => {
                        *text = parent.map_or_else(
                            || self.scrub(text),
                            |key| agent_core::redact::redact_keyed(key, &self.scrub(text)),
                        );
                        false
                    }
                    Err(_) => {
                        *text = "[Sensitive JSON text withheld.]".into();
                        true
                    }
                }
            }
            serde_json::Value::Array(values) => {
                let mut changed = false;
                for value in values {
                    changed |= self.walk_value_at(parent, value, depth + 1, remaining);
                }
                changed
            }
            serde_json::Value::Object(values) => {
                let mut changed = false;
                for (key, mut value) in std::mem::take(values) {
                    let mut safe_key = self.scrub(&key);
                    changed |= safe_key != key;
                    // A sensitive ancestor remains authoritative through arrays and objects.
                    let enclosing = parent
                        .filter(|key| agent_core::redact::is_sensitive_name(key))
                        .unwrap_or(&key);
                    changed |=
                        self.walk_value_at(Some(enclosing), &mut value, depth + 1, remaining);
                    while values.contains_key(&safe_key) {
                        safe_key.push('_');
                    }
                    values.insert(safe_key, value);
                }
                changed
            }
            _ => false,
        }
    }

    fn unmasked_json_secret(&self, text: &str, strings: &[std::ops::Range<usize>]) -> bool {
        let mut at = 0;
        let mut checked = None;
        while at < text.len() {
            if let Some(secret) = self
                .0
                .iter()
                .find(|secret| !secret.is_empty() && text[at..].starts_with(secret.as_str()))
            {
                let index = strings.partition_point(|span| span.end <= at);
                let Some(span) = strings
                    .get(index)
                    .filter(|span| span.start < at && at + secret.len() < span.end)
                else {
                    return true;
                };
                if checked != Some(index) {
                    let original = &text[span.clone()];
                    let decoded = serde_json::from_str::<String>(original);
                    let masked = serde_json::from_str::<String>(&self.scrub_exact(original));
                    // A lexical match crossing JSON escapes/delimiters may not be a decoded
                    // secret value. Refuse rather than leave that source spelling exposed.
                    if !matches!((decoded, masked), (Ok(decoded), Ok(masked)) if masked == self.scrub_exact(&decoded))
                    {
                        return true;
                    }
                    checked = Some(index);
                }
                at += secret.len();
            } else if let Some(ch) = text[at..].chars().next() {
                at += ch.len_utf8();
            }
        }
        false
    }
    pub fn longest_secret(&self) -> usize {
        self.0.first().map_or(0, String::len)
    }
    /// Emit only a prefix whose secret matches can be decided using the retained lookahead.
    pub fn drain_stream(&self, pending: &mut String, complete: bool) -> String {
        let limit = if complete {
            pending.len()
        } else {
            pending.len().saturating_sub(self.longest_secret())
        };
        let mut at = 0;
        let mut safe = String::new();
        while at < limit {
            if let Some(secret) = self
                .0
                .iter()
                .find(|secret| pending[at..].starts_with(secret.as_str()))
            {
                safe.push_str("[REDACTED]");
                at += secret.len();
            } else if let Some(ch) = pending[at..].chars().next() {
                safe.push(ch);
                at += ch.len_utf8();
            }
        }
        pending.drain(..at);
        agent_core::redact::redact(&safe)
    }
    pub fn scrub(&self, text: &str) -> String {
        agent_core::redact::redact(&self.scrub_exact(text))
    }
    fn scrub_exact(&self, text: &str) -> String {
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
        safe
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
    fn enclosing_sensitive_keys_mask_arrays_and_objects_and_lossy_keys_retain_records() {
        let scrubber = Scrubber::default();
        let mut value = json!({"tokens":["array-private-1111",["nested-private-2222"],{"innocent":["object-private-3333"]}],"safe":"visible"});
        scrubber.value(&mut value).unwrap();
        let encoded = value.to_string();
        for secret in ["array-private", "nested-private", "object-private"] {
            assert!(!encoded.contains(secret));
        }
        assert_eq!(value["safe"], "visible");
        let key = format!("ghp_{}", "a".repeat(32));
        let mut invalid = json!({key.clone():"retained","ordinary":"record"});
        assert!(scrubber.value(&mut invalid).is_err());
        let mut saved = json!({key.clone():"retained","ordinary":"record"});
        scrubber.value_lossy(&mut saved);
        assert_eq!(saved["ordinary"], "record");
        assert!(!saved.to_string().contains(&key));
        assert!(saved.to_string().contains("retained"));
    }

    #[test]
    fn unfaithful_json_credentials_are_refused_before_approval() {
        for secret in [
            r#"{"ordinary":"json-private-1111"}"#,
            r#"["json-private-2222"]"#,
        ] {
            let scrubber = Scrubber(vec![secret.into()]);
            let mut value =
                json!({"arguments":{"value":secret},"content":[{"type":"text","text":secret}]});
            assert!(scrubber.value(&mut value).is_err());
            assert!(!value.to_string().contains("json-private"));
            assert!(value.to_string().contains("Sensitive JSON text withheld."));
        }
        let scrubber = Scrubber(vec!["password".into()]);
        let mut value = json!({"text":r#"{"password":["opaque-private-12345"]}"#});
        assert!(scrubber.value(&mut value).is_err());
        assert!(!value.to_string().contains("opaque-private"));
    }
    #[test]
    fn argument_text_is_masked_in_place_and_encoded_key_changes_refuse() {
        let secret = "opaque-value-12345";
        let scrubber = Scrubber(vec![secret.into()]);
        for text in [
            format!("[ -d . ] && echo visible # {secret}"),
            format!("{{ echo visible; }} # {secret}"),
            format!(" {{\n  \"value\" : \"{secret}\", \"visible\": true\n}} "),
            format!("{{\"value\":\"{secret}\"}}\n{{\"second\":\"{secret}\"}}\n"),
        ] {
            let mut value = json!({"argument":text});
            scrubber.value(&mut value).unwrap();
            assert_eq!(value["argument"], text.replace(secret, "[REDACTED]"));
        }
        for text in [
            format!("{{\"{secret}\":\"visible\"}}"),
            format!(
                "{{\"nested\":{}}}",
                serde_json::to_string(&format!("{{\"{secret}\":\"visible\"}}")).unwrap()
            ),
        ] {
            assert!(scrubber.value(&mut json!({"argument":text})).is_err());
        }
    }
    #[test]
    fn sensitive_parents_keep_encoded_containers_and_refuse_encoded_keys() {
        let secret = "opaque-value-12345";
        let scrubber = Scrubber(vec![secret.into()]);
        for parent in ["key", "private_config", "tokens"] {
            let text = r#"{"wipe":true,"values":["private-1111",{"ordinary":"private-2222"}]}"#;
            let mut value = json!({parent:text});
            scrubber.value(&mut value).unwrap();
            let shown: serde_json::Value =
                serde_json::from_str(value[parent].as_str().unwrap()).unwrap();
            assert_eq!(shown["wipe"], true);
            assert_eq!(shown["values"][0], "****1111");
            assert_eq!(shown["values"][1]["ordinary"], "****2222");
            let mut key = json!({parent:format!("{{\"{secret}\":\"visible\"}}")});
            assert!(scrubber.value(&mut key).is_err());
        }
    }
    #[test]
    fn command_primitives_preserve_escapes_and_changed_json_escapes_refuse() {
        let secret = "opaque-value-12345";
        let scrubber = Scrubber(vec![secret.into()]);
        let command = format!(r#""\u0065cho a\/b # {secret}""#);
        let mut value = json!({"command":command});
        scrubber.value(&mut value).unwrap();
        assert_eq!(value["command"], command.replace(secret, "[REDACTED]"));
        for text in [
            format!(r#"{{"value":"\u0065cho # {secret}"}}"#),
            format!(r#"{{"value":"echo a\/b # {secret}"}}"#),
        ] {
            assert!(scrubber.value(&mut json!({"argument":text})).is_err());
        }
    }
    #[test]
    fn residual_checks_ignore_inserted_markers_but_reject_original_syntax_secrets() {
        for (secrets, input, expected) in [
            (
                vec!["ACT"],
                r#"{"value":"ACT"}"#,
                r#"{"value":"[REDACTED]"}"#,
            ),
            (
                vec!["foo", "TED]"],
                r#"{"tokens":["foo"]}"#,
                r#"{"tokens":["****TED]"]}"#,
            ),
        ] {
            let scrubber = Scrubber(secrets.into_iter().map(str::to_owned).collect());
            let mut strict = json!({"argument":input});
            scrubber.value(&mut strict).unwrap();
            assert_eq!(strict["argument"], expected);
            let mut saved = json!({"argument":input});
            scrubber.value_lossy(&mut saved);
            assert_eq!(saved, strict);
        }
        for (secret, input) in [
            ("true", r#"{"value":true}"#),
            (r#"\n"#, r#"{"value":"a\n ACT"}"#),
        ] {
            let scrubber = Scrubber(vec![secret.into(), "ACT".into()]);
            assert!(scrubber.value(&mut json!({"argument":input})).is_err());
        }
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
