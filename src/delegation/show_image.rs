//! `show_image`: an agent shows the user image files in its own thread.
//!
//! A path is read as the Local agent reads without asking (`agent_kit::local_tools::
//! read_unasked`): resolved once against the caller's working folder, read only when it leads
//! inside it to a file that is neither sensitive nor hard-linked, opened from the pinned folder
//! without following a link, never a pipe or a device. Nothing here can ask the user, so a path
//! a read would ask about is refused with that reason. The bytes are normalised and stored off
//! the main thread, and the images go on the call's own tool item as `Event::Attachments`
//! (`Origin::AgentShown`). The model is told only a line per image, never its bytes.

use std::path::Path;
use std::rc::Rc;

use agent_core::attachment::{AttachmentRef, Origin};
use agent_core::delegation::correlation::{item_id, InvocationKey};
use agent_core::delegation::wire::{ImageSpec, ShowImageRequest};
use agent_core::delegation::{ErrorCode, ToolError, WorkerId};
use agent_kit::attachments::{BlobStore, MAX_INPUT_BYTES};
use agent_kit::local_tools::{read_unasked, UnaskedRead, Workspace};
use gtk4::gio;
use serde_json::{json, Value};

use super::seam::ConnId;
use super::supervisor::{error, Supervisor};

/// What became of one image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Stored; `file` is the file's name as the result line says it.
    Stored { meta: AttachmentRef, file: String },
    /// Not shown: a sentence saying why.
    Failed(String),
}

/// Reads and stores each image `images` names, relative to `cwd`. Blocking: run it on a worker.
pub(super) fn read_images(
    cwd: &Path,
    images: &[ImageSpec],
    store: Option<&BlobStore>,
) -> Vec<Outcome> {
    let ws = match Workspace::open(cwd) {
        Ok(ws) => ws,
        Err(why) => {
            return images
                .iter()
                .map(|_| Outcome::Failed(why.clone()))
                .collect()
        }
    };
    images
        .iter()
        .map(|spec| read_one(&ws, spec, store))
        .collect()
}

fn read_one(ws: &Workspace, spec: &ImageSpec, store: Option<&BlobStore>) -> Outcome {
    let path = agent_core::redact::redact(&spec.path);
    let (target, bytes) = match read_unasked(ws, &spec.path, MAX_INPUT_BYTES) {
        Ok(read) => read,
        Err(UnaskedRead::MustAsk { outside: true, .. }) => {
            return Outcome::Failed(format!(
                "Only images inside the working folder can be shown; this one is outside it: \
                 {path}"
            ))
        }
        Err(UnaskedRead::MustAsk { outside: false, .. }) => {
            return Outcome::Failed(format!(
                "Only images that can be read without asking can be shown; this one is a \
                 sensitive file or has another hard link: {path}"
            ))
        }
        Err(UnaskedRead::Failed(why)) => return Outcome::Failed(agent_core::redact::redact(&why)),
    };
    let Some(store) = store else {
        return Outcome::Failed("There is no data folder to keep images in.".to_owned());
    };
    let name = target.file_name().and_then(|n| n.to_str());
    match store.ingest_bytes(&bytes, name, Origin::AgentShown) {
        Ok(normalised) => {
            let mut meta = normalised.meta;
            let file = meta.name.clone().unwrap_or_else(|| path.clone());
            if let Some(caption) = spec.caption.as_deref().filter(|c| !c.trim().is_empty()) {
                meta.name = Some(agent_core::redact::redact(caption));
            }
            Outcome::Stored { meta, file }
        }
        Err(e) => Outcome::Failed(format!("{path}: {e}")),
    }
}

/// The line the model is told for one image.
pub(super) fn result_line(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Stored { meta, file } => {
            format!("Shown to the user: {file} ({}×{})", meta.width, meta.height)
        }
        Outcome::Failed(why) => format!("Not shown: {why}"),
    }
}

/// The tool's output: how many were shown, and a line per image, in order.
pub(super) fn output(outcomes: &[Outcome]) -> Value {
    let shown = outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Stored { .. }))
        .count();
    json!({
        "api_version": 1,
        "shown": shown,
        "results": outcomes.iter().map(result_line).collect::<Vec<_>>(),
    })
}

/// Marks every stored image past the first `kept` failed with `why`.
pub(super) fn keep_first(outcomes: &mut [Outcome], kept: usize, why: &str) {
    for o in outcomes
        .iter_mut()
        .filter(|o| matches!(o, Outcome::Stored { .. }))
        .skip(kept)
    {
        *o = Outcome::Failed(why.to_owned());
    }
}

impl Supervisor {
    /// `show_image` for `worker`: read in its working folder off the main thread, then shown on
    /// the call's own item in its thread. Its authority is checked again after the read.
    pub(super) async fn show_images(
        self: &Rc<Self>,
        conn: ConnId,
        worker: &WorkerId,
        invocation: Option<&InvocationKey>,
        req: ShowImageRequest,
    ) -> Result<Value, ToolError> {
        let cwd = self
            .state
            .borrow()
            .workers
            .get(worker)
            .map(|w| w.cwd.clone())
            .ok_or_else(|| error(ErrorCode::NotFound, "caller not found"))?;
        let images = req.images;
        let mut outcomes = gio::spawn_blocking(move || {
            read_images(&cwd, &images, crate::attachments::blob_store().as_ref())
        })
        .await
        .map_err(|_| error(ErrorCode::InternalError, "the images could not be read"))?;
        self.still_authorized(conn, worker)?;
        let refs: Vec<AttachmentRef> = outcomes
            .iter()
            .filter_map(|o| match o {
                Outcome::Stored { meta, .. } => Some(meta.clone()),
                Outcome::Failed(_) => None,
            })
            .collect();
        if !refs.is_empty() {
            let session = self
                .state
                .borrow()
                .workers
                .get(worker)
                .and_then(|w| w.session());
            match session {
                Some(session) => match session.show_agent_images(invocation.map(item_id), refs) {
                    Ok(n) => keep_first(
                        &mut outcomes,
                        n,
                        "this turn has already shown as many images as it may",
                    ),
                    Err(why) => keep_first(&mut outcomes, 0, &why),
                },
                None => keep_first(&mut outcomes, 0, "there is no conversation to show it in"),
            }
        }
        Ok(output(&outcomes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// A 1×1 PNG.
    fn png() -> Vec<u8> {
        agent_kit::attachments::from_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=",
        )
        .expect("png")
    }

    fn spec(path: &str, caption: Option<&str>) -> ImageSpec {
        ImageSpec {
            path: path.to_owned(),
            caption: caption.map(str::to_owned),
        }
    }

    /// Inside the folder is read and stored as the agent's; outside, a link out, a pipe and a
    /// file that is no image are refused with a sentence; each image has its own line.
    #[test]
    fn images_inside_the_folder_are_shown_and_the_rest_refused_with_a_reason() {
        let work = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let blobs = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(blobs.path());
        std::fs::write(work.path().join("chart.png"), png()).unwrap();
        std::fs::write(work.path().join("notes.txt"), b"not an image").unwrap();
        std::fs::write(elsewhere.path().join("out.png"), png()).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("out.png"),
            work.path().join("link.png"),
        )
        .unwrap();
        let fifo = work.path().join("pipe.png");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid path string; makes a FIFO in the test's own folder.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let outside = elsewhere.path().join("out.png");
        let images = [
            spec("chart.png", None),
            spec("chart.png", Some("Quarterly totals")),
            spec(outside.to_str().unwrap(), None),
            spec("link.png", None),
            spec("pipe.png", None),
            spec("notes.txt", None),
        ];
        let outcomes = read_images(work.path(), &images, Some(&store));
        let lines: Vec<String> = outcomes.iter().map(result_line).collect();

        assert_eq!(lines[0], "Shown to the user: chart.png (1×1)");
        match &outcomes[0] {
            Outcome::Stored { meta, .. } => {
                assert_eq!(meta.origin, Origin::AgentShown);
                assert_eq!(meta.name.as_deref(), Some("chart.png"));
                // Stored, and read back by its content address.
                assert!(store.read(meta).is_ok());
            }
            other => panic!("{other:?}"),
        }
        // A caption names the image; the model is still told the file's name.
        assert_eq!(lines[1], "Shown to the user: chart.png (1×1)");
        match &outcomes[1] {
            Outcome::Stored { meta, .. } => {
                assert_eq!(meta.name.as_deref(), Some("Quarterly totals"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            lines[2],
            format!(
                "Not shown: Only images inside the working folder can be shown; this one is \
                 outside it: {}",
                outside.display()
            )
        );
        // A link inside the folder is judged where it leads: outside.
        assert!(lines[3].contains("outside it: link.png"), "{}", lines[3]);
        assert!(lines[4].contains("not a regular file"), "{}", lines[4]);
        assert!(
            lines[5].starts_with("Not shown: notes.txt: Not an image"),
            "{}",
            lines[5]
        );
        // Only short text reaches the model.
        let out = output(&outcomes);
        assert_eq!(out["shown"], 2);
        assert_eq!(out["results"].as_array().map(Vec::len), Some(6));
        assert!(out.to_string().len() < 2048);
        // Without a data folder nothing is stored.
        let none = read_images(work.path(), &images[..1], None);
        assert!(matches!(&none[0], Outcome::Failed(why) if why.contains("no data folder")));
    }

    #[test]
    fn past_the_turn_cap_the_rest_are_said_not_shown() {
        let meta = |n: u8| AttachmentRef {
            id: format!("{n:064x}"),
            kind: Default::default(),
            mime: "image/png".into(),
            width: 1,
            height: 1,
            bytes: 1,
            name: None,
            origin: Origin::AgentShown,
        };
        let mut outcomes = vec![
            Outcome::Stored {
                meta: meta(1),
                file: "a.png".into(),
            },
            Outcome::Failed("x".into()),
            Outcome::Stored {
                meta: meta(2),
                file: "b.png".into(),
            },
        ];
        keep_first(&mut outcomes, 1, "full");
        assert!(matches!(&outcomes[0], Outcome::Stored { .. }));
        assert_eq!(outcomes[1], Outcome::Failed("x".into()));
        assert_eq!(outcomes[2], Outcome::Failed("full".into()));
    }
}
