//! Runs a Local server's headroom probe ([`agent_core::headroom_probe`]): the configured command,
//! exec'd directly (never through a shell), with a minimal environment, no input and a time
//! limit, and its output read as one number.
//!
//! The probe runs in a process group of its own. What it prints is read as it comes, at most
//! [`MAX_OUTPUT_BYTES`]: past that the group is killed and there is no reading. When the time
//! limit passes, or once the probe has exited, the whole group is killed before the probe is
//! reaped, so nothing it started outlives it and the group's id is never one reused by another
//! process. Its standard error is discarded.
//!
//! Blocking: call it off the main thread.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use agent_core::headroom_probe::{reading, HeadroomProbe, MAX_OUTPUT_BYTES};

/// The variables a probe inherits from the app; nothing else of the app's environment (no
/// credential the app holds, no agent socket such as `SSH_AUTH_SOCK`) reaches it.
const PASSED: [&str; 6] = [
    "HOME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "USER",
];
/// How long a probe that exited may keep its output open (a child it left behind) before the
/// rest of its group is killed.
const EXIT_GRACE: Duration = Duration::from_millis(200);

/// The probe's reading, or `None` when there is none: a probe that is not valid, cannot start,
/// runs past its time limit, prints more than [`MAX_OUTPUT_BYTES`], exits unsuccessfully or
/// prints no number. `path` is the `PATH` it runs with (the user's shell's, when known), else
/// the app's own.
pub fn run(probe: &HeadroomProbe, path: Option<&str>) -> Option<f64> {
    probe.validate().ok()?;
    let (program, args) = probe.argv.split_first()?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    match path {
        Some(path) => {
            cmd.env("PATH", path);
        }
        None => {
            if let Some(path) = std::env::var_os("PATH") {
                cmd.env("PATH", path);
            }
        }
    }
    for name in PASSED {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(probe.timeout_s);
    let output = read_bounded(&mut child, deadline);
    // The group goes before its leader is reaped: until then the leader's pid, and so the
    // group's id, stays taken, so the kill can never reach another process's group.
    kill_group(&child);
    let status = child.wait().ok()?;
    let output = output?;
    status.success().then(|| reading(&output)).flatten()
}

/// The probe's output, read until it closes and the probe exited, or `None` once it passed
/// [`MAX_OUTPUT_BYTES`], the deadline came, or waiting failed. Leaves the probe unreaped.
fn read_bounded(child: &mut Child, deadline: Instant) -> Option<Vec<u8>> {
    let mut stdout = child.stdout.take()?;
    let fd = stdout.as_raw_fd();
    let mut output = Vec::new();
    let mut open = true;
    let mut exited_at: Option<Instant> = None;
    let mut chunk = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        if exited_at.is_none() && exited(child)? {
            exited_at = Some(now);
        }
        match (open, exited_at) {
            (false, Some(_)) => return Some(output),
            (true, Some(at)) if now.duration_since(at) >= EXIT_GRACE => return Some(output),
            _ => {}
        }
        if !open {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let mut ready = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a valid pollfd; `fd` stays open while `stdout` (owned here) lives.
        let polled = unsafe { libc::poll(&mut ready, 1, 50) };
        if polled < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if polled == 0 {
            continue;
        }
        match stdout.read(&mut chunk) {
            Ok(0) => open = false,
            Ok(n) => {
                if output.len() + n > MAX_OUTPUT_BYTES {
                    return None;
                }
                output.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// Whether the probe has exited, leaving it unreaped (`WNOWAIT`).
fn exited(child: &Child) -> Option<bool> {
    // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: the pid is this child's; WNOHANG never blocks, WNOWAIT reaps nothing.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r != 0 {
        return (std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted)
            .then_some(false);
    }
    // SAFETY: waitid filled `info`; si_pid is 0 when the child has not exited.
    Some(unsafe { info.si_pid() } != 0)
}

/// Kills the probe's whole group (it leads it: `process_group(0)`).
fn kill_group(child: &Child) {
    // SAFETY: killpg with the group this unreaped child leads; at worst it names no process.
    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(argv: &[&str], timeout_s: u64) -> HeadroomProbe {
        HeadroomProbe {
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            low_below: 100.0,
            timeout_s,
        }
    }

    #[test]
    fn a_probe_that_prints_a_number_is_read() {
        assert_eq!(run(&probe(&["printf", "8123\\n"], 5), None), Some(8123.0));
    }

    #[test]
    fn its_arguments_reach_it_unparsed_by_any_shell() {
        // `$HOME` and `;` are plain text to printf: no shell expands or splits them.
        assert_eq!(run(&probe(&["printf", "%s", "1;$HOME"], 5), None), None);
        assert_eq!(run(&probe(&["printf", "%s", "42"], 5), None), Some(42.0));
    }

    #[test]
    fn no_reading_when_it_fails_is_missing_prints_text_or_runs_too_long() {
        assert_eq!(run(&probe(&["false"], 5), None), None);
        assert_eq!(run(&probe(&["sh", "-c", "echo 5; exit 3"], 5), None), None);
        assert_eq!(
            run(&probe(&["agent-terminal-no-such-probe-command"], 5), None),
            None
        );
        assert_eq!(run(&probe(&["echo", "busy"], 5), None), None);
        let started = Instant::now();
        assert_eq!(run(&probe(&["sleep", "10"], 1), None), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            run(&probe(&[], 5), None),
            None,
            "an invalid probe never runs"
        );
    }

    /// A probe that floods its output is stopped at the cap: no reading, and it is killed long
    /// before its time limit.
    #[test]
    fn a_flooding_probe_is_cut_off_at_the_cap() {
        let started = Instant::now();
        assert_eq!(run(&probe(&["yes", "1"], 20), None), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed at the cap"
        );
        // Just under the cap is still read.
        let fits = format!("printf '7\\n%0{}d' 0", MAX_OUTPUT_BYTES - 3);
        assert_eq!(run(&probe(&["sh", "-c", &fits], 5), None), Some(7.0));
    }

    /// A probe's timeout ends its whole group: a child it started (here one that would sleep
    /// a minute, holding the output open) does not survive it.
    #[test]
    fn a_timed_out_probe_leaves_no_child_behind() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild");
        let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let started = Instant::now();
        assert_eq!(run(&probe(&["sh", "-c", &script], 1), None), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .expect("the wrapper wrote its child's pid")
            .trim()
            .parse()
            .expect("a pid");
        // Killed, then reaped by init: give it a moment to go.
        let gone = (0..50).any(|_| {
            // SAFETY: signal 0 only checks whether the process exists.
            let alive = unsafe { libc::kill(pid, 0) } == 0
                && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"));
            if alive {
                std::thread::sleep(Duration::from_millis(20));
            }
            !alive
        });
        assert!(gone, "the probe's child {pid} outlived its timeout");
    }

    /// Exiting promptly is not waiting on a child left holding the output: the group goes.
    #[test]
    fn a_probe_that_exits_is_not_held_open_by_its_child() {
        let started = Instant::now();
        let script = "echo 9; sleep 60 &";
        assert_eq!(run(&probe(&["sh", "-c", script], 10), None), Some(9.0));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn it_never_sees_the_apps_other_variables() {
        // The probe's whole environment: PATH, the passed names, and what `sh` adds itself
        // (`PWD`, `SHLVL`, `_` at most).
        let count = run(&probe(&["sh", "-c", "env | wc -l"], 5), None).expect("a count");
        assert!(count <= (PASSED.len() + 1 + 3) as f64, "{count} variables");
    }
}
