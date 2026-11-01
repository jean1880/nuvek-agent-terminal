//! Internal process owner. The lifetime descriptor is private and never inherited by a server.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const GUARDIAN_FLAG: &str = "--local-mcp-guardian";
pub(super) const LIFETIME_FD: i32 = 3;
const MAX_SETUP: usize = 2 * 1024 * 1024;

// Intentionally no Debug: the exact environment can contain credentials.
#[derive(Serialize, Deserialize)]
pub(super) struct Launch {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
}

/// Must run before GTK or logging. stdin starts with one bounded private setup frame,
/// followed by the server's protocol stream; no buffered read may consume protocol bytes.
pub fn main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(_) => {
            eprintln!("MCP guardian could not start or supervise its server.");
            1
        }
    }
}

fn run() -> io::Result<()> {
    let mut setup = Vec::new();
    // std::io::Stdin has an internal buffer even for one-byte reads. Use the raw
    // descriptor so no MCP bytes can be read ahead and stranded in this process.
    let mut input = unsafe { File::from_raw_fd(libc::STDIN_FILENO) };
    loop {
        let mut byte = [0];
        input.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        if setup.len() >= MAX_SETUP {
            return Err(io::ErrorKind::InvalidData.into());
        }
        setup.push(byte[0]);
    }
    let launch: Launch =
        serde_json::from_slice(&setup).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    // SAFETY: the runner reserves descriptor 3 exclusively for this role.
    if unsafe { libc::fcntl(LIFETIME_FD, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let lifetime = unsafe { File::from_raw_fd(LIFETIME_FD) };
    let mut command = Command::new(&launch.command);
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .env_clear()
        .envs(&launch.env)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);
    // SAFETY: close is async-signal-safe; the child must not retain its own lifetime pipe.
    unsafe {
        command.pre_exec(|| {
            libc::close(LIFETIME_FD);
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let group = child.id() as i32;
    // The server owns the only read end of its stdin now. Closing the runner's writer
    // therefore delivers EOF even while this supervisor is alive.
    drop(input);
    // The leader stays unreaped until its group is killed: its pid, and so the group's id,
    // stays taken, so neither signal can reach a group that reused the id.
    let mut reapable = true;
    loop {
        let mut descriptor = libc::pollfd {
            fd: lifetime.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut descriptor, 1, 20) };
        if polled > 0 && descriptor.revents != 0 {
            break;
        }
        match exited(group) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Reaped by someone else: its id may be another process's group already.
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                reapable = false;
                break;
            }
            Err(_) => break,
        }
        if polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
    if !reapable {
        return Ok(());
    }
    // Group cleanup is unconditional: even a clean leader exit can leave descendants
    // holding stdout/stderr open. Deliberately escaped groups are outside this ownership.
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        libc::kill(-group, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until {
        if !matches!(child.try_wait(), Ok(None)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Whether the child `pid` has exited, leaving it unreaped (`WNOWAIT`).
fn exited(pid: i32) -> io::Result<bool> {
    // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `pid` is this process's child; WNOHANG never blocks, WNOWAIT reaps nothing.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid filled `info`; si_pid is 0 when the child has not exited.
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exited_leader_is_seen_without_being_reaped() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let until = Instant::now() + Duration::from_secs(5);
        while !exited(pid).unwrap() {
            assert!(Instant::now() < until, "the child never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Still unreaped: its pid (and group id) stays taken until this wait.
        assert!(exited(pid).unwrap());
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(exited(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }
}
