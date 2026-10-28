//! What a handoff carries: each tool item under its title, read from real agent frames (an
//! empty-output command included); a fork's inherited context exactly once through later
//! switches; and the newest request always, cut when it is too long.

use super::*;
use agent_core::claude::ClaudeAdapter;
use agent_core::codex::CodexAdapter;
use serde_json::{json, Value};

const CLAUDE: &str =
    include_str!("../../../../crates/agent-core/tests/fixtures/claude-turn-approval.ndjson");
const CODEX: &str =
    include_str!("../../../../crates/agent-core/tests/fixtures/codex-synthetic-turn.ndjson");
const AGY: &str =
    include_str!("../../../../crates/agent-core/tests/fixtures/agy-hook-approval.ndjson");

/// The stdout frames of a recording, in order.
fn out_frames(fixture: &str) -> Vec<Value> {
    fixture
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["dir"] == "out")
        .map(|v| v["frame"].clone())
        .collect()
}

/// `frames` fed through `adapter` and stored in a fresh thread, as a session stores them; then
/// the handoff a switch would build from that thread.
fn handoff_of(adapter: &mut dyn Adapter, frames: &[Value]) -> Handoff {
    let store = Store::open_in_memory().expect("store");
    let thread = fresh(&store, "/work/repo");
    for frame in frames {
        for env in adapter.feed(&frame.to_string()) {
            store.append_event(&thread, None, &env).expect("stored");
        }
    }
    let messages = store.transcript_messages(&thread).expect("messages");
    build_handoff(&messages, "test thread")
}

/// The first frame of `kind` (its `type`) whose text contains `needle`, edited by `edit`.
fn edited(frames: &[Value], kind: &str, needle: &str, edit: impl FnOnce(&mut Value)) -> Value {
    let mut frame = frames
        .iter()
        .find(|f| f["type"] == kind && f.to_string().contains(needle))
        .cloned()
        .unwrap_or_else(|| panic!("no {kind} frame has {needle}"));
    edit(&mut frame);
    frame
}

#[test]
fn claude_tool_items_carry_their_titles_and_a_silent_command_still_shows() {
    let mut frames = out_frames(CLAUDE);
    // A Bash call whose result says nothing, from the recording's own Read call and result.
    let start = edited(&frames, "stream_event", "\"name\":\"Read\"", |f| {
        let block = &mut f["event"]["content_block"];
        block["id"] = json!("toolu_silent");
        block["name"] = json!("Bash");
    });
    let call = edited(&frames, "assistant", "\"name\":\"Read\"", |f| {
        let block = &mut f["message"]["content"][0];
        block["id"] = json!("toolu_silent");
        block["name"] = json!("Bash");
        block["input"] = json!({"command": "touch done.txt"});
        f["wire_tool_inputs"] = json!({"toolu_silent": {"command": "touch done.txt"}});
        f["message"]["id"] = json!("msg_silent");
        f["uuid"] = json!("00000000-0000-4000-8000-000000000001");
    });
    let stop = edited(&frames, "stream_event", "content_block_stop", |f| {
        f["event"]["index"] = json!(1);
    });
    let result = edited(&frames, "user", "toolu_013W6i8dWhVXTKNjh3dCqsXN", |f| {
        let block = &mut f["message"]["content"][0];
        block["tool_use_id"] = json!("toolu_silent");
        block["content"] = json!("");
        f["tool_use_result"] = json!({"stdout": "", "stderr": "", "interrupted": false});
    });
    // After the recorded edit's result, before the closing answer.
    let at = 1 + frames
        .iter()
        .position(|f| {
            f["type"] == "user" && f.to_string().contains("toolu_01FtAUGzxPrHFJZCNmbyLX9W")
        })
        .expect("the edit's result");
    frames.splice(at..at, [start, call, stop, result]);
    let handoff = handoff_of(&mut ClaudeAdapter::new(), &frames);
    let summary = &handoff.summary;
    // Claude stores the bare tool name; the handoff says what the tool acted on.
    assert!(
        summary.contains("Read /work/repo/calc.py"),
        "the read's target: {summary}"
    );
    assert!(
        summary.contains("Edit /work/repo/calc.py"),
        "the edit's target: {summary}"
    );
    assert!(
        summary.contains(": Run touch done.txt"),
        "the silent command: {summary}"
    );
    // The read's output is still carried after its title.
    assert!(summary.contains("def add(a, b):"), "{summary}");
}

#[test]
fn codex_commands_carry_their_command_line_with_or_without_output() {
    let frames = out_frames(CODEX);
    let handoff = handoff_of(&mut CodexAdapter::new(), &frames);
    assert!(
        handoff.summary.contains("command: ") && handoff.summary.contains("ls -la"),
        "{}",
        handoff.summary
    );
    assert!(handoff.summary.contains("a.txt"), "the output follows");

    // The same command with no output: the item still says what ran.
    let silent: Vec<Value> = frames
        .iter()
        .filter(|f| f["method"] != "item/commandExecution/outputDelta")
        .map(|f| {
            let mut f = f.clone();
            if f["method"] == "item/completed" && f["params"]["item"]["id"] == "cmd_1" {
                f["params"]["item"]["aggregatedOutput"] = json!("");
            }
            f
        })
        .collect();
    let handoff = handoff_of(&mut CodexAdapter::new(), &silent);
    let line = handoff
        .summary
        .lines()
        .find(|l| l.starts_with("command: "))
        .unwrap_or_else(|| panic!("no command line: {}", handoff.summary));
    assert!(line.contains("ls -la") || line.contains("List"), "{line}");
    assert!(!handoff.summary.contains("total 8"), "it printed nothing");
}

#[test]
fn agy_commands_carry_their_command_line_with_or_without_output() {
    let frames = out_frames(AGY);
    let handoff = handoff_of(&mut agent_core::agy::AgyAdapter::new("agy"), &frames);
    assert!(
        handoff.summary.contains(": Run echo allow-me"),
        "{}",
        handoff.summary
    );
    assert!(handoff.summary.contains(": Run echo deny-me"));

    // A command that printed nothing.
    let silent: Vec<Value> = frames
        .iter()
        .map(|f| {
            let mut f = f.clone();
            if f["step_update"]["tool_info"]["output"] == "allow-me\r\n" {
                f["step_update"]["tool_info"]["output"] = json!("");
            }
            f
        })
        .collect();
    let handoff = handoff_of(&mut agent_core::agy::AgyAdapter::new("agy"), &silent);
    assert!(
        handoff.summary.contains(": Run echo allow-me"),
        "{}",
        handoff.summary
    );
}

/// A fork, switched twice with history near the budget: what it inherited is carried exactly
/// once each time, and the newest request is always there.
#[test]
fn a_forks_inherited_context_survives_two_switches_once_with_the_newest_request() {
    let store = Store::open_in_memory().expect("store");
    // The source thread the fork was made from.
    let source = fresh(&store, "/w");
    store
        .append_user_message(&source, None, "INHERITED-REQUEST: port the parser")
        .expect("source request");
    let seed = build_handoff(
        &store.transcript_messages(&source).expect("source"),
        "Claude thread source",
    );
    assert!(seed.carried > 0);

    // The fork: created with that seed, then its own work.
    let fork = fresh(&store, "/w");
    for env in seeded_handoff_events(&seed, "“source”") {
        store.append_event(&fork, None, &env).expect("seeded");
    }
    let reply = |text: &str, id: &str| {
        for env in [
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            }),
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: text.to_owned(),
            }),
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            }),
        ] {
            store
                .append_event(&fork, None, &env.item(id))
                .expect("reply");
        }
    };
    store
        .append_user_message(&fork, None, "first own request")
        .expect("request");
    for i in 0..6 {
        reply(&format!("work {i} {}", "w".repeat(2_500)), &format!("a{i}"));
    }
    let switch = |driver: Driver| {
        let handoff = build_handoff(
            &store.transcript_messages(&fork).expect("fork"),
            "fork thread",
        );
        for env in switched_handoff_events(&handoff, driver) {
            store.append_event(&fork, None, &env).expect("switched");
        }
        handoff
    };
    let first = switch(Driver::Codex);
    assert_eq!(first.summary.matches("INHERITED-REQUEST").count(), 1);

    store
        .append_user_message(&fork, None, "NEWEST-REQUEST: now the tests")
        .expect("request");
    for i in 6..12 {
        reply(&format!("more {i} {}", "m".repeat(2_500)), &format!("a{i}"));
    }
    let second = switch(Driver::Claude);
    assert!(
        second.carried < second.total,
        "near the budget: not all of it fits"
    );
    assert_eq!(
        second.summary.matches("INHERITED-REQUEST").count(),
        1,
        "inherited once, never once per seed: {}",
        second.summary
    );
    assert!(second.summary.contains("NEWEST-REQUEST"));

    // An oversized latest request is kept, cut.
    let long = format!("HUGE-HEAD {} HUGE-TAIL", "h".repeat(40_000));
    store
        .append_user_message(&fork, None, &long)
        .expect("request");
    let third = switch(Driver::Codex);
    assert!(third.summary.contains("HUGE-HEAD"), "its head is kept");
    assert!(third.summary.contains("HUGE-TAIL"), "its tail is kept");
    assert!(third.summary.contains(" bytes omitted …]"));
}
