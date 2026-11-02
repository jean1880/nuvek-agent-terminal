//! The user's model tag file on disk (`model-tags.user.json` by default): the one file of tags
//! only the app writes, so a tool that overwrites the shared tag file cannot lose a hand-made
//! change. Reading and editing are [`agent_core::user_tags`]; this is the blocking I/O around it.
//!
//! A write is atomic and private (`0600`: temp file, `sync_all`, rename), refuses a file it could
//! not read back (another version, not an object) rather than replace it, and stamps the file
//! with [`MARKER`], which is how the uninstall knows the file is the app's when the user moved it
//! outside the app's folder.

use std::path::Path;

use agent_core::user_tags::{check_document, new_document};
use serde_json::Value;

/// The key and value the app puts in every file it writes.
pub const MARKER_KEY: &str = "generator";
pub const MARKER: &str = "agent-terminal";

/// A larger file is refused unread.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// The file as a JSON document: an empty v1 one when there is no file; `Err` says why a file
/// that is there cannot be edited.
pub fn load(path: &Path) -> Result<Value, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(new_document()),
        Err(e) => return Err(format!("{} is unreadable: {}", path.display(), e.kind())),
    };
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("{} is unreadable: {}", path.display(), e.kind()))?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    check_document(&doc).map_err(|why| format!("{}: {why}", path.display()))?;
    Ok(doc)
}

/// Writes `doc` (marked as the app's) atomically and privately.
pub fn save(path: &Path, doc: &Value) -> Result<(), String> {
    check_document(doc)?;
    let mut doc = doc.clone();
    if let Some(o) = doc.as_object_mut() {
        o.insert(MARKER_KEY.into(), Value::String(MARKER.into()));
    }
    let mut text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    text.push('\n');
    crate::fsutil::write_private_atomic(path, text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Loads the file, applies `edit`, and writes the result only when `edit` succeeds.
pub fn update<T>(
    path: &Path,
    edit: impl FnOnce(&mut Value) -> Result<T, String>,
) -> Result<T, String> {
    let mut doc = load(path)?;
    let out = edit(&mut doc)?;
    save(path, &doc)?;
    Ok(out)
}

/// Whether `path` is a regular file (not a link) the app wrote.
pub fn is_ours(path: &Path) -> bool {
    let regular = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
    regular
        && load(path).is_ok_and(|doc| doc.get(MARKER_KEY).and_then(Value::as_str) == Some(MARKER))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use agent_core::adapter::Driver;
    use agent_core::user_tags::{find, upsert, UserEntry};

    use super::*;

    fn entry() -> UserEntry {
        UserEntry {
            tier: Some("fast".into()),
            tags: vec!["code".into()],
            best_for: None,
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_document_and_a_write_is_private_marked_and_atomic() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("sub").join("model-tags.user.json");
        assert_eq!(load(&path).expect("empty"), new_document());
        assert!(!is_ours(&path));
        update(&path, |doc| upsert(doc, "m", Some(Driver::Local), &entry())).expect("write");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["model-tags.user.json"], "no temp file left");
        assert!(is_ours(&path));
        let doc = load(&path).expect("read back");
        assert_eq!(doc[MARKER_KEY], MARKER);
        assert_eq!(
            find(&doc, "m", Some(Driver::Local)).and_then(|e| e.tier),
            Some("fast".into())
        );
    }

    #[test]
    fn a_failed_edit_writes_nothing_and_an_unreadable_file_is_never_replaced() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.json");
        let refused: Result<(), String> = update(&path, |_| Err("no".into()));
        assert_eq!(refused, Err("no".into()));
        assert!(!path.exists());
        for bad in ["not json", "[]", r#"{"version": 2, "models": []}"#] {
            std::fs::write(&path, bad).expect("write");
            assert!(update(&path, |doc| upsert(doc, "m", None, &entry())).is_err());
            assert_eq!(std::fs::read_to_string(&path).expect("read"), bad);
        }
        std::fs::write(&path, "x".repeat(MAX_FILE_BYTES as usize + 1)).expect("write");
        assert!(load(&path).expect_err("big").contains("too large"));
    }

    #[test]
    fn only_a_regular_file_with_the_marker_is_ours() {
        let dir = tempfile::tempdir().expect("tmp");
        let hand = dir.path().join("hand.json");
        std::fs::write(&hand, r#"{"version": 1, "models": []}"#).expect("write");
        assert!(!is_ours(&hand), "no marker");
        let link = dir.path().join("link.json");
        let ours = dir.path().join("ours.json");
        save(&ours, &new_document()).expect("save");
        std::os::unix::fs::symlink(&ours, &link).expect("link");
        assert!(
            is_ours(&ours) && !is_ours(&link),
            "a link is never followed"
        );
        assert!(!is_ours(dir.path()));
    }
}
