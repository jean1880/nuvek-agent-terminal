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
}

impl Workspace {
    pub fn open(cwd: &Path) -> Result<Self, String> {
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
        Ok(Self {
            cwd: cwd.to_path_buf(),
            root,
            folder,
        })
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

fn text<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is required"))
}

/// Runs `tool` with `input` in the thread's folder: what the model is told, or why it failed.
/// `may_leave` says a write outside the folder was approved (or every request is: Bypass);
/// without it a write that now leads outside is refused. `cancel` stops a running command (its
/// whole process group) or a search, when it is set.
pub fn run(
    tool: Tool,
    input: &Value,
    ws: &Workspace,
    may_leave: bool,
    cancel: &AtomicBool,
) -> Result<String, String> {
    match tool {
        Tool::ReadFile => read_file(input, &ws.cwd),
        Tool::ListDir => list_dir(input, &ws.cwd),
        Tool::Search => search(input, &ws.cwd, cancel),
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
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A regular file's text at `path` (links followed: a read goes where the path leads).
fn read_text(path: &Path, max: u64) -> Result<String, String> {
    let fd = open_at(
        None,
        &cstring(path.as_os_str())?,
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    )
    .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    read_open(fd, path, max)
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

fn read_file(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = absolute(cwd, text(input, "file_path")?);
    let content = read_text(&path, MAX_READ_BYTES)?;
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .map_or(1, |o| o.max(1) as usize);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_READ_LINES, |l| l.max(1) as usize);
    let total = content.lines().count();
    let mut out: String = content
        .lines()
        .enumerate()
        .skip(offset - 1)
        .take(limit)
        .map(|(i, line)| format!("{:>6}\t{line}\n", i + 1))
        .collect();
    if offset - 1 + limit < total {
        out.push_str(&format!(
            "… {} more lines (read on with offset {})\n",
            total - (offset - 1 + limit),
            offset + limit
        ));
    }
    if out.is_empty() {
        out = "(empty)".to_owned();
    }
    Ok(out)
}

fn list_dir(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = absolute(
        cwd,
        input.get("path").and_then(Value::as_str).unwrap_or("."),
    );
    let entries =
        std::fs::read_dir(&path).map_err(|e| format!("Cannot list {}: {e}", path.display()))?;
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            match e.file_type() {
                Ok(t) if t.is_dir() => format!("{name}/"),
                _ => name,
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

fn search(input: &Value, cwd: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let pattern = text(input, "pattern")?;
    if pattern.is_empty() {
        return Err("pattern is empty".to_owned());
    }
    let root = absolute(
        cwd,
        input.get("path").and_then(Value::as_str).unwrap_or("."),
    );
    let mut matches = Vec::new();
    let mut stack = vec![root.clone()];
    let mut more = false;
    'walk: while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return Err("Stopped by the user.".to_owned());
            }
            // Not followed: a link, a pipe or a device is never read.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                let name = entry.file_name();
                if !SKIPPED_DIRS.iter().any(|s| name == *s) {
                    stack.push(path);
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            // Opened without following a link or blocking, read only if still a regular file.
            let Ok(fd) = cstring(path.as_os_str()).and_then(|p| {
                open_at(
                    None,
                    &p,
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW,
                    0,
                )
                .map_err(|e| e.to_string())
            }) else {
                continue;
            };
            let Ok(content) = read_open(fd, &path, MAX_SEARCH_FILE) else {
                continue;
            };
            if content.bytes().take(8192).any(|b| b == 0) {
                continue; // binary
            }
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
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
    }
    if matches.is_empty() {
        return Ok(format!("No line contains {pattern:?}."));
    }
    let mut out = matches.join("\n");
    if more {
        out.push_str(&format!(
            "\n… more than {MAX_MATCHES} matches; narrow the search"
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
    let content = read_open(fd, &path, MAX_READ_BYTES)?;
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
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if cancel.load(Ordering::Relaxed) => {
                kill_group();
                let _ = child.wait();
                return Err("Stopped by the user.".to_owned());
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_group();
                let _ = child.wait();
                return Err(format!(
                    "The command did not finish within {secs}s and was ended."
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                kill_group();
                return Err(format!("Could not wait for the command: {e}"));
            }
        }
    };
    // A background job it left in its group would keep the drains reading: end the group, then
    // take what the pipes gave within a short grace.
    kill_group();
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
        // A device is not read, whatever its size says.
        let err = go(Tool::ReadFile, &json!({"file_path": "/dev/zero"}), cwd).unwrap_err();
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
