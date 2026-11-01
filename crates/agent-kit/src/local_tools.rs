//! The Local agent's tools (`agent_core::local_agent::Tool`), run by its runner in the thread's
//! folder. Blocking; each is bounded (file sizes, result counts, a command's time).
//!
//! What may run without asking is decided before a tool runs (`agent_core::local_agent::gate`),
//! and [`inside`] tells it whether a write stays in the folder: the path is resolved through any
//! symbolic link, so a link pointing out of the folder counts as outside, and a link that cannot
//! be resolved (a dangling one) does too.
//!
//! A write is then bound to what was judged, however long its approval took: its folders are
//! opened one at a time from the thread's folder (or `/` for an approved write outside it)
//! without following any link (`O_NOFOLLOW`), and the file is opened in the last of them the
//! same way. A folder swapped for a link meanwhile fails the write instead of redirecting it, and
//! a write judged inside never lands outside. Files are opened without blocking and read only
//! when they are regular files, at most a fixed size: a pipe or a device is refused.

use std::ffi::{CString, OsStr};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_core::local_agent::Tool;
use serde_json::Value;

/// The largest file read whole.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
/// The most of a command's output kept (each of stdout and stderr); the rest is drained unseen.
const MAX_COMMAND_OUTPUT: usize = 256 * 1024;
/// Lines shown by a read with no limit.
const DEFAULT_READ_LINES: usize = 2000;
const MAX_LIST: usize = 1000;
const MAX_MATCHES: usize = 200;
const MAX_SEARCH_FILE: u64 = 2 * 1024 * 1024;
const MAX_MATCH_LINE: usize = 300;
const DEFAULT_COMMAND_SECS: u64 = 120;
const MAX_COMMAND_SECS: u64 = 600;
/// Folders a search never walks into: generated or vendored trees.
const SKIPPED_DIRS: [&str; 6] = [
    ".git",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    ".cache",
];

/// `path` made absolute against `cwd`, with `.` and `..` resolved by name.
pub fn absolute(cwd: &Path, path: &str) -> PathBuf {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `path` with its deepest existing ancestor resolved through symbolic links, so a link that
/// leaves a folder is seen to leave it. `None` when it cannot be told where the path leads: a
/// component that is a link to nothing (it would be followed to wherever it names), or an
/// ancestor that cannot be resolved.
fn resolved(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        // A dangling link: `exists` follows it and says no, but writing through it would not.
        if std::fs::symlink_metadata(&existing).is_ok() {
            return None;
        }
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
            return None;
        };
        rest.push(name.to_owned());
        existing = parent.to_path_buf();
    }
    let mut out = existing.canonicalize().ok()?;
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    Some(out)
}

/// The thread's folder, pinned when the agent starts: the path it was given, where that led
/// then, and the folder itself, open. Writes and commands are anchored to the open folder, so
/// the folder renamed or replaced by a link later never moves them, and a path that no longer
/// leads into it is outside.
#[derive(Debug)]
pub struct Workspace {
    /// As given: relative tool paths are joined to it.
    cwd: PathBuf,
    /// Where it led when opened.
    root: PathBuf,
    folder: OwnedFd,
    /// The user's homes, resolved, for the dotfile rule of [`read_must_ask`]: `$HOME` and the
    /// account's own, which an environment may not agree on. Empty when neither resolves, and
    /// then every dot-named path asks and so does every search.
    homes: Vec<PathBuf>,
}

/// The account's home from the password database, whatever `$HOME` says.
fn account_home() -> Option<PathBuf> {
    // SAFETY: getpwuid_r writes only into `pwd` and `buf`, both owned here and sized as given;
    // `pw_dir` points into `buf`, read before `buf` is dropped.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let r = libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        );
        if r != 0 || found.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr(pwd.pw_dir).to_bytes();
        (!dir.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(dir)))
    }
}

impl Workspace {
    /// [`Self::open_with_homes`] with `$HOME` and the account's home.
    pub fn open(cwd: &Path) -> Result<Self, String> {
        let env = std::env::var_os("HOME").map(PathBuf::from);
        Self::open_with_homes(cwd, env.iter().cloned().chain(account_home()))
    }

    /// [`Self::open_with_homes`] with only `home` (none: home unknown).
    pub fn open_with_home(cwd: &Path, home: Option<&Path>) -> Result<Self, String> {
        Self::open_with_homes(cwd, home.map(Path::to_path_buf))
    }

    pub fn open_with_homes(
        cwd: &Path,
        homes: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, String> {
        let root = cwd
            .canonicalize()
            .map_err(|e| format!("Cannot open the thread's folder {}: {e}", cwd.display()))?;
        let folder = open_at(
            None,
            &cstring(root.as_os_str())?,
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(|e| format!("Cannot open the thread's folder {}: {e}", cwd.display()))?;
        let mut resolved_homes: Vec<PathBuf> = Vec::new();
        for home in homes {
            if let Ok(real) = home.canonicalize() {
                if !resolved_homes.contains(&real) {
                    resolved_homes.push(real);
                }
            }
        }
        Ok(Self {
            cwd: cwd.to_path_buf(),
            root,
            folder,
            homes: resolved_homes,
        })
    }

    /// The homes `dir` (resolved) is, or is above.
    fn homes_under<'a>(&'a self, dir: &'a Path) -> impl Iterator<Item = &'a Path> {
        self.homes
            .iter()
            .map(PathBuf::as_path)
            .filter(move |h| h.starts_with(dir))
    }

    /// Whether `target` is a top-level dotfile or dot-folder (or below one) of a home that
    /// `dir` holds, or, with no home known, has a dot-named component below `dir`.
    fn home_dotfile(&self, dir: &Path, target: &Path) -> bool {
        let first_is_dot = |rel: &Path| {
            lowered(rel)
                .into_iter()
                .next()
                .is_some_and(|first| first.starts_with('.'))
        };
        if self.homes.is_empty() {
            return target
                .strip_prefix(dir)
                .is_ok_and(|rel| lowered(rel).iter().any(|n| n.starts_with('.')));
        }
        self.homes_under(dir)
            .any(|home| target.strip_prefix(home).is_ok_and(first_is_dot))
    }

    /// Whether `target` (resolved, inside the folder) is sensitive: a name
    /// `agent_core::sensitive` lists below the folder, or, when the folder holds a home, a
    /// top-level dotfile or dot-folder of that home.
    fn sensitive(&self, target: &Path) -> bool {
        target
            .strip_prefix(&self.root)
            .is_ok_and(|rel| agent_core::sensitive::names_sensitive(&lowered(rel)))
            || self.home_dotfile(&self.root, target)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

/// Whether `path` (relative to the thread's folder or absolute) is inside the folder as it was
/// pinned, links followed. Anything that cannot be resolved is outside: it asks.
pub fn inside(ws: &Workspace, path: &str) -> bool {
    resolved(&absolute(&ws.cwd, path)).is_some_and(|p| p.starts_with(&ws.root))
}

/// The path a read-only `tool` call names: `file_path` for a read, `path` for a listing, and
/// for a search its `path` or the folder itself.
fn read_path(tool: Tool, input: &Value) -> Option<&str> {
    let key = if tool == Tool::ReadFile {
        "file_path"
    } else {
        "path"
    };
    input
        .get(key)
        .and_then(Value::as_str)
        .or((tool == Tool::Search).then_some("."))
}

/// `rel`'s component names, lowercased, as the sensitive list compares them.
fn lowered(rel: &Path) -> Vec<String> {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// Whether a file has a second hard link, which may name it from outside the folder.
fn hard_linked(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.is_file() && meta.nlink() > 1
}

/// Whether a read-only `tool` call must ask, on the lines of the agy hook's read policy: its
/// path leads outside the folder (links followed, so one that stays inside is judged where it
/// leads; one that cannot be resolved counts as outside), names a sensitive file
/// ([`Workspace::sensitive`]) or a file with a second hard link, or, for a search, is a home or
/// above one (any search, when no home is known). Anything it cannot tell asks.
pub fn read_must_ask(ws: &Workspace, tool: Tool, input: &Value) -> bool {
    let Some(target) = read_path(tool, input).and_then(|p| resolved(&absolute(&ws.cwd, p))) else {
        return true;
    };
    if !target.starts_with(&ws.root) || ws.sensitive(&target) {
        return true;
    }
    if std::fs::symlink_metadata(&target).is_ok_and(|m| hard_linked(&m)) {
        return true;
    }
    tool == Tool::Search && (ws.homes.is_empty() || ws.homes_under(&target).next().is_some())
}

/// Opens `target` (resolved) inside the pinned folder, one component at a time from the folder
/// opened when the agent started and never through a link (`O_NOFOLLOW`): the path was judged
/// with every link resolved, so a component that became a link since fails the open instead of
/// redirecting it. `None` when `target` is not inside the folder.
fn open_inside(
    ws: &Workspace,
    target: &Path,
    leaf: libc::c_int,
) -> Option<std::io::Result<OwnedFd>> {
    let rel = target.strip_prefix(&ws.root).ok()?;
    let names: Vec<_> = rel.components().map(|c| c.as_os_str().to_owned()).collect();
    let open = || -> std::io::Result<OwnedFd> {
        let Some((last, folders)) = names.split_last() else {
            return ws.folder.try_clone();
        };
        let mut dir = ws.folder.try_clone()?;
        let invalid = |_| std::io::Error::from(std::io::ErrorKind::InvalidInput);
        for name in folders {
            let name = CString::new(name.as_bytes()).map_err(invalid)?;
            dir = open_at(
                Some(&dir),
                &name,
                libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW,
                0,
            )?;
        }
        let name = CString::new(last.as_bytes()).map_err(invalid)?;
        open_at(Some(&dir), &name, leaf | libc::O_NOFOLLOW, 0)
    };
    Some(open())
}

/// Opens what a read names: inside the folder, bound to the folder ([`open_inside`]); outside
/// it (an approved read), where the path leads now.
fn open_for_read(ws: &Workspace, shown: &Path, leaf: libc::c_int) -> Result<OwnedFd, String> {
    let fail = |e: std::io::Error| format!("Cannot read {}: {e}", shown.display());
    let target = resolved(shown).ok_or_else(|| {
        format!(
            "{} leads through a link that names nothing; nothing was read",
            shown.display()
        )
    })?;
    match open_inside(ws, &target, leaf) {
        Some(opened) => opened.map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) {
                format!(
                    "{} is not as it was judged (a part of its path is not a folder, or is a \
                     link now), so nothing was read",
                    shown.display()
                )
            } else {
                fail(e)
            }
        }),
        None => open_at(None, &cstring(target.as_os_str())?, leaf, 0).map_err(fail),
    }
}

/// What an entry of a folder is, never following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Folder,
    File,
    Other,
}

/// The entries of an open folder (`.` and `..` left out), read from the folder itself, so
/// neither its path nor `/proc` is needed.
fn entries_of(dir: &OwnedFd) -> std::io::Result<Vec<(std::ffi::OsString, Kind)>> {
    let dup = dir.try_clone()?;
    // SAFETY: on success fdopendir owns `dup`'s descriptor, released by closedir below; on
    // failure it owns nothing and `dup` closes it.
    let stream = unsafe { libc::fdopendir(dup.as_raw_fd()) };
    if stream.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    std::mem::forget(dup);
    // A duplicate shares its position with the folder it came from (the pinned folder among
    // them, listed again and again): always read from the start.
    // SAFETY: `stream` is open.
    unsafe { libc::rewinddir(stream) };
    let mut out = Vec::new();
    let result = loop {
        // SAFETY: `stream` is open; errno is reset so an end can be told from a failure.
        let ent = unsafe {
            *libc::__errno_location() = 0;
            libc::readdir(stream)
        };
        if ent.is_null() {
            let err = std::io::Error::last_os_error();
            break if err.raw_os_error().unwrap_or(0) == 0 {
                Ok(())
            } else {
                Err(err)
            };
        }
        // SAFETY: readdir returned an entry valid until the next call; its name is
        // NUL-terminated, and the name is copied before that call.
        let (name, d_type) = unsafe {
            (
                std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()).to_owned(),
                (*ent).d_type,
            )
        };
        if matches!(name.as_bytes(), b"." | b"..") {
            continue;
        }
        let kind = match d_type {
            libc::DT_DIR => Kind::Folder,
            libc::DT_REG => Kind::File,
            libc::DT_UNKNOWN => {
                // SAFETY: an all-zero stat is valid for fstatat to fill; `name` is
                // NUL-terminated and `dir` an open folder.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                let r = unsafe {
                    libc::fstatat(
                        dir.as_raw_fd(),
                        name.as_ptr(),
                        &mut st,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                match st.st_mode & libc::S_IFMT {
                    _ if r != 0 => Kind::Other,
                    libc::S_IFDIR => Kind::Folder,
                    libc::S_IFREG => Kind::File,
                    _ => Kind::Other,
                }
            }
            _ => Kind::Other,
        };
        out.push((OsStr::from_bytes(name.as_bytes()).to_owned(), kind));
    };
    // SAFETY: `stream` is open and closed once, here.
    unsafe { libc::closedir(stream) };
    result.map(|()| out)
}

fn text<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is required"))
}

/// Runs `tool` with `input` in the thread's folder: what the model is told, or why it failed.
/// `may_leave` says a call past the folder was approved (or every request is: Bypass): a write
/// outside it, or a read [`read_must_ask`] asks for. Without it a write that now leads outside
/// is refused, and a read is judged again just before it runs (refused if it now must ask) and
/// then opened bound to the folder ([`open_inside`]), a file with a second hard link refused. A
/// search never reads a sensitive file, approved or not. `cancel` stops a running command (its
/// whole process group) or a search, when it is set.
pub fn run(
    tool: Tool,
    input: &Value,
    ws: &Workspace,
    may_leave: bool,
    cancel: &AtomicBool,
) -> Result<String, String> {
    tool.validate_arguments(input).map_err(str::to_owned)?;
    if !tool.writes() && !may_leave && read_must_ask(ws, tool, input) {
        return Err(format!(
            "{} now leads outside the folder or to a sensitive file; nothing was read",
            read_path(tool, input).unwrap_or_default()
        ));
    }
    match tool {
        Tool::ReadFile => read_file(input, ws, may_leave),
        Tool::ListDir => list_dir(input, ws),
        Tool::Search => search(input, ws, cancel),
        Tool::WriteFile => write_file(input, ws, may_leave),
        Tool::EditFile => edit_file(input, ws, may_leave),
        Tool::RunCommand => run_command(input, ws, cancel),
    }
}

fn cstring(part: &OsStr) -> Result<CString, String> {
    CString::new(part.as_bytes()).map_err(|_| "The path holds a NUL byte".to_owned())
}

/// `openat(2)` relative to `dir` (the process's folder when `None`), close-on-exec.
fn open_at(
    dir: Option<&OwnedFd>,
    name: &CString,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> std::io::Result<OwnedFd> {
    let base = dir.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
    // SAFETY: `name` is a valid NUL-terminated string and `base` an open folder (or AT_FDCWD).
    let fd = unsafe { libc::openat(base, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened here and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The open text of a regular file, at most `max` bytes: a folder, a pipe or a device is
/// refused (it was opened without blocking, so a pipe never hangs the open), and the read itself
/// is bounded, whatever the file's size said.
fn read_open(fd: OwnedFd, shown: &Path, max: u64) -> Result<String, String> {
    read_open_bytes(fd, shown, max).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// [`read_open`]'s bytes, as they are.
fn read_open_bytes(fd: OwnedFd, shown: &Path, max: u64) -> Result<Vec<u8>, String> {
    let file = std::fs::File::from(fd);
    let meta = file
        .metadata()
        .map_err(|e| format!("Cannot read {}: {e}", shown.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder; use list_dir", shown.display()));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", shown.display()));
    }
    if meta.len() > max {
        return Err(format!(
            "{} is {} MB, too large to read whole",
            shown.display(),
            meta.len() / (1024 * 1024)
        ));
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Cannot read {}: {e}", shown.display()))?;
    if bytes.len() as u64 > max {
        return Err(format!("{} grew too large to read whole", shown.display()));
    }
    Ok(bytes)
}

/// The folder a write of `path` lands in, and the file's name in it. The path is resolved as
/// [`inside`] judges it; then each folder from the pinned thread folder (or from `/`, only when
/// `may_leave`) is opened in turn without following a link, made when `create` and missing. A
/// folder that became a link, or a path that now leads outside when it may not (the thread's
/// folder itself replaced included), fails it.
fn write_target(
    ws: &Workspace,
    path: &str,
    may_leave: bool,
    create: bool,
) -> Result<(OwnedFd, CString, PathBuf), String> {
    let shown = absolute(&ws.cwd, path);
    let target = resolved(&shown).ok_or_else(|| {
        format!(
            "{} leads through a link that names nothing; nothing was written",
            shown.display()
        )
    })?;
    let changed = |e: std::io::Error| {
        format!(
            "A folder on the way to {} changed (it is a link now, or gone), so nothing was \
             written: {e}",
            shown.display()
        )
    };
    // Inside: from the folder opened when the agent started, never from its path again.
    let (base, rel) = match target.strip_prefix(&ws.root) {
        Ok(rel) => (ws.folder.try_clone().map_err(changed)?, rel.to_path_buf()),
        Err(_) if may_leave => (
            open_at(
                None,
                &cstring(OsStr::new("/"))?,
                libc::O_DIRECTORY | libc::O_RDONLY,
                0,
            )
            .map_err(changed)?,
            target.strip_prefix("/").unwrap_or(&target).to_path_buf(),
        ),
        Err(_) => {
            return Err(format!(
                "{} now leads outside the folder; nothing was written",
                shown.display()
            ))
        }
    };
    let names: Vec<_> = rel.components().map(|c| c.as_os_str().to_owned()).collect();
    let Some((last, folders)) = names.split_last() else {
        return Err(format!("{} is a folder", shown.display()));
    };
    let folder_flags = libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW;
    let mut dir = base;
    for name in folders {
        let name = cstring(name)?;
        let next = match open_at(Some(&dir), &name, folder_flags, 0) {
            Ok(fd) => fd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                // SAFETY: `dir` is an open folder and `name` a valid NUL-terminated string.
                if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(format!(
                            "Cannot create a folder for {}: {e}",
                            shown.display()
                        ));
                    }
                }
                open_at(Some(&dir), &name, folder_flags, 0).map_err(changed)?
            }
            Err(e) => return Err(changed(e)),
        };
        dir = next;
    }
    Ok((dir, cstring(last)?, shown))
}

/// Writes `content` as the file `name` in the open folder `dir`, never through a link and only
/// to a regular file: it is opened without blocking (a pipe with no reader is refused at once,
/// never waited on) and without truncating, checked, and only then emptied and written.
fn write_at(dir: &OwnedFd, name: &CString, shown: &Path, content: &str) -> Result<(), String> {
    use std::io::Write;
    let fd = open_at(
        Some(dir),
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0o666,
    )
    .map_err(|e| format!("Cannot write {}: {e}", shown.display()))?;
    let file = std::fs::File::from(fd);
    let meta = file
        .metadata()
        .map_err(|e| format!("Cannot write {}: {e}", shown.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file; nothing was written",
            shown.display()
        ));
    }
    file.set_len(0)
        .map_err(|e| format!("Cannot write {}: {e}", shown.display()))?;
    (&file)
        .write_all(content.as_bytes())
        .map_err(|e| format!("Cannot write {}: {e}", shown.display()))
}

/// `approved`: the read was allowed past the folder, so a second hard link does not refuse it.
fn read_file(input: &Value, ws: &Workspace, approved: bool) -> Result<String, String> {
    let path = absolute(&ws.cwd, text(input, "file_path")?);
    let fd = open_for_read(ws, &path, libc::O_RDONLY | libc::O_NONBLOCK)?;
    if !approved {
        let file = std::fs::File::from(fd.try_clone().map_err(|e| e.to_string())?);
        if file.metadata().is_ok_and(|m| hard_linked(&m)) {
            return Err(format!(
                "{} has another hard link now; nothing was read",
                path.display()
            ));
        }
    }
    let content = read_open(fd, &path, MAX_READ_BYTES)?;
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .map_or(1, |o| o.max(1) as usize);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_READ_LINES, |l| l.max(1) as usize);
    let total = content.lines().count();
    // The model picks both: neither may overflow, whatever it sends.
    let start = (offset - 1).min(total);
    let end = start.saturating_add(limit).min(total);
    let mut out: String = content
        .lines()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(i, line)| format!("{:>6}\t{line}\n", i + 1))
        .collect();
    if end < total {
        out.push_str(&format!(
            "… {} more lines (read on with offset {})\n",
            total - end,
            end + 1
        ));
    }
    if out.is_empty() {
        out = "(empty)".to_owned();
    }
    Ok(out)
}

fn list_dir(input: &Value, ws: &Workspace) -> Result<String, String> {
    let path = absolute(
        &ws.cwd,
        input.get("path").and_then(Value::as_str).unwrap_or("."),
    );
    let dir = open_for_read(ws, &path, libc::O_DIRECTORY | libc::O_RDONLY)?;
    let entries = entries_of(&dir).map_err(|e| format!("Cannot list {}: {e}", path.display()))?;
    let mut names: Vec<String> = entries
        .into_iter()
        .map(|(name, kind)| {
            let name = name.to_string_lossy().into_owned();
            if kind == Kind::Folder {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    let more = names.len().saturating_sub(MAX_LIST);
    names.truncate(MAX_LIST);
    let mut out = names.join("\n");
    if more > 0 {
        out.push_str(&format!("\n… {more} more entries"));
    }
    Ok(if out.is_empty() {
        "(empty folder)".to_owned()
    } else {
        out
    })
}

/// Walks the folder a search names (bound to the thread's folder when inside it, see
/// [`open_for_read`]) from open folders only, never following a link. Whether or not it was
/// approved, it never reads a sensitive file, a file with a second hard link, nor a home's
/// dotfiles: inside the thread's folder as [`Workspace::sensitive`] judges them (with names
/// below the search's root, and home's dotfiles when the search starts at a home or above);
/// outside it, by the whole resolved path, wherever the search started. A read by name, which
/// asks, is the way to see one. A folder is opened only when its turn comes, so the open
/// folders never outnumber the depth; what could not be opened or read is counted and said.
fn search(input: &Value, ws: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
    use std::rc::Rc;
    let pattern = text(input, "pattern")?;
    if pattern.is_empty() {
        return Err("pattern is empty".to_owned());
    }
    let root = absolute(
        &ws.cwd,
        input.get("path").and_then(Value::as_str).unwrap_or("."),
    );
    let real_root = resolved(&root)
        .ok_or_else(|| format!("{} leads through a link that names nothing", root.display()))?;
    let skipped = |rel: &Path| {
        let real = real_root.join(rel);
        if real.starts_with(&ws.root) {
            return agent_core::sensitive::names_sensitive(&lowered(rel))
                || ws.sensitive(&real)
                || ws.home_dotfile(&real_root, &real);
        }
        agent_core::sensitive::names_sensitive(&lowered(&real))
            || if ws.homes.is_empty() {
                lowered(&real).iter().any(|n| n.starts_with('.'))
            } else {
                ws.homes.iter().any(|home| {
                    real.strip_prefix(home).is_ok_and(|rel| {
                        lowered(rel)
                            .into_iter()
                            .next()
                            .is_some_and(|first| first.starts_with('.'))
                    })
                })
            }
    };
    let listed = |dir: &OwnedFd| -> std::io::Result<Vec<(std::ffi::OsString, Kind)>> {
        let mut entries = entries_of(dir)?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    };
    let top = Rc::new(open_for_read(
        ws,
        &root,
        libc::O_DIRECTORY | libc::O_RDONLY,
    )?);
    // Its own folder unreadable is an error, never an empty result.
    let top_entries = listed(&top).map_err(|e| format!("Cannot list {}: {e}", root.display()))?;
    let mut matches = Vec::new();
    let mut unread = 0usize;
    let mut withheld = 0usize;
    let mut more = false;
    // Folders to visit: the open folder they are in, their name, and where they are.
    let mut stack: Vec<(Rc<OwnedFd>, std::ffi::OsString, PathBuf)> = Vec::new();
    let mut current = Some((top, PathBuf::new(), top_entries));
    'walk: loop {
        let (dir, dir_rel, entries) = match current.take() {
            Some(next) => next,
            None => {
                let Some((parent, name, rel)) = stack.pop() else {
                    break;
                };
                let flags = libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW;
                let opened = cstring(&name)
                    .ok()
                    .and_then(|c| open_at(Some(&parent), &c, flags, 0).ok());
                let Some(sub) = opened else {
                    unread += 1;
                    continue;
                };
                let Ok(entries) = listed(&sub) else {
                    unread += 1;
                    continue;
                };
                (Rc::new(sub), rel, entries)
            }
        };
        let mut folders = Vec::new();
        for (name, kind) in entries {
            if cancel.load(Ordering::Relaxed) {
                return Err("Stopped by the user.".to_owned());
            }
            let rel = dir_rel.join(&name);
            // Generated trees go unremarked; anything else withheld is counted and said.
            if kind == Kind::Folder && SKIPPED_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            if skipped(&rel) {
                withheld += 1;
                continue;
            }
            // Not followed: a link, a pipe or a device is never read.
            match kind {
                Kind::Folder => {
                    folders.push((dir.clone(), name, rel));
                    continue;
                }
                Kind::Other => continue,
                Kind::File => {}
            }
            // Opened in its folder without following a link or blocking, read only if still a
            // regular file with no second hard link.
            let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW;
            let Some(fd) = cstring(&name)
                .ok()
                .and_then(|c| open_at(Some(&dir), &c, flags, 0).ok())
            else {
                unread += 1;
                continue;
            };
            let Ok(meta) = fd
                .try_clone()
                .and_then(|f| std::fs::File::from(f).metadata())
            else {
                unread += 1;
                continue;
            };
            if hard_linked(&meta) {
                withheld += 1;
                continue;
            }
            let path = root.join(&rel);
            // Too large, or no longer a regular file: not searched, and said so.
            let Ok(content) = read_open(fd, &path, MAX_SEARCH_FILE) else {
                unread += 1;
                continue;
            };
            if content.bytes().take(8192).any(|b| b == 0) {
                continue; // binary
            }
            let shown = rel.display().to_string();
            for (i, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    if matches.len() == MAX_MATCHES {
                        more = true;
                        break 'walk;
                    }
                    let line: String = line.trim().chars().take(MAX_MATCH_LINE).collect();
                    matches.push(format!("{shown}:{}: {line}", i + 1));
                }
            }
        }
        // Visited in name order: the stack takes the last first.
        stack.extend(folders.into_iter().rev());
    }
    let mut out = if matches.is_empty() {
        format!("No line contains {pattern:?}.")
    } else {
        matches.join("\n")
    };
    if more {
        out.push_str(&format!(
            "\n… more than {MAX_MATCHES} matches; narrow the search"
        ));
    }
    if unread > 0 {
        out.push_str(&format!(
            "\n… {unread} folders or files could not be opened or read whole (over {} MB), so \
             the search is incomplete",
            MAX_SEARCH_FILE / (1024 * 1024)
        ));
    }
    if withheld > 0 {
        out.push_str(&format!(
            "\n… {withheld} sensitive or hard-linked files or folders were not searched; read \
             one by name to see it (that asks first)"
        ));
    }
    Ok(out)
}

fn write_file(input: &Value, ws: &Workspace, may_leave: bool) -> Result<String, String> {
    let content = text(input, "content")?;
    let (dir, name, path) = write_target(ws, text(input, "file_path")?, may_leave, true)?;
    write_at(&dir, &name, &path, content)?;
    Ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        path.display()
    ))
}

fn edit_file(input: &Value, ws: &Workspace, may_leave: bool) -> Result<String, String> {
    let old = text(input, "old_string")?;
    let new = text(input, "new_string")?;
    if old.is_empty() {
        return Err("old_string is empty; use write_file to create a file".to_owned());
    }
    if old == new {
        return Err("old_string and new_string are the same".to_owned());
    }
    // Read and written through the same folder, opened without following links.
    let (dir, name, path) = write_target(ws, text(input, "file_path")?, may_leave, false)?;
    let fd = open_at(
        Some(&dir),
        &name,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW,
        0,
    )
    .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    // Edited as text only when it is text: a lossy reading would rewrite every other byte.
    let content = String::from_utf8(read_open_bytes(fd, &path, MAX_READ_BYTES)?).map_err(|_| {
        format!(
            "{} is not UTF-8 text, so it cannot be edited; nothing was changed",
            path.display()
        )
    })?;
    let count = content.matches(old).count();
    let all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if count == 0 {
        return Err(format!("old_string is not in {}", path.display()));
    }
    if count > 1 && !all {
        let file = path.display();
        return Err(format!(
            "old_string occurs {count} times in {file}; add context to make it unique \
             or set replace_all"
        ));
    }
    let edited = if all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    write_at(&dir, &name, &path, &edited)?;
    let replaced = if all { count } else { 1 };
    Ok(format!(
        "Edited {} ({replaced} replacement{})",
        path.display(),
        if replaced == 1 { "" } else { "s" }
    ))
}

/// Runs a command line with `sh -c` in `cwd`, in a process group of its own so a timeout or a
/// Stop (`cancel`) ends it and everything it started. Each output is kept up to
/// [`MAX_COMMAND_OUTPUT`]; past that it is read and dropped, so the command never blocks on a
/// full pipe and never fills memory.
fn run_command(input: &Value, ws: &Workspace, cancel: &AtomicBool) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::Instant;
    let command = text(input, "command")?;
    if command.trim().is_empty() {
        return Err("command is empty".to_owned());
    }
    let secs = input
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_COMMAND_SECS, |s| s.clamp(1, MAX_COMMAND_SECS));
    let folder = ws.folder.as_raw_fd();
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: only `fchdir`, async-signal-safe, between fork and exec; `folder` stays open in
    // the child until exec (it closes there: the descriptor is close-on-exec). It runs in the
    // folder opened when the agent started, never wherever its path leads now.
    unsafe {
        cmd.pre_exec(move || {
            if libc::fchdir(folder) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not run the command: {e}"))?;
    let group = child.id() as libc::pid_t;
    // Each pipe drains on its own thread into a shared, capped buffer and says when it ended.
    // The result never waits on them past a short grace: a descendant that left the group
    // (`setsid`) and kept a pipe open would otherwise hold the call open forever. The drains poll
    // in short slices and stop once the call returns (`stop`), closing their end of the pipe, so
    // nothing is left behind either.
    type Drained = (
        std::sync::Arc<std::sync::Mutex<(Vec<u8>, usize)>>,
        std::sync::mpsc::Receiver<()>,
    );
    struct StopOnDrop(std::sync::Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _stop_on_return = StopOnDrop(stop.clone());
    let drain = |pipe: Option<(Box<dyn Read + Send>, libc::c_int)>| -> Drained {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), 0usize)));
        let (done, finished) = std::sync::mpsc::channel();
        let (sink, stop) = (buffer.clone(), stop.clone());
        std::thread::spawn(move || {
            if let Some((mut pipe, fd)) = pipe {
                let mut chunk = [0u8; 8192];
                while !stop.load(Ordering::Relaxed) {
                    let mut ready = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: a valid pollfd; `fd` stays open while `pipe` (owned here) lives.
                    match unsafe { libc::poll(&mut ready, 1, 100) } {
                        0 => continue,
                        n if n < 0 => {
                            if std::io::Error::last_os_error().kind()
                                == std::io::ErrorKind::Interrupted
                            {
                                continue;
                            }
                            break;
                        }
                        _ => {}
                    }
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut held) = sink.lock() {
                                let room = MAX_COMMAND_OUTPUT.saturating_sub(held.0.len());
                                held.0.extend_from_slice(&chunk[..n.min(room)]);
                                held.1 += n.saturating_sub(room);
                            }
                        }
                    }
                }
            }
            let _ = done.send(());
        });
        (buffer, finished)
    };
    let collect = |(buffer, finished): Drained, until: std::time::Instant| -> (Vec<u8>, usize) {
        let _ = finished.recv_timeout(until.saturating_duration_since(std::time::Instant::now()));
        buffer
            .lock()
            .map(|held| (held.0.clone(), held.1))
            .unwrap_or_default()
    };
    let stdout = drain(child.stdout.take().map(|p| {
        let fd = p.as_raw_fd();
        (Box::new(p) as Box<dyn Read + Send>, fd)
    }));
    let stderr = drain(child.stderr.take().map(|p| {
        let fd = p.as_raw_fd();
        (Box::new(p) as Box<dyn Read + Send>, fd)
    }));
    let kill_group = || {
        // SAFETY: killpg with the group this child leads; at worst it names no process.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    };
    // Whether the shell has exited, left unreaped: its pid, and so the group's id, stays taken
    // until it is reaped, so `kill_group` can never reach a group that reused the id.
    let exited = || -> std::io::Result<bool> {
        // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `group` is this child's pid; WNOHANG never blocks, WNOWAIT reaps nothing.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                group as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: waitid filled `info`; si_pid is 0 when the child has not exited.
        Ok(unsafe { info.si_pid() } != 0)
    };
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match exited() {
            Ok(true) => break,
            Ok(false) if cancel.load(Ordering::Relaxed) => {
                kill_group();
                let _ = child.wait();
                return Err("Stopped by the user.".to_owned());
            }
            Ok(false) if Instant::now() >= deadline => {
                kill_group();
                let _ = child.wait();
                return Err(format!(
                    "The command did not finish within {secs}s and was ended."
                ));
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // Reaped by someone else: its id may be another process's group already.
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                let _ = child.wait();
                return Err(format!("Could not wait for the command: {e}"));
            }
            Err(e) => {
                kill_group();
                let _ = child.wait();
                return Err(format!("Could not wait for the command: {e}"));
            }
        }
    }
    // A background job it left in its group would keep the drains reading: end the group while
    // the shell still holds its id, then reap it and take what the pipes gave within a short
    // grace.
    kill_group();
    let status = child
        .wait()
        .map_err(|e| format!("Could not wait for the command: {e}"))?;
    let grace = Instant::now() + Duration::from_millis(500);
    let (out, out_dropped) = collect(stdout, grace);
    let (err, err_dropped) = collect(stderr, grace);
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if out_dropped > 0 {
        text.push_str(&format!("\n… {out_dropped} more bytes of output not kept"));
    }
    let err_text = String::from_utf8_lossy(&err);
    if !err_text.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&err_text);
        if err_dropped > 0 {
            text.push_str(&format!("\n… {err_dropped} more bytes of errors not kept"));
        }
    }
    match status.code() {
        Some(0) => {}
        Some(code) => text.push_str(&format!("\n(exit code {code})")),
        None => text.push_str("\n(ended by a signal)"),
    }
    Ok(if text.trim().is_empty() {
        "(no output)".to_owned()
    } else {
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn ws(cwd: &Path) -> Workspace {
        Workspace::open(cwd).expect("workspace")
    }

    /// The note a search ends with when it withheld `n` files or folders.
    fn withheld(n: usize) -> String {
        format!(
            "\n… {n} sensitive or hard-linked files or folders were not searched; read one by \
             name to see it (that asks first)"
        )
    }

    /// [`run`] with no Stop, and no write outside approved.
    fn go(tool: Tool, input: &Value, cwd: &Path) -> Result<String, String> {
        run(tool, input, &ws(cwd), false, &AtomicBool::new(false))
    }

    #[test]
    fn the_thread_folder_replaced_by_a_link_never_moves_a_write_or_a_command() {
        let parent = dir();
        let cwd = parent.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let pinned = ws(&cwd);
        let outside = dir();
        // Judged inside, then the whole folder renamed and its path made a link out.
        assert!(inside(&pinned, "x.txt"));
        std::fs::rename(&cwd, parent.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &cwd).unwrap();
        let err = run(
            Tool::WriteFile,
            &json!({"file_path": "x.txt", "content": "x"}),
            &pinned,
            false,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("nothing was written"), "{err}");
        assert!(!outside.path().join("x.txt").exists());
        assert!(!inside(&pinned, "x.txt"), "it now leads outside: it asks");
        // A command still runs in the folder that was pinned.
        run(
            Tool::RunCommand,
            &json!({"command": "touch made"}),
            &pinned,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(parent.path().join("moved/made").exists());
        assert!(!outside.path().join("made").exists());
    }

    #[test]
    fn a_write_to_a_pipe_is_refused_at_once_and_truncates_nothing_first() {
        let d = dir();
        let fifo = d.path().join("pipe");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid path string; makes a FIFO in the test's own folder.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        let err = go(
            Tool::WriteFile,
            &json!({"file_path": "pipe", "content": "x"}),
            d.path(),
        )
        .unwrap_err();
        assert!(
            err.contains("Cannot write") || err.contains("not a regular file"),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "never waits for a reader"
        );
    }

    #[test]
    fn a_folder_swapped_for_a_link_after_the_check_never_redirects_a_write() {
        let d = dir();
        let cwd = d.path();
        let outside = dir();
        std::fs::create_dir(cwd.join("sub")).unwrap();
        // Judged inside (no approval needed), then swapped while the card was open.
        assert!(inside(&ws(cwd), "sub/x.txt"));
        std::fs::remove_dir(cwd.join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.path(), cwd.join("sub")).unwrap();
        let err = go(
            Tool::WriteFile,
            &json!({"file_path": "sub/x.txt", "content": "x"}),
            cwd,
        )
        .unwrap_err();
        assert!(err.contains("nothing was written"), "{err}");
        assert!(!outside.path().join("x.txt").exists());
        // The same for an edit.
        std::fs::write(outside.path().join("y.txt"), "old").unwrap();
        let err = go(
            Tool::EditFile,
            &json!({"file_path": "sub/y.txt", "old_string": "old", "new_string": "new"}),
            cwd,
        )
        .unwrap_err();
        assert!(err.contains("nothing was written"), "{err}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("y.txt")).unwrap(),
            "old"
        );
        // An approved write outside goes where it was shown to go.
        let target = outside.path().join("approved.txt");
        run(
            Tool::WriteFile,
            &json!({"file_path": target.to_str().unwrap(), "content": "ok"}),
            &ws(cwd),
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "ok");
    }

    #[test]
    fn a_pipe_never_blocks_a_read_or_a_search_and_a_stop_ends_a_search() {
        let d = dir();
        let fifo = d.path().join("pipe");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid path string; makes a FIFO in the test's own folder.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        std::fs::write(d.path().join("a.rs"), "needle\n").unwrap();
        let err = go(Tool::ReadFile, &json!({"file_path": "pipe"}), d.path()).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        let found = go(Tool::Search, &json!({"pattern": "needle"}), d.path()).unwrap();
        assert_eq!(found, "a.rs:1: needle");
        let stopped = run(
            Tool::Search,
            &json!({"pattern": "needle"}),
            &ws(d.path()),
            false,
            &AtomicBool::new(true),
        );
        assert_eq!(stopped.unwrap_err(), "Stopped by the user.");
    }

    #[test]
    fn a_descendant_that_leaves_the_group_never_holds_the_result() {
        let d = dir();
        let started = std::time::Instant::now();
        let out = go(
            Tool::RunCommand,
            &json!({"command": "setsid sleep 20 & echo done"}),
            d.path(),
        )
        .unwrap();
        assert!(out.contains("done"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_link_to_nothing_or_a_device_or_a_write_through_a_link_is_refused() {
        let d = dir();
        let cwd = d.path();
        let other = dir();
        // A dangling link to a file not made yet, in a folder outside: outside, so it asks.
        std::os::unix::fs::symlink(other.path().join("absent"), cwd.join("dangling")).unwrap();
        assert!(!inside(&ws(cwd), "dangling"));
        assert!(!inside(&ws(cwd), "dangling/below"));
        // Even approved, a write never follows a link at its last component.
        let err = go(
            Tool::WriteFile,
            &json!({"file_path": "dangling", "content": "x"}),
            cwd,
        )
        .unwrap_err();
        assert!(err.contains("nothing was written"), "{err}");
        assert!(!other.path().join("absent").exists());
        // A device is not read, whatever its size says, even approved.
        let err = run(
            Tool::ReadFile,
            &json!({"file_path": "/dev/zero"}),
            &ws(cwd),
            true,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        assert!(
            Workspace::open(Path::new("/no/such/folder")).is_err(),
            "an unresolvable folder"
        );
    }

    #[test]
    fn a_flooding_command_is_capped_and_a_stop_ends_its_whole_group() {
        let d = dir();
        let out = go(
            Tool::RunCommand,
            &json!({"command": "head -c 600000 /dev/zero | tr '\\0' a"}),
            d.path(),
        )
        .unwrap();
        assert!(out.len() < MAX_COMMAND_OUTPUT + 200, "{}", out.len());
        assert!(out.contains("more bytes of output not kept"));
        // A child left running in the background is ended with its shell.
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let cwd = d.path().to_path_buf();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            run(
                Tool::RunCommand,
                &json!({"command": "sleep 30 & sleep 30; touch late"}),
                &ws(&cwd),
                false,
                &stop,
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        cancel.store(true, Ordering::Relaxed);
        let result = worker.join().unwrap();
        assert_eq!(result.unwrap_err(), "Stopped by the user.");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(!d.path().join("late").exists());
    }

    #[test]
    fn paths_resolve_against_the_folder_and_links_out_are_outside() {
        let d = dir();
        let cwd = d.path();
        std::fs::create_dir(cwd.join("src")).unwrap();
        assert!(inside(&ws(cwd), "src/new.rs"), "a file not made yet");
        assert!(inside(&ws(cwd), "./a/../b.rs"));
        assert!(!inside(&ws(cwd), "../escape.rs"));
        assert!(!inside(&ws(cwd), "/etc/passwd"));
        let other = dir();
        std::os::unix::fs::symlink(other.path(), cwd.join("link")).unwrap();
        assert!(!inside(&ws(cwd), "link/x.rs"), "a link out of the folder");
        assert_eq!(
            absolute(Path::new("/w"), "a/./b/../c"),
            PathBuf::from("/w/a/c")
        );
    }

    #[test]
    fn read_numbers_lines_and_pages() {
        let d = dir();
        std::fs::write(d.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = go(Tool::ReadFile, &json!({"file_path": "a.txt"}), d.path()).unwrap();
        assert_eq!(out, "     1\tone\n     2\ttwo\n     3\tthree\n");
        let page = go(
            Tool::ReadFile,
            &json!({"file_path": "a.txt", "offset": 2, "limit": 1}),
            d.path(),
        )
        .unwrap();
        assert!(page.starts_with("     2\ttwo\n"));
        assert!(page.contains("1 more lines (read on with offset 3)"));
        assert!(go(Tool::ReadFile, &json!({"file_path": "nope"}), d.path()).is_err());
        assert!(go(Tool::ReadFile, &json!({}), d.path()).is_err());
    }

    #[test]
    fn paging_never_overflows_whatever_the_model_sends() {
        let d = dir();
        std::fs::write(d.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let read = |offset: u64, limit: u64| {
            go(
                Tool::ReadFile,
                &json!({"file_path": "a.txt", "offset": offset, "limit": limit}),
                d.path(),
            )
            .unwrap()
        };
        assert_eq!(read(u64::MAX, 1), "(empty)");
        assert_eq!(read(u64::MAX, u64::MAX), "(empty)");
        assert_eq!(read(4, 10), "(empty)", "just past the end");
        assert_eq!(read(3, u64::MAX), "     3\tthree\n");
        assert_eq!(
            read(1, 2),
            "     1\tone\n     2\ttwo\n… 1 more lines (read on with offset 3)\n"
        );
    }

    #[test]
    fn a_read_past_the_folder_or_of_a_sensitive_file_must_ask() {
        let d = dir();
        let cwd = d.path();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::write(cwd.join("src/a.rs"), "needle\n").unwrap();
        std::fs::write(cwd.join(".envrc"), "export KEY=needle\n").unwrap();
        let other = dir();
        std::os::unix::fs::symlink(other.path(), cwd.join("link")).unwrap();
        let home_elsewhere = dir();
        let pinned = Workspace::open_with_home(cwd, Some(home_elsewhere.path())).unwrap();
        let read = |p: &str| read_must_ask(&pinned, Tool::ReadFile, &json!({"file_path": p}));
        assert!(!read("src/a.rs"));
        assert!(!read("src/not-yet.rs"));
        assert!(read("/etc/hostname"), "outside");
        assert!(read("../x"), "outside by name");
        assert!(read("link/x"), "a link out");
        assert!(
            read(".git/config") && read(".envrc") && read("AGENTS.md"),
            "sensitive"
        );
        assert!(
            read_must_ask(&pinned, Tool::ReadFile, &json!({})),
            "no path"
        );
        assert!(!read_must_ask(
            &pinned,
            Tool::ListDir,
            &json!({"path": "."})
        ));
        assert!(read_must_ask(&pinned, Tool::ListDir, &json!({"path": "/"})));
        assert!(!read_must_ask(
            &pinned,
            Tool::Search,
            &json!({"pattern": "x"})
        ));

        // Unapproved, a read that must ask is refused even if the gate was passed somehow;
        // approved, it runs.
        let unapproved = |input: &Value| {
            run(
                Tool::ReadFile,
                input,
                &pinned,
                false,
                &AtomicBool::new(false),
            )
        };
        let outside = other.path().join("o.txt");
        std::fs::write(&outside, "secret\n").unwrap();
        let input = json!({"file_path": outside.to_str().unwrap()});
        assert!(unapproved(&input).unwrap_err().contains("nothing was read"));
        let approved = run(
            Tool::ReadFile,
            &input,
            &pinned,
            true,
            &AtomicBool::new(false),
        );
        assert!(approved.unwrap().contains("secret"));

        // A search skips sensitive files, approved or not: a read by name is the way to one.
        let search = |approved: bool| {
            run(
                Tool::Search,
                &json!({"pattern": "needle"}),
                &pinned,
                approved,
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        assert_eq!(search(false), format!("src/a.rs:1: needle{}", withheld(1)));
        assert_eq!(search(true), format!("src/a.rs:1: needle{}", withheld(1)));
    }

    #[test]
    fn links_are_judged_where_they_lead_and_a_search_never_follows_one() {
        let d = dir();
        let cwd = d.path();
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join(".git/config"), "needle\n").unwrap();
        std::fs::write(cwd.join("src/a.rs"), "needle\n").unwrap();
        let other = dir();
        std::fs::write(other.path().join("far.rs"), "needle\n").unwrap();
        let link = |to: &Path, name: &str| std::os::unix::fs::symlink(to, cwd.join(name)).unwrap();
        link(&cwd.join(".git/config"), "notes");
        link(&cwd.join("src/a.rs"), ".envrc");
        link(other.path(), "out");
        let home_elsewhere = dir();
        let pinned = Workspace::open_with_home(cwd, Some(home_elsewhere.path())).unwrap();
        let read = |p: &str| read_must_ask(&pinned, Tool::ReadFile, &json!({"file_path": p}));
        assert!(read("notes"), "a link to a sensitive file inside asks");
        assert!(
            !read(".envrc"),
            "a link named like one, to an ordinary file, does not"
        );
        let list = |p: &str| read_must_ask(&pinned, Tool::ListDir, &json!({"path": p}));
        assert!(list(".git") && list("out"));
        assert!(read_must_ask(
            &pinned,
            Tool::Search,
            &json!({"pattern": "needle", "path": "out"})
        ));
        let found = run(
            Tool::Search,
            &json!({"pattern": "needle"}),
            &pinned,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            found,
            format!("src/a.rs:1: needle{}", withheld(1)),
            "no link is followed"
        );
    }

    #[test]
    fn a_hard_linked_file_asks_and_an_unapproved_read_of_one_is_refused() {
        let d = dir();
        let other = dir();
        let secret = other.path().join("credentials");
        std::fs::write(&secret, "private\n").unwrap();
        // Same filesystem: both are temp folders.
        if std::fs::hard_link(&secret, d.path().join("innocent.txt")).is_err() {
            return;
        }
        let pinned = Workspace::open_with_home(d.path(), None).unwrap();
        let input = json!({"file_path": "innocent.txt"});
        assert!(read_must_ask(&pinned, Tool::ReadFile, &input));
        let err = run(
            Tool::ReadFile,
            &input,
            &pinned,
            false,
            &AtomicBool::new(false),
        );
        assert!(err.unwrap_err().contains("nothing was read"));
        let searched = run(
            Tool::Search,
            &json!({"pattern": "private"}),
            &pinned,
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(searched.starts_with("No line contains"), "{searched}");
        let approved = run(
            Tool::ReadFile,
            &input,
            &pinned,
            true,
            &AtomicBool::new(false),
        );
        assert!(approved.unwrap().contains("private"));
    }

    #[test]
    fn a_folder_swapped_for_a_link_after_the_check_never_redirects_a_read() {
        let d = dir();
        let cwd = d.path();
        std::fs::create_dir(cwd.join("sub")).unwrap();
        std::fs::write(cwd.join("sub/a.txt"), "inside\n").unwrap();
        let other = dir();
        std::fs::write(other.path().join("a.txt"), "outside\n").unwrap();
        let pinned = Workspace::open_with_home(cwd, None).unwrap();
        let target = resolved(&cwd.join("sub/a.txt")).unwrap();
        // Judged inside, then the folder on the way becomes a link out.
        std::fs::rename(cwd.join("sub"), cwd.join("moved")).unwrap();
        std::os::unix::fs::symlink(other.path(), cwd.join("sub")).unwrap();
        let opened = open_inside(&pinned, &target, libc::O_RDONLY).expect("inside");
        assert!(opened.is_err(), "the link is never followed");
    }

    #[test]
    fn an_approved_search_rooted_in_a_secret_folder_outside_reads_nothing_of_it() {
        let home = dir();
        std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
        std::fs::write(home.path().join(".ssh/id_ed25519"), "PRIVATE KEY\n").unwrap();
        let repo = dir();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::write(
            repo.path().join(".git/config"),
            "url = https://PRIVATE KEY@example.com\n",
        )
        .unwrap();
        let work = dir();
        let pinned = Workspace::open_with_home(work.path(), Some(home.path())).unwrap();
        for root in [home.path().join(".ssh"), repo.path().join(".git")] {
            let found = run(
                Tool::Search,
                &json!({"pattern": "PRIVATE KEY", "path": root.to_str().unwrap()}),
                &pinned,
                true,
                &AtomicBool::new(false),
            )
            .unwrap();
            // It says what it withheld, so "no line" is never taken for "not there".
            assert!(
                found.starts_with("No line contains") && found.ends_with(&withheld(1)),
                "{root:?}: {found}"
            );
        }
    }

    #[test]
    fn a_search_wider_than_the_open_file_limit_misses_nothing() {
        let d = dir();
        // More sibling folders than a default soft limit of 1024 open files.
        for i in 0..2500 {
            std::fs::create_dir(d.path().join(format!("f{i:04}"))).unwrap();
        }
        std::fs::write(d.path().join("f2499/last.txt"), "needle\n").unwrap();
        let found = go(Tool::Search, &json!({"pattern": "needle"}), d.path()).unwrap();
        assert_eq!(found, "f2499/last.txt:1: needle");
    }

    #[test]
    fn the_folder_lists_the_same_every_time_and_through_a_link_inside() {
        let d = dir();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/a.txt"), "").unwrap();
        std::os::unix::fs::symlink(d.path().join("sub"), d.path().join("alias")).unwrap();
        let pinned = ws(d.path());
        let list = |p: &str| {
            run(
                Tool::ListDir,
                &json!({"path": p}),
                &pinned,
                false,
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        // The pinned folder's own handle is listed each time: never from where it last stopped.
        assert_eq!(list("."), "alias\nsub/");
        assert_eq!(list("."), "alias\nsub/");
        assert_eq!(list("alias"), "a.txt");
    }

    #[test]
    fn with_no_home_known_every_dot_path_and_every_search_asks() {
        let d = dir();
        std::fs::write(d.path().join("plain.txt"), "x").unwrap();
        let blind = Workspace::open_with_homes(d.path(), []).unwrap();
        let read = |p: &str| read_must_ask(&blind, Tool::ReadFile, &json!({"file_path": p}));
        assert!(!read("plain.txt"));
        assert!(read(".bashrc") && read(".config/app/token"));
        assert!(read_must_ask(
            &blind,
            Tool::Search,
            &json!({"pattern": "x"})
        ));
        // A home that does not resolve is not known either.
        let gone = Workspace::open_with_home(d.path(), Some(Path::new("/no/such/home"))).unwrap();
        assert!(read_must_ask(
            &gone,
            Tool::ReadFile,
            &json!({"file_path": ".ssh/id"})
        ));
        // The account's own home is found whatever `$HOME` says.
        assert!(account_home().is_some());
    }

    #[test]
    fn a_folder_that_holds_home_asks_for_its_dotfiles_and_a_search_of_it() {
        let home = dir();
        std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
        std::fs::create_dir_all(home.path().join("projects/app")).unwrap();
        // The thread's folder is home itself.
        let at_home = Workspace::open_with_home(home.path(), Some(home.path())).unwrap();
        let read =
            |w: &Workspace, p: &str| read_must_ask(w, Tool::ReadFile, &json!({"file_path": p}));
        assert!(read(&at_home, ".ssh/id_ed25519"));
        assert!(read(&at_home, ".bashrc"));
        assert!(!read(&at_home, "projects/app/main.rs"));
        let search = |w: &Workspace, p: &str| {
            read_must_ask(w, Tool::Search, &json!({"pattern": "x", "path": p}))
        };
        assert!(search(&at_home, "."), "a search of all of home");
        assert!(!search(&at_home, "projects"));
        assert!(read_must_ask(
            &at_home,
            Tool::ListDir,
            &json!({"path": ".ssh"})
        ));
        // Approved, a search of all of home still never reads its dotfiles.
        std::fs::write(home.path().join(".ssh/id_ed25519"), "PRIVATE KEY\n").unwrap();
        std::fs::write(home.path().join("projects/app/notes.md"), "PRIVATE KEY\n").unwrap();
        let found = run(
            Tool::Search,
            &json!({"pattern": "PRIVATE KEY"}),
            &at_home,
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            found,
            format!("projects/app/notes.md:1: PRIVATE KEY{}", withheld(1))
        );
        // The same from a folder outside it, approved to search home.
        let elsewhere = dir();
        let outside = Workspace::open_with_home(elsewhere.path(), Some(home.path())).unwrap();
        let found = run(
            Tool::Search,
            &json!({"pattern": "PRIVATE KEY", "path": home.path().to_str().unwrap()}),
            &outside,
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            found,
            format!("projects/app/notes.md:1: PRIVATE KEY{}", withheld(1))
        );
        // A project below home is not exposed to home's dotfiles.
        let below = home.path().join("projects/app");
        let project = Workspace::open_with_home(&below, Some(home.path())).unwrap();
        std::fs::write(below.join(".prettierrc"), "{}").unwrap();
        assert!(!read(&project, ".prettierrc"));
        assert!(!search(&project, "."));
    }

    #[test]
    fn a_finished_command_ends_its_group_and_keeps_its_status() {
        let d = dir();
        let out = go(
            Tool::RunCommand,
            &json!({"command": "sleep 30 & echo $! > bg; echo hi; exit 4"}),
            d.path(),
        )
        .unwrap();
        assert!(out.contains("hi") && out.contains("(exit code 4)"), "{out}");
        let pid: libc::pid_t = std::fs::read_to_string(d.path().join("bg"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The job it left behind was ended with its group (only a zombie may remain). The kill
        // is delivered asynchronously, so give it a moment.
        let alive = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap_or_default()
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                .is_some_and(|s| s != "Z" && s != "X")
        };
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while alive() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the background job outlived its command");
    }

    #[test]
    fn edits_must_match_once_unless_all_are_asked_for() {
        let d = dir();
        let file = d.path().join("f.rs");
        std::fs::write(&file, "a b a").unwrap();
        let edit = |old: &str, new: &str, all: bool| {
            go(
                Tool::EditFile,
                &json!({"file_path": "f.rs", "old_string": old, "new_string": new, "replace_all": all}),
                d.path(),
            )
        };
        assert!(edit("a", "x", false).unwrap_err().contains("2 times"));
        assert!(edit("zz", "x", false).is_err());
        assert!(edit("", "x", false).is_err());
        edit("b", "c", false).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a c a");
        edit("a", "x", true).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x c x");
    }

    #[test]
    fn an_edit_of_a_file_that_is_not_utf8_changes_nothing() {
        let d = dir();
        let file = d.path().join("latin1.txt");
        let bytes = b"caf\xe9 old\n".to_vec();
        std::fs::write(&file, &bytes).unwrap();
        let err = go(
            Tool::EditFile,
            &json!({"file_path": "latin1.txt", "old_string": "old", "new_string": "new"}),
            d.path(),
        )
        .unwrap_err();
        assert!(err.contains("not UTF-8"), "{err}");
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    }

    #[test]
    fn writes_create_folders_and_lists_mark_them() {
        let d = dir();
        go(
            Tool::WriteFile,
            &json!({"file_path": "new/deep/f.txt", "content": "hi"}),
            d.path(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("new/deep/f.txt")).unwrap(),
            "hi"
        );
        assert_eq!(
            go(Tool::ListDir, &json!({"path": "."}), d.path()).unwrap(),
            "new/"
        );
    }

    #[test]
    fn search_finds_lines_and_skips_generated_trees_and_binaries() {
        let d = dir();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "fn needle() {}\nother\n").unwrap();
        std::fs::write(d.path().join("target/b.rs"), "needle\n").unwrap();
        std::fs::write(d.path().join("bin"), b"needle\0").unwrap();
        let out = go(Tool::Search, &json!({"pattern": "needle"}), d.path()).unwrap();
        assert_eq!(out, "src/a.rs:1: fn needle() {}");
        let none = go(Tool::Search, &json!({"pattern": "absent"}), d.path()).unwrap();
        assert!(none.starts_with("No line contains"));
    }

    #[test]
    fn a_file_too_large_to_search_is_counted_never_skipped_silently() {
        let d = dir();
        std::fs::write(d.path().join("a.rs"), "needle\n").unwrap();
        let mut big = "needle\n".to_owned();
        big.push_str(&"x".repeat(MAX_SEARCH_FILE as usize));
        std::fs::write(d.path().join("big.log"), big).unwrap();
        let out = go(Tool::Search, &json!({"pattern": "needle"}), d.path()).unwrap();
        assert!(out.starts_with("a.rs:1: needle"), "{out}");
        assert!(out.contains("1 folders or files could not be"), "{out}");
    }

    #[test]
    fn commands_run_in_the_folder_and_report_failure() {
        let d = dir();
        std::fs::write(d.path().join("marker"), "").unwrap();
        let out = go(Tool::RunCommand, &json!({"command": "ls"}), d.path()).unwrap();
        assert!(out.contains("marker"));
        let failed = go(
            Tool::RunCommand,
            &json!({"command": "echo oops >&2; exit 3"}),
            d.path(),
        )
        .unwrap();
        assert!(failed.contains("oops") && failed.contains("(exit code 3)"));
    }
}
