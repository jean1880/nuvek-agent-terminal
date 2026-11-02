//! Small file helpers shared by the app's caches.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The process's effective uid, the owner its private files and folders must have.
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Writes `bytes` to `path` through a `0600` temp file in the same directory, `sync_all`, a
/// rename, then a sync of the directory, so a crash leaves the old file or the new one and never
/// a half-written one, and the contents are never readable by anyone else, even briefly. The
/// directory is created when missing. A failure removes the temp file.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_private_atomic_checked(path, bytes, |_| Ok(()))
}

/// [`write_private_atomic`] with a check between the write and the rename: `validate` gets the
/// complete temp file (named `<file>.tmp<pid>-<n>`, so a loader that picks files by extension
/// never sees it) and an `Err` from it leaves `path` exactly as it was. The temp file is
/// created exclusively (`O_EXCL`), so a link planted at its name is never followed.
pub fn write_private_atomic_checked(
    path: &Path,
    bytes: &[u8],
    validate: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    // Unique per call, not just per process: two threads writing the same file must not share a
    // temp file (one would rename the other's half-written bytes).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(
        ".tmp{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        validate(&tmp)?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    } else {
        // The rename lives in the directory entry; without this a crash can still lose it. Some
        // filesystems refuse to sync a directory, and the file itself is already safe, so a
        // failure here is not an error.
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    result
}

/// What identifies one version of a file: its modification time and size, and the inode and
/// change time, so a same-length edit within one mtime tick, or a file renamed into place, is
/// still seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    ino: u64,
    ctime: (i64, i64),
}

impl FileStamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            ino: meta.ino(),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// The file's size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Why [`open_regular`] or [`read_limited`] gave nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    NotFound,
    NotRegular,
    TooLarge,
    Io(std::io::ErrorKind),
}

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => ReadError::NotFound,
            kind => ReadError::Io(kind),
        }
    }
}

/// Opens `path` without blocking (a FIFO or device would otherwise hang the open) and checks,
/// on the open file itself, that it is a regular file: the stamp is of what will be read.
pub fn open_regular(path: &Path) -> Result<(std::fs::File, FileStamp), ReadError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(ReadError::NotRegular);
    }
    Ok((file, FileStamp::of(&meta)))
}

/// All of `file` as UTF-8 text, at most `max` bytes: read through `take(max + 1)`, so a file
/// that grows after it was checked is still bounded.
pub fn read_limited(file: std::fs::File, max: u64) -> Result<String, ReadError> {
    use std::io::Read;
    let mut buf = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(ReadError::TooLarge);
    }
    String::from_utf8(buf).map_err(|_| ReadError::Io(std::io::ErrorKind::InvalidData))
}

/// [`open_regular`] then [`read_limited`]: the one bounded reader of a small settings file.
pub fn read_bounded(path: &Path, max: u64) -> Result<(FileStamp, String), ReadError> {
    let (file, stamp) = open_regular(path)?;
    if stamp.len() > max {
        return Err(ReadError::TooLarge);
    }
    Ok((stamp, read_limited(file, max)?))
}

/// An exclusive advisory lock on `path` (created `0600` if missing), held until the guard is
/// dropped. Blocks: callers run off the main thread.
pub struct FileLock {
    _file: std::fs::File,
}

impl FileLock {
    pub fn take(path: &Path) -> std::io::Result<FileLock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)?;
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this struct owns.
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(FileLock { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn the_bounded_reader_refuses_what_is_not_a_small_regular_text_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("f.json");
        assert_eq!(read_bounded(&path, 8).unwrap_err(), ReadError::NotFound);
        std::fs::write(&path, "12345678").expect("write");
        let (stamp, text) = read_bounded(&path, 8).expect("fits");
        assert_eq!((text.as_str(), stamp.len()), ("12345678", 8));
        std::fs::write(&path, "123456789").expect("write");
        assert_eq!(read_bounded(&path, 8).unwrap_err(), ReadError::TooLarge);
        // Grown after the check: the read itself is bounded.
        let (file, _) = open_regular(&path).expect("open");
        assert_eq!(read_limited(file, 4).unwrap_err(), ReadError::TooLarge);
        assert_eq!(
            read_bounded(dir.path(), 8).unwrap_err(),
            ReadError::NotRegular
        );
        std::fs::write(&path, [0xff, 0xfe]).expect("write");
        assert!(matches!(
            read_bounded(&path, 8).unwrap_err(),
            ReadError::Io(_)
        ));
        // A FIFO is refused without opening it for a peer.
        let fifo = dir.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).expect("cstr");
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(read_bounded(&fifo, 8).unwrap_err(), ReadError::NotRegular);
    }

    #[test]
    fn a_stamp_sees_a_same_length_edit_and_a_replaced_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("f.json");
        std::fs::write(&path, "aaaa").expect("write");
        let (_, first) = open_regular(&path).expect("open");
        // Same length, same second: the change time and inode still move.
        let other = dir.path().join("g.json");
        std::fs::write(&other, "bbbb").expect("write");
        std::fs::rename(&other, &path).expect("rename");
        let (_, second) = open_regular(&path).expect("open");
        assert_ne!(
            first, second,
            "a replaced file of the same size is a new version"
        );
        let (_, again) = open_regular(&path).expect("open");
        assert_eq!(second, again);
    }

    #[test]
    fn a_lock_serialises_read_modify_write_across_threads() {
        let dir = tempfile::tempdir().expect("tmp");
        let (path, lock) = (dir.path().join("n"), dir.path().join("n.lock"));
        std::fs::write(&path, "0").expect("write");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (path, lock) = (path.clone(), lock.clone());
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let _guard = FileLock::take(&lock).expect("lock");
                        let n: u32 = std::fs::read_to_string(&path)
                            .expect("read")
                            .parse()
                            .expect("n");
                        std::fs::write(&path, (n + 1).to_string()).expect("write");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "160");
        let mode = std::fs::metadata(&lock).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_write_is_private_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("sub").join("models.json");
        write_private_atomic(&path, b"one").expect("first");
        write_private_atomic(&path, b"two").expect("second");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["models.json"]);
    }

    #[test]
    fn concurrent_writers_never_share_a_temp_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("shared.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        let body = format!("{i}").repeat(4096);
                        write_private_atomic(&path, body.as_bytes()).expect("write");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        // Whole, from one writer: a shared temp file would interleave digits.
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.len(), 4096);
        let first = text.chars().next().expect("non-empty");
        assert!(text.chars().all(|c| c == first));
        let names: Vec<_> = std::fs::read_dir(dir.path()).expect("dir").collect();
        assert_eq!(names.len(), 1, "no temp file left behind");
    }

    #[test]
    fn a_failed_check_keeps_the_old_file_and_no_temp() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("x.rules");
        write_private_atomic(&path, b"old").expect("first");
        let mut seen = None;
        let result = write_private_atomic_checked(&path, b"new", |tmp| {
            seen = Some((
                tmp.to_path_buf(),
                std::fs::read(tmp).expect("candidate is complete"),
            ));
            Err(std::io::Error::other("rejected"))
        });
        assert_eq!(result.expect_err("rejected").to_string(), "rejected");
        assert_eq!(std::fs::read(&path).expect("read"), b"old");
        let (tmp, candidate) = seen.expect("validated");
        assert_eq!(candidate, b"new");
        assert!(!tmp.to_string_lossy().ends_with(".rules"));
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 1);
        write_private_atomic_checked(&path, b"new", |_| Ok(())).expect("accepted");
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
    }

    #[test]
    fn a_failed_write_cleans_up_after_itself() {
        let dir = tempfile::tempdir().expect("tmp");
        // The target is a directory, so the rename fails.
        let target = dir.path().join("taken");
        std::fs::create_dir(&target).expect("dir");
        assert!(write_private_atomic(&target, b"x").is_err());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["taken"]);
    }
}
