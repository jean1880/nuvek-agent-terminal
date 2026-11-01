//! A model server's key, handed to Claude Code through a private file rather than its
//! environment: every child Claude starts (its shell tool, hooks, MCP servers) inherits that
//! environment, so a key there is one `env` away from the model's context. The files are
//! `$XDG_RUNTIME_DIR/agent-terminal/provider-keys/<instance>-<n>.key`, `0600` in `0700` folders,
//! one per agent process, read by the settings overlay's `apiKeyHelper` and removed when the
//! process exits. There is no fallback to `/tmp`: without a runtime directory the launch is
//! refused. Blocking file I/O: run it off the main thread.
//!
//! Which files are a crashed run's is told by a lock, not a pid (which another namespace or a
//! reused pid would confuse): each app instance holds an exclusive `flock` on its own
//! `<instance>.lock`, with a random instance id, for as long as it runs ([`Instance`]). A lock
//! that can be taken is a gone instance's, and its files are swept ([`sweep_stale`]).

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The folder's name under `$XDG_RUNTIME_DIR/agent-terminal`.
pub const DIR: &str = "provider-keys";

/// How many names [`KeyFile::write_new`] tries before it gives up.
const NAME_TRIES: usize = 16;

/// `$XDG_RUNTIME_DIR/agent-terminal/provider-keys`; `None` when the runtime directory is unset
/// or not absolute. Takes the value, so tests never read the real environment.
pub fn key_root(xdg_runtime_dir: Option<&str>) -> Option<PathBuf> {
    let base = xdg_runtime_dir.filter(|d| Path::new(d).is_absolute())?;
    Some(Path::new(base).join("agent-terminal").join(DIR))
}

/// `path` as a real `0700` folder of this user, created if absent: never a link.
fn private_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(path)?;
    // SAFETY: getuid(2) takes no arguments and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != uid {
        return Err(std::io::Error::other(format!(
            "{} is not a folder of this user's",
            path.display()
        )));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `root` and the app folder above it, as [`private_dir`]s.
fn private_root(root: &Path) -> std::io::Result<()> {
    if let Some(app) = root.parent() {
        private_dir(app)?;
    }
    private_dir(root)
}

/// Opens `path` for writing, new, `0600`, never through a link.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// `flock(2)` on `file`, without waiting: whether it was taken.
fn try_lock(file: &std::fs::File) -> bool {
    // SAFETY: flock(2) on a descriptor this function borrows; it touches no memory.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// A random instance id: 16 bytes from the kernel, in hex.
fn random_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: getrandom(2) writes at most the given length into the buffer it is given.
        let n = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        filled += n as usize;
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `id` names an instance ([`random_id`]: 32 lowercase hex digits).
fn is_instance_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// This app instance's claim on the key folder: its random id and the exclusive lock it holds
/// on `<id>.lock` while it runs. Its key files are named for it, so a later instance can tell
/// them from its own whatever the pids.
#[derive(Debug)]
pub struct Instance {
    root: PathBuf,
    id: String,
    /// The next key file's number.
    next: std::sync::atomic::AtomicU64,
    /// Held open, locked, for the instance's life: the lock goes with it.
    _lock: std::fs::File,
}

impl Instance {
    /// Takes a new instance id under `root` and locks it. The lock file is made and locked under
    /// a name the sweep ignores, then renamed into place, so no sweep can take it in between.
    pub fn claim(root: &Path) -> std::io::Result<Self> {
        private_root(root)?;
        let id = random_id()?;
        let pending = root.join(format!(".{id}.lock"));
        let lock = create_private(&pending)?;
        if !try_lock(&lock) {
            let _ = std::fs::remove_file(&pending);
            return Err(std::io::Error::other(
                "the instance lock could not be taken",
            ));
        }
        std::fs::rename(&pending, root.join(format!("{id}.lock")))?;
        Ok(Self {
            root: root.to_path_buf(),
            id,
            next: std::sync::atomic::AtomicU64::new(0),
            _lock: lock,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// A path no other launch of this instance has named: `<root>/<id>-<n>.key`. Choosing it
    /// does no I/O, so a command line can name it before the file is written.
    pub fn next_path(&self) -> PathBuf {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.root.join(format!("{}-{n}.key", self.id))
    }

    /// Removes this instance's key files that are still there, then its lock file (unlinked
    /// while still held, so no sweep takes it meanwhile): at exit, once its agents are gone.
    /// Returns how many key files went.
    pub fn remove_own_keys(&self) -> usize {
        let removed = remove_keys_of(&self.root, &self.id);
        let _ = std::fs::remove_file(self.root.join(format!("{}.lock", self.id)));
        removed
    }
}

/// Removes `<id>-*.key` under `root`.
fn remove_keys_of(root: &Path, id: &str) -> usize {
    let prefix = format!("{id}-");
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".key"))
        })
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
}

/// A key written for one agent process. Dropping it removes the file (blocking: drop it off the
/// main thread).
#[derive(Debug)]
pub struct KeyFile {
    path: PathBuf,
}

impl KeyFile {
    /// Writes `key` to `path` (from [`Instance::next_path`]): its folders `0700` and real, the
    /// file new (`O_EXCL`), never through a link (`O_NOFOLLOW`), `0600`.
    pub fn write(path: &Path, key: &str) -> std::io::Result<Self> {
        let root = path
            .parent()
            .ok_or_else(|| std::io::Error::other("a key file needs a folder"))?;
        private_root(root)?;
        let file = create_private(path)?;
        Self::fill(path, file, key)
    }

    /// [`Self::write`] under a new name of `instance`'s, trying the next name when one is taken
    /// (a leftover of a crash, say), so a stale file never refuses a start.
    pub fn write_new(instance: &Instance, key: &str) -> std::io::Result<Self> {
        private_root(instance.root())?;
        let mut last = None;
        for _ in 0..NAME_TRIES {
            let path = instance.next_path();
            match create_private(&path) {
                Ok(file) => return Self::fill(&path, file, key),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no key file name was free")))
    }

    fn fill(path: &Path, mut file: std::fs::File, key: &str) -> std::io::Result<Self> {
        let written = KeyFile {
            path: path.to_path_buf(),
        };
        file.write_all(key.as_bytes())?;
        file.sync_all()?;
        Ok(written)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Removes the key files of instances that are gone: each `<id>.lock` whose lock can be taken
/// (no instance holds it), with its `<id>-*.key` files, and key files whose instance has no lock
/// at all. A live instance's files, `except`'s among them, are left alone. Run at start-up;
/// best effort. Returns how many files were removed.
pub fn sweep_stale(root: &Path, except: Option<&str>) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let mut removed = 0;
    let mut live: Vec<&str> = Vec::new();
    let mut gone: Vec<&str> = Vec::new();
    for name in &names {
        let Some(id) = name.strip_suffix(".lock").filter(|id| !id.starts_with('.')) else {
            continue;
        };
        if Some(id) == except {
            live.push(id);
            continue;
        }
        let path = root.join(name);
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        else {
            live.push(id); // unreadable: never guessed gone
            continue;
        };
        if !try_lock(&file) {
            live.push(id);
            continue;
        }
        // Gone: its keys, then its lock (unlinked while still held, so no one takes it).
        gone.push(id);
        removed += remove_keys_of(root, id);
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    for name in &names {
        let Some(id) = name
            .strip_suffix(".key")
            .and_then(|s| s.rsplit_once('-'))
            .map(|p| p.0)
        else {
            continue;
        };
        // A key named for an instance whose lock this read did not see is left alone: its lock
        // may have been renamed into place while the folder was read. Only an older scheme's
        // leftover (not named for an instance) goes without its lock.
        if is_instance_id(id) && !gone.contains(&id) {
            continue;
        }
        if !live.contains(&id) && std::fs::remove_file(root.join(name)).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_follows_the_runtime_directory_or_there_is_none() {
        assert_eq!(
            key_root(Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/agent-terminal/provider-keys"))
        );
        assert_eq!(key_root(None), None);
        assert_eq!(key_root(Some("relative")), None);
        assert_eq!(key_root(Some("")), None);
    }

    #[test]
    fn a_key_file_is_private_new_and_gone_when_dropped() {
        let runtime = tempfile::tempdir().unwrap();
        let root = key_root(runtime.path().to_str()).unwrap();
        let instance = Instance::claim(&root).unwrap();
        let path = instance.next_path();
        assert_ne!(instance.next_path(), path, "every launch names its own");
        assert!(path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(&format!("{}-", instance.id())));
        let file = KeyFile::write(&path, "k-secret").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "k-secret");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(root.parent().unwrap()), 0o700);
        assert_eq!(mode(&root.join(format!("{}.lock", instance.id()))), 0o600);
        // Never over an existing file, nor through a link.
        assert!(KeyFile::write(&path, "other").is_err());
        let target = runtime.path().join("elsewhere");
        let link = instance.next_path();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(KeyFile::write(&link, "k-secret").is_err());
        assert!(!target.exists());
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn a_name_left_over_is_skipped_never_a_refusal() {
        let runtime = tempfile::tempdir().unwrap();
        let root = key_root(runtime.path().to_str()).unwrap();
        let instance = Instance::claim(&root).unwrap();
        // The first names are taken already (leftovers under the same names).
        let taken: Vec<PathBuf> = (0..3)
            .map(|n| root.join(format!("{}-{n}.key", instance.id())))
            .collect();
        for path in &taken {
            std::fs::write(path, "old").unwrap();
        }
        let file = KeyFile::write_new(&instance, "k-new").unwrap();
        assert!(!taken.contains(&file.path().to_path_buf()));
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "k-new");
    }

    #[test]
    fn a_folder_that_is_a_link_is_refused() {
        let runtime = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(runtime.path().join("agent-terminal")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path(),
            runtime.path().join("agent-terminal").join(DIR),
        )
        .unwrap();
        let root = key_root(runtime.path().to_str()).unwrap();
        assert!(Instance::claim(&root).is_err());
        assert!(KeyFile::write(&root.join("x-0.key"), "k-secret").is_err());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    /// Two instances: the sweep of a new one never touches a live instance's files, whatever its
    /// pid or namespace, and removes a gone one's (its lock free) with its lock file.
    #[test]
    fn the_sweep_removes_only_files_of_instances_that_are_gone() {
        let runtime = tempfile::tempdir().unwrap();
        let root = key_root(runtime.path().to_str()).unwrap();
        let live = Instance::claim(&root).unwrap();
        let live_key = KeyFile::write_new(&live, "k-live").unwrap();
        let gone = Instance::claim(&root).unwrap();
        let gone_key = KeyFile::write_new(&gone, "k-gone").unwrap();
        let gone_path = gone_key.path().to_path_buf();
        let gone_lock = root.join(format!("{}.lock", gone.id()));
        std::mem::forget(gone_key); // a crash leaves its file
        drop(gone); // ... and frees its lock
                    // A key file of an instance with no lock at all (an older scheme's leftover).
        let orphan = root.join("12345-0.key");
        std::fs::write(&orphan, "old").unwrap();
        let me = Instance::claim(&root).unwrap();
        assert_eq!(sweep_stale(&root, Some(me.id())), 3);
        assert!(live_key.path().exists(), "a live instance's file was swept");
        assert!(root.join(format!("{}.lock", live.id())).exists());
        assert!(!gone_path.exists() && !gone_lock.exists() && !orphan.exists());
        assert_eq!(sweep_stale(&runtime.path().join("missing"), None), 0);
        // At exit an instance removes what of its own is left.
        let left = KeyFile::write_new(&me, "k").unwrap();
        let left_path = left.path().to_path_buf();
        std::mem::forget(left);
        assert_eq!(me.remove_own_keys(), 1);
        assert!(!left_path.exists());
        assert!(
            !root.join(format!("{}.lock", me.id())).exists(),
            "an instance leaves its lock behind at exit"
        );
    }

    /// A key named for an instance whose lock the sweep did not see (renamed into place while
    /// the folder was read) is never taken for a leftover.
    #[test]
    fn a_key_whose_instance_lock_was_not_seen_is_kept() {
        let runtime = tempfile::tempdir().unwrap();
        let root = key_root(runtime.path().to_str()).unwrap();
        let me = Instance::claim(&root).unwrap();
        let unseen = root.join(format!("{}-0.key", "0123456789abcdef0123456789abcdef"));
        std::fs::write(&unseen, "k").unwrap();
        assert_eq!(sweep_stale(&root, Some(me.id())), 0);
        assert!(unseen.exists());
    }
}
