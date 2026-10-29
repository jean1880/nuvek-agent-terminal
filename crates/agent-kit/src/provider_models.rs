//! The model list of a profile's model server ([`agent_core::provider`]): one GET of
//! `<base>/v1/models`, the address the user configured and nothing else. Redirects are not
//! followed, so the request never leaves that address. Blocking, bounded by a timeout: run it
//! off the main thread.

use std::time::Duration;

use agent_core::provider::{
    parse_lmstudio_models, parse_models, parse_ollama_show, ModelInfo, ModelProvider, ProviderKind,
};

/// How long the whole request may take.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a reply read: a list of hundreds of models is well under this.
const MAX_BODY: u64 = 2 * 1024 * 1024;

/// The model ids `provider` serves. `key` is the value of its key variable, sent as a bearer
/// token when set; it never appears in an error. The error says what failed, for the user.
pub fn fetch(
    provider: &ModelProvider,
    key: Option<&str>,
    user_agent: &str,
) -> Result<Vec<String>, String> {
    let url = provider.models_url()?;
    let server = provider.kind.label();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .max_redirects(0)
        .user_agent(user_agent)
        .build()
        .into();
    let mut request = agent.get(&url).header("Accept", "application/json");
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        request = request.header("Authorization", &format!("Bearer {key}"));
    }
    let mut reply = request.call().map_err(|e| match e {
        ureq::Error::StatusCode(401 | 403) => format!("{server} refused the API key."),
        ureq::Error::StatusCode(code) => format!("{server} answered with HTTP {code}."),
        _ => format!(
            "Could not reach {server} at {}.",
            provider.base().unwrap_or_default()
        ),
    })?;
    // An unfollowed redirect arrives as a reply of its own.
    let status = reply.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{server} answered with HTTP {status}."));
    }
    let body = reply
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|_| format!("Could not read {server}'s model list."))?;
    parse_models(&body)
}

/// The most models whose details are asked of an Ollama server, one request each: the rest are
/// judged by name.
const MAX_DETAILED: usize = 50;
/// How long one detail request may take.
const DETAIL_TIMEOUT: Duration = Duration::from_secs(5);

/// [`fetch`], then what the server says of each model where it says it: Ollama's
/// `POST /api/show` (its `capabilities`), LM Studio's `GET /api/v1/models` (`type`,
/// `trained_for_tool_use`). Any other server, or a detail request that fails, leaves a model to
/// its name ([`ModelInfo::by_name`]): the list itself never fails for want of details.
pub fn fetch_info(
    provider: &ModelProvider,
    key: Option<&str>,
    user_agent: &str,
) -> Result<Vec<ModelInfo>, String> {
    let ids = fetch(provider, key, user_agent)?;
    let base = provider.base()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DETAIL_TIMEOUT))
        .max_redirects(0)
        .user_agent(user_agent)
        .build()
        .into();
    let bearer = key.filter(|k| !k.is_empty()).map(|k| format!("Bearer {k}"));
    let read = |reply: Result<ureq::http::Response<ureq::Body>, ureq::Error>| -> Option<String> {
        let mut reply = reply.ok()?;
        if !reply.status().is_success() {
            return None;
        }
        reply
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .ok()
    };
    Ok(match provider.kind {
        ProviderKind::Ollama => ids
            .iter()
            .enumerate()
            .map(|(at, id)| {
                let shown = (at < MAX_DETAILED).then(|| {
                    let mut request = agent.post(&format!("{base}/api/show"));
                    if let Some(bearer) = &bearer {
                        request = request.header("Authorization", bearer);
                    }
                    read(request.send_json(serde_json::json!({ "model": id })))
                });
                match shown.flatten().as_deref().and_then(parse_ollama_show) {
                    Some((chat, tools)) => ModelInfo {
                        id: id.clone(),
                        chat,
                        tools: Some(tools),
                    },
                    None => ModelInfo::by_name(id),
                }
            })
            .collect(),
        ProviderKind::LmStudio => {
            let mut request = agent.get(&format!("{base}/api/v1/models"));
            if let Some(bearer) = &bearer {
                request = request.header("Authorization", bearer);
            }
            let described = read(request.call())
                .map(|body| parse_lmstudio_models(&body))
                .unwrap_or_default();
            ids.iter()
                .map(|id| {
                    described
                        .iter()
                        .find(|m| &m.id == id)
                        .cloned()
                        .unwrap_or_else(|| ModelInfo::by_name(id))
                })
                .collect()
        }
        _ => ids.iter().map(|id| ModelInfo::by_name(id)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::provider::ProviderKind;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// Serves one request on a loopback port with `status` and `body`, and hands back the
    /// request's head.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            stream.write_all(reply.as_bytes()).unwrap();
            head
        });
        (base, handle)
    }

    fn at(base: &str) -> ModelProvider {
        ModelProvider {
            base_url: Some(base.to_owned()),
            ..ModelProvider::new(ProviderKind::Ollama)
        }
    }

    #[test]
    fn lists_the_models_at_v1_models_with_the_key_as_a_bearer() {
        let (base, server) = serve_once("200 OK", r#"{"data":[{"id":"qwen3.5:9b"}]}"#);
        let models = fetch(&at(&base), Some("k-123"), "agent-terminal-test").unwrap();
        assert_eq!(models, vec!["qwen3.5:9b".to_owned()]);
        let head = server.join().unwrap();
        assert!(head.starts_with("GET /v1/models HTTP/1.1"), "{head}");
        assert!(
            head.contains("authorization: Bearer k-123")
                || head.contains("Authorization: Bearer k-123")
        );
    }

    #[test]
    fn no_key_sends_no_authorization() {
        let (base, server) = serve_once("200 OK", r#"{"data":[]}"#);
        assert_eq!(fetch(&at(&base), None, "t").unwrap(), Vec::<String>::new());
        assert!(!server
            .join()
            .unwrap()
            .to_lowercase()
            .contains("authorization"));
    }

    #[test]
    fn errors_name_the_server_and_never_the_key() {
        let (base, server) = serve_once("401 Unauthorized", "{}");
        let err = fetch(&at(&base), Some("k-secret"), "t").unwrap_err();
        server.join().unwrap();
        assert_eq!(err, "Ollama refused the API key.");
        let (base, server) = serve_once("302 Found", "");
        let err = fetch(&at(&base), Some("k-secret"), "t").unwrap_err();
        server.join().unwrap();
        assert!(err.contains("302"), "a redirect is not followed: {err}");
        assert!(!err.contains("k-secret"));
        // Nothing listens on port 9 of loopback (discard): unreachable.
        let err = fetch(&at("http://127.0.0.1:9"), Some("k-secret"), "t").unwrap_err();
        assert!(
            err.starts_with("Could not reach Ollama at http://127.0.0.1:9"),
            "{err}"
        );
        assert!(!err.contains("k-secret"));
    }

    /// Serves `n` requests on a loopback port, each answered by `reply(head, body)` with a status
    /// line and a JSON body.
    fn serve_many(
        n: usize,
        reply: fn(&str, &str) -> (&'static str, String),
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::Read;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            for _ in 0..n {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (status, json) = reply(&head, &String::from_utf8_lossy(&body));
                let answer = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                );
                stream.write_all(answer.as_bytes()).unwrap();
            }
        });
        (base, handle)
    }

    #[test]
    fn ollama_details_say_what_chats_and_calls_tools_and_a_failure_falls_back_to_the_name() {
        let (base, server) = serve_many(4, |head, body| {
            if head.starts_with("GET /v1/models") {
                let list = r#"{"data":[{"id":"qwen3.5:9b"},{"id":"nomic-embed-text"},{"id":"mystery-embed"}]}"#;
                return ("200 OK", list.to_owned());
            }
            assert!(head.starts_with("POST /api/show"), "{head}");
            if body.contains("qwen3.5:9b") {
                (
                    "200 OK",
                    r#"{"capabilities":["completion","tools"]}"#.to_owned(),
                )
            } else if body.contains("nomic-embed-text") {
                ("200 OK", r#"{"capabilities":["embedding"]}"#.to_owned())
            } else {
                ("500 Internal Server Error", "{}".to_owned())
            }
        });
        let info = fetch_info(&at(&base), None, "t").unwrap();
        server.join().unwrap();
        assert_eq!(
            info,
            vec![
                ModelInfo {
                    id: "qwen3.5:9b".into(),
                    chat: true,
                    tools: Some(true)
                },
                ModelInfo {
                    id: "nomic-embed-text".into(),
                    chat: false,
                    tools: Some(false)
                },
                ModelInfo::by_name("mystery-embed"),
            ]
        );
    }

    #[test]
    fn a_bad_address_is_refused_before_any_request() {
        assert!(fetch(&at("localhost:11434"), None, "t").is_err());
    }
}
