//! The Local runner's side of delegation, on the real binary over its pipes: its capability
//! arrives only in its startup frame, its own client connects from its own process, every call
//! is shown under its own key before it is sent, and nothing it runs ever holds the capability.
//! A loopback model and a stand-in delegation socket; no display, no network.
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agent_core::adapter::Command as AgentCommand;
use agent_core::delegation::correlation::InvocationKey;
use agent_core::delegation::ipc::CallOutcome;
use agent_core::delegation::launch::LocalBridge;
use agent_core::delegation::{CapabilityToken, InstanceNonce};
use agent_core::event::{Envelope, Event, ItemKind};
use agent_kit::test_support::{FakeDelegation, ScriptedModel};
use serde_json::json;

fn proc_text(pid: u32, file: &str) -> String {
    String::from_utf8_lossy(&std::fs::read(format!("/proc/{pid}/{file}")).unwrap_or_default())
        .into_owned()
}

fn tree_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => out.push_str(&String::from_utf8_lossy(
                    &std::fs::read(entry.path()).unwrap_or_default(),
                )),
                _ => {}
            }
        }
    }
    out
}

/// The runner is not dumpable: a command it runs cannot read its environment (where the model
/// server's key was given) nor its memory, and neither can any other process of this user.
#[test]
fn a_command_cannot_read_the_runners_environment_or_memory() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("running as root, which reads any process's files; skipping");
        return;
    }
    let key = format!("proof-server-key-{}", std::process::id());
    let model = ScriptedModel::start(vec![
        ScriptedModel::calls(&[(
            "run_command",
            json!({"command": "cat /proc/$PPID/environ; echo; head -c 16 /proc/$PPID/mem; echo end"}),
        )]),
        ScriptedModel::text("done"),
    ]);
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-terminal"));
    command
        .args([
            "--local-agent",
            "--server",
            &model.base,
            "--key-env",
            "PROOF_SERVER_KEY",
            "--cwd",
            work.path().to_str().unwrap(),
            "--mode",
            "bypass",
            "--model",
            "fixture",
        ])
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("PROOF_SERVER_KEY", &key);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, events) = std::sync::mpsc::channel::<Envelope>();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if let Ok(envelope) = serde_json::from_str(&line) {
                if tx.send(envelope).is_err() {
                    break;
                }
            }
        }
    });
    for command in [
        AgentCommand::configure_local(Default::default()),
        AgentCommand::Prompt {
            attachments: Vec::new(),
            text: "read your parent".into(),
        },
    ] {
        writeln!(input, "{}", serde_json::to_string(&command).unwrap()).unwrap();
        input.flush().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut refused_here = None;
    loop {
        let envelope = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the runner went quiet");
        if refused_here.is_none() && matches!(envelope.event, Event::SessionStarted { .. }) {
            // This process (the same user) is refused too.
            refused_here = Some(
                std::fs::read(format!("/proc/{pid}/environ"))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied),
            );
        }
        if matches!(envelope.event, Event::TurnCompleted { .. }) {
            break;
        }
    }
    drop(input);
    let _ = child.wait();
    let _ = reader.join();
    assert_eq!(refused_here, Some(true), "the runner's environ is readable");
    let bodies = model.bodies();
    let output = bodies[1]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(output.contains("end"), "the command ran: {output}");
    assert!(output.contains("Permission denied"), "{output}");
    assert!(!output.contains(&key), "the key leaked: {output}");
}

#[test]
fn the_runner_binds_delegation_only_through_its_startup_frame() {
    let sock = tempfile::tempdir().unwrap();
    let fake = FakeDelegation::start(
        sock.path(),
        Box::new(|_| Some(CallOutcome::Ok(json!({"workers": []})))),
    );
    let model = ScriptedModel::start(vec![
        ScriptedModel::calls(&[("cross_agent_list", json!({}))]),
        ScriptedModel::calls(&[("run_command", json!({"command": "env"}))]),
        ScriptedModel::text("listed"),
    ]);
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let token = CapabilityToken::from_random([5; 32]);
    let secret = token.expose_secret().to_owned();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-terminal"));
    command
        .args([
            "--local-agent",
            "--server",
            &model.base,
            "--cwd",
            work.path().to_str().unwrap(),
            "--mode",
            "bypass",
            "--model",
            "fixture",
        ])
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CONFIG_HOME", home.path().join("config"));
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, events) = std::sync::mpsc::channel::<(Instant, Envelope)>();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if let Ok(envelope) = serde_json::from_str(&line) {
                if tx.send((Instant::now(), envelope)).is_err() {
                    break;
                }
            }
        }
    });
    let mut send = |command: &AgentCommand| {
        writeln!(input, "{}", serde_json::to_string(command).unwrap()).unwrap();
        input.flush().unwrap();
    };
    send(&AgentCommand::ConfigureLocal {
        context: Box::default(),
        bridge: Some(Box::new(LocalBridge {
            endpoint: fake.path.display().to_string(),
            instance: InstanceNonce::from_bytes([2; 16]),
            token,
        })),
        child: None,
    });
    send(&AgentCommand::Prompt {
        attachments: Vec::new(),
        text: "who works for me?".into(),
    });
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    // The runner stays up for the whole turn: its /proc files are read while it works.
    let mut scanned = None;
    loop {
        let (at, envelope) = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the runner went quiet");
        if scanned.is_none()
            && matches!(
                &envelope.event,
                Event::ItemStarted {
                    kind: ItemKind::Command,
                    ..
                }
            )
        {
            scanned = Some((proc_text(pid, "environ"), proc_text(pid, "cmdline")));
        }
        let done = matches!(envelope.event, Event::TurnCompleted { .. });
        seen.push((at, envelope));
        if done {
            break;
        }
    }
    // Closing stdin ends the runner.
    drop(input);
    let _ = child.wait();
    let _ = reader.join();

    // Its own client connected from its own process, with the frame's capability.
    let hellos = fake.hellos();
    assert_eq!(hellos.len(), 1);
    assert_eq!(hellos[0].0.cap.expose_secret(), secret);
    assert_eq!(hellos[0].0.bridge.pid, pid);
    assert_eq!(hellos[0].1, Some(pid), "SO_PEERCRED: the runner itself");
    // The call went out under the key of the item it showed first, with its arguments.
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    let Some(InvocationKey::Local { item_id }) = calls[0].invocation.clone() else {
        panic!("no Local key: {:?}", calls[0].invocation);
    };
    let shown = seen
        .iter()
        .find(|(_, e)| e.item.as_deref() == Some(item_id.as_str()));
    let (_, shown) = shown.expect("the call's item was shown");
    match &shown.event {
        Event::ItemStarted {
            kind: ItemKind::McpTool,
            input: Some(input),
            ..
        } => {
            assert_eq!(input["server"], "agent_terminal");
            assert_eq!(input["tool"], "cross_agent_list");
            assert_eq!(input["arguments"], calls[0].args);
        }
        other => panic!("not the call's item: {other:?}"),
    }
    // The capability is in no environment, argument, file or line.
    let (environ, cmdline) = scanned.expect("the command ran");
    for text in [&environ, &cmdline] {
        assert!(!text.contains(&secret));
        assert!(!text.contains("AGENT_TERMINAL_DELEGATION"));
    }
    let bodies = model.bodies();
    let command_output = bodies[2]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(command_output.contains("PATH="), "{command_output}");
    assert!(!command_output.contains(&secret));
    for body in &bodies {
        assert!(!body.to_string().contains(&secret));
    }
    for (_, envelope) in &seen {
        assert!(!serde_json::to_string(envelope).unwrap().contains(&secret));
    }
    assert!(!tree_text(home.path()).contains(&secret));
}
