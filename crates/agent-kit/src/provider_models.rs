//! The model list of a profile's model server ([`agent_core::provider`]): one GET of
//! `<base>/v1/models`, the address the user configured and nothing else. Redirects are not
//! followed, so the request never leaves that address. Blocking, bounded by a timeout: run it
//! off the main thread.

use std::time::Duration;

use agent_core::provider::{parse_models, ModelProvider};

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

    #[test]
    fn a_bad_address_is_refused_before_any_request() {
        assert!(fetch(&at("localhost:11434"), None, "t").is_err());
    }
}
