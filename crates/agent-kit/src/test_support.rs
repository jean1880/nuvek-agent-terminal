//! Hermetic git for test fixtures: a fixed identity, no signing, no hooks, `main` as the
//! default branch, and neither the user's global nor the system config. Compiled only for
//! tests (`cfg(test)`, or the `test-support` feature a dependent enables in its
//! `[dev-dependencies]`), so the shipped binary never carries it.

use std::path::Path;
use std::process::{Command, Output};

/// The `-c` options every fixture git call runs with.
const HERMETIC: [&str; 10] = [
    "-c",
    "user.name=t",
    "-c",
    "user.email=t@t",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "init.defaultBranch=main",
];

/// Runs hermetic git in `dir` and returns its output, whatever the exit status.
///
/// Panics only if git cannot be started at all.
pub fn git_output(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(HERMETIC)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

/// Runs hermetic git in `dir`, asserts it succeeded, and returns its trimmed stdout
/// (decoded lossily).
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Initialises a repository in the existing directory `dir`, writes `files` (relative paths,
/// parents created) and commits them all as `init`.
pub fn init_repo_at(dir: &Path, files: &[(&str, &[u8])]) {
    git(dir, &["init", "-q"]);
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{}: {e}", parent.display()));
        }
        std::fs::write(&path, body).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Whether git is installed; says so on stderr when it is not, so a repository test that
/// returns early shows why it was skipped.
pub fn git_or_skip() -> bool {
    let installed = crate::git::git_installed();
    if !installed {
        eprintln!("git not installed; skipping");
    }
    installed
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A loopback OpenAI-compatible chat endpoint that answers each request with the next scripted
/// SSE reply (a plain answer once the script runs out) and records every request body.
pub struct ScriptedModel {
    pub base: String,
    pub address: std::net::SocketAddr,
    pub bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedModel {
    pub fn start(replies: Vec<String>) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("address");
        let base = format!("http://{address}");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorded = bodies.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut replies = std::collections::VecDeque::from(replies);
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let length: usize = String::from_utf8_lossy(&head)
                    .to_lowercase()
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                let _ = stream.read_exact(&mut body);
                if let Ok(value) = serde_json::from_slice(&body) {
                    if let Ok(mut bodies) = recorded.lock() {
                        bodies.push(value);
                    }
                }
                let reply = replies.pop_front().unwrap_or_else(Self::answer);
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        Self {
            base,
            address,
            bodies,
            stop,
            thread: Some(thread),
        }
    }

    /// A plain final answer.
    pub fn answer() -> String {
        Self::text("done")
    }

    /// A final answer saying `text`.
    pub fn text(text: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]})
        )
    }

    /// One reply making these tool calls.
    pub fn calls(calls: &[(&str, serde_json::Value)]) -> String {
        let calls: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(index, (name, args))| {
                serde_json::json!({"index":index,"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})
            })
            .collect();
        format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"delta":{"tool_calls":calls}}]})
        )
    }

    pub fn bodies(&self) -> Vec<serde_json::Value> {
        self.bodies.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

impl Drop for ScriptedModel {
    /// Stops the listener thread: a last connection wakes `accept`, which then sees the flag.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect_timeout(&self.address, Duration::from_secs(2));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What a [`FakeDelegation`] answers a call with: `None` holds it until it is cancelled.
pub type Answer = Box<
    dyn Fn(&agent_core::delegation::ipc::Call) -> Option<agent_core::delegation::ipc::CallOutcome>
        + Send
        + Sync,
>;

/// A hello the fake received, and the pid `SO_PEERCRED` gave for its connection.
pub type SeenHello = (agent_core::delegation::ipc::Hello, Option<u32>);

/// A stand-in for the app's delegation socket: welcomes any hello (recording it and the
/// connecting process's pid from `SO_PEERCRED`), answers calls through `answer`, records
/// cancellations, and revokes the live connection on request.
pub struct FakeDelegation {
    pub path: std::path::PathBuf,
    pub hellos: Arc<Mutex<Vec<SeenHello>>>,
    pub calls: Arc<Mutex<Vec<agent_core::delegation::ipc::Call>>>,
    pub cancels: Arc<Mutex<Vec<u64>>>,
    revoke: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The pid of the process at the other end of `stream`.
pub fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the size passed.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

fn read_frame(
    stream: &mut std::os::unix::net::UnixStream,
) -> Option<agent_core::delegation::ipc::Frame> {
    use std::io::Read;
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).ok()?;
    let mut body = vec![0; u32::from_be_bytes(header) as usize];
    stream.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_frame(
    stream: &mut std::os::unix::net::UnixStream,
    frame: &agent_core::delegation::ipc::Frame,
) -> bool {
    use std::io::Write;
    agent_core::delegation::ipc::encode_frame(frame)
        .ok()
        .is_some_and(|bytes| stream.write_all(&bytes).is_ok())
}

impl FakeDelegation {
    /// Listens on `delegation.sock` in `dir`.
    pub fn start(dir: &Path, answer: Answer) -> Self {
        use agent_core::delegation::ipc::{
            CallOutcome, CallResult, Frame, IpcLimits, Principal, Welcome,
        };
        use agent_core::delegation::{ErrorCode, TaskMode, ToolError, WorkerId};
        let path = dir.join("delegation.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind socket");
        listener.set_nonblocking(true).expect("nonblocking");
        let hellos = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let revoke = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let answer = Arc::new(answer);
        let (h, c, x, r, s) = (
            hellos.clone(),
            calls.clone(),
            cancels.clone(),
            revoke.clone(),
            stop.clone(),
        );
        let thread = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            while !s.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = stream.set_nonblocking(false);
                let (h, c, x, r, s, answer) = (
                    h.clone(),
                    c.clone(),
                    x.clone(),
                    r.clone(),
                    s.clone(),
                    answer.clone(),
                );
                handlers.push(std::thread::spawn(move || {
                    let pid = peer_pid(&stream);
                    let Some(Frame::Hello(hello)) = read_frame(&mut stream) else {
                        return;
                    };
                    if let Ok(mut hellos) = h.lock() {
                        hellos.push((hello, pid));
                    }
                    let Ok(worker) = WorkerId::parse("w_abcdefgh") else {
                        return;
                    };
                    let welcome = Welcome {
                        api: agent_core::delegation::API_VERSION,
                        connection: "c_1".into(),
                        principal: Principal {
                            worker_id: worker.clone(),
                            root_id: worker,
                            driver: agent_core::adapter::Driver::Local,
                            depth: 0,
                            can_spawn: true,
                            task_mode_ceiling: TaskMode::ALL.to_vec(),
                        },
                        authority_epoch: 1,
                        limits: IpcLimits {
                            max_frame: agent_core::delegation::ipc::MAX_FRAME as u32,
                            max_inflight: 4,
                        },
                    };
                    if !write_frame(&mut stream, &Frame::Welcome(welcome)) {
                        return;
                    }
                    let Ok(mut reader) = stream.try_clone() else {
                        return;
                    };
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        while let Some(frame) = read_frame(&mut reader) {
                            if tx.send(frame).is_err() {
                                break;
                            }
                        }
                    });
                    let mut held = Vec::new();
                    loop {
                        if s.load(Ordering::SeqCst) {
                            return;
                        }
                        if r.swap(false, Ordering::SeqCst) {
                            let error = ToolError::new(ErrorCode::AuthenticationFailed, "revoked");
                            write_frame(&mut stream, &Frame::Revoked { error });
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                        match rx.recv_timeout(Duration::from_millis(10)) {
                            Ok(Frame::Call(call)) => {
                                let outcome = answer(&call);
                                let id = call.id;
                                if let Ok(mut calls) = c.lock() {
                                    calls.push(call);
                                }
                                match outcome {
                                    Some(outcome) => {
                                        write_frame(
                                            &mut stream,
                                            &Frame::Result(CallResult { id, outcome }),
                                        );
                                    }
                                    None => held.push(id),
                                }
                            }
                            Ok(Frame::Cancel { id }) => {
                                if let Ok(mut cancels) = x.lock() {
                                    cancels.push(id);
                                }
                                if held.contains(&id) {
                                    held.retain(|h| *h != id);
                                    let outcome = CallOutcome::Err(ToolError::new(
                                        ErrorCode::DeadlineExceeded,
                                        "cancelled",
                                    ));
                                    write_frame(
                                        &mut stream,
                                        &Frame::Result(CallResult { id, outcome }),
                                    );
                                }
                            }
                            Ok(_) => return,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                }));
            }
            for handler in handlers {
                let _ = handler.join();
            }
        });
        Self {
            path,
            hellos,
            calls,
            cancels,
            revoke,
            stop,
            thread: Some(thread),
        }
    }

    /// Revokes the live connection: its next turn of the loop sends `Revoked` and closes.
    pub fn revoke(&self) {
        self.revoke.store(true, Ordering::SeqCst);
    }

    pub fn calls(&self) -> Vec<agent_core::delegation::ipc::Call> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn hellos(&self) -> Vec<SeenHello> {
        self.hellos.lock().map(|h| h.clone()).unwrap_or_default()
    }

    pub fn cancels(&self) -> Vec<u64> {
        self.cancels.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl Drop for FakeDelegation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
