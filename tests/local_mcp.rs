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
    return {'name':name,'inputSchema':{'type':'object'},'annotations':{'readOnlyHint':True,'destructiveHint':False}}
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
        if scenario == 'content-change':
            with open(folder + '/tool.json') as f: result['tools'] = [json.load(f)]
        if scenario == 'strict-schema':
            result['tools'][0]['inputSchema'] = {'type':'object','properties':{'value':{'type':'string','minLength':3}},'required':['value'],'additionalProperties':False}
        if scenario == 'invalid-schema':
            result['tools'][0]['inputSchema'] = {'type':'unknown-private-canary'}
        if scenario == 'external-schema':
            result['tools'][0]['inputSchema'] = {'$ref':'file:///tmp/private-canary.json'}
        if scenario == 'backtracking-schema':
            result['tools'][0]['inputSchema'] = {'type':'object','patternProperties':{'(?=private-canary)':{}}}
        if scenario == 'budget-schema':
            result['tools'].append(dict(tool('large'),description='x'*50000))
        if scenario == 'changed-schema':
            result['tools'][0]['inputSchema'] = {'type':'object','properties':{'value':{'type':'string','minLength':page}}}
        if scenario == 'environment':
            with open(folder + '/environment','w') as f: json.dump(dict(os.environ),f)
        result.update({'resultType':'complete','ttlMs':0,'cacheScope':'private'})
        respond(request, result)
        if scenario == 'changed' and page == 1:
            send({'jsonrpc':'2.0','method':'notifications/tools/list_changed'})
    elif method == 'tools/call':
        with open(folder + '/executions','a') as f: f.write(json.dumps(request['params']) + '\n')
        if scenario == 'call-stall': time.sleep(60)
        if scenario == 'call-exit': sys.exit(0)
        if scenario == 'call-error':
            send({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32000,'message':os.environ.get('TOKEN','error')}}); continue
        if scenario == 'nested-secrets':
            payload = {'tokens':['array-private-1111',['nested-private-2222'],{'ordinary':['object-private-3333']}]}
            respond(request, {'resultType':'complete','content':[{'type':'text','text':json.dumps(payload)}], 'structuredContent':payload}); continue
        if scenario == 'structured-result':
            respond(request, {'resultType':'complete','isError':True,
                'content':[{'type':'text','text':'雪'*20000},{'type':'image','data':'BINARY-CANARY'},{'type':'resource_link','uri':'https://example.com/no-fetch'}],
                'structuredContent':{'nested':[{'value':os.environ.get('TOKEN','safe')}]}}); continue
        respond(request, {'resultType':'complete','content':[{'type':'text','text':os.environ.get('TOKEN','hello')}],'isError':True})
        if scenario == 'changed-schema': send({'jsonrpc':'2.0','method':'notifications/tools/list_changed'})
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

#[test]
fn tool_content_revisions_survive_reconnect_and_unchanged_notifications_but_track_raw_metadata() {
    let folder = tempfile::tempdir().unwrap();
    let config = config(folder.path(), "changed");
    let mut client = session(vec![config.clone()], BTreeMap::new());
    let first = discover(&client)
        .servers
        .remove(0)
        .registry
        .remove(0)
        .resolved;
    std::thread::sleep(Duration::from_millis(50));
    let refreshed = discover(&client)
        .servers
        .remove(0)
        .registry
        .remove(0)
        .resolved;
    assert_eq!(first.schema_revision, refreshed.schema_revision);
    client.shutdown();
    let mut client = session(vec![config.clone()], BTreeMap::new());
    let restarted = discover(&client)
        .servers
        .remove(0)
        .registry
        .remove(0)
        .resolved;
    assert_eq!(first.schema_revision, restarted.schema_revision);
    assert_eq!(first.server_revision, restarted.server_revision);
    client.shutdown();
    let mut definition = config;
    definition.label = "Changed definition".into();
    let mut client = session(vec![definition], BTreeMap::new());
    let changed = discover(&client)
        .servers
        .remove(0)
        .registry
        .remove(0)
        .resolved;
    assert_eq!(first.schema_revision, changed.schema_revision);
    assert_ne!(first.server_revision, changed.server_revision);
    client.shutdown();
    let config = crate::config(folder.path(), "content-change");
    let mut revisions = std::collections::BTreeSet::new();
    for tool in [
        serde_json::json!({"name":"read","inputSchema":{"type":"object"},"description":"one"}),
        serde_json::json!({"name":"read","inputSchema":{"type":"object"},"description":"two"}),
        serde_json::json!({"name":"read","inputSchema":{"type":"object","required":["value"]},"description":"two"}),
        serde_json::json!({"name":"read","inputSchema":{"type":"object"},"description":"two","annotations":{"readOnlyHint":true}}),
    ] {
        std::fs::write(folder.path().join("tool.json"), tool.to_string()).unwrap();
        let mut client = session(vec![config.clone()], BTreeMap::new());
        let tool = discover(&client)
            .servers
            .remove(0)
            .registry
            .remove(0)
            .resolved;
        assert!(revisions.insert(tool.schema_revision));
        client.shutdown();
    }
    let mut config = crate::config(folder.path(), "secret-schema");
    config.env.insert(
        "TOKEN".into(),
        EnvironmentValue::Reference {
            from_env: "SOURCE".into(),
        },
    );
    let mut raw_revisions = std::collections::BTreeSet::new();
    let mut displayed = std::collections::BTreeSet::new();
    for secret in ["opaque-private-value-one", "opaque-private-value-two"] {
        let mut client = session(
            vec![config.clone()],
            BTreeMap::from([("SOURCE".into(), secret.into())]),
        );
        let snapshot = discover(&client).servers.remove(0);
        assert!(raw_revisions.insert(snapshot.registry[0].resolved.schema_revision.clone()));
        displayed.insert(snapshot.tools[0].description.clone());
        client.shutdown();
    }
    assert_eq!(
        displayed.len(),
        1,
        "displayed masking cannot replace raw content identity"
    );
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

/// Processes other than `except` whose command line names `marker`: (pid, process group),
/// read from `/proc`.
fn processes_naming(marker: &str, except: u32) -> Vec<(i32, i32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid != except as i32 && alive(*pid))
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|line| String::from_utf8_lossy(&line).contains(marker))
        })
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let group = stat
                .rsplit_once(") ")?
                .1
                .split_whitespace()
                .nth(2)?
                .parse()
                .ok()?;
            Some((pid, group))
        })
        .collect()
}

/// A loopback OpenAI-compatible chat endpoint that answers each request with the next scripted
/// SSE reply (a plain answer once the script runs out) and records every request body.
struct ScriptedModel {
    base: String,
    bodies: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}
impl ScriptedModel {
    fn start(replies: Vec<String>) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = bodies.clone();
        std::thread::spawn(move || {
            let mut replies = std::collections::VecDeque::from(replies);
            for stream in listener.incoming() {
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
                    recorded.lock().unwrap().push(value);
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
        Self { base, bodies }
    }
    fn answer() -> String {
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()
    }
    fn calls(calls: &[(&str, serde_json::Value)]) -> String {
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
}

/// The real agent-terminal runner over its pipes, with the production client and guardian.
struct LiveRunner {
    child: std::process::Child,
    input: std::process::ChildStdin,
    events: std::sync::mpsc::Receiver<agent_core::event::Envelope>,
}
impl LiveRunner {
    fn start(
        model: &ScriptedModel,
        cwd: &Path,
        state: &Path,
        context: agent_core::local_context::LocalLaunchContext,
    ) -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new(executable())
            .args([
                "--local-agent",
                "--server",
                &model.base,
                "--cwd",
                cwd.to_str().unwrap(),
                "--mode",
                "plan",
                "--model",
                "fixture",
            ])
            .env("XDG_STATE_HOME", state)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if let Ok(envelope) = serde_json::from_str(&line) {
                    if tx.send(envelope).is_err() {
                        break;
                    }
                }
            }
        });
        let run = Self {
            child,
            input,
            events,
        };
        run.send(&agent_core::adapter::Command::ConfigureLocal {
            context: Box::new(context),
        });
        run
    }
    fn send(&self, command: &agent_core::adapter::Command) {
        use std::io::Write;
        let mut input = &self.input;
        writeln!(input, "{}", serde_json::to_string(command).unwrap()).unwrap();
    }
    fn next(&self) -> agent_core::event::Envelope {
        self.events
            .recv_timeout(Duration::from_secs(60))
            .expect("the runner went quiet")
    }
    /// The turn's events up to its completion; `on_card` answers each approval card.
    fn turn(
        &self,
        text: &str,
        mut on_card: impl FnMut(&agent_core::event::Envelope),
    ) -> Vec<agent_core::event::Envelope> {
        use agent_core::event::Event;
        self.send(&agent_core::adapter::Command::Prompt { text: text.into() });
        let mut events = Vec::new();
        loop {
            let envelope = self.next();
            let complete = matches!(envelope.event, Event::TurnCompleted { .. });
            if matches!(envelope.event, Event::ApprovalRequested { .. }) {
                on_card(&envelope);
            }
            events.push(envelope);
            if complete {
                return events;
            }
        }
    }
    fn finish(mut self) {
        drop(self.input);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if Instant::now() >= until {
                self.child.kill().unwrap();
                panic!("the runner did not exit after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Opt-in: the official filesystem MCP server, already installed and pinned by whoever runs
/// this, named by `LOCAL_MCP_TEST_COMMAND`. Nothing is downloaded, and no real home folder is
/// read: the server's only allowed directory, the thread folder and the state folder are
/// temporary. Unset, the test passes without running.
#[test]
fn local_mcp_real_stdio() {
    use agent_core::{
        adapter::{Command, Mode},
        event::{Decision, Event, ItemStatus},
        local_mcp::{alias, ApprovalAnswer, ApprovalCard, APPROVAL_TOOL},
    };
    let Some(command) = std::env::var_os("LOCAL_MCP_TEST_COMMAND") else {
        eprintln!("local_mcp_real_stdio: LOCAL_MCP_TEST_COMMAND is unset; nothing run");
        return;
    };
    let command = PathBuf::from(command);
    assert!(
        command.is_absolute() && command.is_file(),
        "LOCAL_MCP_TEST_COMMAND must be the absolute path of an installed server"
    );
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let folder = root.path().canonicalize().unwrap();
    let fixture = folder.join("fixture.txt");
    let original = "fixture bytes 雪 0123\nsecond line\n";
    let replacement = "written through MCP after approval\n";
    std::fs::write(&fixture, original).unwrap();
    let config: McpServerConfig = serde_json::from_value(serde_json::json!({
        "id":"files","label":"Files","enabled":true,"required":true,
        "command":command.to_str().unwrap(),"args":[folder.to_str().unwrap()],
        "cwd":folder.to_str().unwrap(),"env":{},
        // A tool override only narrows the server's policy: the server approves its reviewed
        // allowlist, and the write is narrowed back to asking.
        "tools":[
            {"name":"read_text_file","read_only":true,"approval":"inherit"},
            {"name":"write_file","read_only":false,"approval":"ask"}
        ],
        "approval":"allow","startup_timeout_secs":60,"call_timeout_secs":60
    }))
    .unwrap();
    let read = alias("files", "read_text_file").unwrap();
    let write = alias("files", "write_file").unwrap();
    let read_call = (
        read.as_str(),
        serde_json::json!({"path": fixture.to_str().unwrap()}),
    );
    let write_call = (
        write.as_str(),
        serde_json::json!({"path": fixture.to_str().unwrap(), "content": replacement}),
    );
    let model = ScriptedModel::start(vec![
        // Plan: the approved read-only read runs; the write is refused.
        ScriptedModel::calls(std::slice::from_ref(&read_call)),
        ScriptedModel::calls(std::slice::from_ref(&write_call)),
        ScriptedModel::answer(),
        // Ask: the write waits for its card, then runs once approved.
        ScriptedModel::calls(std::slice::from_ref(&write_call)),
    ]);
    let run = LiveRunner::start(
        &model,
        &folder,
        state.path(),
        agent_core::local_context::LocalLaunchContext {
            mcp_servers: vec![config],
            ..Default::default()
        },
    );
    let marker = folder.to_str().unwrap().to_owned();
    let runner_pid = run.child.id();
    assert!(
        processes_naming(&marker, runner_pid).is_empty(),
        "setup starts no server"
    );

    let events = run.turn("read the fixture, then try to write it", |envelope| {
        panic!(
            "Plan asks for nothing: {}",
            serde_json::to_string(envelope).unwrap()
        )
    });
    let outputs: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ItemCompleted { output, status, .. } => Some((output.clone(), *status)),
            _ => None,
        })
        .collect();
    let read_back = outputs
        .iter()
        .filter(|(_, status)| *status == ItemStatus::Completed)
        .filter_map(|(output, _)| {
            serde_json::from_str::<serde_json::Value>(output.as_deref()?).ok()
        })
        .find_map(|result| result["content"][0].as_str().map(str::to_owned));
    assert_eq!(
        read_back.as_deref(),
        Some(original),
        "the read-only read ran and returned the exact fixture: {outputs:?}"
    );
    assert!(
        outputs
            .iter()
            .any(|(_, status)| *status == ItemStatus::Failed),
        "the write is refused in Plan: {outputs:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture).unwrap(),
        original,
        "Plan never writes"
    );
    let bodies = model.bodies.lock().unwrap().clone();
    let offered = |body: &serde_json::Value| -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(offered(&bodies[0]).contains(&read));
    assert!(
        !offered(&bodies[0]).contains(&write),
        "Plan offers no writing MCP tool"
    );
    let first = processes_naming(&marker, runner_pid);
    assert!(!first.is_empty(), "the server is a live child process");
    let first_group = first[0].1;
    assert!(first.iter().all(|(_, group)| *group == first_group));
    assert_ne!(first_group, std::process::id() as i32);

    run.send(&Command::SetMode { mode: Mode::Ask });
    let mut card = None;
    let events = run.turn("now write it", |envelope| {
        assert!(card.is_none(), "one card for one write");
        assert_eq!(
            std::fs::read_to_string(&fixture).unwrap(),
            original,
            "nothing is written before approval"
        );
        let Event::ApprovalRequested { tool, input, .. } = &envelope.event else {
            unreachable!()
        };
        assert_eq!(tool, APPROVAL_TOOL);
        let shown: ApprovalCard = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(shown.arguments["content"], replacement);
        run.send(&Command::Approve {
            request: envelope.request.clone().unwrap(),
            decision: Decision::Allow,
            message: None,
            updated_input: Some(
                serde_json::to_value(ApprovalAnswer {
                    binding: shown.binding,
                    edited_arguments: None,
                })
                .unwrap(),
            ),
        });
        card = Some(envelope.request.clone());
    });
    assert!(card.is_some(), "Ask asked before the write");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ApprovalRequested { .. }))
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(&fixture).unwrap(),
        replacement,
        "the approved write landed exactly"
    );
    assert_eq!(
        processes_naming(&marker, runner_pid)
            .iter()
            .map(|(pid, _)| *pid)
            .collect::<Vec<_>>(),
        first.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(),
        "the same generation served both turns"
    );

    run.send(&Command::Interrupt);
    let until = Instant::now() + Duration::from_secs(10);
    while first.iter().any(|(pid, _)| alive(*pid)) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        first.iter().all(|(pid, _)| !alive(*pid)),
        "Stop ends the recorded process group"
    );
    assert!(processes_naming(&marker, runner_pid).is_empty());

    let events = run.turn("again", |_| panic!("a plain answer asks for nothing"));
    assert!(events
        .iter()
        .any(|e| matches!(e.event, Event::TurnCompleted { .. })));
    let second = processes_naming(&marker, runner_pid);
    assert!(
        !second.is_empty(),
        "the next turn starts a fresh generation"
    );
    assert_ne!(second[0].1, first_group, "a new process group");
    assert!(first.iter().all(|(pid, _)| !alive(*pid)));

    run.finish();
    let until = Instant::now() + Duration::from_secs(10);
    while second.iter().any(|(pid, _)| alive(*pid)) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        second.iter().all(|(pid, _)| !alive(*pid)),
        "session end leaves no server behind"
    );
}

mod runner {
    use super::*;
    use agent_core::{
        adapter::Command,
        event::{Decision, Envelope, Event, TurnState},
    };
    use agent_kit::local_agent::{ChatClient, ReplyLine, Runner, RunnerInput};
    use serde_json::{json, Value};
    use std::{
        collections::VecDeque,
        io::Write,
        sync::{
            mpsc::{self, Receiver, Sender},
            Arc, Mutex,
        },
    };

    enum Reply {
        Calls(Vec<(String, Value)>),
        CallsAfterClose(PathBuf, Vec<(String, Value)>),
        Cut(String),
        Answer,
        Stream {
            field: &'static str,
            chunks: Vec<String>,
        },
    }
    struct Model {
        replies: VecDeque<Reply>,
        bodies: Arc<Mutex<Vec<Value>>>,
    }
    impl ChatClient for Model {
        fn start(&mut self, body: &Value) -> Receiver<ReplyLine> {
            self.bodies.lock().unwrap().push(body.clone());
            let reply = match self.replies.pop_front().unwrap_or(Reply::Answer) {
                Reply::CallsAfterClose(folder, calls) => {
                    let pid = pids(&folder)[0];
                    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
                    let guardian: i32 = stat
                        .split_once(") ")
                        .unwrap()
                        .1
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .parse()
                        .unwrap();
                    // SAFETY: PID belongs to our fixture process in the private test directory.
                    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
                    let until = Instant::now() + Duration::from_secs(2);
                    // The guardian keeps its inherited output descriptor until group cleanup.
                    while (alive(pid) || alive(guardian)) && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    assert!(!alive(pid) && !alive(guardian));
                    std::thread::sleep(Duration::from_millis(50));
                    Reply::Calls(calls)
                }
                other => other,
            };
            let (tx, rx) = mpsc::channel();
            match reply {
                Reply::CallsAfterClose(..) => unreachable!("closed before constructing the reply"),
                Reply::Stream { field, chunks } => {
                    for chunk in chunks {
                        tx.send(Ok(format!(
                            "data: {}",
                            json!({"choices":[{"delta":{field:chunk}}]})
                        )))
                        .unwrap();
                    }
                    tx.send(Ok(
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}".into(),
                    ))
                    .unwrap();
                    tx.send(Ok("data: [DONE]".into())).unwrap();
                }
                Reply::Calls(calls) => {
                    let calls: Vec<_> = calls.into_iter().enumerate().map(|(index,(name,args))|
                        json!({"index":index,"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect();
                    tx.send(Ok(format!(
                        "data: {}",
                        json!({"choices":[{"delta":{"tool_calls":calls}}]})
                    )))
                    .unwrap();
                    tx.send(Ok(
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}"
                            .into(),
                    ))
                    .unwrap();
                    tx.send(Ok("data: [DONE]".into())).unwrap();
                }
                Reply::Cut(name) => {
                    tx.send(Ok(format!("data: {}",json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"cut","type":"function","function":{"name":name,"arguments":"{}"}}]}}]})))).unwrap();
                    tx.send(Ok(
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}".into(),
                    ))
                    .unwrap();
                }
                Reply::Answer => {
                    tx.send(Ok("data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}".into())).unwrap();
                    tx.send(Ok("data: [DONE]".into())).unwrap();
                }
            }
            rx
        }
    }
    struct Output(Sender<Envelope>, Vec<u8>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.1.extend_from_slice(bytes);
            while let Some(end) = self.1.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.1.drain(..=end).collect();
                let _ = self.0.send(serde_json::from_slice(&line).unwrap());
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct Run {
        input: Sender<RunnerInput>,
        events: Receiver<Envelope>,
        thread: std::thread::JoinHandle<i32>,
        bodies: Arc<Mutex<Vec<Value>>>,
        state: tempfile::TempDir,
    }
    impl Run {
        fn new(
            configs: Vec<McpServerConfig>,
            mode: Mode,
            env: BTreeMap<String, String>,
            replies: Vec<Reply>,
            budget: Option<agent_core::local_context::ContextBudget>,
        ) -> Self {
            let workspace = configs[0].cwd.clone();
            let state = tempfile::tempdir().unwrap();
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let model = Model {
                replies: replies.into(),
                bodies: bodies.clone(),
            };
            let session = session(configs.clone(), env);
            let context = agent_core::local_context::LocalLaunchContext {
                mcp_servers: configs,
                budget: budget.unwrap_or_default(),
                ..Default::default()
            };
            let (input, commands) = mpsc::channel();
            let (out, events) = mpsc::channel();
            let dir = state.path().to_owned();
            let thread = std::thread::spawn(move || {
                let args = agent_core::local_agent::RunnerArgs {
                    server: "http://127.0.0.1:1".into(),
                    cwd: workspace,
                    model: Some("fixture".into()),
                    mode,
                    ..Default::default()
                };
                Runner::new(args, model, dir, Output(out, Vec::new()))
                    .with_mcp_for_test(session)
                    .run(&commands)
            });
            let run = Self {
                input,
                events,
                thread,
                bodies,
                state,
            };
            run.send(Command::ConfigureLocal {
                context: Box::new(context),
            });
            run
        }
        fn send(&self, command: Command) {
            self.input.send(RunnerInput::Command(command)).unwrap();
        }
        fn next(&self) -> Envelope {
            self.events
                .recv_timeout(Duration::from_secs(12))
                .expect("runner went quiet")
        }
        fn answer(&self, env: &Envelope, decision: Decision) {
            let Event::ApprovalRequested { tool, input, .. } = &env.event else {
                panic!("expected card")
            };
            if tool != agent_core::local_mcp::APPROVAL_TOOL {
                self.send(Command::Approve {
                    request: env.request.clone().unwrap(),
                    decision,
                    message: None,
                    updated_input: None,
                });
                return;
            }
            let card: agent_core::local_mcp::ApprovalCard =
                serde_json::from_value(input.clone()).unwrap();
            self.send(Command::Approve {
                request: env.request.clone().unwrap(),
                decision,
                message: None,
                updated_input: Some(
                    serde_json::to_value(agent_core::local_mcp::ApprovalAnswer {
                        binding: card.binding,
                        edited_arguments: None,
                    })
                    .unwrap(),
                ),
            });
        }
        fn turn(&self) -> Vec<Envelope> {
            let mut events = Vec::new();
            loop {
                let env = self.next();
                let complete = matches!(env.event, Event::TurnCompleted { .. });
                if matches!(env.event, Event::ApprovalRequested { .. }) {
                    self.answer(&env, Decision::Allow);
                }
                events.push(env);
                if complete {
                    return events;
                }
            }
        }
        fn finish(self) -> (Vec<Value>, String) {
            self.input.send(RunnerInput::Closed).unwrap();
            assert_eq!(self.thread.join().unwrap(), 0);
            let transcript = std::fs::read_dir(self.state.path())
                .unwrap()
                .filter_map(Result::ok)
                .find(|e| e.path().extension().is_some_and(|e| e == "json"))
                .map(|e| std::fs::read_to_string(e.path()).unwrap())
                .unwrap_or_default();
            let bodies = self.bodies.lock().unwrap().clone();
            (bodies, transcript)
        }
    }
    fn mcp_alias() -> String {
        agent_core::local_mcp::alias("fixture", "read").unwrap()
    }

    #[test]
    fn split_environment_secrets_are_held_back_from_every_sse_delta() {
        for field in ["content", "reasoning_content"] {
            for chunks in [
                vec!["canary-one-", "canary-two-canary-three"],
                vec!["canary-one-", "canary-two-", "canary-three"],
            ] {
                let folder = tempfile::tempdir().unwrap();
                let mut config = config(folder.path(), "modern");
                config.env.insert(
                    "TOKEN".into(),
                    EnvironmentValue::Reference {
                        from_env: "SOURCE".into(),
                    },
                );
                let run = Run::new(
                    vec![config],
                    Mode::Ask,
                    BTreeMap::from([(
                        "SOURCE".into(),
                        "canary-one-canary-two-canary-three".into(),
                    )]),
                    vec![Reply::Stream {
                        field,
                        chunks: chunks.iter().map(|s| s.to_string()).collect(),
                    }],
                    None,
                );
                run.send(Command::Prompt {
                    text: "test".into(),
                });
                let events = run.turn();
                let rendered = serde_json::to_string(&events).unwrap();
                for part in ["canary-one", "canary-two", "canary-three"] {
                    assert!(!rendered.contains(part), "{field}: leaked delta");
                }
                assert!(rendered.contains("REDACTED"));
                let store = agent_kit::store::Store::open_in_memory().unwrap();
                let thread = store.create_thread("/workspace", None).unwrap();
                for event in events {
                    store.append_event(&thread, None, &event).unwrap();
                }
                let persisted =
                    serde_json::to_string(&store.events(&thread, None, 1000).unwrap()).unwrap();
                let (_, transcript) = run.finish();
                assert!(!persisted.contains("canary-one"));
                assert!(!transcript.contains("canary-one"));
            }
        }
    }

    #[test]
    fn sensitive_ancestor_keys_mask_arguments_text_results_and_structured_content() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "nested-secrets")],
            Mode::Ask,
            BTreeMap::new(),
            vec![Reply::Calls(vec![(
                mcp_alias(),
                json!({"tokens":["array-private-1111",["nested-private-2222"],{"ordinary":["object-private-3333"]}]}),
            )])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let events = run.turn();
        let store = agent_kit::store::Store::open_in_memory().unwrap();
        let thread = store.create_thread("/workspace", None).unwrap();
        for event in &events {
            store.append_event(&thread, None, event).unwrap();
        }
        let persisted = serde_json::to_string(&store.events(&thread, None, 1000).unwrap()).unwrap();
        let (bodies, transcript) = run.finish();
        for safe in [
            serde_json::to_string(&events).unwrap(),
            persisted,
            transcript,
            serde_json::to_string(&bodies).unwrap(),
        ] {
            for part in ["array-private", "nested-private", "object-private"] {
                assert!(!safe.contains(part), "sensitive ancestor leaked");
            }
        }
        assert_eq!(calls(folder.path()).len(), 1);
    }

    #[test]
    fn json_environment_credentials_never_escape_arguments_results_or_persistence() {
        for secret in [
            r#"{"ordinary":"json-private-1111"}"#,
            r#"["json-private-2222"]"#,
        ] {
            let folder = tempfile::tempdir().unwrap();
            let mut config = config(folder.path(), "modern");
            config.env.insert(
                "TOKEN".into(),
                EnvironmentValue::Reference {
                    from_env: "SOURCE".into(),
                },
            );
            let run = Run::new(
                vec![config],
                Mode::Ask,
                BTreeMap::from([("SOURCE".into(), secret.into())]),
                vec![Reply::Calls(vec![(mcp_alias(), json!({"value":secret}))])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let events = run.turn();
            let (bodies, transcript) = run.finish();
            assert!(!serde_json::to_string(&events)
                .unwrap()
                .contains("json-private"));
            assert!(!serde_json::to_string(&bodies)
                .unwrap()
                .contains("json-private"));
            assert!(!transcript.contains("json-private"));
        }
    }

    #[test]
    fn explicit_reconnect_replaces_the_process_and_toggle_stops_without_hidden_restart() {
        use agent_core::adapter::Control;
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "modern")],
            Mode::Ask,
            BTreeMap::new(),
            vec![],
            None,
        );
        let control = |id: &str, control| {
            run.send(Command::Control {
                id: id.into(),
                control,
            });
            loop {
                let event = run.next();
                if event.request.as_deref() == Some(id) {
                    if let Event::ControlResult { ok, error } = event.event {
                        assert!(error.is_none(), "{error:?}");
                        break ok.unwrap_or(Value::Null);
                    }
                }
            }
        };
        assert_eq!(
            control("initial", Control::McpStatus)["mcpServers"][0]["status"],
            "stopped"
        );
        assert!(pids(folder.path()).is_empty());
        control(
            "connect",
            Control::McpReconnect {
                server: "fixture".into(),
            },
        );
        assert_eq!(
            control("connected", Control::McpStatus)["mcpServers"][0]["status"],
            "connected"
        );
        let first = pids(folder.path());
        assert_eq!(first.len(), 1);
        control(
            "replace",
            Control::McpReconnect {
                server: "fixture".into(),
            },
        );
        assert_eq!(pids(folder.path()).len(), 2);
        assert!(first.into_iter().all(|pid| !alive(pid)));
        control(
            "disable",
            Control::McpToggle {
                server: "fixture".into(),
                enabled: false,
            },
        );
        assert_dead(folder.path());
        assert_eq!(
            control("disabled", Control::McpStatus)["mcpServers"][0]["status"],
            "disabled"
        );
        assert_eq!(
            control(
                "enable",
                Control::McpToggle {
                    server: "fixture".into(),
                    enabled: true
                }
            )["restartPending"],
            true
        );
        assert_eq!(
            control("stopped", Control::McpStatus)["mcpServers"][0]["status"],
            "stopped"
        );
        assert_eq!(pids(folder.path()).len(), 2);
        assert!(calls(folder.path()).is_empty());
        run.finish();
    }

    #[test]
    fn pending_mcp_approval_answers_status_and_refuses_busy_mutations_without_queueing_them() {
        let folder = tempfile::tempdir().unwrap();
        let config = config(folder.path(), "modern");
        let server = config.id.clone();
        let run = Run::new(
            vec![config],
            Mode::Ask,
            BTreeMap::new(),
            vec![Reply::Calls(vec![(mcp_alias(), json!({}))]), Reply::Answer],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let card = loop {
            let event = run.next();
            if matches!(event.event, Event::ApprovalRequested { .. }) {
                break event;
            }
        };
        let original_pids = pids(folder.path());
        run.send(Command::Control {
            id: "status-pending".into(),
            control: agent_core::adapter::Control::McpStatus,
        });
        run.send(Command::Control {
            id: "reconnect-busy".into(),
            control: agent_core::adapter::Control::McpReconnect {
                server: server.clone(),
            },
        });
        run.send(Command::Control {
            id: "enable-busy".into(),
            control: agent_core::adapter::Control::McpToggle {
                server: server.clone(),
                enabled: true,
            },
        });
        let mut received = std::collections::BTreeSet::new();
        while received.len() < 3 {
            let event = run.next();
            assert!(!matches!(
                event.event,
                Event::ApprovalResolved { .. } | Event::TurnCompleted { .. }
            ));
            if let Event::ControlResult { ok, error } = &event.event {
                let id = event.request.as_deref().unwrap();
                if id == "status-pending" {
                    assert!(error.is_none());
                    assert_eq!(ok.as_ref().unwrap()["mcpServers"][0]["status"], "connected");
                } else {
                    assert!(error.as_ref().is_some_and(|error| error.contains("busy")));
                }
                received.insert(id.to_owned());
            }
        }
        assert!(calls(folder.path()).is_empty());
        assert_eq!(pids(folder.path()), original_pids);
        run.answer(&card, Decision::Deny);
        let events = run.turn();
        assert!(!events
            .iter()
            .any(|event| matches!(event.event, Event::ControlResult { .. })));
        run.send(Command::Control {
            id: "status-idle".into(),
            control: agent_core::adapter::Control::McpStatus,
        });
        loop {
            let event = run.next();
            if event.request.as_deref() == Some("status-idle") {
                break;
            }
            assert!(!matches!(event.event, Event::ControlResult { .. }));
        }
        assert_eq!(pids(folder.path()), original_pids);
        assert!(calls(folder.path()).is_empty());
        run.finish();
        assert_dead(folder.path());
    }

    #[test]
    fn disconnect_expires_pending_cards_and_late_answers_never_authorise_reconnection() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "modern")],
            Mode::Ask,
            BTreeMap::new(),
            vec![
                Reply::Calls(vec![(mcp_alias(), json!({}))]),
                Reply::Answer,
                Reply::Calls(vec![(mcp_alias(), json!({}))]),
            ],
            None,
        );
        run.send(Command::Prompt {
            text: "first".into(),
        });
        let card = loop {
            let env = run.next();
            if matches!(env.event, Event::ApprovalRequested { .. }) {
                break env;
            }
        };
        let pid = pids(folder.path())[0];
        // SAFETY: this PID belongs to the fixture process recorded in our private directory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let events = run.turn();
        assert!(events
            .iter()
            .any(|e| e.request == card.request && matches!(e.event, Event::ApprovalExpired)));
        assert!(events.iter().any(|e| matches!(&e.event,Event::ItemCompleted {status:agent_core::event::ItemStatus::Failed,error:Some(error),..} if error.contains("The MCP connection closed before the call was approved."))));
        assert!(!events.iter().any(
            |e| e.request == card.request && matches!(e.event, Event::ApprovalResolved { .. })
        ));
        run.send(Command::Prompt {
            text: "second".into(),
        });
        let fresh = loop {
            let env = run.next();
            if matches!(env.event, Event::ApprovalRequested { .. }) {
                break env;
            }
        };
        assert_ne!(card.request, fresh.request);
        run.answer(&card, Decision::AllowAlways);
        std::thread::sleep(Duration::from_millis(30));
        assert!(calls(folder.path()).is_empty());
        run.answer(&fresh, Decision::Deny);
        run.turn();
        run.finish();
        assert!(calls(folder.path()).is_empty());
        assert_dead(folder.path());
    }
    fn calls(folder: &Path) -> Vec<Value> {
        std::fs::read_to_string(folder.join("executions"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn approvals_show_the_executed_arguments_with_only_secret_values_masked() {
        let secret = "opaque-value-12345";
        let commands = [
            format!("[ -d . ] && printf '%s' '{secret}' > marker"),
            format!("{{ printf '%s' '{secret}' > marker; }}"),
        ];
        let arguments = commands
            .into_iter()
            .map(|command| ("run_command", json!({"command": command})))
            .chain([
                ("write_file", json!({"file_path": "marker", "content": format!("[link](example.com) # {secret}")})),
                ("write_file", json!({"file_path": "marker", "content": format!("{{ \"value\" : \"{secret}\" }}")})),
                ("mcp", json!({"value": format!("{{ \"value\" : \"{secret}\" }}")})),
            ]);
        for (tool, argument) in arguments {
            let folder = tempfile::tempdir().unwrap();
            let mut config = config(folder.path(), "modern");
            config.env.insert(
                "TOKEN".into(),
                EnvironmentValue::Reference {
                    from_env: "SOURCE".into(),
                },
            );
            let name = if tool == "mcp" {
                mcp_alias()
            } else {
                tool.into()
            };
            let run = Run::new(
                vec![config],
                Mode::Ask,
                BTreeMap::from([("SOURCE".into(), secret.into())]),
                vec![Reply::Calls(vec![(name, argument.clone())])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let events = run.turn();
            let card = events
                .iter()
                .find_map(|e| match &e.event {
                    Event::ApprovalRequested { input, .. } => Some(if tool == "mcp" {
                        input["arguments"].clone()
                    } else {
                        input.clone()
                    }),
                    _ => None,
                })
                .expect("complete card");
            let expected: Value =
                serde_json::from_str(&argument.to_string().replace(secret, "[REDACTED]")).unwrap();
            assert_eq!(card, expected);
            if tool == "mcp" {
                assert_eq!(calls(folder.path())[0]["arguments"], argument);
            } else {
                let actual = std::fs::read_to_string(folder.path().join("marker")).unwrap();
                assert_eq!(
                    actual,
                    argument
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or(secret)
                );
            }
            run.finish();
        }
        for (secret, argument) in [
            (
                "opaque-value-12345",
                json!({"value":"{\"opaque-value-12345\":\"visible\"}"}),
            ),
            ("approval", json!({"value":"visible"})),
        ] {
            let folder = tempfile::tempdir().unwrap();
            let mut config = config(folder.path(), "modern");
            config.env.insert(
                "TOKEN".into(),
                EnvironmentValue::Reference {
                    from_env: "SOURCE".into(),
                },
            );
            let run = Run::new(
                vec![config],
                Mode::Ask,
                BTreeMap::from([("SOURCE".into(), secret.into())]),
                vec![Reply::Calls(vec![(mcp_alias(), argument)])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let events = run.turn();
            assert!(!events
                .iter()
                .any(|e| matches!(e.event, Event::ApprovalRequested { .. })));
            assert!(calls(folder.path()).is_empty());
            run.finish();
        }
    }

    #[test]
    fn structural_approval_cards_preserve_sensitive_containers_and_mask_provenance() {
        for (secret, argument, expected) in [
            (
                "ACT",
                json!({"value":r#"{"message":"ACT"}"#}),
                Some(json!({"value":r#"{"message":"[REDACTED]"}"#})),
            ),
            (
                "opaque-value-12345",
                json!({"private_config":r#"{"wipe":true}"#}),
                Some(json!({"private_config":r#"{"wipe":true}"#})),
            ),
            (
                "opaque-value-12345",
                json!({"key":r#"{"opaque-value-12345":"visible"}"#}),
                None,
            ),
            (
                "opaque-value-12345",
                json!({"value":r#"{"message":"\u0065cho a\/b opaque-value-12345"}"#}),
                None,
            ),
        ] {
            let folder = tempfile::tempdir().unwrap();
            let mut config = config(folder.path(), "modern");
            config.env.insert(
                "TOKEN".into(),
                EnvironmentValue::Reference {
                    from_env: "SOURCE".into(),
                },
            );
            let run = Run::new(
                vec![config],
                Mode::Ask,
                BTreeMap::from([("SOURCE".into(), secret.into())]),
                vec![Reply::Calls(vec![(mcp_alias(), argument.clone())])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let events = run.turn();
            let card = events.iter().find_map(|event| match &event.event {
                Event::ApprovalRequested { input, .. } => Some(input["arguments"].clone()),
                _ => None,
            });
            assert_eq!(card, expected);
            if card.is_some() {
                assert_eq!(calls(folder.path())[0]["arguments"], argument);
            } else {
                assert!(calls(folder.path()).is_empty());
                assert!(events.iter().any(|event| matches!(
                    event.event,
                    Event::ItemCompleted {
                        status: agent_core::event::ItemStatus::Failed,
                        ..
                    }
                )));
            }
            run.finish();
        }
    }

    #[test]
    fn prose_snapshots_match_streamed_deltas_and_preserve_json_formatting() {
        let secret = "opaque-value-12345";
        for reply in [
            format!("[link](https://example.com) mentions {secret} and keeps the rest."),
            format!("{{\n  \"message\" : \"{secret}\",\n  \"number\" : 2\n}}\n"),
        ] {
            let folder = tempfile::tempdir().unwrap();
            let mut config = config(folder.path(), "modern");
            config.env.insert(
                "TOKEN".into(),
                EnvironmentValue::Reference {
                    from_env: "SOURCE".into(),
                },
            );
            let run = Run::new(
                vec![config],
                Mode::Ask,
                BTreeMap::from([("SOURCE".into(), secret.into())]),
                vec![Reply::Stream {
                    field: "content",
                    chunks: vec![reply.clone()],
                }],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let events = run.turn();
            let deltas = events
                .iter()
                .filter_map(|e| match &e.event {
                    Event::ContentDelta { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            let snapshot = events
                .iter()
                .find_map(|e| match &e.event {
                    Event::ContentSnapshot { text, .. } => Some(text),
                    _ => None,
                })
                .unwrap();
            assert_eq!(*snapshot, reply.replace(secret, "[REDACTED]"));
            assert_eq!(*snapshot, deltas);
            let (bodies, transcript) = run.finish();
            assert_eq!(bodies.len(), 1);
            assert!(!transcript.contains(secret));
            let saved: Value = serde_json::from_str(&transcript).unwrap();
            let assistant = saved["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "assistant")
                .unwrap();
            assert_eq!(assistant["content"], reply.replace(secret, "[REDACTED]"));
        }
    }

    #[test]
    fn a_connection_closed_before_dispatch_never_opens_an_approval_card() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "modern")],
            Mode::Ask,
            BTreeMap::new(),
            vec![Reply::CallsAfterClose(
                folder.path().to_owned(),
                vec![(mcp_alias(), json!({}))],
            )],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let events = run.turn();
        assert!(!events
            .iter()
            .any(|e| matches!(e.event, Event::ApprovalRequested { .. })));
        assert!(events.iter().any(|e|matches!(&e.event,Event::ItemCompleted {status:agent_core::event::ItemStatus::Failed,error:Some(error),..} if error.contains("The MCP connection closed before the call was approved."))));
        assert!(calls(folder.path()).is_empty());
        run.finish();
    }

    #[test]
    fn each_mode_keeps_readonly_and_approval_separate_even_with_server_hints() {
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits, Mode::Bypass] {
            for readonly in [true, false] {
                for policy in [
                    ApprovalPolicy::Ask,
                    ApprovalPolicy::Allow,
                    ApprovalPolicy::Deny,
                ] {
                    let folder = tempfile::tempdir().unwrap();
                    let mut config = config(folder.path(), "modern");
                    config.tools[0].read_only = readonly;
                    config.approval = policy;
                    let run = Run::new(
                        vec![config],
                        mode,
                        BTreeMap::new(),
                        vec![Reply::Calls(vec![(mcp_alias(), json!({}))])],
                        None,
                    );
                    run.send(Command::Prompt {
                        text: "test".into(),
                    });
                    let events = run.turn();
                    let denied = policy == ApprovalPolicy::Deny || mode == Mode::Plan && !readonly;
                    let asks = !denied && mode != Mode::Bypass && policy != ApprovalPolicy::Allow;
                    assert_eq!(
                        events
                            .iter()
                            .filter(|e| matches!(e.event, Event::ApprovalRequested { .. }))
                            .count(),
                        usize::from(asks),
                        "{mode:?} {readonly} {policy:?}"
                    );
                    assert_eq!(
                        calls(folder.path()).len(),
                        usize::from(!denied),
                        "{mode:?} {readonly} {policy:?}"
                    );
                    run.finish();
                }
            }
        }
    }

    #[test]
    fn registry_routes_exact_aliases_and_ignores_model_server_fields() {
        let folder = tempfile::tempdir().unwrap();
        let config = config(folder.path(), "modern");
        let fake = "mcp__fixture__read".to_owned();
        let run = Run::new(
            vec![config],
            Mode::Bypass,
            BTreeMap::new(),
            vec![Reply::Calls(vec![
                (fake, json!({})),
                (
                    mcp_alias(),
                    json!({"server":"forged-server","tool":"forged-tool"}),
                ),
            ])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        run.turn();
        assert_eq!(calls(folder.path()).len(), 1);
        assert_eq!(calls(folder.path())[0]["name"], "read");
        assert_eq!(
            calls(folder.path())[0]["arguments"]["server"],
            "forged-server"
        );
        let (bodies, _) = run.finish();
        let tools = bodies[0]["tools"].as_array().unwrap();
        assert!(tools[..6]
            .iter()
            .all(|t| !t["function"]["name"].as_str().unwrap().starts_with("mcp_")));
        assert_eq!(tools.last().unwrap()["function"]["name"], mcp_alias());
    }

    #[test]
    fn invalid_arguments_and_uncompilable_schemas_never_reach_approval_or_dispatch() {
        for scenario in [
            "strict-schema",
            "invalid-schema",
            "external-schema",
            "backtracking-schema",
        ] {
            let folder = tempfile::tempdir().unwrap();
            let run = Run::new(
                vec![config(folder.path(), scenario)],
                Mode::Bypass,
                BTreeMap::new(),
                vec![Reply::Calls(vec![(
                    mcp_alias(),
                    json!({"value":"private-canary","unexpected":true}),
                )])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let events = run.turn();
            assert!(events
                .iter()
                .all(|e| !matches!(e.event, Event::ApprovalRequested { .. })));
            assert!(calls(folder.path()).is_empty(), "{scenario}");
            if scenario != "strict-schema" {
                assert!(events.iter().any(|e| matches!(&e.event,Event::Notice {text} if text.contains("could not compile"))));
            }
            run.finish();
        }
    }

    #[test]
    fn cut_off_replies_and_over_budget_batches_execute_nothing() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "modern")],
            Mode::Bypass,
            BTreeMap::new(),
            vec![Reply::Cut(mcp_alias())],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let events = run.turn();
        assert!(events.iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted {
                state: TurnState::Failed,
                ..
            }
        )));
        assert!(calls(folder.path()).is_empty());
        run.finish();
        let run = Run::new(
            vec![config(folder.path(), "modern")],
            Mode::Bypass,
            BTreeMap::new(),
            vec![Reply::Calls(vec![
                (mcp_alias(), json!({"large":"x".repeat(100_000)})),
                (mcp_alias(), json!({})),
            ])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        run.turn();
        assert!(calls(folder.path()).is_empty());
        run.finish();
    }

    #[test]
    fn missing_malformed_and_stale_bindings_decline_the_answered_call_without_waiting() {
        for field in [
            "missing",
            "malformed",
            "generation",
            "server",
            "schema",
            "request",
            "arguments",
        ] {
            let folder = tempfile::tempdir().unwrap();
            let run = Run::new(
                vec![config(folder.path(), "strict-schema")],
                Mode::Ask,
                BTreeMap::new(),
                vec![Reply::Calls(vec![(mcp_alias(), json!({"value":"first"}))])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let card = loop {
                let env = run.next();
                if matches!(env.event, Event::ApprovalRequested { .. }) {
                    break env;
                }
            };
            let Event::ApprovalRequested { input, .. } = &card.event else {
                unreachable!()
            };
            let original: agent_core::local_mcp::ApprovalCard =
                serde_json::from_value(input.clone()).unwrap();
            let mut stamp = original.binding;
            match field {
                "generation" => stamp.generation.push('x'),
                "server" => stamp.grant.server_revision = "0".repeat(64),
                "schema" => stamp.grant.schema_revision = "0".repeat(64),
                "request" => stamp.request_id.push('x'),
                "arguments" => stamp.call_id.push('x'),
                _ => {}
            }
            let updated_input = match field {
                "missing" => None,
                "malformed" => Some(json!({"binding":null})),
                _ => Some(json!({"binding":stamp,"edited_arguments":null})),
            };
            run.send(Command::Approve {
                request: card.request.clone().unwrap(),
                decision: Decision::Allow,
                updated_input,
                message: None,
            });
            let events = run.turn();
            assert!(
                events.iter().any(|env| {
                    env.request == card.request
                        && matches!(
                            env.event,
                            Event::ApprovalResolved {
                                decision: Decision::Deny
                            }
                        )
                }),
                "{field}"
            );
            assert!(!events
                .iter()
                .any(|env| matches!(env.event, Event::ApprovalRequested { .. })));
            assert!(calls(folder.path()).is_empty(), "{field}");
            run.finish();
        }
    }

    #[test]
    fn argument_edits_require_a_new_validated_approval() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "strict-schema")],
            Mode::Ask,
            BTreeMap::new(),
            vec![Reply::Calls(vec![(mcp_alias(), json!({"value":"first"}))])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let card = loop {
            let env = run.next();
            if matches!(env.event, Event::ApprovalRequested { .. }) {
                break env;
            }
        };
        let Event::ApprovalRequested { input, .. } = &card.event else {
            unreachable!()
        };
        let original: agent_core::local_mcp::ApprovalCard =
            serde_json::from_value(input.clone()).unwrap();
        run.send(Command::Approve {
            request: card.request.clone().unwrap(),
            decision: Decision::Allow,
            updated_input: Some(
                json!({"binding":original.binding,"edited_arguments":{"value":"edited"}}),
            ),
            message: None,
        });
        let fresh = loop {
            let env = run.next();
            if matches!(env.event, Event::ApprovalRequested { .. }) {
                break env;
            }
        };
        assert_ne!(fresh.request, card.request);
        assert!(calls(folder.path()).is_empty());
        run.answer(&fresh, Decision::Allow);
        run.turn();
        assert_eq!(calls(folder.path())[0]["arguments"]["value"], "edited");
        run.finish();
    }

    #[test]
    fn invalid_edited_arguments_expire_the_card_without_dispatch_or_a_replacement_card() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "strict-schema")],
            Mode::Ask,
            BTreeMap::new(),
            vec![Reply::Calls(vec![(mcp_alias(), json!({"value":"first"}))])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let card = loop {
            let env = run.next();
            if matches!(env.event, Event::ApprovalRequested { .. }) {
                break env;
            }
        };
        let Event::ApprovalRequested { input, .. } = &card.event else {
            unreachable!()
        };
        let original: agent_core::local_mcp::ApprovalCard =
            serde_json::from_value(input.clone()).unwrap();
        run.send(Command::Approve {
            request: card.request.clone().unwrap(),
            decision: Decision::Allow,
            updated_input: Some(json!({"binding":original.binding,"edited_arguments":{"value":1}})),
            message: None,
        });
        let events = run.turn();
        assert!(events
            .iter()
            .any(|env| env.request == card.request && matches!(env.event, Event::ApprovalExpired)));
        assert!(!events
            .iter()
            .any(|env| matches!(env.event, Event::ApprovalRequested { .. })));
        assert!(calls(folder.path()).is_empty());
        run.finish();
    }

    #[test]
    fn builtins_and_mcp_share_a_batch_and_dispatch_serially() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("example.txt"), "fixture bytes").unwrap();
        let run = Run::new(
            vec![config(folder.path(), "modern")],
            Mode::Bypass,
            BTreeMap::new(),
            vec![Reply::Calls(vec![
                ("read_file".into(), json!({"file_path":"example.txt"})),
                (mcp_alias(), json!({})),
            ])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let events = run.turn();
        let started: Vec<_> = events
            .iter()
            .filter_map(|env| match &env.event {
                Event::ItemStarted { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();
        assert!(started.contains(&agent_core::event::ItemKind::McpTool));
        let (bodies, _) = run.finish();
        let tools: Vec<_> = bodies[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        assert_eq!(tools.len(), 2);
        assert!(tools[0]["content"]
            .as_str()
            .unwrap()
            .contains("fixture bytes"));
        assert_eq!(calls(folder.path()).len(), 1);
        assert!(bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == "read_file"));
        assert!(bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == mcp_alias()));
    }

    #[test]
    fn runner_remembers_no_approval_and_stop_expires_pending_and_cancels_a_call() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "modern")],
            Mode::Ask,
            BTreeMap::new(),
            vec![Reply::Calls(vec![
                (mcp_alias(), json!({})),
                (mcp_alias(), json!({})),
            ])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let mut count = 0;
        loop {
            let env = run.next();
            if matches!(env.event, Event::ApprovalRequested { .. }) {
                count += 1;
                run.answer(&env, Decision::AllowForSession);
            }
            if matches!(env.event, Event::TurnCompleted { .. }) {
                break;
            }
        }
        assert_eq!(count, 2);
        run.finish();
        for mode in [Mode::Ask, Mode::Bypass] {
            let folder = tempfile::tempdir().unwrap();
            let run = Run::new(
                vec![config(folder.path(), "call-stall")],
                mode,
                BTreeMap::new(),
                vec![Reply::Calls(vec![
                    (mcp_alias(), json!({})),
                    (mcp_alias(), json!({})),
                ])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            if mode == Mode::Ask {
                loop {
                    if matches!(run.next().event, Event::ApprovalRequested { .. }) {
                        break;
                    }
                }
            } else {
                let until = Instant::now() + Duration::from_secs(3);
                while calls(folder.path()).is_empty() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            run.send(Command::Interrupt);
            let events = run.turn();
            assert!(events.iter().any(|e| matches!(
                e.event,
                Event::TurnCompleted {
                    state: TurnState::Interrupted,
                    ..
                }
            )));
            assert_eq!(
                calls(folder.path()).len(),
                usize::from(mode == Mode::Bypass)
            );
            let (_, transcript) = run.finish();
            let transcript: Value = serde_json::from_str(&transcript).unwrap();
            assert_eq!(
                transcript["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .count(),
                2
            );
            assert_dead(folder.path());
        }
    }

    #[test]
    fn errors_disconnects_and_timeouts_are_never_replayed() {
        for scenario in ["call-stall", "call-exit", "call-error"] {
            let folder = tempfile::tempdir().unwrap();
            let run = Run::new(
                vec![config(folder.path(), scenario)],
                Mode::Bypass,
                BTreeMap::new(),
                vec![Reply::Calls(vec![(mcp_alias(), json!({}))])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            run.turn();
            assert_eq!(calls(folder.path()).len(), 1);
            let (bodies, _) = run.finish();
            assert_eq!(bodies.len(), 2);
            let result = bodies[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(result).unwrap()["isError"],
                true
            );
        }
    }

    #[test]
    fn a_restrictive_mode_change_cancels_an_active_mcp_call_without_replay() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "call-stall")],
            Mode::Bypass,
            BTreeMap::new(),
            vec![Reply::Calls(vec![(mcp_alias(), json!({}))])],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let until = Instant::now() + Duration::from_secs(4);
        while calls(folder.path()).is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(calls(folder.path()).len(), 1);
        run.send(Command::SetMode { mode: Mode::Ask });
        let events = run.turn();
        assert!(events
            .iter()
            .any(|env| matches!(env.event, Event::ModeChanged { mode: Mode::Ask })));
        assert!(events.iter().any(|env| matches!(
            env.event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                ..
            }
        )));
        let (_, transcript) = run.finish();
        assert_eq!(calls(folder.path()).len(), 1);
        assert!(transcript.contains("outcome may be unknown"));
        assert_dead(folder.path());
    }

    #[test]
    fn secret_canaries_never_reach_cards_results_or_transcripts_even_after_disconnect() {
        for scenario in ["modern", "structured-result", "call-error", "call-exit"] {
            let folder = tempfile::tempdir().unwrap();
            let mut config = config(folder.path(), scenario);
            config.env.insert(
                "TOKEN".into(),
                EnvironmentValue::Reference {
                    from_env: "SOURCE".into(),
                },
            );
            let secret = "unpatterned-雪-\"credential\"-12345";
            config.label = format!("Example {secret}");
            let run = Run::new(
                vec![config],
                Mode::Ask,
                BTreeMap::from([("SOURCE".into(), secret.into())]),
                vec![Reply::Calls(vec![(
                    mcp_alias(),
                    json!({"nested":[{"value":secret}],"api_key":"another-private-value-123"}),
                )])],
                None,
            );
            run.send(Command::Prompt {
                text: "test".into(),
            });
            let events = run.turn();
            let rendered = serde_json::to_string(&events).unwrap();
            assert!(!rendered.contains("unpatterned"));
            assert!(!rendered.contains("another-private"));
            let store = agent_kit::store::Store::open_in_memory().unwrap();
            let thread = store.create_thread("/workspace", None).unwrap();
            for event in &events {
                store.append_event(&thread, None, event).unwrap();
            }
            let persisted =
                serde_json::to_string(&store.events(&thread, None, 1000).unwrap()).unwrap();
            assert!(!persisted.contains("unpatterned"));
            assert!(!persisted.contains("another-private"));
            let (bodies, transcript) = run.finish();
            for body in &bodies {
                let encoded = body.to_string();
                assert!(!encoded.contains("unpatterned"));
                assert!(!encoded.contains("another-private"));
            }
            assert!(
                !transcript.contains("unpatterned"),
                "{scenario}: {transcript}"
            );
            assert!(
                !transcript.contains("another-private"),
                "{scenario}: {transcript}"
            );
            assert_eq!(calls(folder.path()).len(), 1);
            let result = bodies[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert!(!result.contains("unpatterned"));
            let parsed: Value = serde_json::from_str(result).unwrap();
            assert_eq!(parsed["isError"], true);
            assert!(result.len() <= 30000);
            assert!(!result.contains("BINARY-CANARY"));
            assert!(!result.contains("https://example.com/no-fetch"));
        }
    }

    #[test]
    fn whole_schema_exclusions_are_reported_once_and_never_execute() {
        let folder = tempfile::tempdir().unwrap();
        let mut config = config(folder.path(), "budget-schema");
        config.tools.push(McpToolPolicy {
            name: "large".into(),
            read_only: true,
            approval: ApprovalPolicy::Inherit,
        });
        let large = agent_core::local_mcp::alias("fixture", "large").unwrap();
        let run = Run::new(
            vec![config],
            Mode::Bypass,
            BTreeMap::new(),
            vec![
                Reply::Calls(vec![(large.clone(), json!({})), (mcp_alias(), json!({}))]),
                Reply::Calls(vec![(mcp_alias(), json!({}))]),
            ],
            None,
        );
        run.send(Command::Prompt {
            text: "test".into(),
        });
        let events = run.turn();
        assert_eq!(calls(folder.path()).len(), 2);
        assert_eq!(
            events
                .iter()
                .filter(
                    |e| matches!(&e.event,Event::Notice {text} if text.starts_with("Excluded tool"))
                )
                .count(),
            1
        );
        let (bodies, _) = run.finish();
        for body in bodies {
            assert!(body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["function"]["name"] != large));
        }
    }

    #[test]
    fn tool_list_changes_freeze_the_current_turn_and_invalidate_the_next_grant() {
        let folder = tempfile::tempdir().unwrap();
        let run = Run::new(
            vec![config(folder.path(), "changed-schema")],
            Mode::Ask,
            BTreeMap::new(),
            vec![
                Reply::Calls(vec![(mcp_alias(), json!({"value":"abc"}))]),
                Reply::Answer,
                Reply::Calls(vec![(mcp_alias(), json!({"value":"abc"}))]),
            ],
            None,
        );
        let mut stamps = Vec::new();
        for turn in ["one", "two"] {
            run.send(Command::Prompt { text: turn.into() });
            loop {
                let env = run.next();
                if let Event::ApprovalRequested { input, .. } = &env.event {
                    stamps.push(
                        serde_json::from_value::<agent_core::local_mcp::ApprovalCard>(
                            input.clone(),
                        )
                        .unwrap()
                        .binding,
                    );
                    run.answer(&env, Decision::AllowAlways);
                }
                if matches!(env.event, Event::TurnCompleted { .. }) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        assert_eq!(stamps.len(), 2);
        assert_ne!(
            stamps[0].grant.schema_revision,
            stamps[1].grant.schema_revision
        );
        assert_eq!(
            stamps[0].grant.server_revision,
            stamps[1].grant.server_revision
        );
        assert_eq!(calls(folder.path()).len(), 2);
        run.finish();
    }
}
