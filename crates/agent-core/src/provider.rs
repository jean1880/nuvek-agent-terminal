//! Running Claude or Codex on a local or self-hosted model server (Ollama, LM Studio, LiteLLM)
//! instead of its vendor's own.
//!
//! Nothing here talks to a server: a [`ModelProvider`] is what a profile stores, [`ModelProvider::launch`]
//! is what the agent process is started with, and [`parse_models`] reads the server's model list
//! (`GET <base>/v1/models`, OpenAI's shape, which all three serve).
//!
//! - Claude: `ANTHROPIC_BASE_URL` at the server's root (its Anthropic-compatible
//!   `/v1/messages`) and `ANTHROPIC_AUTH_TOKEN`. Ollama and LM Studio ignore the token's value
//!   but Claude Code requires one, so each has its documented placeholder.
//! - Codex: a custom provider given with `-c`, since `app-server` takes no `--oss`. The ids
//!   `openai`, `ollama` and `lmstudio` are reserved by Codex, so the app's has its own.
//!
//! An API key is never stored: the profile names the environment variable holding it.

use serde::{Deserialize, Serialize};

use crate::adapter::Driver;

/// The Codex provider id the app defines (`model_providers.<id>`): not one Codex reserves.
pub const CODEX_PROVIDER_ID: &str = "agent_terminal";

/// The most models a server's list may contribute; the rest are dropped.
pub const MAX_MODELS: usize = 500;

/// The longest model id kept.
const MAX_MODEL_ID: usize = 200;

/// The kinds of server a profile can point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Ollama,
    #[serde(rename = "lm_studio")]
    LmStudio,
    #[serde(rename = "litellm")]
    LiteLlm,
}

impl ProviderKind {
    /// Every kind, in the order Settings lists them. Guarded by `all_lists_every_kind`.
    pub const ALL: [ProviderKind; 3] = [
        ProviderKind::Ollama,
        ProviderKind::LmStudio,
        ProviderKind::LiteLlm,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Ollama => "Ollama",
            ProviderKind::LmStudio => "LM Studio",
            ProviderKind::LiteLlm => "LiteLLM",
        }
    }

    /// Where the server listens when it runs on this machine with its own defaults.
    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::Ollama => "http://localhost:11434",
            ProviderKind::LmStudio => "http://localhost:1234",
            ProviderKind::LiteLlm => "http://localhost:4000",
        }
    }

    /// The token Claude Code is given when no key is configured: the value each server's own
    /// documentation uses. The server ignores it unless it was set up to require a key.
    fn placeholder_token(self) -> &'static str {
        match self {
            ProviderKind::Ollama => "ollama",
            ProviderKind::LmStudio => "lmstudio",
            ProviderKind::LiteLlm => "litellm",
        }
    }
}

/// A profile's model server: what kind it is, where (`None`: the kind's default on this
/// machine), and the name of the environment variable holding its API key, if it needs one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProvider {
    pub kind: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// What an agent process gets for its provider: arguments to add after the profile's own,
/// environment to set (after the profile's, so it wins), and variables to remove first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderLaunch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub unset: Vec<String>,
}

/// Claude Code variables that would send its requests somewhere other than the provider, or
/// authenticate it as someone else: removed from a provider launch.
const CLAUDE_UNSET: [&str; 5] = [
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Whether `driver` can run on a provider. agy only speaks to Google's own models.
pub fn supports(driver: Driver) -> bool {
    match driver {
        Driver::Claude | Driver::Codex => true,
        Driver::Agy => false,
    }
}

/// `text` as a server base URL: `http://` or `https://`, a host, no credentials, query or
/// fragment, and nothing that needs quoting. A trailing `/` or `/v1` is dropped, since each
/// agent adds its own path. The error says what is wrong, for Settings to show.
pub fn normalize_base_url(text: &str) -> Result<String, String> {
    let text = text.trim();
    let rest = text
        .strip_prefix("http://")
        .or_else(|| text.strip_prefix("https://"))
        .ok_or_else(|| "Use an http:// or https:// address".to_owned())?;
    if text
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '\\' | '<' | '>' | '`'))
    {
        return Err("The address has a character it cannot contain".to_owned());
    }
    if text.contains('?') || text.contains('#') {
        return Err("Leave out any query (?) or fragment (#)".to_owned());
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return Err("Leave credentials out of the address; name a key variable instead".to_owned());
    }
    // `[v6]` or `[v6]:port`, else `host` or `host:port`.
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let (inside, after) = bracketed
                .split_once(']')
                .ok_or_else(|| "The address has an unclosed [".to_owned())?;
            match after {
                "" => (inside, None),
                _ => (
                    inside,
                    Some(
                        after
                            .strip_prefix(':')
                            .ok_or_else(|| "The address has text after its host".to_owned())?,
                    ),
                ),
            }
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) {
        return Err("The port is not a number from 0 to 65535".to_owned());
    }
    if host.is_empty() {
        return Err("The address has no host".to_owned());
    }
    let mut url = text.trim_end_matches('/').to_owned();
    if let Some(stripped) = url.strip_suffix("/v1") {
        url = stripped.trim_end_matches('/').to_owned();
    }
    Ok(url)
}

/// Whether `name` can be an environment variable's name (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl ModelProvider {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            base_url: None,
            api_key_env: None,
        }
    }

    /// The server's base URL, normalized: the configured one, else the kind's default.
    pub fn base(&self) -> Result<String, String> {
        match self.base_url.as_deref().filter(|u| !u.trim().is_empty()) {
            Some(url) => normalize_base_url(url),
            None => Ok(self.kind.default_base_url().to_owned()),
        }
    }

    /// The configured key variable, trimmed; `None` when there is none.
    pub fn key_env(&self) -> Result<Option<&str>, String> {
        match self.api_key_env.as_deref().map(str::trim) {
            None | Some("") => Ok(None),
            Some(name) if valid_env_name(name) => Ok(Some(name)),
            Some(name) => Err(format!("\"{name}\" is not an environment variable name")),
        }
    }

    /// Why this provider cannot be used, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        self.base()?;
        self.key_env()?;
        Ok(())
    }

    /// Where the server lists its models.
    pub fn models_url(&self) -> Result<String, String> {
        Ok(format!("{}/v1/models", self.base()?))
    }

    /// What `driver` is started with to use this provider. `key` is the value of the key
    /// variable ([`Self::key_env`]) in the agent's environment, `None` when it is unset. A
    /// configured key variable that is unset is refused: the server would only answer 401.
    /// Never falls back to the agent's own vendor: every error means "do not start".
    pub fn launch(&self, driver: Driver, key: Option<&str>) -> Result<ProviderLaunch, String> {
        let args = self.args(driver)?;
        let base = self.base()?;
        let key_env = self.key_env()?;
        let key = key.filter(|k| !k.is_empty());
        if let (Some(name), None) = (key_env, key) {
            return Err(format!(
                "{name} is not set, so {} has no API key",
                self.kind.label()
            ));
        }
        Ok(match driver {
            Driver::Claude => ProviderLaunch {
                args,
                env: vec![
                    ("ANTHROPIC_BASE_URL".to_owned(), base),
                    (
                        "ANTHROPIC_AUTH_TOKEN".to_owned(),
                        key.unwrap_or(self.kind.placeholder_token()).to_owned(),
                    ),
                    // A per-request header that defeats a local server's prompt cache.
                    ("CLAUDE_CODE_ATTRIBUTION_HEADER".to_owned(), "0".to_owned()),
                ],
                unset: CLAUDE_UNSET.map(str::to_owned).to_vec(),
            },
            // agy never gets here: `args` refused it.
            Driver::Codex | Driver::Agy => ProviderLaunch {
                args,
                env: Vec::new(),
                unset: Vec::new(),
            },
        })
    }

    /// The arguments [`Self::launch`] gives `driver`, which never hold the key: what a command
    /// line built before the agent's environment is known (a terminal tab's) can use. Refuses
    /// what `launch` refuses for any reason but an unset key.
    pub fn args(&self, driver: Driver) -> Result<Vec<String>, String> {
        let base = self.base()?;
        let key_env = self.key_env()?;
        Ok(match driver {
            Driver::Agy => {
                return Err(format!(
                    "{} cannot run on {}",
                    driver.info().label,
                    self.kind.label()
                ))
            }
            Driver::Claude => Vec::new(),
            Driver::Codex => {
                let id = CODEX_PROVIDER_ID;
                let mut settings = vec![
                    format!("model_provider={}", toml_string(id)),
                    format!(
                        "model_providers.{id}.name={}",
                        toml_string(self.kind.label())
                    ),
                    format!(
                        "model_providers.{id}.base_url={}",
                        toml_string(&format!("{base}/v1"))
                    ),
                    format!("model_providers.{id}.wire_api={}", toml_string("responses")),
                ];
                // Codex reads the key from this variable itself: only the name is passed.
                if let Some(name) = key_env {
                    settings.push(format!(
                        "model_providers.{id}.env_key={}",
                        toml_string(name)
                    ));
                }
                settings
                    .into_iter()
                    .flat_map(|s| ["-c".to_owned(), s])
                    .collect()
            }
        })
    }
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The model ids of a `/v1/models` answer (`{"data": [{"id": …}, …]}`), in the server's order,
/// without duplicates, empty or oversized ids, or control characters; at most [`MAX_MODELS`].
pub fn parse_models(json: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| "The server's model list is not JSON".to_owned())?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "The server's answer has no model list".to_owned())?;
    let mut ids: Vec<String> = Vec::new();
    for id in data
        .iter()
        .filter_map(|m| m.get("id").and_then(serde_json::Value::as_str))
        .map(str::trim)
    {
        if id.is_empty() || id.len() > MAX_MODEL_ID || id.chars().any(char::is_control) {
            continue;
        }
        if !ids.iter().any(|known| known == id) {
            ids.push(id.to_owned());
        }
        if ids.len() == MAX_MODELS {
            break;
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_kind() {
        for kind in ProviderKind::ALL {
            match kind {
                ProviderKind::Ollama | ProviderKind::LmStudio | ProviderKind::LiteLlm => {}
            }
        }
        assert_eq!(ProviderKind::ALL.len(), 3);
    }

    #[test]
    fn kinds_round_trip_through_their_stored_names() {
        for (kind, name) in [
            (ProviderKind::Ollama, "\"ollama\""),
            (ProviderKind::LmStudio, "\"lm_studio\""),
            (ProviderKind::LiteLlm, "\"litellm\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), name);
            assert_eq!(serde_json::from_str::<ProviderKind>(name).unwrap(), kind);
        }
        let stored: ModelProvider = serde_json::from_str(r#"{"kind":"ollama"}"#).unwrap();
        assert_eq!(stored, ModelProvider::new(ProviderKind::Ollama));
        assert_eq!(
            serde_json::to_string(&stored).unwrap(),
            r#"{"kind":"ollama"}"#
        );
    }

    #[test]
    fn base_urls_are_normalized_and_bad_ones_refused() {
        for (given, want) in [
            ("http://localhost:11434", "http://localhost:11434"),
            ("  http://nas:11434/ ", "http://nas:11434"),
            (
                "https://litellm.example.ca/v1",
                "https://litellm.example.ca",
            ),
            (
                "https://litellm.example.ca/v1/",
                "https://litellm.example.ca",
            ),
            ("http://10.0.0.5:4000/proxy", "http://10.0.0.5:4000/proxy"),
            ("http://[::1]:1234", "http://[::1]:1234"),
        ] {
            assert_eq!(normalize_base_url(given).as_deref(), Ok(want), "{given}");
        }
        for bad in [
            "",
            "localhost:11434",
            "ftp://host",
            "http://",
            "http://:80",
            "http://user:pw@host",
            "http://host?x=1",
            "http://host#frag",
            "http://host:port",
            "http://ho st",
            "http://host\"",
            "http://host\\x",
            "http://ho\nst",
            "http://[::1",
            "http://[::1]x",
        ] {
            assert!(normalize_base_url(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn a_missing_base_url_is_the_kinds_default() {
        for kind in ProviderKind::ALL {
            let p = ModelProvider {
                base_url: Some("  ".into()),
                ..ModelProvider::new(kind)
            };
            assert_eq!(p.base().unwrap(), kind.default_base_url());
        }
        assert_eq!(
            ModelProvider::new(ProviderKind::Ollama)
                .models_url()
                .unwrap(),
            "http://localhost:11434/v1/models"
        );
    }

    #[test]
    fn key_variables_must_be_names() {
        assert!(valid_env_name("LITELLM_API_KEY"));
        assert!(valid_env_name("_K1"));
        for bad in ["", "1KEY", "MY-KEY", "A B", "sk-abc", "$KEY"] {
            assert!(!valid_env_name(bad), "{bad}");
        }
        let p = ModelProvider {
            api_key_env: Some("sk-not-a-name".into()),
            ..ModelProvider::new(ProviderKind::LiteLlm)
        };
        assert!(p.validate().is_err());
        assert!(p.launch(Driver::Claude, Some("x")).is_err());
    }

    fn at(kind: ProviderKind, url: &str, key: Option<&str>) -> ModelProvider {
        ModelProvider {
            kind,
            base_url: Some(url.into()),
            api_key_env: key.map(Into::into),
        }
    }

    #[test]
    fn claude_is_pointed_at_the_servers_root_with_a_token() {
        let p = at(ProviderKind::Ollama, "http://nas:11434/v1", None);
        let launch = p.launch(Driver::Claude, None).unwrap();
        assert!(launch.args.is_empty());
        assert_eq!(
            launch.env,
            vec![
                ("ANTHROPIC_BASE_URL".into(), "http://nas:11434".into()),
                ("ANTHROPIC_AUTH_TOKEN".into(), "ollama".into()),
                ("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "0".into()),
            ]
        );
        for name in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
        ] {
            assert!(launch.unset.iter().any(|u| u == name), "{name}");
        }
        let keyed = at(ProviderKind::LiteLlm, "https://gw.example", Some("GW_KEY"));
        let launch = keyed.launch(Driver::Claude, Some("secret-value")).unwrap();
        assert!(launch
            .env
            .contains(&("ANTHROPIC_AUTH_TOKEN".into(), "secret-value".into())));
    }

    #[test]
    fn codex_gets_its_own_provider_and_only_the_key_variables_name() {
        let p = at(ProviderKind::LiteLlm, "https://gw.example/", Some("GW_KEY"));
        let launch = p.launch(Driver::Codex, Some("secret-value")).unwrap();
        assert!(launch.env.is_empty());
        assert_eq!(
            launch.args,
            [
                "-c",
                "model_provider=\"agent_terminal\"",
                "-c",
                "model_providers.agent_terminal.name=\"LiteLLM\"",
                "-c",
                "model_providers.agent_terminal.base_url=\"https://gw.example/v1\"",
                "-c",
                "model_providers.agent_terminal.wire_api=\"responses\"",
                "-c",
                "model_providers.agent_terminal.env_key=\"GW_KEY\"",
            ]
        );
        assert!(!launch.args.iter().any(|a| a.contains("secret-value")));
        // The same arguments before the key is known; only `launch` checks that it is set.
        assert_eq!(p.args(Driver::Codex).unwrap(), launch.args);
        assert!(p.args(Driver::Claude).unwrap().is_empty());
        assert!(p.args(Driver::Agy).is_err());
        assert!(!["openai", "ollama", "lmstudio"].contains(&CODEX_PROVIDER_ID));
    }

    #[test]
    fn a_launch_never_falls_back_to_the_vendor() {
        let unset_key = at(ProviderKind::LiteLlm, "https://gw.example", Some("GW_KEY"));
        for driver in [Driver::Claude, Driver::Codex] {
            assert!(unset_key.launch(driver, None).is_err());
            assert!(unset_key.launch(driver, Some("")).is_err());
        }
        let bad_url = at(ProviderKind::Ollama, "nas:11434", None);
        assert!(bad_url.launch(Driver::Claude, None).is_err());
        assert!(ModelProvider::new(ProviderKind::Ollama)
            .launch(Driver::Agy, None)
            .is_err());
        assert!(!supports(Driver::Agy));
    }

    #[test]
    fn toml_strings_are_escaped() {
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(toml_string("x\u{1}"), "\"x\\u0001\"");
    }

    #[test]
    fn model_lists_are_read_deduplicated_and_bounded() {
        let json = r#"{"object":"list","data":[
            {"id":"qwen3.5:9b","object":"model"},
            {"id":"qwen3.5:9b"},
            {"id":"  "},
            {"id":"bad\u0007id"},
            {"name":"no-id"},
            {"id":"qwen2.5-coder:3b"}]}"#;
        assert_eq!(
            parse_models(json).unwrap(),
            vec!["qwen3.5:9b".to_owned(), "qwen2.5-coder:3b".to_owned()]
        );
        assert!(parse_models("not json").is_err());
        assert!(parse_models(r#"{"models":[]}"#).is_err());
        let many = format!(
            r#"{{"data":[{}]}}"#,
            (0..MAX_MODELS + 10)
                .map(|i| format!(r#"{{"id":"m{i}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(parse_models(&many).unwrap().len(), MAX_MODELS);
        let long = format!(
            r#"{{"data":[{{"id":"{}"}}]}}"#,
            "x".repeat(MAX_MODEL_ID + 1)
        );
        assert!(parse_models(&long).unwrap().is_empty());
    }
}
