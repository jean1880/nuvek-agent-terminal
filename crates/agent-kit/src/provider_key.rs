//! A model server's key, handed to Claude Code through a private file rather than its
//! environment: every child Claude starts (its shell tool, hooks, MCP servers) inherits that
//! environment, so a key there is one `env` away from the model's context. The file is
//! `$XDG_RUNTIME_DIR/agent-terminal/provider-keys/<pid>-<n>.key`, `0600` in `0700` folders,
//! one per agent process, read by the settings overlay's `apiKeyHelper` and removed when the
//! process exits. There is no fallback to `/tmp`: without a runtime directory the launch is
//! refused. Blocking file I/O: run it off the main thread.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The folder's name under `$XDG_RUNTIME_DIR/agent-terminal`.
pub const DIR: &str = "provider-keys";

/// `$XDG_RUNTIME_DIR/agent-terminal/provider-keys`; `None` when the runtime directory is unset
/// or not absolute. Takes the value, so tests never read the real environment.
pub fn key_root(xdg_runtime_dir: Option<&str>) -> Option<PathBuf> {
    let base = xdg_runtime_dir.filter(|d| Path::new(d).is_absolute())?;
    Some(Path::new(base).join("agent-terminal").join(DIR))
}

/// A path no other launch of this process has named: `<root>/<pid>-<n>.key`. Choosing it does
/// no I/O, so a command line can name it before the file is written.
pub fn next_path(root: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    root.join(format!(
        "{}-{}.key",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
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

/// A key written for one agent process. Dropping it removes the file (blocking: drop it off the
/// main thread).
#[derive(Debug)]
pub struct KeyFile {
    path: PathBuf,
}

impl KeyFile {
    /// Writes `key` to `path` (from [`next_path`]): its folders `0700` and real, the file new
    /// (`O_EXCL`), never through a link (`O_NOFOLLOW`), `0600`.
    pub fn write(path: &Path, key: &str) -> std::io::Result<Self> {
        let root = path
            .parent()
            .ok_or_else(|| std::io::Error::other("a key file needs a folder"))?;
        if let Some(app) = root.parent() {
            private_dir(app)?;
        }
        private_dir(root)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
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

/// Removes the key files under `root` whose process is gone (a crashed run's): a file named for
/// a live process, another instance's, is left alone. Run at start-up; best effort. Returns how
/// many were removed.
pub fn sweep_stale(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let pid = name
                .to_str()
                .and_then(|n| n.split_once('-'))
                .and_then(|(pid, _)| pid.parse::<i32>().ok())
                .filter(|pid| *pid > 0);
            match pid {
                Some(pid) => {
                    // SAFETY: kill(2) with signal 0 only checks that the process exists.
                    let checked = unsafe { libc::kill(pid, 0) };
                    checked != 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                }
                None => true,
            }
        })
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
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
        let path = next_path(&root);
        assert_ne!(next_path(&root), path, "every launch names its own");
        let file = KeyFile::write(&path, "k-secret").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "k-secret");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(root.parent().unwrap()), 0o700);
        // Never over an existing file, nor through a link.
        assert!(KeyFile::write(&path, "other").is_err());
        let target = runtime.path().join("elsewhere");
        let link = next_path(&root);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(KeyFile::write(&link, "k-secret").is_err());
        assert!(!target.exists());
        drop(file);
        assert!(!path.exists());
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
        assert!(KeyFile::write(&next_path(&root), "k-secret").is_err());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_sweep_removes_only_files_of_processes_that_are_gone() {
        let runtime = tempfile::tempdir().unwrap();
        let root = key_root(runtime.path().to_str()).unwrap();
        let mine = KeyFile::write(&next_path(&root), "k").unwrap();
        // A pid no process has (above the kernel's maximum).
        let stale = root.join("2147483646-0.key");
        std::fs::write(&stale, "old").unwrap();
        let odd = root.join("not-a-pid");
        std::fs::write(&odd, "old").unwrap();
        assert_eq!(sweep_stale(&root), 2);
        assert!(mine.path().exists() && !stale.exists() && !odd.exists());
        assert_eq!(sweep_stale(&runtime.path().join("missing")), 0);
    }
}
