//! Bounded, cancellation-safe newline framing shared by stdio MCP endpoints.
use agent_core::delegation::wire::request_id_ok;
use serde_json::Value;
use std::{io, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

pub struct FrameReader<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
    max_frame: usize,
}
impl<R: AsyncRead> FrameReader<R> {
    pub fn new(reader: R, max_frame: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            line: Vec::new(),
            max_frame,
        }
    }
}
impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// A boundary EOF is a close; EOF inside a frame is a protocol error.
    /// Buffered bytes survive cancellation of this future.
    pub async fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                return if self.line.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "partial MCP frame",
                    ))
                };
            }
            let end = chunk.iter().position(|b| *b == b'\n');
            let taken = end.map_or(chunk.len(), |n| n + 1);
            let payload = taken - usize::from(end.is_some());
            if self.line.len().saturating_add(payload) > self.max_frame {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MCP frame exceeds its budget",
                ));
            }
            self.line.extend_from_slice(&chunk[..taken]);
            self.reader.consume(taken);
            if end.is_some() {
                return Ok(Some(std::mem::take(&mut self.line)));
            }
        }
    }
}

pub async fn write_value<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<Option<W>>>,
    value: Value,
    max_frame: usize,
    timeout: Duration,
) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(&value).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    if bytes.len() >= max_frame {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP response exceeds its frame budget",
        ));
    }
    bytes.push(b'\n');
    tokio::time::timeout(timeout, async {
        let mut guard = writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

/// Checks the response id and a cancellation's target before typed SDK dispatch. It never
/// serialises the full request into an error or includes a rejected id in a response.
pub fn bounded_ids(value: &Value) -> bool {
    let within =
        |id: &Value| serde_json::to_string(id).is_ok_and(|encoded| request_id_ok(&encoded));
    value.get("id").is_none_or(within)
        && (value.get("method").and_then(Value::as_str) != Some("notifications/cancelled")
            || value
                .get("params")
                .and_then(|params| params.get("requestId"))
                .is_none_or(within))
}

/// Measure the original encoding as well: serde_json normalises numeric exponents and
/// Unicode escapes, which must not make a 257-byte wire id pass the 256-byte bound.
pub fn bounded_wire_ids(line: &[u8]) -> bool {
    let mut valid = true;
    let _ = visit_field(line, "id", &mut |id| valid &= id.len() <= 256);
    let _ = visit_field(line, "params", &mut |params| {
        let _ = visit_field(params, "requestId", &mut |id| valid &= id.len() <= 256);
    });
    valid
}

fn visit_field(object: &[u8], wanted: &str, visitor: &mut dyn FnMut(&[u8])) -> Option<()> {
    fn ws(bytes: &[u8], mut at: usize) -> usize {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at
    }
    fn end(bytes: &[u8], at: usize) -> Option<usize> {
        match *bytes.get(at)? {
            b'"' => {
                let mut cursor = at + 1;
                while let Some(byte) = bytes.get(cursor) {
                    match byte {
                        b'\\' => cursor += 2,
                        b'"' => return Some(cursor + 1),
                        _ => cursor += 1,
                    }
                }
                None
            }
            b'{' | b'[' => {
                let mut cursor = at + 1;
                let mut depth = 1usize;
                while let Some(byte) = bytes.get(cursor) {
                    match byte {
                        b'"' => cursor = end(bytes, cursor)?,
                        b'{' | b'[' => {
                            depth += 1;
                            cursor += 1;
                        }
                        b'}' | b']' => {
                            depth -= 1;
                            cursor += 1;
                            if depth == 0 {
                                return Some(cursor);
                            }
                        }
                        _ => cursor += 1,
                    }
                }
                None
            }
            _ => {
                let mut cursor = at;
                while bytes
                    .get(cursor)
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && !b",]}".contains(byte))
                {
                    cursor += 1;
                }
                (cursor > at).then_some(cursor)
            }
        }
    }
    let mut cursor = ws(object, 0);
    if object.get(cursor) != Some(&b'{') {
        return None;
    }
    cursor += 1;
    loop {
        cursor = ws(object, cursor);
        if object.get(cursor) == Some(&b'}') {
            return Some(());
        }
        if object.get(cursor) != Some(&b'"') {
            return None;
        }
        let key_end = end(object, cursor)?;
        let key: String = serde_json::from_slice(&object[cursor..key_end]).ok()?;
        cursor = ws(object, key_end);
        if object.get(cursor) != Some(&b':') {
            return None;
        }
        cursor = ws(object, cursor + 1);
        let value_end = end(object, cursor)?;
        if key == wanted {
            visitor(&object[cursor..value_end]);
        }
        cursor = ws(object, value_end);
        if object.get(cursor) == Some(&b'}') {
            return Some(());
        }
        if object.get(cursor) != Some(&b',') {
            return None;
        }
        cursor += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundary_eof_closes_but_partial_frame_is_an_error() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut complete = FrameReader::new(&b"{}\n"[..], 2);
                assert_eq!(complete.next_frame().await.unwrap(), Some(b"{}\n".to_vec()));
                assert!(complete.next_frame().await.unwrap().is_none());
                let mut partial = FrameReader::new(&b"{}"[..], 2);
                assert_eq!(
                    partial.next_frame().await.unwrap_err().kind(),
                    io::ErrorKind::UnexpectedEof
                );
                let mut oversized = FrameReader::new(&b"123\n"[..], 2);
                assert_eq!(
                    oversized.next_frame().await.unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
            });
    }
    #[test]
    fn fragmented_receive_survives_cancellation_and_blocked_writes_time_out() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (mut peer, input) = tokio::io::duplex(32);
                let mut reader = FrameReader::new(input, 32);
                peer.write_all(b"{\"a\":").await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(10), reader.next_frame())
                        .await
                        .is_err()
                );
                peer.write_all(b"1}\n").await.unwrap();
                assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"{\"a\":1}\n");
                let (writer, _blocked) = tokio::io::duplex(1);
                let writer = Arc::new(Mutex::new(Some(writer)));
                assert_eq!(
                    write_value(
                        &writer,
                        serde_json::json!({"a":1}),
                        64,
                        Duration::from_millis(10)
                    )
                    .await
                    .unwrap_err()
                    .kind(),
                    io::ErrorKind::TimedOut
                );
            });
    }
    #[test]
    fn encoded_wire_ids_are_checked_before_normalisation() {
        let escaped = format!("{{\"id\":\"{}\"}}", "\\u0061".repeat(50));
        assert!(!bounded_wire_ids(escaped.as_bytes()));
        assert!(bounded_ids(
            &serde_json::from_str::<Value>(&escaped).unwrap()
        ));
        let nested = format!("{{\"params\":{{\"requestId\":\"{}\"}}}}", "x".repeat(257));
        assert!(!bounded_wire_ids(nested.as_bytes()));
    }
}
