//! A model server's requests carry its key, so they go to the address the user configured and
//! never through a proxy named by the environment. Its own test binary: the proxy variables are
//! process-wide, and no other test may run while they are set.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use agent_core::provider::{ModelProvider, ProviderKind};
use agent_kit::local_agent::{ChatClient, HttpChat};

/// Accepts connections on a loopback port for as long as the test runs, answering each request
/// with `body` and reporting its request line.
fn listen(body: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let mut length = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                if first.is_empty() {
                    first = line.trim().to_owned();
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let mut sent = vec![0; length];
            let _ = reader.read_exact(&mut sent);
            let _ = tx.send(first);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}"), rx)
}

#[test]
fn model_server_requests_ignore_the_proxy_variables() {
    let (proxy, through_proxy) = listen("{}");
    for name in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        std::env::set_var(name, &proxy);
    }
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");

    let (server, direct) = listen(r#"{"data":[{"id":"qwen3.5:9b"}]}"#);
    let provider = ModelProvider {
        base_url: Some(server.clone()),
        ..ModelProvider::new(ProviderKind::LiteLlm)
    };
    let models = agent_kit::provider_models::fetch(&provider, Some("k-proxy"), "t").unwrap();
    assert_eq!(models, vec!["qwen3.5:9b".to_owned()]);
    assert!(direct
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .starts_with("GET /v1/models"));
    agent_kit::provider_models::fetch_info(&provider, Some("k-proxy"), "t").unwrap();
    assert!(direct.recv_timeout(Duration::from_secs(5)).is_ok());

    // The Local agent's chat requests too. The reply is not a stream; only where it went matters.
    let mut chat = HttpChat::new(&server, Some("k-proxy".to_owned()), "t");
    let reply = chat.start(&serde_json::json!({ "model": "qwen3.5:9b", "messages": [] }));
    while reply.recv_timeout(Duration::from_secs(5)).is_ok() {}
    assert!(direct
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .starts_with("POST /v1/chat/completions"));

    assert!(
        through_proxy.try_recv().is_err(),
        "a request went through the proxy"
    );
}
