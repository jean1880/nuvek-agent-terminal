//! Starting an external program the user configured (the diff tool, the editor): an argv, never
//! a shell, detached from the app. Logging names the tool, never a file's path or content.

use std::ffi::OsString;
use std::path::PathBuf;

use tracing::{info, warn};

/// A process ready to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

/// Starts `prepared` detached: no stdin, no output, its own process group, the user's
/// environment. A thread waits for it so it does not linger as a zombie, and logs how it ended.
/// `what` is the kind of tool ("diff tool", "editor") and `name` the configured name.
pub fn spawn_detached(what: &str, name: &str, prepared: &Prepared) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&prepared.program)
        .args(&prepared.args)
        .current_dir(&prepared.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("Could not start {name}: {e}"))?;
    info!(tool = name, "opened the {what}");
    let (what, name) = (what.to_owned(), name.to_owned());
    std::thread::spawn(move || match child.wait() {
        Ok(status) => info!(tool = name, %status, "the {what} exited"),
        Err(e) => warn!(tool = name, error = %e, "could not wait for the {what}"),
    });
    Ok(())
}

/// The environment the app's probe needs to find a program, read on the main thread so a worker
/// can use it.
pub fn probe_env() -> (Option<String>, Option<String>, Option<String>) {
    (
        std::env::var("PATH").ok(),
        std::env::var("HOME").ok(),
        std::env::var("SHELL").ok(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_that_cannot_start_is_an_error_not_a_crash() {
        let prepared = Prepared {
            program: "/nonexistent/tool".into(),
            args: Vec::new(),
            cwd: "/".into(),
        };
        let err = spawn_detached("editor", "Ghost", &prepared).expect_err("no such file");
        assert!(err.starts_with("Could not start Ghost"), "{err}");
    }
}
