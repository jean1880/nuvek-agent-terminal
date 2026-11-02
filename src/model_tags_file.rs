//! The model tag files, read off the main thread and cached until a file's modification time or
//! size changes. Two files are read: the shared tag file (`TerminalConfig::model_tags_path`, by
//! default `~/.config/agent-terminal/model-tags.json`, authored elsewhere) and the user's own
//! (`TerminalConfig::user_model_tags_path`, by default `model-tags.user.json` beside it, written
//! only by the app). Parsing is [`agent_core::model_tags::parse`]. The layers stack user over
//! shared file over the built-in tags ([`agent_core::model_tags::built_in`]), so every install has
//! tags: a missing file is not worth a word, and a refused one is never an error to the caller,
//! only a reason (`status`) the layers below it apply alone.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::delegation::error::truncate_bytes;
use agent_core::delegation::wire::MAX_TAGS_STATUS_BYTES;
use agent_core::model_tags::{built_in, parse, ModelTags, TagSource};
use agent_kit::fsutil::{self, FileStamp as Stamp, ReadError};
use agent_kit::user_tags_file::{read_problem, MAX_FILE_BYTES};
use gtk4::gio;

/// What the capabilities call gets: the tags, or why there are none.
#[derive(Debug, Clone, Default)]
pub struct TagsState {
    pub tags: Option<Arc<ModelTags>>,
    /// Why a tag file is not in `tags` (the layers below it apply alone); `None` when each one
    /// is, or there is none.
    pub status: Option<String>,
}

/// One tag file as read: its tags (none when it is missing or refused) and why it was refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub tags: Option<ModelTags>,
    pub status: Option<String>,
}

impl Layer {
    fn refused(what: &str, why: &str) -> Layer {
        Layer {
            tags: None,
            status: Some(format!("{what} {why}")),
        }
    }
}

/// One look at a file: `Unchanged` when its stamp is `known`.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // a short-lived value, matched at once
pub enum Reading {
    Unchanged,
    Loaded(Option<Stamp>, Layer),
}

/// Blocking: stats `path` and, unless its stamp is `known`, reads and parses it. `what` names the
/// file in a refusal ("model tag file", "user tag file").
pub fn read(path: &Path, known: Option<Stamp>, what: &str) -> Reading {
    let (file, stamp) = match fsutil::open_regular(path) {
        Ok(opened) => opened,
        Err(ReadError::NotFound) => return Reading::Loaded(None, Layer::default()),
        Err(e) => return Reading::Loaded(None, Layer::refused(what, &read_problem(e))),
    };
    if known == Some(stamp) {
        return Reading::Unchanged;
    }
    let layer = match fsutil::read_limited(file, MAX_FILE_BYTES) {
        Ok(text) => match parse(&text) {
            Ok(tags) => Layer {
                tags: Some(tags),
                status: None,
            },
            Err(why) => Layer::refused(what, &format!("refused: {why}")),
        },
        Err(e) => Layer::refused(what, &read_problem(e)),
    };
    Reading::Loaded(Some(stamp), layer)
}

/// The shared file's tags over the built-in ones, and the user's over both. `status` joins the
/// reasons a file was refused.
pub fn combine(file: &Layer, user: &Layer) -> TagsState {
    let base = built_in();
    let shared = match file.tags.clone() {
        Some(tags) => tags.tagged(TagSource::File).over(base),
        None => base,
    };
    let all = match user.tags.clone() {
        Some(tags) => tags.tagged(TagSource::User).over(shared),
        None => shared,
    };
    let reasons: Vec<&str> = [&file.status, &user.status]
        .into_iter()
        .filter_map(|s| s.as_deref())
        .collect();
    TagsState {
        tags: Some(Arc::new(all)),
        status: (!reasons.is_empty())
            .then(|| truncate_bytes(&reasons.join("; "), MAX_TAGS_STATUS_BYTES).to_owned()),
    }
}

/// Blocking: both files read now, combined (the headless commands and tests).
pub fn load_now(file: &Path, user: &Path) -> TagsState {
    let layer = |path: &Path, what: &str| match read(path, None, what) {
        Reading::Loaded(_, layer) => layer,
        Reading::Unchanged => Layer::default(),
    };
    combine(&layer(file, FILE_NAME), &layer(user, USER_FILE_NAME))
}

const FILE_NAME: &str = "model tag file";
const USER_FILE_NAME: &str = "user tag file";

/// A path's last reading.
#[derive(Debug, Clone)]
struct Cached {
    paths: (PathBuf, PathBuf),
    file: (Option<Stamp>, Layer),
    user: (Option<Stamp>, Layer),
    state: TagsState,
}

/// The last reading of the two files asked for.
#[derive(Default)]
pub struct TagsCache {
    last: RefCell<Option<Cached>>,
}

impl TagsCache {
    /// The combined tags: each file re-read off the main thread only when it changed since the
    /// last call.
    pub async fn current(&self, file: PathBuf, user: PathBuf) -> TagsState {
        let last = self.last.borrow().clone();
        let same = last
            .as_ref()
            .filter(|c| c.paths.0 == file && c.paths.1 == user);
        let known = (same.and_then(|c| c.file.0), same.and_then(|c| c.user.0));
        let job = (file.clone(), user.clone());
        let readings = gio::spawn_blocking(move || {
            (
                read(&job.0, known.0, FILE_NAME),
                read(&job.1, known.1, USER_FILE_NAME),
            )
        })
        .await;
        let Ok((file_reading, user_reading)) = readings else {
            return combine(
                &Layer::refused(FILE_NAME, "could not be read"),
                &Layer::default(),
            );
        };
        let cached_file = same.map(|c| c.file.1.clone());
        let cached_user = same.map(|c| c.user.1.clone());
        let resolve = |reading: Reading, cached: Option<Layer>, old: Option<Stamp>| match reading {
            Reading::Loaded(stamp, layer) => (stamp, layer),
            Reading::Unchanged => (old, cached.unwrap_or_default()),
        };
        let file_now = resolve(file_reading, cached_file, known.0);
        let user_now = resolve(user_reading, cached_user, known.1);
        // Nothing moved (a file that is not there never has a stamp to match): the same parse.
        if let Some(c) = same {
            if c.file == file_now && c.user == user_now {
                return c.state.clone();
            }
        }
        let state = combine(&file_now.1, &user_now.1);
        *self.last.borrow_mut() = Some(Cached {
            paths: (file, user),
            file: file_now,
            user: user_now,
            state: state.clone(),
        });
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = r#"{"version": 1, "vocabulary": {"code": "writes code"},
        "models": [{"match": "*opus*", "tags": ["code"], "tier": "deep"}]}"#;

    fn layer(path: &Path) -> Layer {
        match read(path, None, FILE_NAME) {
            Reading::Loaded(_, l) => l,
            Reading::Unchanged => panic!("no stamp known"),
        }
    }

    #[test]
    fn a_missing_or_refused_file_leaves_the_built_in_tags_and_a_good_one_goes_over_them() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("model-tags.json");
        let Reading::Loaded(None, missing) = read(&path, None, FILE_NAME) else {
            panic!("missing file");
        };
        let state = combine(&missing, &Layer::default());
        assert_eq!(state.tags.as_deref(), Some(&built_in()));
        assert_eq!(state.status, None, "no file is not worth a word");

        std::fs::write(&path, r#"{"version": 9, "models": []}"#).expect("write");
        let bad = combine(&layer(&path), &Layer::default());
        assert_eq!(bad.tags.as_deref(), Some(&built_in()));
        assert!(bad
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("model tag file refused: unsupported version 9")));

        std::fs::write(&path, V1).expect("write");
        let Reading::Loaded(Some(stamp), good) = read(&path, None, FILE_NAME) else {
            panic!("good file");
        };
        let good = combine(&good, &Layer::default());
        assert!(good.status.is_none());
        let tags = good.tags.expect("tags");
        assert_eq!(tags.vocabulary["code"], "writes code");
        let opus = tags
            .lookup(agent_core::adapter::Driver::Claude, "opus")
            .expect("opus");
        assert_eq!(opus.tier.as_deref(), Some("deep"), "the file's entry first");
        assert_eq!(opus.source, TagSource::File);
        let luna = tags
            .lookup(agent_core::adapter::Driver::Codex, "gpt-6-luna")
            .expect("built in");
        assert_eq!(
            luna.source,
            TagSource::BuiltIn,
            "the rest from the built-in ones"
        );
        assert!(matches!(
            read(&path, Some(stamp), FILE_NAME),
            Reading::Unchanged
        ));

        std::fs::write(&path, "x".repeat(MAX_FILE_BYTES as usize + 1)).expect("write");
        let Reading::Loaded(_, big) = read(&path, Some(stamp), FILE_NAME) else {
            panic!("big file");
        };
        assert_eq!(big.status.as_deref(), Some("model tag file is too large"));
    }

    /// User over file over built-in: the user's entry for a model beats the shared file's, a
    /// refused user file leaves the shared one in force and says so, and a Local entry never
    /// catches a vendor model.
    #[test]
    fn the_users_file_goes_over_the_shared_one_over_the_built_in_tags() {
        use agent_core::adapter::Driver;
        let dir = tempfile::tempdir().expect("tmp");
        let (file, user) = (
            dir.path().join("model-tags.json"),
            dir.path().join("model-tags.user.json"),
        );
        std::fs::write(&file, V1).expect("write");
        std::fs::write(
            &user,
            r#"{"version": 1, "vocabulary": {"mine": "my tag"}, "models": [
                {"match": "opus", "driver": "claude", "tier": "fast", "tags": ["mine"]},
                {"match": "tiny-local", "driver": "local", "tier": "fast", "tags": ["fast"]}]}"#,
        )
        .expect("write");
        let state = load_now(&file, &user);
        assert_eq!(state.status, None);
        let tags = state.tags.expect("tags");
        let src = |d, id: &str| tags.lookup(d, id).map(|e| (e.source, e.tier.clone()));
        assert_eq!(
            src(Driver::Claude, "opus"),
            Some((TagSource::User, Some("fast".into())))
        );
        assert_eq!(
            src(Driver::Claude, "claude-opus-5-5"),
            Some((TagSource::File, Some("deep".into()))),
            "the user's entry names one model"
        );
        assert_eq!(
            src(Driver::Local, "tiny-local").map(|s| s.0),
            Some(TagSource::User)
        );
        assert_eq!(src(Driver::Claude, "tiny-local"), None);
        assert_eq!(
            src(Driver::Claude, "haiku").map(|s| s.0),
            Some(TagSource::BuiltIn)
        );
        assert_eq!(
            tags.vocabulary["mine"], "my tag",
            "the user file extends the vocabulary"
        );

        std::fs::write(&user, "not json").expect("write");
        let refused = load_now(&file, &user);
        let tags = refused.tags.expect("tags");
        assert_eq!(
            tags.lookup(Driver::Claude, "opus").map(|e| e.source),
            Some(TagSource::File)
        );
        assert!(refused
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("user tag file refused: not valid JSON")));

        // Both refused: both reasons, within the wire bound.
        std::fs::write(&file, "also not json").expect("write");
        let both = load_now(&file, &user).status.expect("status");
        assert!(both.contains("model tag file") && both.contains("; user tag file"));
        assert!(both.len() <= MAX_TAGS_STATUS_BYTES);
    }

    #[test]
    fn the_cache_rereads_only_a_changed_file() {
        use crate::testutil::in_loop;
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("model-tags.json");
        let user = dir.path().join("model-tags.user.json");
        std::fs::write(&path, V1).expect("write");
        in_loop(|ctx| {
            let cache = TagsCache::default();
            let first = ctx.block_on(cache.current(path.clone(), user.clone()));
            let second = ctx.block_on(cache.current(path.clone(), user.clone()));
            let (a, b) = (first.tags.expect("a"), second.tags.expect("b"));
            assert!(Arc::ptr_eq(&a, &b), "unchanged: the cached parse");
            // Another size is another version, whatever the clock's granularity.
            std::fs::write(&path, V1.replace("writes code", "writes more code")).expect("write");
            let third = ctx.block_on(cache.current(path.clone(), user.clone()));
            assert_eq!(
                third.tags.expect("c").vocabulary["code"],
                "writes more code"
            );
            // The user's file appearing is seen, and the shared file is not parsed again.
            std::fs::write(
                &user,
                r#"{"version": 1, "models": [{"match": "*opus*", "tier": "fast"}]}"#,
            )
            .expect("write");
            let fourth = ctx.block_on(cache.current(path.clone(), user.clone()));
            let tags = fourth.tags.expect("d");
            assert_eq!(
                tags.lookup(agent_core::adapter::Driver::Claude, "opus")
                    .map(|e| e.source),
                Some(TagSource::User)
            );
            assert_eq!(tags.vocabulary["code"], "writes more code");
            std::fs::remove_file(&user).expect("rm");
            std::fs::remove_file(&path).expect("rm");
            let gone = ctx.block_on(cache.current(path.clone(), user.clone()));
            assert_eq!(gone.tags.as_deref(), Some(&built_in()));
            assert_eq!(gone.status, None);
            // Another path is not the cached one's reading.
            std::fs::write(&path, V1).expect("write");
            let moved = ctx.block_on(cache.current(path.clone(), dir.path().join("other.json")));
            assert!(moved.tags.is_some());
        });
    }

    #[test]
    fn a_long_reason_is_cut_to_the_wire_bound_on_a_character_boundary() {
        let long = Layer::refused(FILE_NAME, &"é".repeat(1000));
        let status = combine(&long, &Layer::default()).status.expect("status");
        assert!(status.len() <= MAX_TAGS_STATUS_BYTES && status.starts_with("model"));
    }
}
