//! Running a child process with a wall-clock timeout.

use tracing::debug;

/// `run_capture` (in the app's `utils.rs`) for a command already built — for callers that also set
/// its environment. `name` is how errors refer to it.
///
/// Blocking: call it off the main thread.
pub fn run_command(
    cmd: std::process::Command,
    name: &str,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    run(cmd, name, timeout_secs, false)
}

/// [`run_command`] for a command whose own children must not outlive a timeout (the hooks and
/// signing helpers `git commit` starts): it runs in a process group of its own, and on timeout
/// the whole group is killed before the command is reaped, so the group id cannot yet belong to
/// anything else.
pub fn run_command_in_group(
    cmd: std::process::Command,
    name: &str,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    run(cmd, name, timeout_secs, true)
}

/// [`run_command_in_group`] keeping at most `cap` bytes of each of stdout and stderr: what comes
/// after is read and dropped, so a command that prints without end costs its timeout, never
/// memory. For output another machine controls.
pub fn run_command_in_group_capped(
    cmd: std::process::Command,
    name: &str,
    timeout_secs: u64,
    cap: usize,
) -> Result<std::process::Output, String> {
    run_capped(cmd, name, timeout_secs, true, cap)
}

fn run(
    cmd: std::process::Command,
    name: &str,
    timeout_secs: u64,
    group: bool,
) -> Result<std::process::Output, String> {
    run_capped(cmd, name, timeout_secs, group, usize::MAX)
}

fn run_capped(
    mut cmd: std::process::Command,
    name: &str,
    timeout_secs: u64,
    group: bool,
    cap: usize,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    let command = name;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if group {
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Could not run {command}: {err}"))?;

    // Each pipe is drained on its own thread into a shared buffer, and the
    // thread says when it reached end-of-file. Waiting on that signal with a
    // deadline — never joining the thread — is what keeps the timeout honest:
    // a background job the command started (an rc's `foo &`) inherits the
    // pipes and can hold them open long after the command itself exits.
    // Ceiling: such a job is not killed, only no longer waited for.
    //
    // Once this function returns (by any path) `StopOnDrop` raises `stop`, and
    // a drain that wakes up for more output drops its pipe and exits instead
    // of buffering a surviving job's output without bound. The drain polls its
    // pipe in 100 ms slices, so it sees `stop` even when the job never writes.
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_on_return = StopOnDrop(Arc::clone(&stop));
    type Drained = (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>);
    let drain = |pipe: Option<(Box<dyn Read + Send>, i32)>| -> Drained {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel();
        let sink = Arc::clone(&buffer);
        let stop = Arc::clone(&stop);
        #[cfg(test)]
        let probed = command == tests::PROBE_NAME;
        std::thread::spawn(move || {
            #[cfg(test)]
            let _live = tests::LiveDrain::enter(probed);
            if let Some((mut pipe, fd)) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    // Wait for output in short slices so a pipe a descendant
                    // holds open without writing still sees `stop`.
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: `pfd` is a valid, live pollfd and `fd` stays
                    // open while `pipe` (owned by this thread) is alive.
                    let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
                    if ready == 0 {
                        continue;
                    }
                    if ready < 0 {
                        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        break;
                    }
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if stop.load(Ordering::Relaxed) => break,
                        Ok(n) => {
                            if let Ok(mut bytes) = sink.lock() {
                                let room = cap.saturating_sub(bytes.len()).min(n);
                                bytes.extend_from_slice(&chunk[..room]);
                            }
                        }
                    }
                }
            }
            let _ = done.send(());
        });
        (buffer, finished)
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| (p.as_raw_fd(), Box::new(p) as Box<dyn Read + Send>))
            .map(|(fd, p)| (p, fd)),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| (p.as_raw_fd(), Box::new(p) as Box<dyn Read + Send>))
            .map(|(fd, p)| (p, fd)),
    );

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                if group {
                    if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
                        // SAFETY: plain syscall. The leader is not reaped yet, so `pgid` is
                        // still this command's group.
                        unsafe { libc::killpg(pgid, libc::SIGKILL) };
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{command} did not finish within {timeout_secs}s"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Err(format!("Failed while waiting for {command}: {err}")),
        }
    };

    // The command has exited, so everything it wrote is already in the pipes;
    // only a background job writes later. A short grace, shared by both pipes,
    // lets the drains catch up. Waiting until the overall deadline instead
    // would make a leaky rc cost the full timeout on every probe.
    const EXIT_GRACE: Duration = Duration::from_millis(500);
    let grace_end = Instant::now() + EXIT_GRACE;
    let collect = |(buffer, finished): Drained| -> Vec<u8> {
        let wait = grace_end.saturating_duration_since(Instant::now());
        if finished.recv_timeout(wait).is_err() {
            debug!("{command} exited but left its output open; taking what arrived");
        }
        buffer.lock().map(|bytes| bytes.clone()).unwrap_or_default()
    };
    Ok(std::process::Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// The command name that makes `run_command` count its drain threads, so
    /// other tests running `run_command` in parallel never disturb the count.
    pub(super) const PROBE_NAME: &str = "exec-drain-probe";
    static LIVE_DRAINS: AtomicUsize = AtomicUsize::new(0);

    /// Counts one probed drain thread while it lives.
    pub(super) struct LiveDrain(bool);
    impl LiveDrain {
        pub(super) fn enter(probed: bool) -> Self {
            if probed {
                LIVE_DRAINS.fetch_add(1, Ordering::SeqCst);
            }
            Self(probed)
        }
    }
    impl Drop for LiveDrain {
        fn drop(&mut self) {
            if self.0 {
                LIVE_DRAINS.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Output past the cap is read and dropped: the command is not blocked, and the bytes kept
    /// are the first ones.
    #[test]
    fn a_capped_run_keeps_only_the_first_bytes() {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(
            "i=0; while [ $i -lt 2000 ]; do printf 'abcdefghij'; i=$((i+1)); done; echo done >&2",
        );
        let out = run_command_in_group_capped(cmd, "capped", 10, 1000).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.stdout.starts_with(b"abcdefghij"));
        assert_eq!(out.stderr, b"done\n");
    }

    /// Kills the holder when the test ends, however it ends.
    struct Holder(i32);
    impl Drop for Holder {
        fn drop(&mut self) {
            // SAFETY: plain kill(2) on a pid this test started.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    /// A background job holds the command's stdout and stderr open and never
    /// writes. Once `run_command` has returned, both drain threads must exit
    /// on their own (they cannot be woken by output), while the holder is
    /// still alive.
    #[test]
    fn a_silent_background_holder_does_not_keep_the_drains_alive() {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo $!");
        let out = run_command(cmd, PROBE_NAME, 10).unwrap();
        let pid: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("the shell prints the holder's pid");
        let _holder = Holder(pid);
        // SAFETY: signal 0 only checks that the process exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the holder is alive");

        let deadline = Instant::now() + Duration::from_secs(2);
        while LIVE_DRAINS.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            LIVE_DRAINS.load(Ordering::SeqCst),
            0,
            "a drain thread is still blocked on the silent holder's pipe"
        );
        // SAFETY: as above; the holder must still be alive here.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "the holder outlived the drains"
        );
    }
}
