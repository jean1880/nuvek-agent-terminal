use crate::mcp_transport::{bounded_ids, bounded_wire_ids, write_value, FrameReader};
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    future::Future,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite};

pub(super) const MAX_FRAME: usize = 1024 * 1024;
const MAX_UNSOLICITED: usize = 128;

pub(super) struct ClientTransport<R, W> {
    reader: FrameReader<R>,
    writer: Arc<tokio::sync::Mutex<Option<W>>>,
    pending: Arc<Mutex<BTreeSet<String>>>,
    pub stale: Arc<std::sync::atomic::AtomicBool>,
    pub failure: Arc<Mutex<Option<&'static str>>>,
    pub received_bytes: Arc<std::sync::atomic::AtomicUsize>,
    ended: bool,
}

impl<R: AsyncRead, W: AsyncWrite> ClientTransport<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: FrameReader::new(reader, MAX_FRAME),
            writer: Arc::new(tokio::sync::Mutex::new(Some(writer))),
            pending: Default::default(),
            stale: Default::default(),
            failure: Default::default(),
            received_bytes: Default::default(),
            ended: false,
        }
    }
    fn fail(&mut self, why: &'static str) {
        self.ended = true;
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(why);
        }
    }
}

impl<R, W> Transport<RoleClient> for ClientTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Error = io::Error;
    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let writer = self.writer.clone();
        let pending = self.pending.clone();
        async move {
            let value = serde_json::to_value(message)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if let Some(id) = value.get("id") {
                let id = id.to_string();
                let mut pending = pending
                    .lock()
                    .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
                if pending.len() >= 4 || !pending.insert(id) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
            }
            write_value(&writer, value, MAX_FRAME, Duration::from_secs(5)).await
        }
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        if self.ended {
            return None;
        }
        for _ in 0..MAX_UNSOLICITED {
            let line = match self.reader.next_frame().await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    self.fail("MCP server closed its output.");
                    return None;
                }
                Err(_) => {
                    self.fail("MCP server sent an oversized, partial or unreadable frame.");
                    return None;
                }
            };
            self.received_bytes
                .fetch_add(line.len(), std::sync::atomic::Ordering::Relaxed);
            if !bounded_wire_ids(&line) {
                self.fail("MCP server sent an oversized wire ID.");
                return None;
            }
            let value: Value = match serde_json::from_slice(&line) {
                Ok(value) if bounded_ids(&value) => value,
                _ => {
                    self.fail("MCP server sent invalid JSON or an oversized ID.");
                    return None;
                }
            };
            // Reverse requests are unsupported, including during discovery. Answer here so
            // SDK startup cannot ignore them or log their potentially sensitive contents.
            if value.get("method").is_some() {
                if let Some(id) = value.get("id") {
                    if !id.is_string() && id.as_i64().is_none() {
                        self.fail("Invalid MCP request ID.");
                        return None;
                    }
                    if write_value(&self.writer, json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client requests are unsupported"}}), MAX_FRAME, Duration::from_secs(5)).await.is_err() { self.fail("MCP reverse response could not be written."); return None; }
                } else if value.get("method").and_then(Value::as_str)
                    == Some("notifications/tools/list_changed")
                {
                    self.stale.store(true, std::sync::atomic::Ordering::Release);
                }
                tokio::task::yield_now().await;
                continue;
            }
            let correlated = self.pending.lock().is_ok_and(|mut pending| {
                if let Some(id) = value.get("id").filter(|id| !id.is_null()) {
                    pending.remove(&id.to_string())
                        || id
                            .as_str()
                            .and_then(|id| id.parse::<i64>().ok())
                            .is_some_and(|id| pending.remove(&id.to_string()))
                } else if value.get("error").is_some() && pending.len() == 1 {
                    // The SDK accepts an id-less protocol error when there is exactly one
                    // possible request; never guess when startup fallback left two IDs.
                    pending.pop_first();
                    true
                } else {
                    false
                }
            });
            if !correlated {
                self.fail("MCP server returned an uncorrelated response.");
                return None;
            }
            match serde_json::from_value(value) {
                Ok(message) => return Some(message),
                Err(_) => {
                    self.fail("MCP server returned an invalid response.");
                    return None;
                }
            }
        }
        self.fail("MCP server exceeded the unsolicited frame limit.");
        None
    }
    async fn close(&mut self) -> io::Result<()> {
        self.ended = true;
        self.writer.lock().await.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bad_responses_are_terminal_and_never_echo_peer_text() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                for input in [
                    "not-json-secret-canary\n",
                    "{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{}}\n",
                    "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
                ] {
                    let (writer, _peer) = tokio::io::duplex(4096);
                    let mut transport = ClientTransport::new(input.as_bytes(), writer);
                    assert!(transport.receive().await.is_none());
                    assert!(transport.receive().await.is_none());
                    assert!(!transport
                        .failure
                        .lock()
                        .unwrap()
                        .unwrap()
                        .contains("secret-canary"));
                }
            });
    }

    #[test]
    fn numeric_string_and_unambiguous_idless_errors_match_sdk_startup_rules() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            for response in [
                "{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{}}\n",
                "{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32601,\"message\":\"unsupported\"}}\n",
            ] {
                let (writer, _peer) = tokio::io::duplex(4096);
                let mut transport = ClientTransport::new(response.as_bytes(),writer);
                let request = serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"ping"})).unwrap();
                transport.send(request).await.unwrap();
                assert!(transport.receive().await.is_some());
                assert!(transport.pending.lock().unwrap().is_empty());
            }
        });
    }
}
