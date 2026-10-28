//! A delegation root's busy switch, driven as the window drives it: the supervisor prepares the
//! switch (its rebind stops the root, which ends the running turn as an exit would), the session
//! switches, then the rebind ends. The interrupted task continues exactly once on the new
//! process, whether the switch read the thread before the rebind ended (an in-memory store) or
//! after (the store writer); a superseded switch never continues, and a turn that finished on
//! its own is not redone.
use super::*;
use crate::chat::session::{AgentLaunch, SessionStore};
use crate::chat::ChatBackend;
use agent_core::adapter::{Action, Adapter, AdapterError, Command, OpenSession};
use agent_core::caps::Capabilities;
use agent_core::event::Envelope;
use agent_kit::store_writer::{StoreHandle, StoreWriter};
use std::cell::Cell;

const CONTINUATION: &str = "Continue the unfinished task";

/// Claude's protocol under another agent's name: the switch's "other agent" is a scripted
/// Claude too.
struct Relabelled {
    inner: agent_core::claude::ClaudeAdapter,
    driver: Driver,
}

impl Adapter for Relabelled {
    fn driver(&self) -> Driver {
        self.driver
    }
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    fn argv(&self, session: &OpenSession) -> Vec<String> {
        self.inner.argv(session)
    }
    fn handshake(&mut self) -> Vec<String> {
        self.inner.handshake()
    }
    fn handshake_answered(&self) -> bool {
        self.inner.handshake_answered()
    }
    fn prompt_starts_turn(&self, text: &str) -> bool {
        self.inner.prompt_starts_turn(text)
    }
    fn discard_queued_prompts(&mut self) {
        self.inner.discard_queued_prompts();
    }
    fn encode(&mut self, command: Command) -> Result<Vec<Action>, AdapterError> {
        self.inner.encode(command)
    }
    fn feed(&mut self, line: &str) -> Vec<Envelope> {
        self.inner.feed(line)
    }
    fn feed_stderr(&mut self, line: &str) -> Vec<Envelope> {
        self.inner.feed_stderr(line)
    }
    fn feed_side(&mut self, id: &str, stdout: &str, ok: bool) -> Vec<Envelope> {
        self.inner.feed_side(id, stdout, ok)
    }
    fn on_exit(&mut self, code: Option<i32>) -> Vec<Envelope> {
        self.inner.on_exit(code)
    }
    fn pending_request_ids(&self) -> Vec<String> {
        self.inner.pending_request_ids()
    }
}

/// A scripted Claude in `dir`: logs its arguments and every line it is sent, ends its
/// prompt-less first turn, and answers each prompt, holding that turn while a `hold` file exists.
fn agent_script(dir: &std::path::Path) -> PathBuf {
    let script = dir.join("agent.sh");
    std::fs::write(
        &script,
        "D=$(dirname \"$0\")\n\
         echo \"$@\" >> \"$D/args.log\"\n\
         IFS= read -r handshake\n\
         echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
         echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
         while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$D/prompts.log\"; \
         case \"$line\" in *'\"type\":\"user\"'*) \
         while [ -e \"$D/hold\" ]; do sleep 0.02; done; \
         echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'; \
         echo '{\"type\":\"assistant\",\"uuid\":\"u1\",\"parent_tool_use_id\":null,\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}'; \
         echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}';; esac; done\n",
    )
    .unwrap();
    script
}

/// How the root's session stores its thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    /// The supervisor's in-memory store: the switch reads the thread at once, before the
    /// rebind ends.
    Direct,
    /// A file store behind a store writer kept busy: the switch reads the thread after the
    /// rebind ended.
    Writer,
}

struct Root {
    old: PathBuf,
    new: PathBuf,
    session: Rc<ChatSession>,
    thread: String,
    /// The store writer, for [`Storage::Writer`].
    writer: Option<StoreWriter>,
    /// The window's switch serial: a later pick overtakes an earlier one still preparing.
    serial: Rc<Cell<u64>>,
}

impl Root {
    fn sent(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("prompts.log")).unwrap_or_default()
    }

    fn continuations(&self) -> usize {
        Self::sent(&self.old).matches(CONTINUATION).count()
            + Self::sent(&self.new).matches(CONTINUATION).count()
    }

    fn launches(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("args.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn hold(&self, on: bool) {
        let path = self.old.join("hold");
        if on {
            std::fs::write(path, "").unwrap();
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    fn idle(&self) -> bool {
        let status = self.session.status();
        status.alive && !status.running_turn
    }

    /// Keeps the store writer busy for `ms` with a write queued before anything that follows.
    fn hold_writer(&self, ms: u64) {
        if let Some(writer) = &self.writer {
            writer
                .write("a slow write", None, move |_| {
                    std::thread::sleep(Duration::from_millis(ms));
                    Ok(())
                })
                .unwrap();
        }
    }

    /// What the window does when the user picks `driver`/`model` in the root's thread
    /// (`window::imp::threads::delegation::switch`): prepare the root's switch, then switch,
    /// then end the rebind, unless a later pick overtook it meanwhile.
    async fn switch_as_the_window_does(&self, sup: &Rc<Supervisor>, driver: Driver, model: &str) {
        let chosen = self.serial.get().wrapping_add(1);
        self.serial.set(chosen);
        let profile = Profile {
            id: "fixture".into(),
            name: "Fixture".into(),
            command: driver.info().key.into(),
            available_for_delegation: true,
            ..Default::default()
        };
        // Unbound (as for an agy whose hook is not proven): no binding is staged, and the stop,
        // switch and rebind run in the same order as a bound switch's.
        if !sup
            .prepare_root_switch(&self.thread, &profile, driver, false)
            .await
            || self.serial.get() != chosen
        {
            return;
        }
        self.session.switch(driver, Some(model.into()), None);
        self.session.rebind_start();
    }
}

/// The harness's root, in a session stored as `storage` says, busy with a held turn on
/// "REQUEST: port the parser".
async fn busy_root(
    sup: &Rc<Supervisor>,
    dir: &std::path::Path,
    root: &WorkerId,
    storage: Storage,
) -> Root {
    let old = dir.join("old");
    let new = dir.join("new");
    std::fs::create_dir(&old).unwrap();
    std::fs::create_dir(&new).unwrap();
    let script = agent_script(&old);
    let new_script = agent_script(&new);
    let thread = sup.state.borrow().roots[root].row.root_thread_id.clone();
    let (store, writer) = match storage {
        Storage::Direct => {
            let provider = sup
                .services
                .handle
                .reader()
                .add_provider_thread(&thread, "claude", "default")
                .unwrap();
            (
                SessionStore {
                    handle: sup.services.handle.clone(),
                    provider: Some(Some(provider)),
                },
                None,
            )
        }
        Storage::Writer => {
            let path = dir.join("threads.sqlite");
            let reader = Rc::new(Store::open(&path).unwrap());
            reader
                .create_thread_with_id(&thread, &old.to_string_lossy(), Some("root"))
                .unwrap();
            let provider = reader
                .add_provider_thread(&thread, "claude", "default")
                .unwrap();
            let writer = StoreWriter::spawn(&path).unwrap();
            (
                SessionStore {
                    handle: StoreHandle::Queued {
                        reader,
                        writer: writer.clone(),
                    },
                    provider: Some(Some(provider)),
                },
                Some(writer),
            )
        }
    };
    let turns_ended: Rc<Cell<usize>> = Rc::default();
    let open = OpenSession {
        program: "/bin/sh".into(),
        extra_args: vec![script.to_string_lossy().into_owned()],
        cwd: old.to_string_lossy().into_owned(),
        model: None,
        effort: None,
        mode: Mode::Ask,
        resume: None,
        new_session_id: None,
        approval_hook: false,
        delegation: None,
    };
    let session = ChatSession::with_options(
        Box::new(agent_core::claude::ClaudeAdapter::new()),
        open,
        store,
        thread.clone(),
        {
            let ended = turns_ended.clone();
            Rc::new(move |e: &Envelope| {
                if matches!(e.event, agent_core::event::Event::TurnCompleted { .. }) {
                    ended.set(ended.get() + 1);
                }
            })
        },
        Err(None),
        crate::agent_proc::AgentEnv::default(),
        crate::delegation::seam::SessionOptions {
            generation_base: sup.root_generation_base(&thread),
            hooks: None,
            child: None,
        },
    );
    let program = new_script.to_string_lossy().into_owned();
    session.set_adapter_factory(Rc::new(move |driver| {
        Ok(AgentLaunch {
            adapter: Box::new(Relabelled {
                inner: agent_core::claude::ClaudeAdapter::new(),
                driver,
            }),
            program: "/bin/sh".into(),
            extra_args: vec![program.clone()],
            default_model: None,
            default_effort: None,
            env: crate::agent_proc::AgentEnv::default(),
            approval: Err(None),
        })
    }));
    sup.attach_root_session(&thread, Rc::downgrade(&session));
    let r = Root {
        old,
        new,
        session,
        thread,
        writer,
        serial: Rc::default(),
    };
    // Claude's prompt-less first turn has ended: the next turn is the request's.
    pump_until(|| r.idle() && turns_ended.get() >= 1).await;
    r.hold(true);
    r.session.send_prompt("REQUEST: port the parser");
    pump_until(|| Root::sent(&r.old).contains("REQUEST: port the parser")).await;
    assert!(r.session.status().running_turn);
    r
}

/// Longer than any continuation needs: what has not happened by then is not merely late.
async fn settle() {
    glib::timeout_future(Duration::from_millis(400)).await;
}

/// A busy switch on a root continues its task exactly once on the new process, with the
/// handoff, for a model switch (same agent) and an agent switch, in both orders of the switch's
/// read and the rebind's end.
#[test]
fn a_busy_root_switch_continues_once_whichever_finishes_first() {
    for storage in [Storage::Direct, Storage::Writer] {
        for driver in [Driver::Claude, Driver::Codex] {
            in_context(async {
                let (dir, sup, root) = harness();
                let r = busy_root(&sup, dir.path(), &root, storage).await;
                r.hold_writer(600);
                r.hold(false);
                r.switch_as_the_window_does(&sup, driver, "opus").await;
                pump_until(|| r.continuations() == 1).await;
                settle().await;
                let case = format!("{storage:?} to {driver:?}");
                assert_eq!(r.continuations(), 1, "continued once ({case})");
                // On the new process, with the handoff of the interrupted request.
                let (dir_sent, launches) = if driver == Driver::Claude {
                    (Root::sent(&r.old), Root::launches(&r.old))
                } else {
                    (Root::sent(&r.new), Root::launches(&r.new))
                };
                let continuation = dir_sent
                    .lines()
                    .find(|l| l.contains(CONTINUATION))
                    .unwrap_or_default()
                    .to_owned();
                assert!(
                    continuation.contains("REQUEST: port the parser"),
                    "{case}: {continuation}"
                );
                assert!(
                    launches.last().is_some_and(|l| l.contains("opus")),
                    "{case}: the new process runs the chosen model: {launches:?}"
                );
                assert_eq!(r.session.status().driver, driver, "{case}");
                pump_until(|| r.idle()).await;
                r.session.shutdown(false);
                stop(&sup).await;
            });
        }
    }
}

/// A switch picked while an earlier one still prepares overtakes it: the earlier one never
/// switches nor continues; the interrupted task continues once, on the later choice.
#[test]
fn a_superseded_root_switch_never_continues() {
    in_context(async {
        let (dir, sup, root) = harness();
        let r = Rc::new(busy_root(&sup, dir.path(), &root, Storage::Direct).await);
        r.hold(false);
        let first = {
            let (r, sup) = (r.clone(), sup.clone());
            glib::spawn_future_local(async move {
                r.switch_as_the_window_does(&sup, Driver::Claude, "first-choice")
                    .await;
            })
        };
        // The first pick has stopped the root and waits for its exit when the second comes.
        pump_until(|| r.serial.get() == 1 && !r.session.status().alive).await;
        r.switch_as_the_window_does(&sup, Driver::Claude, "second-choice")
            .await;
        first.await.unwrap();
        pump_until(|| r.continuations() == 1).await;
        settle().await;
        assert_eq!(r.continuations(), 1);
        let launches = Root::launches(&r.old);
        assert!(
            launches.last().is_some_and(|l| l.contains("second-choice")),
            "{launches:?}"
        );
        assert!(
            launches.iter().all(|l| !l.contains("first-choice")),
            "the overtaken choice never launched: {launches:?}"
        );
        pump_until(|| r.idle()).await;
        r.session.shutdown(false);
        stop(&sup).await;
    });
}

/// A turn that finished on its own before the switch was prepared is not redone: the switch is
/// an idle one, in both orders.
#[test]
fn a_root_switch_after_the_turn_finished_continues_nothing() {
    for storage in [Storage::Direct, Storage::Writer] {
        in_context(async {
            let (dir, sup, root) = harness();
            let r = busy_root(&sup, dir.path(), &root, storage).await;
            r.hold(false);
            pump_until(|| r.idle()).await;
            r.hold_writer(600);
            r.switch_as_the_window_does(&sup, Driver::Claude, "opus")
                .await;
            pump_until(|| {
                r.idle()
                    && Root::launches(&r.old)
                        .last()
                        .is_some_and(|l| l.contains("opus"))
            })
            .await;
            settle().await;
            assert_eq!(r.continuations(), 0, "{storage:?}");
            r.session.shutdown(false);
            stop(&sup).await;
        });
    }
}
