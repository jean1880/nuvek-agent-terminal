//! The Local agent's tools (`agent_core::local_agent::Tool`), run by its runner in the thread's
//! folder. Blocking; each is bounded (file sizes, result counts, a command's time).
//!
//! What may run without asking is decided before a tool runs (`agent_core::local_agent::gate`),
//! and [`inside`] tells it whether a write stays in the folder: the path is resolved through any
//! symbolic link, so a link pointing out of the folder counts as outside, and a link that cannot
//! be resolved (a dangling one) does too. A write never follows a link at its last component.
//!
//! Residual risk (not closed): a folder in the path swapped for a link between that check and
//! the write is not seen. The check and the write are a few calls apart in one process.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Whether `path` (relative to `cwd` or absolute) is inside `cwd`, links followed. Anything that
/// cannot be resolved is outside: it asks.
pub fn inside(cwd: &Path, path: &str) -> bool {
    let Ok(root) = cwd.canonicalize() else {
        return false;
    };
    resolved(&absolute(cwd, path)).is_some_and(|p| p.starts_with(root))
}

fn text<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is required"))
}

/// Runs `tool` with `input` in `cwd`: what the model is told, or why it failed. `cancel` stops a
/// running command (its whole process group) when it is set.
pub fn run(tool: Tool, input: &Value, cwd: &Path, cancel: &AtomicBool) -> Result<String, String> {
    match tool {
        Tool::ReadFile => read_file(input, cwd),
        Tool::ListDir => list_dir(input, cwd),
        Tool::Search => search(input, cwd),
        Tool::WriteFile => write_file(input, cwd),
        Tool::EditFile => edit_file(input, cwd),
        Tool::RunCommand => run_command(input, cwd, cancel),
    }
}

/// A regular file's text, at most [`MAX_READ_BYTES`]: a device, a pipe or a folder is refused, and
/// the read itself is bounded, whatever the file's size said.
fn read_text(path: &Path) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder; use list_dir", path.display()));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(format!(
            "{} is {} MB, too large to read whole",
            path.display(),
            meta.len() / (1024 * 1024)
        ));
    }
    let file =
        std::fs::File::open(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(format!("{} grew too large to read whole", path.display()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Writes `content` to `path`, never through a link at its last component (`O_NOFOLLOW`).
fn write_no_follow(path: &Path, content: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    file.write_all(content.as_bytes())
        .map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

fn read_file(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = absolute(cwd, text(input, "file_path")?);
    let content = read_text(&path)?;
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

fn search(input: &Value, cwd: &Path) -> Result<String, String> {
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
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                let name = entry.file_name();
                if !SKIPPED_DIRS.iter().any(|s| name == *s) {
                    stack.push(path);
                }
                continue;
            }
            if entry.metadata().map_or(true, |m| m.len() > MAX_SEARCH_FILE) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes.iter().take(8192).any(|b| *b == 0) {
                continue; // binary
            }
            let content = String::from_utf8_lossy(&bytes);
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

fn write_file(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = absolute(cwd, text(input, "file_path")?);
    let content = text(input, "content")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }
    write_no_follow(&path, content)?;
    Ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        path.display()
    ))
}

fn edit_file(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = absolute(cwd, text(input, "file_path")?);
    let old = text(input, "old_string")?;
    let new = text(input, "new_string")?;
    if old.is_empty() {
        return Err("old_string is empty; use write_file to create a file".to_owned());
    }
    if old == new {
        return Err("old_string and new_string are the same".to_owned());
    }
    let content = read_text(&path)?;
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
    write_no_follow(&path, &edited)?;
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
fn run_command(input: &Value, cwd: &Path, cancel: &AtomicBool) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let command = text(input, "command")?;
    if command.trim().is_empty() {
        return Err("command is empty".to_owned());
    }
    let secs = input
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_COMMAND_SECS, |s| s.clamp(1, MAX_COMMAND_SECS));
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("Could not run the command: {e}"))?;
    let group = child.id() as libc::pid_t;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut dropped = 0usize;
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let room = MAX_COMMAND_OUTPUT.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                    dropped += n.saturating_sub(room);
                }
            }
            (kept, dropped)
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
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
    // A background job it left holding the pipes would keep the drains reading: end the group.
    kill_group();
    let (out, out_dropped) = stdout.join().unwrap_or_default();
    let (err, err_dropped) = stderr.join().unwrap_or_default();
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

    /// [`run`] with no Stop.
    fn go(tool: Tool, input: &Value, cwd: &Path) -> Result<String, String> {
        run(tool, input, cwd, &AtomicBool::new(false))
    }

    #[test]
    fn a_link_to_nothing_or_a_device_or_a_write_through_a_link_is_refused() {
        let d = dir();
        let cwd = d.path();
        let other = dir();
        // A dangling link to a file not made yet, in a folder outside: outside, so it asks.
        std::os::unix::fs::symlink(other.path().join("absent"), cwd.join("dangling")).unwrap();
        assert!(!inside(cwd, "dangling"));
        assert!(!inside(cwd, "dangling/below"));
        // Even approved, a write never follows a link at its last component.
        let err = go(
            Tool::WriteFile,
            &json!({"file_path": "dangling", "content": "x"}),
            cwd,
        )
        .unwrap_err();
        assert!(err.starts_with("Cannot write"), "{err}");
        assert!(!other.path().join("absent").exists());
        // A device is not read, whatever its size says.
        let err = go(Tool::ReadFile, &json!({"file_path": "/dev/zero"}), cwd).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        assert!(
            !inside(Path::new("/no/such/folder"), "x"),
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
                &cwd,
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
        assert!(inside(cwd, "src/new.rs"), "a file not made yet");
        assert!(inside(cwd, "./a/../b.rs"));
        assert!(!inside(cwd, "../escape.rs"));
        assert!(!inside(cwd, "/etc/passwd"));
        let other = dir();
        std::os::unix::fs::symlink(other.path(), cwd.join("link")).unwrap();
        assert!(!inside(cwd, "link/x.rs"), "a link out of the folder");
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
