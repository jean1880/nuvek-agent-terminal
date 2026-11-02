//! A bounded, multiplexed client of the app's delegation socket: the stdio bridge's
//! (`agent-terminal --cross-agent-mcp`) and the Local runner's own. A dropped request releases
//! its supervisor wait via `CancelOnDrop`; it never retries a mutation after losing the
//! transport.
//!
//! Nothing it holds (the capability above all) is logged or printed.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::delegation::ipc::{
    encode_frame, ApiRange, BridgeInfo, Call, CallOutcome, Frame, Hello, Welcome, MAX_FRAME,
    RECONNECT_DEADLINE_MS, WELCOME_DEADLINE_MS,
};
use agent_core::delegation::{CapabilityToken, ErrorCode, InstanceNonce, ToolError, API_VERSION};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

const IO_DEADLINE: Duration = Duration::from_secs(5);
const MAX_INFLIGHT: usize = 4;
const COMMAND_QUEUE: usize = 16;

/// A delegation failure that says why the runtime cannot be reached.
pub fn unavailable(reason: &str) -> ToolError {
    ToolError::new(ErrorCode::RuntimeUnavailable, reason)
}

// Deliberately no Debug: this holds the capability for the single reconnect.
pub struct Config {
    endpoint: PathBuf,
    hello: Hello,
}

impl Config {
    /// A client of `endpoint` that presents `cap` for app instance `instance`, as process
    /// `pid` running this build.
    pub fn new(endpoint: PathBuf, instance: InstanceNonce, cap: CapabilityToken, pid: u32) -> Self {
        Self {
            endpoint,
            hello: Hello {
                api: ApiRange {
                    min: API_VERSION,
                    max: API_VERSION,
                },
                instance,
                cap,
                bridge: BridgeInfo {
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    pid,
                },
            },
        }
    }
}

#[derive(Clone)]
enum State {
    Connecting,
    Ready,
    Degraded(ToolError),
}

/// Where a client stands, for a caller that cannot await (the Local runner's offers).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Ready,
    Degraded(ToolError),
}

type Reply = oneshot::Sender<Result<CallOutcome, ToolError>>;

enum Command {
    Call(Call, Reply),
    Cancel(u64),
}

#[derive(Clone)]
pub struct Client {
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<State>,
    next: Arc<AtomicU64>,
}

impl Client {
    /// Starts the connection actor on the current tokio runtime. `started` opens the welcome
    /// deadline.
    pub fn start(config: Result<Config, ToolError>, started: Instant) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
        let (state_tx, state) = watch::channel(State::Connecting);
        tokio::spawn(async move {
            match config {
                Ok(config) => actor(config, started, state_tx, receiver).await,
                Err(error) => {
                    state_tx.send_replace(State::Degraded(error));
                }
            }
        });
        Self {
            commands,
            state,
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Where it stands now, without waiting.
    pub fn status(&self) -> Status {
        match self.state.borrow().clone() {
            State::Connecting => Status::Connecting,
            State::Ready => Status::Ready,
            State::Degraded(error) => Status::Degraded(error),
        }
    }

    pub async fn ready(&self) -> Result<(), ToolError> {
        let mut state = self.state.clone();
        loop {
            match state.borrow_and_update().clone() {
                State::Ready => return Ok(()),
                State::Degraded(error) => return Err(error),
                State::Connecting => {}
            }
            if state.changed().await.is_err() {
                return Err(unavailable("the delegation connection is unavailable"));
            }
        }
    }

    pub async fn call(&self, call: Call) -> Result<CallOutcome, ToolError> {
        let timeout = Duration::from_millis(u64::from(call.timeout_ms));
        tokio::time::timeout(timeout, async {
            self.ready().await?;
            let id = call.id;
            let (reply, result) = oneshot::channel();
            self.commands.try_send(Command::Call(call, reply)).map_err(|_| {
                ToolError::new(ErrorCode::CapacityReached, "the delegation bridge is busy")
            })?;
            let mut cancel = CancelOnDrop { id, commands: self.commands.clone(), armed: true };
            let outcome = result.await.map_err(|_| lost());
            // Answered: there is nothing left to cancel.
            cancel.armed = false;
            outcome?
        }).await.map_err(|_| {
            ToolError::new(ErrorCode::DeadlineExceeded, "the delegation call deadline expired; retry mutations with the same idempotency key")
        })?
    }
}

/// Cancels its call at the supervisor when the wait for it is dropped before the answer came
/// (a deadline, the caller giving up); disarmed once answered.
struct CancelOnDrop {
    id: u64,
    commands: mpsc::Sender<Command>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // At most four calls are admitted; the bounded 16-entry queue has room for their
        // cancellation notices even while a frame write is in progress.
        let _ = self.commands.try_send(Command::Cancel(self.id));
    }
}

fn lost() -> ToolError {
    ToolError::new(
        ErrorCode::TransportLost,
        "the delegation connection was lost; retry mutations with the same idempotency key",
    )
}

async fn connect(config: &Config, deadline: Instant) -> Result<(UnixStream, Welcome), ToolError> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
        let mut socket = UnixStream::connect(&config.endpoint)
            .await
            .map_err(|_| unavailable("cannot connect to the delegation runtime"))?;
        write_frame(&mut socket, &Frame::Hello(config.hello.clone())).await?;
        match read_frame(&mut socket).await? {
            Frame::Welcome(welcome)
                if welcome.api == API_VERSION
                    && welcome.limits.max_inflight > 0
                    && welcome.limits.max_frame > 0
                    && welcome.limits.max_frame as usize <= MAX_FRAME =>
            {
                Ok((socket, welcome))
            }
            Frame::Refused { error } | Frame::Revoked { error } => Err(error),
            _ => Err(ToolError::new(
                ErrorCode::ProtocolIncompatible,
                "the delegation runtime sent an invalid welcome",
            )),
        }
    })
    .await
    .map_err(|_| unavailable("the delegation welcome deadline expired"))?
}

async fn actor(
    config: Config,
    started: Instant,
    state: watch::Sender<State>,
    mut commands: mpsc::Receiver<Command>,
) {
    let mut deadline = started + Duration::from_millis(WELCOME_DEADLINE_MS);
    let mut reconnect_used = false;
    loop {
        let (socket, welcome) = match connect(&config, deadline).await {
            Ok(connected) => connected,
            Err(error) => {
                state.send_replace(State::Degraded(error));
                return;
            }
        };
        state.send_replace(State::Ready);
        // Agy's capability is single use: spent by this hello, it can never
        // authenticate again, so a reconnect would only present a used credential to be refused.
        let single_use = welcome.principal.driver == agent_core::adapter::Driver::Agy;
        let max_inflight = (welcome.limits.max_inflight as usize).min(MAX_INFLIGHT);
        let (mut reader, mut writer) = socket.into_split();
        let (incoming_tx, mut incoming) = mpsc::channel(1);
        let read_task = tokio::spawn(async move {
            loop {
                let frame = read_frame(&mut reader).await;
                let stop = frame.is_err();
                if incoming_tx.send(frame).await.is_err() || stop {
                    break;
                }
            }
        });
        let mut pending: BTreeMap<u64, Reply> = BTreeMap::new();
        let mut revoked = None;
        let mut shutting_down = false;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None => { shutting_down = true; break; }
                    Some(Command::Call(call, reply)) => {
                        if reply.is_closed() { continue; }
                        if pending.len() >= max_inflight {
                            let _ = reply.send(Err(ToolError::new(ErrorCode::CapacityReached, "four delegation calls are already in flight")));
                            continue;
                        }
                        let id = call.id;
                        pending.insert(id, reply);
                        if write_frame(&mut writer, &Frame::Call(call)).await.is_err() { break; }
                    }
                    Some(Command::Cancel(id)) => {
                        if pending.contains_key(&id)
                            && write_frame(&mut writer, &Frame::Cancel { id }).await.is_err() { break; }
                    }
                },
                frame = incoming.recv() => match frame {
                    Some(Ok(Frame::Result(result))) => {
                        if let Some(reply) = pending.remove(&result.id) {
                            let _ = reply.send(Ok(result.outcome));
                        } else {
                            // A result for an unrecognised id is a protocol violation.
                            break;
                        }
                    }
                    Some(Ok(Frame::Revoked { error })) | Some(Ok(Frame::Refused { error })) => {
                        revoked = Some(error);
                        break;
                    }
                    _ => break,
                }
            }
        }
        read_task.abort();
        drop(writer);
        for (_, reply) in pending {
            let _ = reply.send(Err(lost()));
        }
        if shutting_down {
            return;
        }
        if let Some(error) = revoked {
            state.send_replace(State::Degraded(error));
            return;
        }
        if single_use {
            state.send_replace(State::Degraded(unavailable(
                "the delegation connection was lost; its single-use capability cannot reconnect, \
                 so delegation returns when the agent is rebound before its next turn",
            )));
            return;
        }
        // One reconnect, counted here only: the supervisor does not count them (it allows one
        // live connection per token, and a Local one only from its runner's own process).
        if reconnect_used {
            state.send_replace(State::Degraded(unavailable(
                "the delegation connection was lost after its one reconnect",
            )));
            return;
        }
        reconnect_used = true;
        state.send_replace(State::Connecting);
        deadline = Instant::now() + Duration::from_millis(RECONNECT_DEADLINE_MS);
    }
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Frame, ToolError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await.map_err(|_| lost())?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(lost());
    }
    let mut body = vec![0; length];
    tokio::time::timeout(IO_DEADLINE, reader.read_exact(&mut body))
        .await
        .map_err(|_| lost())?
        .map_err(|_| lost())?;
    serde_json::from_slice(&body).map_err(|_| lost())
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &Frame,
) -> Result<(), ToolError> {
    let bytes = encode_frame(frame).map_err(|_| lost())?;
    tokio::time::timeout(IO_DEADLINE, writer.write_all(&bytes))
        .await
        .map_err(|_| lost())?
        .map_err(|_| lost())
}

/// A [`Client`] on a runtime thread of its own, for a synchronous caller (the Local runner).
/// Dropping it ends the connection.
pub struct Blocking {
    handle: tokio::runtime::Handle,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    client: Client,
}

/// How a blocking call ended.
pub enum Waited {
    Done(Result<CallOutcome, ToolError>),
    /// `stop` said to stop first; the call was dropped, so the supervisor was told to cancel it.
    Stopped,
}

impl Blocking {
    /// Starts the client on a single-threaded runtime driven by a thread of its own.
    pub fn start(config: Config) -> Result<Self, ToolError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("delegation-client".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                else {
                    return;
                };
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                if ready_tx.send((runtime.handle().clone(), stop_tx)).is_err() {
                    return;
                }
                runtime.block_on(async {
                    let _ = stop_rx.await;
                });
                runtime.shutdown_timeout(Duration::from_millis(200));
            })
            .map_err(|_| unavailable("cannot start the delegation client thread"))?;
        let (handle, shutdown) = ready_rx
            .recv()
            .map_err(|_| unavailable("cannot start the delegation client runtime"))?;
        let client = {
            let _entered = handle.enter();
            Client::start(Ok(config), Instant::now())
        };
        Ok(Self {
            handle,
            shutdown: Some(shutdown),
            thread: Some(thread),
            client,
        })
    }

    pub fn status(&self) -> Status {
        self.client.status()
    }

    /// Waits until the connection is ready or has failed, at most `limit`, on the client's own
    /// state channel.
    pub fn wait_ready(&self, limit: Duration) -> Status {
        let (tx, rx) = std::sync::mpsc::channel();
        let client = self.client.clone();
        self.handle.spawn(async move {
            let _ = client.ready().await;
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(limit);
        self.status()
    }

    pub fn next_id(&self) -> u64 {
        self.client.next_id()
    }

    /// Runs `call`, looking at `stop` every 20 ms while it waits. A stop drops the call, which
    /// cancels it at the supervisor (accepted mutations are unchanged).
    pub fn call(&self, call: Call, stop: &mut dyn FnMut() -> bool) -> Waited {
        let (tx, mut rx) = oneshot::channel();
        let client = self.client.clone();
        let task = self.handle.spawn(async move {
            let _ = tx.send(client.call(call).await);
        });
        loop {
            match rx.try_recv() {
                Ok(result) => return Waited::Done(result),
                Err(oneshot::error::TryRecvError::Closed) => {
                    return Waited::Done(Err(lost()));
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
            if stop() {
                task.abort();
                return Waited::Stopped;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Blocking {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        // The runtime's shutdown is bounded (200 ms), so this join is too.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;

    pub fn config(endpoint: PathBuf) -> Config {
        Config::new(
            endpoint,
            InstanceNonce::from_bytes([4; 16]),
            CapabilityToken::from_random([7; 32]),
            123,
        )
    }

    pub async fn read(reader: &mut (impl AsyncRead + Unpin)) -> Frame {
        read_frame(reader).await.expect("frame")
    }

    pub async fn write(writer: &mut (impl AsyncWrite + Unpin), frame: &Frame) {
        write_frame(writer, frame).await.expect("write")
    }
}
