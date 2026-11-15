//! Exporting a thread to a file: read its folded items from the store, render the document
//! ([`agent_core::export`]), redact it once more and write it atomically with mode `0600`.
//!
//! The app writes only where the user chose, so no app folder is added here (nothing for
//! `uninstall` to remove). The read and the write are separate so a caller can do the first on
//! the store's own worker and the second on another thread; [`export_thread`] joins them.

use std::path::Path;

use agent_core::export::{render, ExportItem, ExportMeta, ExportOptions};

use crate::fsutil::write_private_atomic;
use crate::store::{FoldOptions, Store, StoreError, TranscriptItem, NOTICE_ROLE};

/// Why an export did not happen. Nothing is written for any of them.
#[derive(Debug)]
pub enum ExportError {
    /// The thread is not in the store.
    NotFound,
    /// The thread has nothing to export.
    Empty,
    /// The store could not be read.
    Store(StoreError),
    /// The file could not be written.
    Io(std::io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::NotFound => write!(f, "The thread no longer exists"),
            ExportError::Empty => write!(f, "The thread has nothing to export yet"),
            ExportError::Store(e) => write!(f, "Cannot read the thread: {e}"),
            ExportError::Io(e) => write!(f, "Cannot write the file: {e}"),
        }
    }
}

impl std::error::Error for ExportError {}

/// What was read of a thread: its details and its items, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSource {
    pub meta: ExportMeta,
    pub items: Vec<ExportItem>,
}

/// Reads `thread` through [`Store::transcript_items`], keeping the reasoning only when
/// `options` ask and always the notices (they become dividers).
pub fn read_thread(
    store: &Store,
    thread: &str,
    options: &ExportOptions,
    app_version: &str,
) -> Result<ExportSource, ExportError> {
    let summary = store
        .thread_summary(thread)
        .map_err(ExportError::Store)?
        .ok_or(ExportError::NotFound)?;
    let folded = store
        .transcript_items(
            thread,
            FoldOptions {
                keep_reasoning: options.include_reasoning,
                keep_notices: true,
            },
        )
        .map_err(ExportError::Store)?;
    // A thread's own agent stands in for an item no stored event named.
    let fallback = summary.driver.clone();
    let items = folded
        .into_iter()
        .map(|i| export_item(i, fallback.as_deref()))
        .collect();
    Ok(ExportSource {
        meta: ExportMeta {
            id: summary.id,
            title: summary.title,
            folder: summary.cwd,
            last_active: Some(summary.updated_at / 1000).filter(|s| *s > 0),
            app_version: app_version.to_owned(),
        },
        items,
    })
}

fn export_item(item: TranscriptItem, fallback_agent: Option<&str>) -> ExportItem {
    let TranscriptItem { message, agent, .. } = item;
    // A notice names no agent of its own worth a heading; the agent that stored it is enough.
    let agent = agent.or_else(|| {
        (message.role != NOTICE_ROLE)
            .then(|| fallback_agent.map(str::to_owned))
            .flatten()
    });
    ExportItem {
        role: message.role,
        kind: message.kind,
        text: message.text,
        status: message.status,
        title: message.title,
        agent,
    }
}

/// Renders `source` (its last step redacts the whole document) and writes it to `target` (`0600`, through a
/// temp file beside it, so a link at `target` is replaced, never written through). A source with
/// no items writes nothing.
pub fn write_export(
    source: &ExportSource,
    options: &ExportOptions,
    target: &Path,
) -> Result<(), ExportError> {
    if source.items.is_empty() {
        return Err(ExportError::Empty);
    }
    // `render` already ends with the whole-document pass, safely for each format: a second pass
    // here could break a JSON document the first one kept intact.
    let document = render(&source.meta, &source.items, options);
    write_private_atomic(target, document.as_bytes()).map_err(ExportError::Io)
}

/// [`read_thread`], then [`write_export`].
pub fn export_thread(
    store: &Store,
    thread: &str,
    options: &ExportOptions,
    app_version: &str,
    target: &Path,
) -> Result<(), ExportError> {
    let source = read_thread(store, thread, options, app_version)?;
    write_export(&source, options, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind};
    use agent_core::export::ExportFormat;
    use std::os::unix::fs::PermissionsExt;

    fn options() -> ExportOptions {
        ExportOptions::default()
    }

    fn started(item: &str, kind: ItemKind) -> Envelope {
        Envelope::new(Event::ItemStarted {
            kind,
            title: String::new(),
            input: None,
            parent: None,
        })
        .item(item)
    }

    fn completed(item: &str) -> Envelope {
        Envelope::new(Event::ItemCompleted {
            status: ItemStatus::Completed,
            output: None,
            error: None,
        })
        .item(item)
    }

    fn delta(item: &str, stream: StreamKind, text: &str) -> Envelope {
        Envelope::new(Event::ContentDelta {
            stream,
            text: text.into(),
        })
        .item(item)
    }

    /// A thread with one user message, reasoning, a reply and a notice.
    fn small_thread(store: &Store) -> String {
        let t = store.create_thread("/work", Some("Fix it")).expect("t");
        let p = store.add_provider_thread(&t, "claude", "m").expect("p");
        store.append_user_message(&t, Some(&p), "hello").expect("u");
        for e in [
            started("r1", ItemKind::Reasoning),
            delta("r1", StreamKind::Reasoning, "thinking hard"),
            completed("r1"),
            started("a1", ItemKind::AssistantMessage),
            delta("a1", StreamKind::Assistant, "hi there"),
            completed("a1"),
            Envelope::new(Event::Notice {
                text: "Compacted.".into(),
            }),
        ] {
            store.append_event(&t, Some(&p), &e).expect("append");
        }
        t
    }

    #[test]
    fn export_reads_the_whole_thread_in_order() {
        let store = Store::open_in_memory().expect("store");
        let t = store.create_thread("/work", Some("Long")).expect("t");
        let p = store.add_provider_thread(&t, "claude", "m").expect("p");
        // Two events a message: well past one 1,000-event page.
        for i in 0..1200 {
            store
                .append_user_message(&t, Some(&p), &format!("message {i:04}"))
                .expect("append");
        }
        let source = read_thread(&store, &t, &options(), "1").expect("read");
        assert_eq!(source.items.len(), 1200);
        for (i, item) in source.items.iter().enumerate() {
            assert_eq!(item.text, format!("message {i:04}"));
            assert_eq!(item.agent.as_deref(), Some("claude"));
        }
        assert_eq!(source.meta.title, "Long");
        assert_eq!(source.meta.folder, "/work");
    }

    #[test]
    fn reasoning_follows_the_option_and_the_hand_off_path_never_has_it() {
        let store = Store::open_in_memory().expect("store");
        let t = small_thread(&store);
        let off = read_thread(&store, &t, &options(), "1").expect("read");
        assert!(off.items.iter().all(|i| i.kind != "reasoning"));
        let on = ExportOptions {
            include_reasoning: true,
            ..options()
        };
        let on = read_thread(&store, &t, &on, "1").expect("read");
        assert!(on
            .items
            .iter()
            .any(|i| i.kind == "reasoning" && i.text == "thinking hard"));
        // Notices are always read: they become dividers.
        assert!(off.items.iter().any(|i| i.role == NOTICE_ROLE));
        // The hand-off's own read is unchanged.
        let handoff = store.transcript_messages(&t).expect("messages");
        assert!(handoff.iter().all(|m| m.kind != "reasoning"));
        assert!(handoff.iter().all(|m| m.role != NOTICE_ROLE));
    }

    #[test]
    fn export_is_private_and_atomic() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open_in_memory().expect("store");
        let t = small_thread(&store);
        let target = dir.path().join("out.md");
        export_thread(&store, &t, &options(), "9.9", &target).expect("export");
        let mode = std::fs::metadata(&target)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let text = std::fs::read_to_string(&target).expect("read");
        assert!(text.contains("# Fix it"), "{text}");
        assert!(text.contains("### Claude\n\nhi there"), "{text}");
        assert!(text.contains("*Compacted.*"), "{text}");
        assert!(!text.contains("thinking hard"));
        // No temp file is left beside it.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["out.md"]);

        // A failed write leaves an existing file as it was (a folder that cannot be written to;
        // root ignores the mode, so the check is skipped there).
        if crate::fsutil::effective_uid() != 0 {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
                .expect("chmod");
            let failed = export_thread(&store, &t, &options(), "1", &target);
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod back");
            assert!(matches!(failed, Err(ExportError::Io(_))));
            assert_eq!(std::fs::read_to_string(&target).expect("read"), text);
        }
    }

    #[test]
    fn a_symlink_at_the_target_is_replaced_not_followed() {
        let dir = tempfile::tempdir().expect("tmp");
        let elsewhere = dir.path().join("elsewhere.txt");
        std::fs::write(&elsewhere, "precious").expect("write");
        let target = dir.path().join("out.json");
        std::os::unix::fs::symlink(&elsewhere, &target).expect("link");
        let store = Store::open_in_memory().expect("store");
        let t = small_thread(&store);
        let json = ExportOptions {
            format: ExportFormat::Json,
            ..options()
        };
        export_thread(&store, &t, &json, "1", &target).expect("export");
        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("read"),
            "precious"
        );
        let meta = std::fs::symlink_metadata(&target).expect("meta");
        assert!(
            meta.file_type().is_file(),
            "the link was replaced by a file"
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).expect("read")).expect("json");
        assert_eq!(v["version"], 1);
    }

    #[test]
    fn a_read_that_returns_nothing_writes_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open_in_memory().expect("store");
        let blank = store.create_thread("/work", None).expect("t");
        let target = dir.path().join("out.md");
        assert!(matches!(
            export_thread(&store, &blank, &options(), "1", &target),
            Err(ExportError::Empty)
        ));
        assert!(matches!(
            export_thread(&store, "no-such-thread", &options(), "1", &target),
            Err(ExportError::NotFound)
        ));
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn a_whole_document_pass_never_leaves_json_that_does_not_parse() {
        // A header name with no value, after a quote: alone it masks nothing, but in the
        // finished document the escaped quote reads as closing a string, so the header's value is
        // the `",` that follows and masking it eats the closing quote. The renderer keeps its own
        // whole-document pass only when the result still parses; nothing may redact the file
        // again after it.
        let dir = tempfile::tempdir().expect("tmp");
        for text in ["a\" cookie:", "a\" x-api-key:"] {
            let source = ExportSource {
                meta: ExportMeta::default(),
                items: vec![
                    ExportItem {
                        role: "assistant".into(),
                        kind: "assistant_message".into(),
                        text: text.into(),
                        status: "completed".into(),
                        ..ExportItem::default()
                    },
                    ExportItem {
                        role: "user".into(),
                        kind: "user_message".into(),
                        text: "next".into(),
                        status: "completed".into(),
                        ..ExportItem::default()
                    },
                ],
            };
            let json = ExportOptions {
                format: ExportFormat::Json,
                ..options()
            };
            let target = dir.path().join("out.json");
            write_export(&source, &json, &target).expect("write");
            let written = std::fs::read_to_string(&target).expect("read");
            let v: serde_json::Value = serde_json::from_str(&written)
                .unwrap_or_else(|e| panic!("{text:?}: the file does not parse ({e}):\n{written}"));
            assert_eq!(v["items"].as_array().map(Vec::len), Some(2), "{text:?}");
        }
    }

    #[test]
    fn an_image_exports_as_its_name_and_never_its_id() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open_in_memory().expect("store");
        let t = store.create_thread("/work", Some("Pics")).expect("t");
        let p = store.add_provider_thread(&t, "claude", "m").expect("p");
        let id = "ab".repeat(32);
        let image = agent_core::attachment::AttachmentRef {
            id: id.clone(),
            kind: Default::default(),
            mime: "image/png".into(),
            width: 10,
            height: 10,
            bytes: 123,
            name: Some("shot.png".into()),
            origin: Default::default(),
        };
        store
            .append_user_input(&t, Some(&p), "look", &[image])
            .expect("message");
        for format in ExportFormat::ALL {
            let options = ExportOptions {
                format,
                ..options()
            };
            let target = dir.path().join(format!("out.{}", format.extension()));
            export_thread(&store, &t, &options, "1", &target).expect("export");
            let text = std::fs::read_to_string(&target).expect("read");
            assert!(text.contains("[image: shot.png]"), "{text}");
            assert!(!text.contains(&id), "{text}");
            assert!(!text.contains("image/png"), "{text}");
        }
    }

    #[test]
    fn the_written_file_is_redacted_even_when_the_source_was_not() {
        let dir = tempfile::tempdir().expect("tmp");
        let token = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123456789");
        let source = ExportSource {
            meta: ExportMeta::default(),
            items: vec![ExportItem {
                role: "user".into(),
                kind: "user_message".into(),
                text: format!("token {token}"),
                status: "completed".into(),
                ..ExportItem::default()
            }],
        };
        let target = dir.path().join("out.md");
        write_export(&source, &options(), &target).expect("write");
        assert!(!std::fs::read_to_string(&target)
            .expect("read")
            .contains(&token));
    }
}
