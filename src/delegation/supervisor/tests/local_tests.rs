//! The Local agent in delegation, end to end: the real runner (this build's binary) on a
//! scripted loopback model server, reaching the supervisor through the real socket.
use super::*;
use crate::chat::ChatBackend;
use agent_kit::test_support::ScriptedModel;
use serde_json::json;

/// This build's own binary, beside the test binary: `make test` builds it with the tests. A
/// missing one fails, never skips.
fn runner() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let target = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target directory");
    let runner = target.join("agent-terminal");
    assert!(
        runner.is_file(),
        "the app binary is not built: {} (make test builds it)",
        runner.display()
    );
    // A runner older than its sources is not the code under test (`cargo test --bin` does not
    // rebuild it). Its own test harness is no guide: cargo links the two in either order.
    let built = std::fs::metadata(&runner)
        .and_then(|m| m.modified())
        .expect("runner mtime");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = vec![root.join("src")];
    for entry in std::fs::read_dir(root.join("crates"))
        .expect("crates")
        .flatten()
    {
        sources.push(entry.path().join("src"));
    }
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    while let Some(dir) = sources.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                        newest = Some((modified, path));
                    }
                }
            }
        }
    }
    if let Some((modified, path)) = newest {
        assert!(
            built >= modified,
            "the runner {} is older than {}: rebuild it (make test does)",
            runner.display(),
            path.display()
        );
    }
    runner
}

/// Makes the harness's root a Local principal whose token was issued to process `runner`, and
/// returns that token.
fn local_principal(sup: &Rc<Supervisor>, root: &WorkerId, runner: u32) -> CapabilityToken {
    let cap = CapabilityToken::from_random([71; 32]);
    let epoch = sup.epoch(root).unwrap();
    let mut state = sup.state.borrow_mut();
    let w = state.workers.get_mut(root).unwrap();
    w.driver = Driver::Local;
    let generation = w.generation;
    w.token = Some(TokenSlot::new(epoch, generation, cap.clone(), Some(runner)));
    cap
}

fn local_hello(sup: &Rc<Supervisor>, cap: &CapabilityToken, pid: u32) -> Hello {
    Hello {
        api: ApiRange { min: 1, max: 1 },
        instance: sup.instance.clone(),
        cap: cap.clone(),
        bridge: BridgeInfo {
            version: "test".into(),
            pid,
        },
    }
}

/// G-L2 on the real supervisor: the worker's own valid token, presented from any process but
/// its runner (whatever pid the hello claims), is refused; from its runner it is accepted.
#[test]
fn a_local_token_from_another_process_is_refused() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let me = std::process::id();
        let runner = me.wrapping_add(1);
        let cap = local_principal(&sup, &root, runner);
        for (conn, claimed) in [(ConnId(81), me), (ConnId(82), runner)] {
            let refused = sup
                .authenticate(
                    conn,
                    local_hello(&sup, &cap, claimed),
                    PeerFacts { pid: Some(me) },
                )
                .await
                .expect_err("not its runner");
            assert_eq!(refused.code, ErrorCode::AuthenticationFailed, "{refused:?}");
        }
        // Its runner speaking for itself, but naming another pid in its hello: refused too.
        let lying = sup
            .authenticate(
                ConnId(83),
                local_hello(&sup, &cap, me),
                PeerFacts { pid: Some(runner) },
            )
            .await
            .expect_err("a hello naming another pid");
        assert_eq!(lying.code, ErrorCode::AuthenticationFailed);
        sup.authenticate(
            ConnId(84),
            local_hello(&sup, &cap, runner),
            PeerFacts { pid: Some(runner) },
        )
        .await
        .expect("its own runner");
    });
}

/// G-L3 on the real supervisor: a Local call whose key its stream never showed, or that names
/// none, is denied as unattested; the same call shown first in its stream is accepted.
#[test]
fn an_unobserved_local_call_is_denied() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let me = std::process::id();
        let cap = local_principal(&sup, &root, me);
        sup.authenticate(
            ConnId(85),
            local_hello(&sup, &cap, me),
            PeerFacts { pid: Some(me) },
        )
        .await
        .expect("its own runner");
        let keyed = |id: u64, key: Option<&str>| {
            let mut call = caps_call(id, false);
            call.invocation =
                key.map(|k| agent_core::delegation::InvocationKey::Local { item_id: k.into() });
            call
        };
        for (id, key) in [(1, Some("call-abcd1234-9")), (2, None)] {
            match sup.call(ConnId(85), keyed(id, key)).await {
                agent_core::delegation::ipc::CallOutcome::Err(e) => {
                    assert_eq!(e.code, ErrorCode::AuthorityDenied, "{e:?}");
                    assert_eq!(
                        e.details.as_ref().and_then(|d| d.get("reason")),
                        Some(&json!("unattested")),
                        "{e:?}"
                    );
                }
                agent_core::delegation::ipc::CallOutcome::Ok(v) => {
                    panic!("an unobserved call ran: {v}")
                }
            }
        }
        // Shown in its stream first, under that key and with those arguments: accepted.
        let call = keyed(3, Some("call-abcd1234-1"));
        let line = json!({"event": {"type": "item_started", "kind": "mcp_tool",
            "title": "cross_agent_capabilities",
            "input": {"server": "agent_terminal", "tool": "cross_agent_capabilities",
                      "arguments": call.args.clone()}},
            "item": "call-abcd1234-1"})
        .to_string();
        let generation = sup.state.borrow().workers[&root].generation;
        let hooks = super::super::super::attest::Hooks {
            supervisor: Rc::downgrade(&sup),
            worker: root.clone(),
        };
        crate::delegation::seam::DelegationHooks::line(&hooks, generation, &line, false);
        assert!(matches!(
            sup.call(ConnId(85), call).await,
            agent_core::delegation::ipc::CallOutcome::Ok(_)
        ));
    });
}

/// A harness whose `local` profile runs the real runner on `model`, with the IPC socket bound
/// as the app binds it.
fn local_harness(model: &ScriptedModel) -> (tempfile::TempDir, Rc<Supervisor>, WorkerId) {
    TEST_LOCAL.with(|l| *l.borrow_mut() = Some((runner(), model.base.clone())));
    let (dir, sup, root) = harness();
    let server = crate::delegation::ipc::IpcServer::bind(
        &sup.rundir,
        sup.services.app_pid,
        sup.limits,
        sup.clone(),
    )
    .expect("bind the delegation socket");
    *sup.ipc.borrow_mut() = Some(server);
    (dir, sup, root)
}

/// Waits for `worker`'s run to complete; on a failure or a timeout, says where it stood and
/// what the model was sent.
async fn finished(sup: &Rc<Supervisor>, worker: &WorkerId, model: &ScriptedModel) {
    let start = Instant::now();
    loop {
        let (state, reason, status, ready) = {
            let state = sup.state.borrow();
            let w = &state.workers[worker];
            (
                w.row.state.clone(),
                w.row.delegation_reason.clone(),
                w.row.delegation_status.clone(),
                w.readback.ready,
            )
        };
        if state == "completed" {
            return;
        }
        if state == "failed" || start.elapsed() > Duration::from_secs(30) {
            panic!(
                "worker {state} ({status}, ready {ready}): {reason:?}; the model saw {} \
                 request(s): {:?}",
                model.bodies().len(),
                model
                    .bodies()
                    .iter()
                    .map(|b| b["messages"].as_array().and_then(|m| m.last()).cloned())
                    .collect::<Vec<_>>()
            );
        }
        glib::timeout_future(Duration::from_millis(10)).await;
    }
}

/// The text of `pid`'s `/proc` file, lossily.
fn proc_text(pid: u32, file: &str) -> String {
    String::from_utf8_lossy(&std::fs::read(format!("/proc/{pid}/{file}")).unwrap_or_default())
        .into_owned()
}

/// Every file under `dir`, as text, lossily.
fn tree_text(dir: &std::path::Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() && !t.is_symlink() => stack.push(path),
                Ok(t) if t.is_file() => {
                    out.push_str(&String::from_utf8_lossy(
                        &std::fs::read(&path).unwrap_or_default(),
                    ));
                }
                _ => {}
            }
        }
    }
    out
}

/// A Local child on the real runner: it writes in its own worktree (its Accept edits answered
/// by the app, inside the lease), is refused an agent launch by its inherited authority,
/// reports to its parent through its own pinned and attested client, and its final reply is
/// its result in its parent's mailbox. Its capability is in no environment, argument or file.
#[test]
fn a_local_child_runs_on_the_real_runner_and_reports_to_its_parent() {
    if !agent_kit::test_support::git_or_skip() {
        return;
    }
    let model = ScriptedModel::start(vec![
        ScriptedModel::calls(&[(
            "write_file",
            json!({"file_path": "notes.txt", "content": "hello from local"}),
        )]),
        ScriptedModel::calls(&[("run_command", json!({"command": "claude -p hi"}))]),
        ScriptedModel::calls(&[(
            "cross_agent_send",
            json!({"target": "parent", "text": "halfway there", "kind": "report",
                   "delivery": "next_turn", "idempotency_key": "local-report-1"}),
        )]),
        ScriptedModel::text("done: notes written"),
    ]);
    in_context(async {
        let (dir, sup, root) = local_harness(&model);
        agent_kit::test_support::init_repo_at(dir.path(), &[("README.md", b"x\n")]);
        sup.state.borrow_mut().workers.get_mut(&root).unwrap().mode = Mode::AcceptEdits;
        let receipt = sup
            .spawn_worker(
                &root,
                SpawnRequest {
                    profile_id: ProfileId::parse("local").unwrap(),
                    name: "local-worker".into(),
                    task: "write the notes".into(),
                    task_mode: TaskMode::AcceptEdits,
                    capabilities: None,
                    model: None,
                    effort: Some("high".into()),
                    workspace: WorkspaceRequest::IsolatedWorktree { base: None },
                    expected_authority_epoch: None,
                    idempotency_key: IdempotencyKey::parse("local-spawn-child").unwrap(),
                },
            )
            .await
            .expect("spawned");
        let worker = spawned(&receipt);
        // The Local agent has no effort: ignored, said, never passed.
        assert!(
            receipt.to_string().contains("effort_not_supported"),
            "{receipt}"
        );
        finished(&sup, &worker, &model).await;
        let lease = sup.state.borrow().child_specs[&worker].cwd.clone();
        assert_eq!(
            std::fs::read_to_string(lease.join("notes.txt")).unwrap(),
            "hello from local"
        );
        assert!(
            !dir.path().join("notes.txt").exists(),
            "only in its worktree"
        );
        let bodies = model.bodies();
        assert_eq!(bodies.len(), 4, "{bodies:?}");
        let told = |n: usize| {
            bodies[n]["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        // The app's own reason reaches the model, never "the user declined".
        assert!(
            told(2).contains("cannot start their own sub-agents"),
            "{}",
            told(2)
        );
        assert!(
            !told(3).contains("\"error\""),
            "the report was accepted: {}",
            told(3)
        );
        // Its own runner's tools and none of a user's MCP servers; never more than its mode.
        let offered: Vec<String> = bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
            .collect();
        assert!(offered.contains(&"write_file".to_owned()));
        assert!(offered.contains(&"cross_agent_send".to_owned()));
        assert!(!offered.iter().any(|t| t.starts_with("mcp_")));
        // Its result and its report reach the parent.
        connect(&sup, &root, ConnId(61)).await;
        let page = tool(
            &sup,
            ConnId(61),
            1,
            ToolRequest::Wait(agent_core::delegation::wire::WaitRequest {
                workers: Some(vec![worker.clone()]),
                mailbox: Some(true),
                after: None,
                timeout_ms: Some(0),
                max_events: None,
                max_bytes: None,
            }),
        )
        .await
        .to_string();
        assert!(page.contains("done: notes written"), "{page}");
        assert!(page.contains("halfway there"), "{page}");
        // The capability is in the runner's memory only.
        let pid = sup.state.borrow().workers[&worker]
            .token
            .as_ref()
            .and_then(|t| t.pid)
            .expect("the child's runner is up");
        assert!(!proc_text(pid, "environ").contains("atdc_"));
        assert!(!proc_text(pid, "environ").contains("AGENT_TERMINAL_DELEGATION"));
        assert!(!proc_text(pid, "cmdline").contains("atdc_"));
        assert!(!tree_text(&dir.path().join("state")).contains("atdc_"));
        stop(&sup).await;
    });
}

/// A Local root on the real runner spawns a sub-agent (a scripted Claude), waits for it and
/// gets its result, through its own pinned and attested client.
#[test]
fn a_local_root_spawns_a_sub_agent_and_hears_its_result() {
    let model = ScriptedModel::start(vec![
        ScriptedModel::calls(&[(
            "cross_agent_spawn",
            json!({"profile_id": "fixture", "name": "helper", "task": "answer the question",
                   "task_mode": "plan", "workspace": {"kind": "shared_read_only"},
                   "idempotency_key": "local-root-spawn-1"}),
        )]),
        // A wait returns on any new event, so the first page may hold only the child's progress:
        // wait again, as a model would, before answering.
        ScriptedModel::calls(&[("cross_agent_wait", json!({"timeout_ms": 8000}))]),
        ScriptedModel::calls(&[("cross_agent_wait", json!({"timeout_ms": 3000}))]),
        ScriptedModel::text("The helper said its piece."),
    ]);
    in_context(async {
        let (dir, sup, _claude_root) = local_harness(&model);
        // The scripted Claude child's answer.
        std::fs::write(dir.path().join("task-result"), "The helper's own answer.").unwrap();
        let work = dir.path().join("local-root");
        std::fs::create_dir(&work).unwrap();
        let store = sup.services.handle.reader();
        let thread = store
            .create_thread(&work.to_string_lossy(), Some("local root"))
            .expect("thread");
        let profile = (sup.services.profiles)()
            .into_iter()
            .find(|p| p.id == "local")
            .unwrap();
        let hooks = sup
            .prepare_root(&thread, &profile, Driver::Local)
            .await
            .expect("a Local root is bound");
        let resolved = (sup.services.resolve)(&profile).unwrap();
        let provider = store
            .add_provider_thread(&thread, Driver::Local.info().key, "default")
            .unwrap();
        let open = agent_core::adapter::OpenSession {
            program: resolved.program.clone(),
            extra_args: resolved.extra_args.clone(),
            cwd: work.to_string_lossy().into_owned(),
            model: Some("fixture-model".into()),
            effort: None,
            mode: Mode::Ask,
            resume: None,
            new_session_id: None,
            approval_hook: false,
            delegation: None,
        };
        let session = ChatSession::with_options(
            Box::new(agent_core::local_agent::LocalAdapter::new()),
            open,
            crate::chat::session::SessionStore {
                handle: sup.services.handle.clone(),
                provider: Some(Some(provider)),
            },
            thread.clone(),
            Rc::new(|_| {}),
            Err(None),
            resolved.env,
            crate::delegation::seam::SessionOptions {
                generation_base: sup.root_generation_base(&thread),
                hooks: Some(hooks),
                child: None,
            },
        );
        sup.attach_root_session(&thread, Rc::downgrade(&session));
        session.send_prompt("ask a helper");
        pump_until(|| model.bodies().len() >= 4 && !session.status().running_turn).await;
        let bodies = model.bodies();
        let told = |n: usize| {
            bodies[n]["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        assert!(told(1).contains("worker_id"), "spawned: {}", told(1));
        assert!(
            told(2).contains("The helper's own answer.")
                || told(3).contains("The helper's own answer."),
            "the child's result: {} / {}",
            told(2),
            told(3)
        );
        // The root's runner offered the delegation tools and was told it may use them.
        let system = bodies[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("cross_agent_capabilities first"));
        let pid = {
            let state = sup.state.borrow();
            let id = state.threads[&thread].clone();
            state.workers[&id].token.as_ref().and_then(|t| t.pid)
        }
        .expect("the root's runner is up");
        assert!(!proc_text(pid, "environ").contains("atdc_"));
        assert!(!proc_text(pid, "cmdline").contains("atdc_"));
        drop(session);
        stop(&sup).await;
    });
}
