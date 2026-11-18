//! A thread on another machine: its agent runs through the user's `ssh` in batch mode with every
//! forwarding off, in nothing of this machine's profile, socket or delegation binding; agy and
//! the Local agent are refused before anything runs; a remote card offers Allow once and Deny,
//! and no rule ever answers it. No test connects anywhere: `ssh` is a recording fake
//! ([`set_test_ssh`]), and a test that installs none runs `false`.

use super::*;
use agent_core::claude::ClaudeAdapter;
use agent_core::event::ResponseCapability;
use agent_core::remote::{RemoteFolder, RemoteProbe};

const FOLDER: &str = "/srv/my 'app' $x";

fn launch(cli: &str) -> RemoteLaunch {
    RemoteLaunch {
        folder: RemoteFolder::new("box", FOLDER).expect("a folder"),
        probe: RemoteProbe {
            shell: "/bin/bash".into(),
            path: "/opt/bin:/usr/bin".into(),
            cli: format!("/opt/bin/{cli}"),
        },
    }
}

/// A fake `ssh` in `dir` that writes what it was started with (its exported environment, then
/// its arguments, one a line) with shell builtins only, and exits `code`. It runs nothing else.
fn fake_ssh(dir: &Path, code: i32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("ssh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexport -p > '{d}/env'\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done \
             > '{d}/argv'\nexit {code}\n",
            d = dir.display()
        ),
    )
    .expect("the fake ssh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    script
}

fn remote_session(
    adapter: Box<dyn Adapter>,
    store: &Rc<Store>,
    env: LaunchEnv,
    cli: &str,
) -> (Rc<ChatSession>, Seen) {
    let thread = fresh(store, &RemoteFolder::new("box", FOLDER).unwrap().cwd());
    let (sink, seen) = make_sink();
    let session = ChatSession::remote(
        adapter,
        open_session(cli, Vec::new(), ""),
        store.clone(),
        thread,
        sink,
        env,
        launch(cli),
    );
    (session, seen)
}

#[test]
fn a_remote_agent_runs_through_batch_ssh_with_nothing_of_this_machine() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        set_test_ssh(&fake_ssh(tmp.path(), 0).to_string_lossy());
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let env = LaunchEnv {
            // A profile's environment file would put this here: it never reaches ssh.
            env: vec![("AT_PROFILE_SECRET".into(), "s3cret-value".into())],
            // `clear_env`: removed from what ssh inherits.
            unset: vec!["HOME".into()],
        };
        let (session, seen) = remote_session(Box::new(ClaudeAdapter::new()), &store, env, "claude");
        assert!(session.is_remote());
        assert!(
            pump_until(ctx, 10, || exited_count(&seen) >= 1),
            "the fake ssh ran and exited"
        );
        let argv = std::fs::read_to_string(tmp.path().join("argv")).expect("argv");
        let argv: Vec<&str> = argv.lines().collect();
        let options = agent_core::remote::BATCH_OPTIONS;
        assert_eq!(argv[..options.len()], *options, "batch options first");
        assert_eq!(argv[options.len()..options.len() + 2], ["--", "box"]);
        for needed in [
            "BatchMode=yes",
            "ClearAllForwardings=yes",
            "ForwardAgent=no",
            "ForwardX11=no",
            "PermitLocalCommand=no",
            "Tunnel=no",
            "RemoteCommand=none",
        ] {
            assert!(argv.contains(&needed), "{needed}");
        }
        assert!(!argv.iter().any(|a| *a == "-t" || *a == "-tt"));
        // One remote command line after the host, everything in it quoted.
        assert_eq!(argv.len(), options.len() + 3, "{argv:?}");
        let line = argv[argv.len() - 1];
        let prefix = format!(
            "cd -- {} && exec env 'PATH=/opt/bin:/usr/bin' '/opt/bin/claude' ",
            agent_core::remote::sh_quote(FOLDER)
        );
        assert!(line.starts_with(&prefix), "{line}");
        assert!(
            line.contains("'--permission-prompt-tool' 'stdio'"),
            "{line}"
        );
        assert!(!line.contains("thinking"), "no locally probed flag: {line}");
        assert!(
            !line.contains("--replay-user-messages"),
            "no delegation: {line}"
        );
        // Nothing of the profile, the approval socket or delegation in ssh's environment.
        let env = std::fs::read_to_string(tmp.path().join("env")).expect("env");
        assert!(!env.contains("AT_PROFILE_SECRET") && !env.contains("s3cret-value"));
        assert!(!env.contains("export HOME="), "clear_env applies: {env}");
        assert!(!env.contains(agent_core::approval::ENV_SOCKET));
        for name in agent_core::delegation::env::STRIP_ON_SPAWN {
            assert!(!env.contains(&format!("export {name}=")), "{name}");
        }
        // The adapter was given no local folder (Codex would send it as a path there).
        assert_eq!(session.inner.open.borrow().cwd, "");
    });
}

#[test]
fn agy_and_the_local_agent_are_refused_before_anything_runs() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        set_test_ssh(&fake_ssh(tmp.path(), 0).to_string_lossy());
        let store = Rc::new(Store::open_in_memory().expect("store"));
        for (adapter, why) in [
            (
                Box::new(AgyAdapter::new("agy")) as Box<dyn Adapter>,
                "Antigravity cannot run a thread on another machine",
            ),
            (
                Box::new(agent_core::local_agent::LocalAdapter::new()),
                "The Local agent cannot run a thread on another machine",
            ),
        ] {
            let (session, seen) = remote_session(adapter, &store, LaunchEnv::default(), "agy");
            pump_until(ctx, 1, || false);
            assert!(
                has(
                    &seen,
                    |e| matches!(e, Event::Error { message } if message.contains(why))
                ),
                "{why}"
            );
            // A message is refused the same way: still nothing runs.
            session.send_prompt("hello");
            pump_until(ctx, 1, || false);
            assert!(!tmp.path().join("argv").exists(), "ssh never ran");
            assert!(session.inner.proc.borrow().is_none());
        }
    });
}

#[test]
fn ssh_failing_itself_says_the_machine_is_out_of_reach() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        set_test_ssh(&fake_ssh(tmp.path(), 255).to_string_lossy());
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let (_session, seen) = remote_session(
            Box::new(ClaudeAdapter::new()),
            &store,
            LaunchEnv::default(),
            "claude",
        );
        assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
            e,
            Event::Notice { text } if text.starts_with("Could not reach box.")
        ))));
    });
}

fn card(tool: &str, options: Vec<Decision>) -> Envelope {
    Envelope::new(Event::ApprovalRequested {
        tool: tool.into(),
        title: None,
        input: serde_json::json!({"command": "ls", "file_path": "/srv/my 'app' $x/a.rs"}),
        reason: None,
        options,
        response: ResponseCapability::Live,
        remembers: None,
    })
    .request("r1")
}

fn options_of(env: &Envelope) -> Vec<Decision> {
    match &env.event {
        Event::ApprovalRequested { options, .. } => options.clone(),
        other => panic!("not a card: {other:?}"),
    }
}

#[test]
fn a_remote_card_offers_allow_once_and_deny_and_no_rule_answers_it() {
    in_loop(|_| {
        // No fake installed: the session's ssh is `false`, which connects nowhere.
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let (session, _) = remote_session(
            Box::new(ClaudeAdapter::new()),
            &store,
            LaunchEnv::default(),
            "claude",
        );
        let every = vec![
            Decision::Allow,
            Decision::AllowForSession,
            Decision::AllowAlways,
            Decision::Deny,
            Decision::Cancel,
        ];
        for tool in ["Bash", "Edit", "mcp__server__tool"] {
            let (shown, auto) = session.inner.gate_approval(card(tool, every.clone()));
            assert_eq!(
                options_of(&shown),
                [Decision::Allow, Decision::Deny],
                "{tool}"
            );
            assert!(!auto, "{tool}: nothing answers a remote card but the user");
            assert!(
                matches!(
                    &shown.event,
                    Event::ApprovalRequested {
                        remembers: None,
                        ..
                    }
                ),
                "{tool}: no rule is described"
            );
        }
        assert!(
            session.inner.pending_rules.borrow().is_empty(),
            "no rule is recorded to keep from an answer"
        );
        // Not even in dangerous permissive mode.
        session.inner.state.borrow_mut().mode = Mode::Bypass;
        let (_, auto) = session.inner.gate_approval(card("Bash", every.clone()));
        assert!(!auto);
        session.inner.state.borrow_mut().mode = Mode::Ask;

        // A Codex permission request names paths there: it can only be denied.
        let permission = card(agent_core::codex::PERMISSIONS_TOOL, every.clone());
        let generation = session.inner.generation.get();
        let held = session
            .inner
            .hold_codex_permissions(generation, permission)
            .expect("shown at once, never checked on this disk");
        let (shown, auto) = session.inner.gate_approval(held);
        assert_eq!(options_of(&shown), [Decision::Deny]);
        assert!(!auto);
    });
}

#[test]
fn codex_there_takes_no_images_and_a_viewed_file_is_never_read_here() {
    in_loop(|_| {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let (codex, seen) = remote_session(
            Box::new(agent_core::codex::CodexAdapter::new()),
            &store,
            LaunchEnv::default(),
            "codex",
        );
        let status = codex.status();
        assert!(!status.capabilities.image_input);
        assert_eq!(
            status.images_refused,
            Some(agent_core::remote::CODEX_IMAGES_REFUSED)
        );
        let (claude, _) = remote_session(
            Box::new(ClaudeAdapter::new()),
            &store,
            LaunchEnv::default(),
            "claude",
        );
        assert!(claude.status().capabilities.image_input);
        assert_eq!(claude.status().images_refused, None);

        // An image Codex viewed there, at a path this machine also has: never read here.
        let tmp = tempfile::tempdir().expect("tmp");
        let local = tmp.path().join("same.png");
        std::fs::write(&local, b"not read").expect("file");
        codex.inner.emit(Envelope::new(Event::ToolImage {
            image: agent_core::attachment::ToolImageSource::File {
                path: local.to_string_lossy().into_owned(),
            },
        }));
        assert!(has(&seen, |e| matches!(
            e,
            Event::Notice { text } if text == REMOTE_TOOL_IMAGE
        )));
        assert!(!has(&seen, |e| matches!(e, Event::Attachments { .. })));
    });
}
