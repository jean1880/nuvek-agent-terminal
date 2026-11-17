//! A review's comments checked against the working files when it is sent, so the message never
//! quotes lines that moved or went without saying so. Blocking: the app runs it off the main
//! thread.
//!
//! Each file is read once, and only when it is inside the thread's folder (every link followed,
//! so one that leads out is outside), a regular file of at most [`MAX_BYTES`] and text. Nothing
//! read reaches the message: only where the comment's own (already quoted) lines are now. A file that is not there has lost the comment's lines; one
//! that cannot be read is said to be unchecked, never "still there".

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use agent_core::review::{locate, Checked, Unchecked};

/// The largest working file read for the check.
pub const MAX_BYTES: u64 = crate::filediff::MAX_FILE_BYTES;

/// One new-side comment to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckAsk {
    /// Relative to the repository root, as the diff named it.
    pub path: String,
    /// 1-based, inclusive, on the new side.
    pub start: u32,
    pub end: u32,
    /// The diff rows chosen, prefixes kept, as the draft keeps them (redacted).
    pub quoted: Vec<String>,
}

/// The canonical root of the repository `dir` is in, or `None`. Runs git.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    crate::git::discover(dir)
        .ok()
        .flatten()
        .and_then(|repo| repo.toplevel.canonicalize().ok())
}

/// `rel` as plain components only: no root, no `..`, no `.` (which a repository path never has).
fn plain(rel: &str) -> Option<&Path> {
    let path = Path::new(rel);
    (!rel.is_empty()
        && !rel.contains('\0')
        && path.components().all(|c| matches!(c, Component::Normal(_))))
    .then_some(path)
}

/// A card's file, named by its header from `root` (the repository its diff was taken in), as a
/// path from `top` (the repository the thread's review names files from); `None` when it is not
/// in that repository.
pub fn repo_path(top: &Path, root: &Path, rel: &str) -> Option<String> {
    let abs = root.join(plain(rel)?);
    let rel = abs.strip_prefix(top).ok()?;
    let text = rel.to_str()?;
    (!text.is_empty()).then(|| text.to_owned())
}

/// The nearest ancestor of `path` that exists, canonical.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|a| a.canonicalize().ok())
}

/// The working text of `rel` (from `top`), when the thread may read it: `Ok(None)` when it is
/// not there.
fn read_text(folder: &Path, top: &Path, rel: &str) -> Result<Option<String>, Unchecked> {
    let rel = plain(rel).ok_or(Unchecked::Outside)?;
    let folder = folder.canonicalize().map_err(|_| Unchecked::Unreadable)?;
    let abs = top.join(rel);
    let target = match crate::editor::resolve_target(&folder, &abs.to_string_lossy()) {
        Ok(target) => target,
        Err(_) if std::fs::symlink_metadata(&abs).is_err() => {
            // Not there: gone, if where it would be is the thread's.
            return match nearest_existing(&abs) {
                Some(at) if at.starts_with(&folder) => Ok(None),
                _ => Err(Unchecked::Outside),
            };
        }
        // There but not a regular file, or a link to nothing.
        Err(_) => {
            return match abs.parent().and_then(|p| p.canonicalize().ok()) {
                Some(parent) if parent.starts_with(&folder) => Err(Unchecked::Unreadable),
                _ => Err(Unchecked::Outside),
            };
        }
    };
    if !target.inside {
        return Err(Unchecked::Outside);
    }
    // Opened without following a link (the canonical path has none) and without blocking (a
    // pipe swapped in is never waited on), then checked again as opened.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&target.abs)
        .map_err(|_| Unchecked::Unreadable)?;
    let meta = file.metadata().map_err(|_| Unchecked::Unreadable)?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(Unchecked::Unreadable);
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Unchecked::Unreadable)?;
    if bytes.len() as u64 > MAX_BYTES || crate::filediff::is_binary(&bytes) {
        return Err(Unchecked::Unreadable);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| Unchecked::Unreadable)
}

/// Where each of `asks` is in its working file, in order. `folder` is the thread's folder,
/// `top` the root of the repository the paths are named from (`None`: there is none, and
/// nothing is checked).
pub fn check(folder: &Path, top: Option<&Path>, asks: &[CheckAsk]) -> Vec<Checked> {
    let Some(top) = top else {
        return vec![Checked::Unchecked(Unchecked::NoRepository); asks.len()];
    };
    let mut read: std::collections::HashMap<&str, Result<Option<String>, Unchecked>> =
        std::collections::HashMap::new();
    asks.iter()
        .map(|ask| {
            let text = read
                .entry(ask.path.as_str())
                .or_insert_with(|| read_text(folder, top, &ask.path));
            match text {
                Ok(Some(text)) => locate(text, ask.start, ask.end, &ask.quoted),
                Ok(None) => Checked::Gone,
                Err(why) => Checked::Unchecked(*why),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn ask(path: &str, start: u32, quoted: &[&str]) -> CheckAsk {
        CheckAsk {
            path: path.into(),
            start,
            end: start + quoted.len() as u32 - 1,
            quoted: quoted.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// A repository root with the thread's folder `sub` in it, and a folder beside the root.
    fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tmp");
        let top = tmp.path().join("repo");
        let sub = top.join("sub");
        std::fs::create_dir_all(&sub).expect("dirs");
        std::fs::create_dir_all(tmp.path().join("elsewhere")).expect("dirs");
        let top = top.canonicalize().expect("canon");
        let sub = sub.canonicalize().expect("canon");
        (tmp, top, sub)
    }

    #[test]
    fn a_comment_is_found_where_it_is_moved_or_gone_in_the_working_file() {
        let (_tmp, top, sub) = tree();
        std::fs::write(sub.join("a.rs"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(sub.join("b.rs"), "x\nx\nnew\nlater\n").expect("write");
        let asks = [
            ask("sub/a.rs", 2, &[" two", "+three"]),
            ask("sub/b.rs", 1, &["+new", " later"]),
            ask("sub/a.rs", 1, &["+not there"]),
            ask("sub/deleted.rs", 1, &["+x"]),
            ask("sub/gone/deep.rs", 1, &["+x"]),
        ];
        assert_eq!(
            check(&sub, Some(&top), &asks),
            vec![
                Checked::Exact,
                Checked::Moved { start: 3, end: 4 },
                Checked::Gone,
                Checked::Gone,
                Checked::Gone,
            ]
        );
        assert_eq!(
            check(&sub, None, &asks[..1]),
            vec![Checked::Unchecked(Unchecked::NoRepository)]
        );
    }

    #[test]
    fn a_file_outside_the_threads_folder_is_never_read() {
        let (tmp, top, sub) = tree();
        // In the repository, but not in the thread's folder.
        std::fs::write(top.join("root.rs"), "x\n").expect("write");
        // A link in the folder that leads out, to a file and to a folder.
        let outside = tmp.path().join("elsewhere");
        std::fs::write(outside.join("secret.rs"), "x\n").expect("write");
        symlink(outside.join("secret.rs"), sub.join("link.rs")).expect("link");
        symlink(&outside, sub.join("out")).expect("link");
        // A link that stays inside is followed.
        std::fs::write(sub.join("real.rs"), "x\n").expect("write");
        symlink(sub.join("real.rs"), sub.join("alias.rs")).expect("link");
        let asks = [
            ask("root.rs", 1, &["+x"]),
            ask("sub/link.rs", 1, &["+x"]),
            ask("sub/out/secret.rs", 1, &["+x"]),
            ask("sub/out/missing.rs", 1, &["+x"]),
            ask("../elsewhere/secret.rs", 1, &["+x"]),
            ask("/etc/passwd", 1, &["+x"]),
            ask("sub/alias.rs", 1, &["+x"]),
        ];
        let outside = Checked::Unchecked(Unchecked::Outside);
        assert_eq!(
            check(&sub, Some(&top), &asks),
            vec![
                outside,
                outside,
                outside,
                outside,
                outside,
                outside,
                Checked::Exact
            ]
        );
    }

    #[test]
    fn only_a_regular_text_file_of_bounded_size_is_read() {
        let (_tmp, top, sub) = tree();
        std::fs::write(sub.join("bin.dat"), b"x\0y\n").expect("write");
        std::fs::write(sub.join("latin.txt"), b"caf\xe9\n").expect("write");
        std::fs::create_dir(sub.join("dir")).expect("dir");
        let pipe = sub.join("pipe");
        let c = std::ffi::CString::new(pipe.to_string_lossy().as_bytes()).expect("path");
        // SAFETY: a valid, NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let big = sub.join("big.txt");
        let file = std::fs::File::create(&big).expect("big");
        file.set_len(MAX_BYTES + 1).expect("size");
        let asks = [
            ask("sub/bin.dat", 1, &["+x"]),
            ask("sub/latin.txt", 1, &["+x"]),
            ask("sub/dir", 1, &["+x"]),
            ask("sub/pipe", 1, &["+x"]),
            ask("sub/big.txt", 1, &["+x"]),
        ];
        let unreadable = Checked::Unchecked(Unchecked::Unreadable);
        assert_eq!(
            check(&sub, Some(&top), &asks),
            vec![unreadable, unreadable, unreadable, unreadable, unreadable,]
        );
    }

    #[test]
    fn a_cards_path_is_named_from_the_threads_repository() {
        let top = Path::new("/r");
        assert_eq!(
            repo_path(top, Path::new("/r"), "src/a.rs").as_deref(),
            Some("src/a.rs")
        );
        // A card diffed in a repository inside the thread's one.
        assert_eq!(
            repo_path(top, Path::new("/r/vendor/x"), "a.rs").as_deref(),
            Some("vendor/x/a.rs")
        );
        assert_eq!(repo_path(top, Path::new("/other"), "a.rs"), None);
        assert_eq!(repo_path(top, Path::new("/r"), "../a.rs"), None);
        assert_eq!(repo_path(top, Path::new("/r"), "/etc/passwd"), None);
        assert_eq!(repo_path(top, Path::new("/r"), ""), None);
    }
}
