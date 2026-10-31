//! Real pipe/process lifecycle tests; no display, sockets or external model server.
use agent_core::{
    adapter::Mode,
    local_mcp::{ApprovalPolicy, EnvironmentValue, McpServerConfig, McpToolPolicy},
};
use agent_kit::local_mcp::{Discovery, Session};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const FIXTURE: &str = r#"
import json, os, signal, subprocess, sys, time
scenario, folder = sys.argv[1:]
def send(value):
    sys.stdout.write(json.dumps(value) + '\n'); sys.stdout.flush()
def respond(request, result):
    send({'jsonrpc':'2.0','id':request['id'],'result':result})
def tool(name='read'):
    return {'name':name,'inputSchema':{'type':'object'}}
with open(folder + '/pids', 'a') as f: f.write(str(os.getpid()) + '\n')
if scenario in ('descendant','stderr-flood','leader-exit','abrupt'):
    child = subprocess.Popen([sys.executable, '-c', 'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(60)'])
    with open(folder + '/pids', 'a') as f: f.write(str(child.pid) + '\n')
if scenario == 'leader-exit': sys.exit(0)
if scenario == 'stderr-flood':
    sys.stderr.write('x' * 300000 + '\n' + os.environ.get('TOKEN','') + '\n'); sys.stderr.flush()
if scenario in ('startup-stall','abrupt'): time.sleep(60)
page = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    with open(folder + '/requests', 'a') as f: f.write(json.dumps(request) + '\n')
    if not method: continue
    if method == 'server/discover':
        if scenario in ('legacy','legacy-silent'):
            if scenario == 'legacy':
                send({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32601,'message':'unsupported'}})
            continue
        if scenario == 'wrong-id':
            send({'jsonrpc':'2.0','id':99999,'result':{}}); continue
        if scenario == 'invalid-json':
            sys.stdout.write('bad json\n'); sys.stdout.flush(); continue
        if scenario == 'partial':
            sys.stdout.write('{"jsonrpc":'); sys.stdout.flush(); sys.exit(0)
        if scenario == 'oversize-frame':
            sys.stdout.write('x' * (1024*1024+1)); sys.stdout.flush(); continue
        if scenario == 'oversize-id':
            send({'jsonrpc':'2.0','id':'x'*257,'result':{}}); continue
        if scenario == 'reverse':
            send({'jsonrpc':'2.0','id':'reverse','method':'sampling/createMessage','params':{'secret':os.environ.get('TOKEN','')}})
        if scenario == 'notification-flood':
            for _ in range(129): send({'jsonrpc':'2.0','method':'notifications/message','params':{}})
        respond(request, {'resultType':'complete','supportedVersions':['2026-07-28'],'capabilities':{'tools':{}},'ttlMs':0,'cacheScope':'private'})
    elif method == 'initialize':
        respond(request, {'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}})
    elif method == 'tools/list':
        page += 1
        if scenario == 'discovery-stall': time.sleep(60)
        tools = [tool()]
        result = {'tools':tools}
        if scenario == 'pagination':
            result = {'tools':[tool('read' if page == 1 else 'extra')]}
            if page == 1: result['nextCursor'] = 'next'
        if scenario == 'cursor-loop': result['nextCursor'] = 'same'
        if scenario == 'cursor-long': result['nextCursor'] = 'x'*4097
        if scenario == 'page-bound': result = {'tools':[], 'nextCursor':str(page)}
        if scenario == 'tool-bound': result['tools'] = [tool(str(n)) for n in range(257)]
        if scenario == 'max-tools': result['tools'] = [tool(str(n)) for n in range(256)]
        if scenario == 'schema-bound': result['tools'] += [dict(tool('large'),description='x'*65536)]
        if scenario == 'byte-bound':
            result = {'tools':[dict(tool(str(page)),description='x'*700000)], 'nextCursor':str(page)}
        if scenario == 'duplicate': result['tools'] += [tool()]
        if scenario == 'secret-schema': result['tools'][0]['description'] = os.environ['TOKEN']
        if scenario == 'environment':
            with open(folder + '/environment','w') as f: json.dump(dict(os.environ),f)
        result.update({'resultType':'complete','ttlMs':0,'cacheScope':'private'})
        respond(request, result)
        if scenario == 'changed' and page == 1:
            send({'jsonrpc':'2.0','method':'notifications/tools/list_changed'})
    elif method == 'tools/call':
        if scenario == 'call-stall': time.sleep(60)
        if scenario == 'call-exit': sys.exit(0)
        respond(request, {'resultType':'complete','content':[{'type':'text','text':os.environ.get('TOKEN','hello')}],'isError':True})
"#;

fn config(folder: &Path, scenario: &str) -> McpServerConfig {
    McpServerConfig {
        id: "fixture".into(),
        label: "Fixture".into(),
        enabled: true,
        required: false,
        command: "/usr/bin/python3".into(),
        args: vec![
            "-u".into(),
            "-c".into(),
            FIXTURE.into(),
            scenario.into(),
            folder.to_string_lossy().into_owned(),
        ],
        cwd: folder.to_string_lossy().into_owned(),
        env: BTreeMap::new(),
        tools: vec![McpToolPolicy {
            name: "read".into(),
            read_only: true,
            approval: ApprovalPolicy::Inherit,
        }],
        approval: ApprovalPolicy::Ask,
        startup_timeout_secs: 2,
        call_timeout_secs: 1,
    }
}
fn executable() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-terminal"))
}
fn session(configs: Vec<McpServerConfig>, env: BTreeMap<String, String>) -> Session {
    Session::for_test(configs, executable(), env).unwrap()
}
fn discover(session: &Session) -> Discovery {
    session
        .discover(Mode::Ask)
        .unwrap()
        .recv_timeout(Duration::from_secs(20))
        .unwrap()
}
fn pids(folder: &Path) -> Vec<i32> {
    std::fs::read_to_string(folder.join("pids"))
        .unwrap_or_default()
        .lines()
        .map(|s| s.parse().unwrap())
        .collect()
}
fn alive(pid: i32) -> bool {
    // Linux can retain an adopted zombie until PID 1 reaps it; that process cannot run
    // or hold a pipe, so it counts as dead for the process-group ownership assertion.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}
fn assert_dead(folder: &Path) {
    let until = Instant::now() + Duration::from_secs(4);
    while pids(folder).iter().any(|pid| alive(*pid)) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!pids(folder).is_empty(), "fixture did not start");
    assert!(
        pids(folder).iter().all(|pid| !alive(*pid)),
        "fixture processes survived cleanup"
    );
}

#[test]
fn modern_and_legacy_startup_paginate_without_changing_bridge_lifecycle() {
    for scenario in ["modern", "legacy", "pagination", "reverse"] {
        let folder = tempfile::tempdir().unwrap();
        let mut session = session(vec![config(folder.path(), scenario)], BTreeMap::new());
        let report = discover(&session);
        assert!(
            report.servers[0].notices.is_empty(),
            "{scenario}: {:?}",
            report.servers[0].notices
        );
        assert_eq!(report.servers[0].tools.len(), 1);
        session.shutdown();
        assert_dead(folder.path());
        let requests = std::fs::read_to_string(folder.path().join("requests")).unwrap();
        assert_eq!(
            requests.contains("\"method\": \"initialize\""),
            scenario == "legacy"
        );
        if scenario == "modern" {
            assert!(requests.contains("io.modelcontextprotocol/protocolVersion"));
        }
        if scenario == "reverse" {
            assert!(requests.contains("Client requests are unsupported"));
        }
    }
}

#[test]
fn legacy_fallback_after_silent_discovery_stays_inside_the_startup_deadline() {
    let folder = tempfile::tempdir().unwrap();
    let mut config = config(folder.path(), "legacy-silent");
    config.startup_timeout_secs = 12;
    let mut session = session(vec![config], BTreeMap::new());
    let started = Instant::now();
    let report = discover(&session);
    assert!(
        report.servers[0].notices.is_empty(),
        "{:?}",
        report.servers[0].notices
    );
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(14));
    session.shutdown();
    assert_dead(folder.path());
}

#[test]
fn invalid_frames_wrong_ids_and_discovery_bounds_fail_closed_and_clean_up() {
    for scenario in [
        "wrong-id",
        "invalid-json",
        "partial",
        "oversize-frame",
        "oversize-id",
        "notification-flood",
        "cursor-loop",
        "cursor-long",
        "page-bound",
        "tool-bound",
        "byte-bound",
        "duplicate",
        "leader-exit",
    ] {
        let folder = tempfile::tempdir().unwrap();
        let mut config = config(folder.path(), scenario);
        config.required = true;
        let mut session = session(vec![config], BTreeMap::new());
        let report = discover(&session);
        assert!(report.servers[0].required_failed, "{scenario}");
        assert!(report.servers[0].tools.is_empty(), "{scenario}");
        assert!(!report.servers[0].notices.is_empty(), "{scenario}");
        session.shutdown();
        assert_dead(folder.path());
    }
}

#[test]
fn oversized_schemas_are_unavailable_without_losing_other_tools() {
    let folder = tempfile::tempdir().unwrap();
    let mut session = session(vec![config(folder.path(), "schema-bound")], BTreeMap::new());
    let report = discover(&session);
    assert_eq!(report.servers[0].tools.len(), 1);
    assert_eq!(report.servers[0].notices.len(), 1);
    assert!(!report.servers[0].required_failed);
    session.shutdown();
}

#[test]
fn exact_environment_and_nested_result_schema_and_stderr_redaction() {
    for scenario in ["environment", "secret-schema", "stderr-flood"] {
        let folder = tempfile::tempdir().unwrap();
        let mut config = config(folder.path(), scenario);
        config.env.insert(
            "TOKEN".into(),
            EnvironmentValue::Reference {
                from_env: "FIXTURE_KEY".into(),
            },
        );
        config
            .env
            .insert("PUBLIC".into(), EnvironmentValue::Literal("visible".into()));
        let secret = "arbitrary-credential-雪-12345";
        let effective = BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("FIXTURE_KEY".into(), secret.into()),
            ("UNRELATED_SECRET".into(), "should-not-arrive".into()),
            ("AGENT_TERMINAL_APPROVAL_SOCKET".into(), "control".into()),
        ]);
        let mut session = session(vec![config], effective);
        let report = discover(&session);
        assert!(
            report.servers[0].notices.is_empty(),
            "{:?}",
            report.servers[0].notices
        );
        assert!(!serde_json::to_string(&report.servers[0].tools)
            .unwrap()
            .contains(secret));
        let result = session
            .call_discovered("fixture".into(), "read".into(), Default::default())
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_eq!(result["isError"], true);
        assert!(!result.to_string().contains(secret));
        if scenario == "environment" {
            let env: BTreeMap<String, String> =
                serde_json::from_slice(&std::fs::read(folder.path().join("environment")).unwrap())
                    .unwrap();
            assert_eq!(env["TOKEN"], secret);
            assert_eq!(env["PUBLIC"], "visible");
            assert!(!env.contains_key("UNRELATED_SECRET"));
            assert!(!env.contains_key("FIXTURE_KEY"));
            assert!(!env.contains_key("AGENT_TERMINAL_APPROVAL_SOCKET"));
        }
        session.shutdown();
        assert_dead(folder.path());
    }
}

#[test]
fn stop_and_eof_clean_up_idle_servers_and_descendants() {
    for explicit_stop in [true, false] {
        let folder = tempfile::tempdir().unwrap();
        let mut session = session(vec![config(folder.path(), "descendant")], BTreeMap::new());
        assert!(discover(&session).servers[0].notices.is_empty());
        assert_eq!(pids(folder.path()).len(), 2);
        if explicit_stop {
            session.shutdown();
        }
        drop(session);
        assert_dead(folder.path());
    }
}

#[test]
fn startup_discovery_and_call_timeouts_and_cancellation_clean_up_without_replay() {
    for scenario in [
        "startup-stall",
        "discovery-stall",
        "call-stall",
        "call-exit",
    ] {
        let folder = tempfile::tempdir().unwrap();
        let mut session = session(vec![config(folder.path(), scenario)], BTreeMap::new());
        let report = discover(&session);
        if scenario.starts_with("call-") {
            assert!(report.servers[0].notices.is_empty());
            let result = session
                .call_discovered("fixture".into(), "read".into(), Default::default())
                .unwrap()
                .recv_timeout(Duration::from_secs(4))
                .unwrap();
            assert!(result.is_err());
            let requests = std::fs::read_to_string(folder.path().join("requests")).unwrap();
            assert_eq!(requests.matches("\"method\": \"tools/call\"").count(), 1);
        } else {
            assert!(!report.servers[0].notices.is_empty());
        }
        session.shutdown();
        assert_dead(folder.path());
    }
    for scenario in ["startup-stall", "discovery-stall", "call-stall"] {
        let folder = tempfile::tempdir().unwrap();
        let mut session = session(vec![config(folder.path(), scenario)], BTreeMap::new());
        let pending = session.discover(Mode::Ask).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while pids(folder.path()).is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        if scenario == "call-stall" {
            pending.recv_timeout(Duration::from_secs(3)).unwrap();
            let _call = session
                .call_discovered("fixture".into(), "read".into(), Default::default())
                .unwrap();
            std::thread::sleep(Duration::from_millis(50));
        }
        let started = Instant::now();
        session.shutdown();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_dead(folder.path());
    }
}

#[test]
fn list_changes_refresh_only_at_the_next_turn_boundary() {
    let folder = tempfile::tempdir().unwrap();
    let mut session = session(vec![config(folder.path(), "changed")], BTreeMap::new());
    assert_eq!(discover(&session).servers[0].tools.len(), 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(discover(&session).servers[0].tools.len(), 1);
    let requests = std::fs::read_to_string(folder.path().join("requests")).unwrap();
    assert_eq!(requests.matches("\"method\": \"tools/list\"").count(), 2);
    session.shutdown();
}

#[test]
fn session_tool_ceiling_and_selected_server_ceiling_are_enforced() {
    let folders: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let configs: Vec<_> = folders
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut c = config(f.path(), "max-tools");
            c.id = format!("fixture-{i}");
            c
        })
        .collect();
    let mut session = session(configs, BTreeMap::new());
    let report = discover(&session);
    assert_eq!(
        report
            .servers
            .iter()
            .filter(|s| !s.notices.is_empty())
            .count(),
        1
    );
    session.shutdown();
    for folder in &folders {
        assert_dead(folder.path());
    }
    assert!(Session::for_test(
        vec![config(folders[0].path(), "modern"); 17],
        executable(),
        BTreeMap::new()
    )
    .is_err());
}

#[test]
fn abrupt_runner_death_closes_the_private_lifetime_pipe_and_kills_descendants() {
    let folder = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let context = agent_core::local_context::LocalLaunchContext {
        mcp_servers: vec![config(folder.path(), "abrupt")],
        ..Default::default()
    };
    let mut runner = std::process::Command::new(executable())
        .args([
            "--local-agent",
            "--server",
            "http://127.0.0.1:1",
            "--cwd",
            folder.path().to_str().unwrap(),
            "--model",
            "fixture",
        ])
        .env("XDG_STATE_HOME", state.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let input = runner.stdin.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            serde_json::to_string(&agent_core::adapter::Command::ConfigureLocal {
                context: Box::new(context)
            })
            .unwrap()
        )
        .unwrap();
        writeln!(
            input,
            "{}",
            serde_json::to_string(&agent_core::adapter::Command::Prompt {
                text: "hello".into()
            })
            .unwrap()
        )
        .unwrap();
    }
    let until = Instant::now() + Duration::from_secs(5);
    while pids(folder.path()).len() < 2 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    runner.kill().unwrap();
    runner.wait().unwrap();
    assert_dead(folder.path());
}

#[test]
fn guardian_preserves_a_protocol_frame_coalesced_with_private_setup() {
    use std::io::{BufRead, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    let folder = tempfile::tempdir().unwrap();
    let config = config(folder.path(), "modern");
    let setup =
        serde_json::json!({"command":config.command,"args":config.args,"cwd":config.cwd,"env":{}});
    let mut descriptors = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let read_end = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
    let write_end = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
    let fd = read_end.as_raw_fd();
    let mut command = std::process::Command::new(executable());
    command
        .arg(agent_kit::mcp_guardian::GUARDIAN_FLAG)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut guardian = command.spawn().unwrap();
    drop(read_end);
    // One write, before reading any output: the guardian must consume exactly its setup.
    let frames =
        format!("{setup}\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\"}}\n");
    guardian
        .stdin
        .as_mut()
        .unwrap()
        .write_all(frames.as_bytes())
        .unwrap();
    let output = guardian.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(output).read_line(&mut line);
        let _ = tx.send(line);
    });
    let reply = rx.recv_timeout(Duration::from_secs(3));
    drop(write_end);
    guardian.stdin.take();
    assert!(guardian.wait().unwrap().success());
    reader.join().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply.unwrap()).unwrap()["id"],
        1
    );
    assert_dead(folder.path());
}

#[test]
fn startup_concurrency_is_four_and_cancellation_never_launches_the_next_batch() {
    let folders: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let configs = folders
        .iter()
        .enumerate()
        .map(|(i, folder)| {
            let mut config = config(folder.path(), "startup-stall");
            config.id = format!("fixture-{i}");
            config.startup_timeout_secs = 10;
            config
        })
        .collect();
    let mut session = session(configs, BTreeMap::new());
    let _pending = session.discover(Mode::Ask).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while folders.iter().map(|f| pids(f.path()).len()).sum::<usize>() < 4 && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        folders.iter().map(|f| pids(f.path()).len()).sum::<usize>(),
        4
    );
    assert!(pids(folders[4].path()).is_empty());
    session.shutdown();
    for folder in &folders[..4] {
        assert_dead(folder.path());
    }
    assert!(pids(folders[4].path()).is_empty());
}

#[test]
fn runner_stop_restarts_a_fresh_generation_and_stdin_eof_awaits_cleanup() {
    use std::io::Write;
    let folder = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let context = agent_core::local_context::LocalLaunchContext {
        mcp_servers: vec![config(folder.path(), "abrupt")],
        ..Default::default()
    };
    let mut runner = std::process::Command::new(executable())
        .args([
            "--local-agent",
            "--server",
            "http://127.0.0.1:1",
            "--cwd",
            folder.path().to_str().unwrap(),
            "--model",
            "fixture",
        ])
        .env("XDG_STATE_HOME", state.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = runner.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        serde_json::to_string(&agent_core::adapter::Command::ConfigureLocal {
            context: Box::new(context)
        })
        .unwrap()
    )
    .unwrap();
    // Merely configuring a session must not launch any MCP executable.
    std::thread::sleep(Duration::from_millis(50));
    assert!(pids(folder.path()).is_empty());
    writeln!(
        input,
        "{}",
        serde_json::to_string(&agent_core::adapter::Command::Prompt {
            text: "first".into()
        })
        .unwrap()
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    while pids(folder.path()).len() < 2 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pids(folder.path()).len(), 2);
    writeln!(
        input,
        "{}",
        serde_json::to_string(&agent_core::adapter::Command::Interrupt).unwrap()
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        serde_json::to_string(&agent_core::adapter::Command::Prompt {
            text: "second".into()
        })
        .unwrap()
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    while pids(folder.path()).len() < 4 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pids(folder.path()).len(), 4);
    assert!(pids(folder.path())[..2].iter().all(|pid| !alive(*pid)));
    drop(input);
    let until = Instant::now() + Duration::from_secs(4);
    let status = loop {
        if let Some(status) = runner.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= until {
            runner.kill().unwrap();
            panic!("runner failed to await bounded EOF cleanup");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    assert!(
        pids(folder.path()).iter().all(|pid| !alive(*pid)),
        "runner exited before cleanup"
    );
}
