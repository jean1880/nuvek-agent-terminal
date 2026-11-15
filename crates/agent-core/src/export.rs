//! A thread as one document a person can read outside the app: Markdown or JSON, rendered from a
//! plain list of items. Pure text in, text out: reading the store and writing the file are the
//! caller's.
//!
//! What the document holds: each message under its speaker (the agent that wrote it, so a thread
//! that switched agents is labelled correctly), each tool with its one-line title and, when asked,
//! its output fenced and capped, the model's reasoning only when asked, and a divider wherever the
//! agent changes or the app said something. An image is only ever its `[image: name]` line, never
//! bytes or ids. The finished document is redacted once more as a whole: the stored text is
//! already scrubbed, this is the belt to those braces.

use serde::Serialize;

use crate::adapter::Driver;
use crate::fence::{fenced, quote};
use crate::redact::redact;

/// `format` of a JSON export.
pub const JSON_FORMAT: &str = "agent-terminal-thread";
/// `version` of a JSON export: the contract for later changes.
pub const JSON_VERSION: u32 = 1;
/// The most characters of one tool's output a document keeps.
pub const TOOL_OUTPUT_CAP: usize = 20_000;
/// What the document says of its masking, and the export dialog too.
pub const REDACTION_NOTE: &str =
    "Secrets matching known patterns are masked; one in another format may remain.";

/// `role` of an item holding a thread's inherited context (a fork's or compaction's seed).
pub const ROLE_INHERITED: &str = "inherited";
/// `role` of an item holding a notice the app posted.
pub const ROLE_NOTICE: &str = "notice";

/// The file formats an export can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    #[default]
    Markdown,
    Json,
}

impl ExportFormat {
    /// Every format, in the order the dialog's dropdown offers them (the first is the default).
    pub const ALL: [ExportFormat; 2] = [ExportFormat::Markdown, ExportFormat::Json];

    /// The name the dropdown shows.
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Json => "JSON",
        }
    }

    /// The file extension, without its dot.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            ExportFormat::Json => "json",
        }
    }
}

/// What a document keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// The model's private reasoning. Off by default: a hand-off never carries it either.
    pub include_reasoning: bool,
    /// What each tool printed, fenced and capped. On by default; off leaves each tool's title.
    pub include_tool_output: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Markdown,
            include_reasoning: false,
            include_tool_output: true,
        }
    }
}

/// What the document says of the thread itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExportMeta {
    pub id: String,
    pub title: String,
    pub folder: String,
    /// When the thread was last active, in seconds since the Unix epoch.
    pub last_active: Option<i64>,
    /// The app's version, for the header.
    pub app_version: String,
}

/// One folded item of a thread, in order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExportItem {
    /// `user`, `assistant`, `tool`, [`ROLE_INHERITED`] or [`ROLE_NOTICE`].
    pub role: String,
    /// The item kind in snake case (`user_message`, `reasoning`, `command`, …).
    pub kind: String,
    pub text: String,
    /// `open`, `completed`, `failed`, `declined` or `interrupted`.
    pub status: String,
    /// A tool's one-line title (`Run cargo test`); empty for any other item.
    pub title: String,
    /// The driver key (`claude`, `agy`, …) the item came from, when known.
    pub agent: Option<String>,
}

impl ExportItem {
    fn is_reasoning(&self) -> bool {
        self.kind == "reasoning"
    }
}

/// An agent's name as the document shows it: the driver's label, else the key as stored.
pub fn agent_label(key: &str) -> String {
    Driver::from_key(key).map_or_else(|| key.to_owned(), |d| d.info().label.to_owned())
}

/// `secs` since the Unix epoch as `yyyy-mm-dd` (UTC).
pub fn utc_date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `secs` since the Unix epoch as `yyyy-mm-ddThh:mm:ssZ` (UTC).
pub fn utc_timestamp(secs: i64) -> String {
    let rem = secs.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        utc_date(secs),
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// A file name for a thread: the title as lowercase words joined by `-`, at most 48 characters,
/// `thread` when nothing usable is left.
pub fn file_slug(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if slug.chars().count() >= 48 {
            break;
        }
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "thread".to_owned()
    } else {
        slug.to_owned()
    }
}

/// `text` cut to at most `cap` characters. The second value is how many were left out.
fn capped(text: &str, cap: usize) -> (&str, usize) {
    match text.char_indices().nth(cap) {
        Some((at, _)) => (&text[..at], text[at..].chars().count()),
        None => (text, 0),
    }
}

/// `text` on one line, for a heading or a divider.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` as an inline code span the text cannot close.
fn code_span(text: &str) -> String {
    let text = one_line(text);
    let mut ticks = "`".to_owned();
    while text.contains(&ticks) {
        ticks.push('`');
    }
    format!("{ticks} {text} {ticks}")
}

/// `text` as one italic line (`*…*`), its own asterisks escaped.
fn italic(text: &str) -> String {
    format!("*{}*", one_line(text).replace('*', "\\*"))
}

/// The agent of each item: its own, else the last one seen (a notice or a held item may carry
/// none).
fn resolved_agents(items: &[&ExportItem]) -> Vec<Option<String>> {
    let mut last: Option<String> = None;
    items
        .iter()
        .map(|i| {
            if let Some(a) = &i.agent {
                last = Some(a.clone());
            }
            last.clone()
        })
        .collect()
}

/// The agents used, in the order they first appear.
fn agents_used(agents: &[Option<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in agents.iter().flatten() {
        if !out.contains(a) {
            out.push(a.clone());
        }
    }
    out
}

/// The items a document holds under `options`: reasoning only when asked, empty reasoning never.
fn kept<'a>(items: &'a [ExportItem], options: &ExportOptions) -> Vec<&'a ExportItem> {
    items
        .iter()
        .filter(|i| !i.is_reasoning() || (options.include_reasoning && !i.text.trim().is_empty()))
        .collect()
}

/// The document for `items` in `options.format`, redacted as a whole.
pub fn render(meta: &ExportMeta, items: &[ExportItem], options: &ExportOptions) -> String {
    match options.format {
        ExportFormat::Markdown => redact(&render_markdown(meta, items, options)),
        ExportFormat::Json => render_json(meta, items, options),
    }
}

fn render_markdown(meta: &ExportMeta, items: &[ExportItem], options: &ExportOptions) -> String {
    let items = kept(items, options);
    let agents = resolved_agents(&items);
    let mut out = String::new();
    let title = one_line(&meta.title);
    out.push_str(&format!(
        "# {}\n\n",
        if title.is_empty() { "Thread" } else { &title }
    ));
    if !meta.folder.is_empty() {
        out.push_str(&format!("- **Folder:** {}\n", code_span(&meta.folder)));
    }
    if let Some(at) = meta.last_active {
        out.push_str(&format!("- **Last active:** {}\n", utc_date(at)));
    }
    let used: Vec<String> = agents_used(&agents)
        .iter()
        .map(|a| agent_label(a))
        .collect();
    if !used.is_empty() {
        out.push_str(&format!("- **Agents:** {}\n", used.join(", ")));
    }
    if !meta.app_version.is_empty() {
        out.push_str(&format!(
            "- **Exported by:** Agent Terminal {}\n",
            one_line(&meta.app_version)
        ));
    }
    out.push_str(&format!("\n{}\n", italic(REDACTION_NOTE)));

    let mut previous: Option<&str> = None;
    for (item, agent) in items.iter().zip(&agents) {
        if let Some(agent) = agent.as_deref() {
            if previous.is_some_and(|p| p != agent) {
                out.push_str(&format!(
                    "\n---\n\n{}\n",
                    italic(&format!("Switched to {}", agent_label(agent)))
                ));
            }
            previous = Some(agent);
        }
        let label = agent
            .as_deref()
            .map_or_else(|| "Agent".to_owned(), agent_label);
        out.push('\n');
        out.push_str(&markdown_block(item, &label, options));
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// One item as Markdown, ending in a newline.
fn markdown_block(item: &ExportItem, label: &str, options: &ExportOptions) -> String {
    let text = item.text.trim_end_matches('\n');
    match item.role.as_str() {
        "user" => format!("### You\n\n{text}\n"),
        ROLE_INHERITED => format!(
            "---\n\n{}\n\n{}\n",
            italic("Context carried into this thread"),
            quote(text)
        ),
        ROLE_NOTICE => format!("---\n\n{}\n", italic(text)),
        "tool" => {
            let title = if item.title.trim().is_empty() {
                "Tool".to_owned()
            } else {
                one_line(&item.title)
            };
            let mut out = format!("**{}:** {}\n", item.kind.replace('_', " "), title);
            match item.status.as_str() {
                "failed" | "declined" | "interrupted" => {
                    out.push_str(&format!("\n{}\n", italic(&format!("({})", item.status))));
                }
                _ => {}
            }
            if options.include_tool_output && !text.trim().is_empty() {
                // Redacted whole first, then cut: a secret straddling the cut, or a PEM block
                // losing its END line, would otherwise leave a fragment the redactor cannot see.
                let masked = redact(text);
                let (shown, omitted) = capped(&masked, TOOL_OUTPUT_CAP);
                out.push('\n');
                out.push_str(&fenced(shown));
                out.push('\n');
                if omitted > 0 {
                    out.push_str(&format!(
                        "\n{}\n",
                        italic(&format!("[… {omitted} more characters omitted]"))
                    ));
                }
            }
            out
        }
        _ if item.is_reasoning() => {
            format!("> **Reasoning**\n>\n{}\n", quote(text))
        }
        _ => format!("### {label}\n\n{text}\n"),
    }
}

// ---- JSON ----

#[derive(Serialize)]
struct JsonThread {
    id: String,
    title: String,
    folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_active: Option<String>,
    agents: Vec<String>,
}

#[derive(Serialize)]
struct JsonItem {
    role: String,
    agent: Option<String>,
    kind: String,
    title: String,
    text: String,
    status: String,
    /// Characters of `text` left out by the per-tool cap; absent when none were.
    #[serde(skip_serializing_if = "is_zero")]
    omitted_chars: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Serialize)]
struct JsonDocument {
    format: &'static str,
    version: u32,
    app_version: String,
    note: &'static str,
    thread: JsonThread,
    items: Vec<JsonItem>,
}

/// The JSON document: every text field redacted first, so the whole-document pass can never be
/// what breaks the syntax; the whole is then redacted once more, and kept only if it still parses.
/// A secret that only a whole-document pass can see (a PEM block split across two items) is
/// caught only by that pass, which is discarded when it breaks the JSON: an accepted limit, since
/// each item's own text is already redacted.
fn render_json(meta: &ExportMeta, items: &[ExportItem], options: &ExportOptions) -> String {
    let items = kept(items, options);
    let agents = resolved_agents(&items);
    let json_items = items
        .iter()
        .zip(&agents)
        .map(|(item, agent)| {
            let is_tool = item.role == "tool";
            let (text, omitted) = if is_tool && !options.include_tool_output {
                (String::new(), 0)
            } else if is_tool {
                // Redacted whole first, then cut (see the Markdown renderer).
                let masked = redact(&item.text);
                let (shown, omitted) = capped(&masked, TOOL_OUTPUT_CAP);
                (shown.to_owned(), omitted)
            } else {
                (item.text.clone(), 0)
            };
            JsonItem {
                role: redact(&item.role),
                agent: agent.as_deref().map(redact),
                kind: redact(&item.kind),
                title: redact(&item.title),
                text: redact(&text),
                status: redact(&item.status),
                omitted_chars: omitted,
            }
        })
        .collect();
    let doc = JsonDocument {
        format: JSON_FORMAT,
        version: JSON_VERSION,
        app_version: redact(&meta.app_version),
        note: REDACTION_NOTE,
        thread: JsonThread {
            id: redact(&meta.id),
            title: redact(&meta.title),
            folder: redact(&meta.folder),
            last_active: meta.last_active.map(utc_timestamp),
            agents: agents_used(&agents).iter().map(|a| redact(a)).collect(),
        },
        items: json_items,
    };
    let mut text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    let again = redact(&text);
    if serde_json::from_str::<serde_json::Value>(&again).is_ok() {
        again
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(role: &str, kind: &str, text: &str, agent: Option<&str>) -> ExportItem {
        ExportItem {
            role: role.into(),
            kind: kind.into(),
            text: text.into(),
            status: "completed".into(),
            title: String::new(),
            agent: agent.map(str::to_owned),
        }
    }

    fn tool(title: &str, output: &str) -> ExportItem {
        ExportItem {
            title: title.into(),
            ..item("tool", "command", output, Some("claude"))
        }
    }

    fn meta() -> ExportMeta {
        ExportMeta {
            id: "t1".into(),
            title: "Fix the build".into(),
            folder: "/work/app".into(),
            last_active: Some(1_700_000_000),
            app_version: "1.2.3".into(),
        }
    }

    fn md() -> ExportOptions {
        ExportOptions::default()
    }

    fn json() -> ExportOptions {
        ExportOptions {
            format: ExportFormat::Json,
            ..ExportOptions::default()
        }
    }

    #[test]
    fn export_formats_cover_every_variant() {
        // Exhaustive: a new variant fails to compile here until it is listed in `ALL`.
        for f in ExportFormat::ALL {
            match f {
                ExportFormat::Markdown => assert_eq!(f.extension(), "md"),
                ExportFormat::Json => assert_eq!(f.extension(), "json"),
            }
            assert!(!f.label().is_empty());
        }
        assert_eq!(ExportFormat::ALL.len(), 2);
        assert_eq!(ExportFormat::ALL[0], ExportFormat::default());
        let mut seen = ExportFormat::ALL.to_vec();
        seen.dedup();
        assert_eq!(seen.len(), ExportFormat::ALL.len());
    }

    #[test]
    fn markdown_labels_each_message_with_its_agent() {
        let items = [
            item("user", "user_message", "hello", Some("claude")),
            item("assistant", "assistant_message", "hi", Some("claude")),
            item("notice", "notice", "Switched agent.", Some("codex")),
            item("user", "user_message", "again", Some("codex")),
            item(
                "assistant",
                "assistant_message",
                "hello again",
                Some("codex"),
            ),
        ];
        let doc = render(&meta(), &items, &md());
        assert!(doc.starts_with("# Fix the build\n"), "{doc}");
        assert!(doc.contains("- **Agents:** Claude, Codex"), "{doc}");
        assert!(doc.contains("### Claude\n\nhi"), "{doc}");
        assert!(doc.contains("### Codex\n\nhello again"), "{doc}");
        assert!(doc.contains("---\n\n*Switched to Codex*"), "{doc}");
        assert!(doc.contains("*Switched agent.*"), "{doc}");
        assert_eq!(doc.matches("### You").count(), 2);
        assert!(doc.contains(&format!("*{REDACTION_NOTE}*")), "{doc}");
        assert!(doc.contains("Agent Terminal 1.2.3"), "{doc}");
        assert!(doc.contains("**Last active:** 2023-11-14"), "{doc}");
    }

    #[test]
    fn tool_output_is_fenced_past_inner_backticks() {
        let doc = render(&meta(), &[tool("Run ls", "a\n```\nb\n````\nc")], &md());
        assert!(doc.contains("**command:** Run ls"), "{doc}");
        // The longest run inside is four, so the fence is five.
        assert!(doc.contains("`````\na\n```\nb\n````\nc\n`````"), "{doc}");
    }

    #[test]
    fn tool_output_is_capped_with_a_note() {
        let long = "x".repeat(TOOL_OUTPUT_CAP + 123);
        let doc = render(&meta(), &[tool("Run cat", &long)], &md());
        assert!(doc.contains(&"x".repeat(TOOL_OUTPUT_CAP)));
        assert!(!doc.contains(&"x".repeat(TOOL_OUTPUT_CAP + 1)));
        assert!(doc.contains("[… 123 more characters omitted]"), "{doc}");
        // Counted in characters, never cutting one in half.
        let wide = "é".repeat(TOOL_OUTPUT_CAP + 1);
        let doc = render(&meta(), &[tool("Run cat", &wide)], &md());
        assert!(doc.contains("[… 1 more characters omitted]"), "{doc}");
        let out = render(&meta(), &[tool("Run cat", &long)], &json());
        let v: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["items"][0]["omitted_chars"], 123);
    }

    #[test]
    fn tool_output_can_be_left_out() {
        let options = ExportOptions {
            include_tool_output: false,
            ..md()
        };
        let items = [tool("Run ls", "SECRET-LISTING")];
        let doc = render(&meta(), &items, &options);
        assert!(doc.contains("**command:** Run ls"), "{doc}");
        assert!(!doc.contains("SECRET-LISTING"), "{doc}");
        let options = ExportOptions {
            format: ExportFormat::Json,
            ..options
        };
        assert!(!render(&meta(), &items, &options).contains("SECRET-LISTING"));
    }

    #[test]
    fn images_render_as_name_placeholders_and_never_bytes_or_ids() {
        // The store hands over the message with its placeholder already in it.
        let items = [item(
            "user",
            "user_message",
            "look\n[image: shot.png]",
            Some("claude"),
        )];
        for options in [md(), json()] {
            let doc = render(&meta(), &items, &options);
            assert!(doc.contains("[image: shot.png]"), "{doc}");
            assert!(!doc.contains("base64"), "{doc}");
            assert!(!doc.contains("sha256"), "{doc}");
            assert!(!doc.contains("data:image"), "{doc}");
        }
    }

    #[test]
    fn reasoning_is_left_out_unless_asked() {
        let items = [
            item("user", "user_message", "q", Some("claude")),
            item("assistant", "reasoning", "private thoughts", Some("claude")),
            item("assistant", "assistant_message", "answer", Some("claude")),
        ];
        for format in ExportFormat::ALL {
            let off = ExportOptions { format, ..md() };
            let on = ExportOptions {
                include_reasoning: true,
                ..off
            };
            assert!(!render(&meta(), &items, &off).contains("private thoughts"));
            assert!(render(&meta(), &items, &on).contains("private thoughts"));
        }
        let on = ExportOptions {
            include_reasoning: true,
            ..md()
        };
        assert!(render(&meta(), &items, &on).contains("> **Reasoning**\n>\n> private thoughts"));
    }

    #[test]
    fn json_is_versioned_and_round_trips() {
        let items = [
            item("user", "user_message", "hi \"there\"\nline", Some("claude")),
            tool("Run ls", "out"),
            item("notice", "notice", "Switched.", Some("codex")),
        ];
        let doc = render(&meta(), &items, &json());
        let v: serde_json::Value = serde_json::from_str(&doc).expect("parses");
        assert_eq!(v["format"], "agent-terminal-thread");
        assert_eq!(v["version"], 1);
        assert_eq!(v["thread"]["id"], "t1");
        assert_eq!(v["thread"]["title"], "Fix the build");
        assert_eq!(v["thread"]["folder"], "/work/app");
        assert_eq!(v["thread"]["last_active"], "2023-11-14T22:13:20Z");
        assert_eq!(
            v["thread"]["agents"],
            serde_json::json!(["claude", "codex"])
        );
        assert_eq!(v["items"].as_array().map(Vec::len), Some(3));
        let first = &v["items"][0];
        assert_eq!(first["role"], "user");
        assert_eq!(first["agent"], "claude");
        assert_eq!(first["kind"], "user_message");
        assert_eq!(first["text"], "hi \"there\"\nline");
        assert_eq!(first["status"], "completed");
        assert_eq!(v["items"][1]["title"], "Run ls");
        assert_eq!(v["items"][1]["text"], "out");
        assert!(v["items"][1].get("omitted_chars").is_none());
    }

    #[test]
    fn the_document_is_redacted_again() {
        let token = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123456789");
        let items = [
            item("user", "user_message", &format!("my token {token}"), None),
            tool(&format!("Run echo {token}"), &format!("out {token}")),
        ];
        let mut m = meta();
        m.title = format!("title {token}");
        for options in [md(), json()] {
            let doc = render(&m, &items, &options);
            assert!(!doc.contains(&token), "{doc}");
            assert!(!doc.contains("ghp_abcdefghij"), "{doc}");
        }
        let doc = render(&m, &items, &json());
        assert!(serde_json::from_str::<serde_json::Value>(&doc).is_ok());
    }

    #[test]
    fn a_secret_straddling_the_cap_is_masked_before_the_cut() {
        let token = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123456789");
        // The token starts 10 characters before the cap, so cutting first would leave a fragment.
        let pad = "x ".repeat(TOOL_OUTPUT_CAP / 2 - 5);
        let output = format!("{pad}{token}{}", " tail".repeat(50));
        for options in [md(), json()] {
            let doc = render(&meta(), &[tool("Run cat", &output)], &options);
            assert!(!doc.contains("ghp_abc"), "{doc}");
            assert!(doc.contains("****"), "{doc}");
        }
    }

    #[test]
    fn inherited_context_and_failed_tools_are_marked() {
        let mut failed = tool("Run false", "");
        failed.status = "failed".into();
        let items = [
            item("inherited", "inherited_context", "earlier work", None),
            failed,
        ];
        let doc = render(&meta(), &items, &md());
        assert!(doc.contains("*Context carried into this thread*"), "{doc}");
        assert!(doc.contains("> earlier work"), "{doc}");
        assert!(doc.contains("*(failed)*"), "{doc}");
    }

    #[test]
    fn dates_and_slugs() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_timestamp(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(utc_date(-1), "1969-12-31");
        assert_eq!(file_slug("Fix the build!"), "fix-the-build");
        assert_eq!(file_slug("  ../../etc/passwd "), "etc-passwd");
        assert_eq!(file_slug("***"), "thread");
        assert!(file_slug(&"a".repeat(200)).len() <= 48);
    }

    #[test]
    fn an_empty_thread_still_renders_a_header() {
        let doc = render(&meta(), &[], &md());
        assert!(doc.starts_with("# Fix the build"));
        assert!(doc.ends_with('\n'));
    }
}
