//! Attachments: images (for now) a user sends with a prompt, or an agent shows back.
//!
//! The event stream, the store and the view only ever carry an [`AttachmentRef`]: the content
//! address of a normalised blob the app keeps on disk (`agent_kit::attachments`), never its bytes.
//! The bytes travel only in a [`PromptAttachment`], from the app to an adapter, at the moment a
//! prompt is written to the agent, and in the transient [`crate::event::Event::ToolImage`] from an
//! adapter to the app, which stores them and replaces the event with refs before anything is
//! persisted or shown.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most images one message may carry.
pub const MAX_PER_MESSAGE: usize = 5;
/// The most images one agent item (a reply, a tool result, a `show_image` call) may show.
pub const MAX_PER_ITEM: usize = 10;
/// The most images a turn may show; past it the view says how many were left out.
pub const MAX_PER_TURN: usize = 50;

/// What an attachment is. Only images today; a new kind adds a variant and its mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    #[default]
    Image,
}

/// Who put the attachment in the thread: the reader can always tell what the user sent, what the
/// agent chose to show, and what a tool handed the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    User,
    /// The agent showed it (`show_image`, or an image in its reply).
    AgentShown,
    /// A tool returned it to the agent (a file it read, a screenshot).
    ToolResult,
}

/// One stored attachment, by content address. What events, the store and the view carry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttachmentRef {
    /// Lowercase hex SHA-256 of the stored (normalised) bytes.
    pub id: String,
    #[serde(default)]
    pub kind: AttachmentKind,
    /// `image/png` or `image/jpeg`.
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// The original file name, redacted; `None` for a pasted image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub origin: Origin,
}

impl AttachmentRef {
    /// Whether `id` is a content address this app wrote: 64 lowercase hex digits. Anything else
    /// (a crafted ref in an imported or damaged event) is never turned into a path.
    pub fn valid_id(id: &str) -> bool {
        id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    /// The stored file's extension for `mime`; `None` for a type the store never writes.
    pub fn extension_for(mime: &str) -> Option<&'static str> {
        match mime {
            "image/png" => Some("png"),
            "image/jpeg" => Some("jpg"),
            _ => None,
        }
    }

    /// The stored file's name (`<id>.<ext>`), or `None` when the ref is not one the store wrote.
    pub fn file_name(&self) -> Option<String> {
        let ext = Self::extension_for(&self.mime)?;
        Self::valid_id(&self.id).then(|| format!("{}.{ext}", self.id))
    }

    /// What stands in for the image where only text can go (a handoff brief, a wake notice, a
    /// copy of the message): `[image: name]` or `[image: W×H]`.
    pub fn placeholder(&self) -> String {
        match &self.name {
            Some(name) if !name.trim().is_empty() => format!("[image: {}]", name.trim()),
            _ => format!("[image: {}×{}]", self.width, self.height),
        }
    }

    /// The screen-reader name: "Image, name, W by H".
    pub fn accessible_label(&self) -> String {
        match &self.name {
            Some(name) if !name.trim().is_empty() => {
                format!("Image, {}, {} by {}", name.trim(), self.width, self.height)
            }
            _ => format!("Image, {} by {}", self.width, self.height),
        }
    }
}

/// An attachment on its way to an agent: the ref, the stored file, and its bytes as base64.
/// Codex reads the path (`localImage`); Claude and the Local agent send the bytes inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAttachment {
    pub meta: AttachmentRef,
    pub path: String,
    pub data_base64: String,
}

impl PromptAttachment {
    /// A `data:` URL of the image (the OpenAI-compatible `image_url` form).
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.meta.mime, self.data_base64)
    }
}

/// The local files a reply's markdown images name (`![alt](src)`), in order, at most
/// [`MAX_PER_ITEM`], each once: absolute or relative paths and `file://` URLs. A web address,
/// a `data:` URL or any other scheme is never one (the app never fetches an image), and an
/// image inside a code span or block is text, not an image.
pub fn markdown_image_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // Code spans: only what lies outside backticks is read.
        for (i, part) in line.split('`').enumerate() {
            if i % 2 == 1 {
                continue;
            }
            let mut rest = part;
            while let Some(at) = rest.find("![") {
                rest = &rest[at + 2..];
                let Some(close) = rest.find("](") else {
                    break;
                };
                rest = &rest[close + 2..];
                let Some(end) = rest.find(')') else {
                    break;
                };
                let inner = rest[..end].trim();
                rest = &rest[end + 1..];
                // `<path with spaces>`, or a path then an optional "title".
                let src = match inner.strip_prefix('<') {
                    Some(angled) => angled.split('>').next().unwrap_or(""),
                    None => inner.split_whitespace().next().unwrap_or(""),
                };
                let src = src.strip_prefix("file://").unwrap_or(src);
                let scheme = src.split_once(':').is_some_and(|(s, _)| {
                    !s.is_empty()
                        && s.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
                });
                if src.is_empty() || scheme || src.starts_with("//") {
                    continue;
                }
                if !out.iter().any(|p| p == src) {
                    out.push(src.to_owned());
                }
                if out.len() == MAX_PER_ITEM {
                    return out;
                }
            }
        }
    }
    out
}

/// Where a tool-result image is ([`crate::event::Event::ToolImage`]): its bytes, or a file the
/// agent already read, which the app reads wherever it is (Codex `imageView`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ToolImageSource {
    Inline { mime: String, data_base64: String },
    File { path: String },
}

/// The refs a user message's `ItemStarted.input` (or any item's) carries under `attachments`.
/// Malformed entries, and refs whose id is not a content address, are dropped.
pub fn refs_in(input: Option<&Value>) -> Vec<AttachmentRef> {
    input
        .and_then(|v| v.get("attachments"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| serde_json::from_value::<AttachmentRef>(v.clone()).ok())
                .filter(|r| AttachmentRef::valid_id(&r.id))
                .collect()
        })
        .unwrap_or_default()
}

/// The one `ItemStarted.input` shape of a user message: `{"queued": bool, "attachments": [...]}`
/// (each key only when it says something), so the live path, the queue and the store agree.
pub fn user_message_input(queued: bool, refs: &[AttachmentRef]) -> Option<Value> {
    let mut map = serde_json::Map::new();
    if queued {
        map.insert("queued".into(), Value::Bool(true));
    }
    if !refs.is_empty() {
        map.insert(
            "attachments".into(),
            serde_json::to_value(refs).unwrap_or(Value::Null),
        );
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

/// `text` with a placeholder line per image appended: what a text-only path (a handoff, a
/// notice) carries instead of the images.
pub fn text_with_placeholders(text: &str, refs: &[AttachmentRef]) -> String {
    if refs.is_empty() {
        return text.to_owned();
    }
    let marks = refs
        .iter()
        .map(AttachmentRef::placeholder)
        .collect::<Vec<_>>()
        .join(" ");
    if text.trim().is_empty() {
        marks
    } else {
        format!("{text}\n{marks}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(id: &str) -> AttachmentRef {
        AttachmentRef {
            id: id.to_owned(),
            kind: AttachmentKind::Image,
            mime: "image/png".into(),
            width: 320,
            height: 200,
            bytes: 1234,
            name: Some("shot.png".into()),
            origin: Origin::User,
        }
    }

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn only_a_content_address_names_a_file() {
        assert!(AttachmentRef::valid_id(ID));
        assert_eq!(r(ID).file_name().as_deref(), Some(&*format!("{ID}.png")));
        for bad in [
            "",
            "../etc/passwd",
            &ID.to_uppercase(),
            &ID[..63],
            &format!("{ID}0"),
        ] {
            assert!(!AttachmentRef::valid_id(bad), "{bad}");
            assert_eq!(r(bad).file_name(), None);
        }
        let mut gif = r(ID);
        gif.mime = "image/gif".into();
        assert_eq!(gif.file_name(), None);
    }

    #[test]
    fn the_user_message_input_round_trips_and_drops_bad_refs() {
        assert_eq!(user_message_input(false, &[]), None);
        assert_eq!(user_message_input(true, &[]), Some(json!({"queued": true})));
        let input = user_message_input(true, &[r(ID)]).expect("input");
        assert_eq!(input["queued"], true);
        assert_eq!(refs_in(Some(&input)), vec![r(ID)]);
        let mut bad = serde_json::to_value(r("nope")).expect("json");
        bad["width"] = json!(1);
        let mixed =
            json!({"attachments": [bad, {"id": 3}, serde_json::to_value(r(ID)).expect("json")]});
        assert_eq!(refs_in(Some(&mixed)), vec![r(ID)]);
        assert!(refs_in(None).is_empty());
        assert!(refs_in(Some(&json!({"queued": true}))).is_empty());
    }

    #[test]
    fn an_old_ref_without_kind_or_origin_reads_as_a_user_image() {
        let v = json!({"id": ID, "mime": "image/jpeg", "width": 1, "height": 2, "bytes": 3});
        let read: AttachmentRef = serde_json::from_value(v).expect("ref");
        assert_eq!(read.kind, AttachmentKind::Image);
        assert_eq!(read.origin, Origin::User);
        assert_eq!(read.name, None);
    }

    #[test]
    fn text_paths_get_placeholders() {
        let mut pasted = r(ID);
        pasted.name = None;
        assert_eq!(text_with_placeholders("hi", &[]), "hi");
        assert_eq!(
            text_with_placeholders("look", &[r(ID), pasted.clone()]),
            "look\n[image: shot.png] [image: 320×200]"
        );
        assert_eq!(
            text_with_placeholders("  ", &[pasted.clone()]),
            "[image: 320×200]"
        );
        assert_eq!(pasted.accessible_label(), "Image, 320 by 200");
        assert_eq!(r(ID).accessible_label(), "Image, shot.png, 320 by 200");
    }

    #[test]
    fn only_local_files_are_read_from_a_reply() {
        let reply = "Here is the chart:\n\n![latency](out/latency.png \"Latency\")\n\
            and ![abs](/tmp/a.png), ![file](file:///home/me/b.png), ![sp](<my shots/c.png>)\n\
            not ![web](https://example.com/x.png) nor ![d](data:image/png;base64,AAAA)\n\
            nor ![p](//cdn.example.com/y.png) nor `![code](z.png)`\n\
            ```\n![fenced](f.png)\n```\n![again](out/latency.png)";
        assert_eq!(
            markdown_image_paths(reply),
            [
                "out/latency.png",
                "/tmp/a.png",
                "/home/me/b.png",
                "my shots/c.png"
            ]
        );
        let many: String = (0..20).map(|i| format!("![x]({i}.png) ")).collect();
        assert_eq!(markdown_image_paths(&many).len(), MAX_PER_ITEM);
        assert!(markdown_image_paths("no images ![broken(").is_empty());
    }

    #[test]
    fn a_data_url_names_the_type() {
        let a = PromptAttachment {
            meta: r(ID),
            path: "/x".into(),
            data_base64: "AAAA".into(),
        };
        assert_eq!(a.data_url(), "data:image/png;base64,AAAA");
    }
}
