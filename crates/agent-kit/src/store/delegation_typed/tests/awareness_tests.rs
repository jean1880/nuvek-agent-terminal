//! Pages a parent reads: an activity line is one short line, and a question or a form the user
//! has to answer says so.
use super::*;
use agent_core::event::{ItemStatus, Question};

/// Publishes `event` as `worker`'s, linked under `category`, and returns the page events a
/// root reads afterwards.
fn page_of(event: Event, category: EventCategory) -> Vec<Value> {
    let f = fx();
    let child = spawn(&f.s, &f.root.worker, "k1").worker.expect("w");
    let thread =
        f.s.delegation_worker(child.as_str())
            .expect("q")
            .expect("row")
            .backing_thread_id
            .expect("t");
    f.s.publish_delegation_event(&PublishEvent {
        thread,
        provider_thread: None,
        envelope: Envelope::new(event),
        links: vec![LinkSpec {
            process_generation: 0,
            worker: child.clone(),
            run: None,
            category,
            invocation: None,
        }],
        worker_states: Vec::new(),
        run_states: Vec::new(),
        aliases: Vec::new(),
    })
    .expect("published");
    let scope = vec![f.root.worker.clone(), child];
    let out =
        f.s.read_delegation_page(&query(&f.root.worker, &f.root.worker, scope, None))
            .expect("s")
            .expect("page");
    out.page["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e["category"] == json!(name_of(&category)))
        .collect()
}

fn completed(output: &str) -> Event {
    Event::ItemCompleted {
        status: ItemStatus::Completed,
        output: Some(output.to_owned()),
        error: None,
    }
}

#[test]
fn an_activity_summary_is_its_first_line_cut_on_a_character_boundary() {
    let long = format!("{}\nsecond line", "é".repeat(400));
    let events = page_of(completed(&long), EventCategory::Activity);
    let summary = events[0]["summary"].as_str().expect("summary");
    assert!(summary.len() <= 240, "{} bytes", summary.len());
    assert!(summary.ends_with('…'));
    assert!(!summary.contains("second line"));
}

#[test]
fn a_multi_line_output_page_shows_only_its_first_line() {
    let events = page_of(
        completed("running 3 tests\ntest a ... ok\ntest b ... ok"),
        EventCategory::Activity,
    );
    let summary = events[0]["summary"].as_str().expect("summary");
    assert!(summary.starts_with("item completed: running 3 tests"));
    assert!(!summary.contains("test a"));
}

#[test]
fn an_activity_summary_hides_a_secret_before_it_is_cut() {
    let secret = format!("ghp_{}", "a1B2".repeat(10));
    let line = format!("{}{secret}", "x".repeat(215));
    let events = page_of(completed(&line), EventCategory::Activity);
    let summary = events[0]["summary"].as_str().expect("summary");
    assert!(!summary.contains("ghp_a1B2"), "{summary}");
}

#[test]
fn a_question_page_names_the_question_and_is_an_approval() {
    let events = page_of(
        Event::QuestionRequested {
            questions: vec![Question {
                id: "q1".into(),
                header: "Scope".into(),
                question: "Which crate should I change?\nplease say".into(),
                options: Vec::new(),
                multi_select: false,
            }],
            source: agent_core::event::QuestionSource::Agent,
        },
        EventCategory::Approval,
    );
    assert_eq!(events.len(), 1);
    let summary = events[0]["summary"].as_str().expect("summary");
    assert!(summary.starts_with("question: Which crate should I change?"));
    assert!(!summary.contains("please say"));
}

#[test]
fn a_cut_never_lands_inside_a_redaction_marker() {
    for pad in 215..=240 {
        let text = format!("{}****abcd{}", "x".repeat(pad), "y".repeat(100));
        let summary = first_line(&text, 240);
        assert!(summary.len() <= 240, "{pad}: {} bytes", summary.len());
        let body = summary.trim_end_matches('…');
        match body.find("****") {
            None => {}
            Some(at) => assert_eq!(&body[at..at + 8], "****abcd", "pad {pad}: {body}"),
        }
        assert!(
            !body.ends_with("*") && !body.ends_with("***"),
            "pad {pad}: {body}"
        );
    }
}

#[test]
fn a_form_page_names_the_form() {
    let events = page_of(
        Event::FormRequested {
            server: "github".into(),
            message: "Confirm the repository".into(),
            form: agent_core::form::Form::Fields { fields: Vec::new() },
        },
        EventCategory::Approval,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["summary"].as_str(),
        Some("form: Confirm the repository")
    );
}
