//! Images in threads, on the app side: taking an image in (a file, a paste, an agent's tool
//! result) off the main thread, and turning a stored one back into a texture for the view.
//!
//! The bytes live in `agent_kit::attachments::BlobStore` (content addressed, `0600`). Everything
//! else carries an [`AttachmentRef`]. Decoding untrusted bytes is bounded there
//! (`normalise`), and only its re-encoded output is ever shown.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use agent_core::attachment::{AttachmentRef, Origin, ToolImageSource};
use agent_kit::attachments::BlobStore;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::chat::PromptImage;

/// Where the app keeps images, or `None` when no data folder can be named.
pub fn blob_store() -> Option<BlobStore> {
    BlobStore::default_location().map(BlobStore::new)
}

/// An image taken in for a message, ready to send: built on a worker (so `Send`), turned into a
/// [`PromptImage`] on the main thread.
pub struct Staged {
    pub meta: AttachmentRef,
    pub path: String,
    pub data_base64: String,
}

impl Staged {
    pub fn into_prompt_image(self) -> PromptImage {
        PromptImage {
            meta: self.meta,
            path: self.path,
            data_base64: self.data_base64.into(),
        }
    }
}

/// What the user gave the composer.
pub enum Source {
    /// A file they picked or dropped.
    File(PathBuf),
    /// A pasted or dropped image, encoded on the worker (never on the main thread).
    Texture(gdk::Texture),
}

/// Takes `source` in on a worker: normalised (bounded decode, resized, metadata dropped), stored,
/// and encoded for sending. The error is a sentence the user can read.
pub async fn stage(source: Source) -> Result<Staged, String> {
    gio::spawn_blocking(move || stage_blocking(source))
        .await
        .unwrap_or_else(|_| Err("Couldn't read the image.".to_owned()))
}

fn stage_blocking(source: Source) -> Result<Staged, String> {
    let store =
        blob_store().ok_or_else(|| "There is no data folder to keep images in.".to_owned())?;
    let normalised = match source {
        Source::File(path) => store.ingest_path(&path, Origin::User),
        Source::Texture(texture) => {
            store.ingest_bytes(&texture.save_to_png_bytes(), None, Origin::User)
        }
    }
    .map_err(|e| e.to_string())?;
    let path = store
        .path_of(&normalised.meta)
        .ok_or_else(|| "The image could not be stored.".to_owned())?;
    Ok(Staged {
        data_base64: agent_kit::attachments::to_base64(&normalised.bytes),
        meta: normalised.meta,
        path: path.to_string_lossy().into_owned(),
    })
}

/// Stores an image an agent's tool handed back ([`agent_core::event::Event::ToolImage`]) and
/// returns its ref. Blocking: call it on a worker. A file (Codex `imageView`) is a path an agent
/// named, so it is read only as the read policy lets a read run without asking in the thread's
/// folder `cwd` ([`agent_kit::local_tools::read_unasked`]: inside it, not sensitive, links judged
/// where they lead, a regular file under the size cap); its bytes, like any, are decoded under
/// the same bounds as the user's.
pub fn ingest_tool_image(
    image: ToolImageSource,
    origin: Origin,
    cwd: &str,
) -> Result<AttachmentRef, String> {
    let store = blob_store().ok_or_else(|| "no data folder for images".to_owned())?;
    let normalised = match image {
        ToolImageSource::Inline { data_base64, .. } => {
            let bytes = agent_kit::attachments::from_base64(&data_base64)
                .map_err(|e| format!("the image was not valid base64: {e}"))?;
            store.ingest_bytes(&bytes, None, origin)
        }
        ToolImageSource::File { path } => {
            use agent_kit::local_tools::{read_unasked, UnaskedRead, Workspace};
            if cwd.is_empty() {
                return Err("the thread has no folder to read it in".to_owned());
            }
            let ws = Workspace::open(std::path::Path::new(cwd))?;
            let (target, bytes) = read_unasked(&ws, &path, agent_kit::attachments::MAX_INPUT_BYTES)
                .map_err(|e| match e {
                    UnaskedRead::MustAsk { .. } => {
                        format!("{path} is not one the app reads without asking")
                    }
                    UnaskedRead::Failed(why) => why,
                })?;
            let name = target.file_name().map(|n| n.to_string_lossy().into_owned());
            store.ingest_bytes(&bytes, name.as_deref(), origin)
        }
    }
    .map_err(|e| e.to_string())?;
    Ok(normalised.meta)
}

/// Stored images kept decoded, newest last, within [`CACHE_BUDGET`] bytes of pixels.
#[derive(Default)]
struct TextureCache {
    textures: HashMap<String, gdk::Texture>,
    order: VecDeque<String>,
    pixels: usize,
}

/// What decoded images may hold in memory at once (RGBA): a long thread's images are decoded
/// again when scrolled back to rather than all kept.
const CACHE_BUDGET: usize = 64 * 1024 * 1024;

fn pixel_bytes(texture: &gdk::Texture) -> usize {
    let w = usize::try_from(texture.width()).unwrap_or(0);
    let h = usize::try_from(texture.height()).unwrap_or(0);
    w.saturating_mul(h).saturating_mul(4)
}

impl TextureCache {
    fn get(&mut self, id: &str) -> Option<gdk::Texture> {
        let texture = self.textures.get(id)?.clone();
        self.order.retain(|k| k != id);
        self.order.push_back(id.to_owned());
        Some(texture)
    }

    fn put(&mut self, id: &str, texture: gdk::Texture) {
        if self.textures.contains_key(id) {
            return;
        }
        self.pixels = self.pixels.saturating_add(pixel_bytes(&texture));
        self.textures.insert(id.to_owned(), texture);
        self.order.push_back(id.to_owned());
        while self.pixels > CACHE_BUDGET && self.order.len() > 1 {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(t) = self.textures.remove(&old) {
                self.pixels = self.pixels.saturating_sub(pixel_bytes(&t));
            }
        }
    }
}

thread_local! {
    static CACHE: RefCell<TextureCache> = RefCell::default();
}

/// The stored image of `r` as a texture: from the cache, or read (checked against its content
/// address) and decoded on a worker. The error says why it is unavailable.
pub async fn texture(r: &AttachmentRef) -> Result<gdk::Texture, String> {
    if let Some(t) = CACHE.with(|c| c.borrow_mut().get(&r.id)) {
        return Ok(t);
    }
    let wanted = r.clone();
    let texture = gio::spawn_blocking(move || -> Result<gdk::Texture, String> {
        let store = blob_store().ok_or_else(|| "No data folder".to_owned())?;
        let bytes = store.read(&wanted).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "The image file was removed.".to_owned(),
            _ => format!("The image couldn't be read: {e}"),
        })?;
        gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
            .map_err(|e| format!("The image couldn't be decoded: {e}"))
    })
    .await
    .unwrap_or_else(|_| Err("The image couldn't be read.".to_owned()))?;
    CACHE.with(|c| c.borrow_mut().put(&r.id, texture.clone()));
    Ok(texture)
}

/// A private copy of `r` for another app to open (`$XDG_RUNTIME_DIR/agent-terminal/open/`), so an
/// external editor can never change or delete the stored one. Blocking: call it on a worker.
pub fn private_copy(r: &AttachmentRef) -> Result<PathBuf, String> {
    let store = blob_store().ok_or_else(|| "No data folder".to_owned())?;
    let bytes = store.read(r).map_err(|e| e.to_string())?;
    let dir = glib::user_runtime_dir().join("agent-terminal").join("open");
    agent_kit::attachments::write_private_copy(&dir, r, &bytes).map_err(|e| e.to_string())
}

/// Removes the private copies [`private_copy`] made (at quit).
pub fn remove_private_copies() {
    let dir = glib::user_runtime_dir().join("agent-terminal").join("open");
    agent_kit::attachments::remove_private_copies(&dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file an agent's tool names (Codex `imageView`) is read only as a read may run without
    /// asking in the thread's folder: one inside is stored as a tool's image; one outside, or a
    /// link inside leading out, is not, and says why; inline bytes need no folder.
    #[test]
    fn a_viewed_file_is_read_only_inside_the_folder() {
        use std::os::unix::fs::symlink;
        let png = agent_kit::attachments::from_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
        )
        .expect("png");
        let root = tempfile::tempdir().expect("dir");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).expect("work");
        std::fs::write(work.join("chart.png"), &png).expect("chart");
        std::fs::write(root.path().join("away.png"), &png).expect("away");
        symlink(root.path().join("away.png"), work.join("link.png")).expect("link");
        let cwd = work.to_string_lossy().into_owned();
        let file = |path: &str| ToolImageSource::File {
            path: path.to_owned(),
        };
        let inside = ingest_tool_image(
            file(&work.join("chart.png").to_string_lossy()),
            Origin::ToolResult,
            &cwd,
        )
        .expect("inside");
        assert_eq!(inside.origin, Origin::ToolResult);
        assert_eq!(inside.name.as_deref(), Some("chart.png"));
        for outside in [
            root.path().join("away.png").to_string_lossy().into_owned(),
            "link.png".to_owned(),
        ] {
            let refused = ingest_tool_image(file(&outside), Origin::ToolResult, &cwd);
            assert!(refused.is_err(), "{outside}: {refused:?}");
        }
        assert!(ingest_tool_image(file("chart.png"), Origin::ToolResult, "").is_err());
        let inline = ToolImageSource::Inline {
            mime: "image/png".into(),
            data_base64: agent_kit::attachments::to_base64(&png),
        };
        assert!(ingest_tool_image(inline, Origin::ToolResult, "").is_ok());
    }
}
