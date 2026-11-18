//! The two questions the app asks another machine before a chat thread runs there, each one
//! batch `ssh` ([`agent_core::remote::BATCH_OPTIONS`]) through the timeout runner: where the
//! agent CLI is on a login shell's `PATH` (the probe), and what a typed folder is canonically
//! (the folder check). Blocking: call them off the main thread. Asked only on a user action
//! (making a thread there, opening one), never at startup.
//!
//! A probe that succeeded is kept for the life of the process, per machine and command, so
//! reopening a thread asks nothing again; a failure is never kept, so fixing the machine and
//! trying again works.

use std::collections::HashMap;
use std::sync::Mutex;

use agent_core::remote::{self, BatchFailure, RemoteProbe};

/// The most either question may take, `ssh`'s own connect timeout included.
pub const TIMEOUT_SECS: u64 = 15;

/// The most of each of stdout and stderr kept from either question.
pub const MAX_OUTPUT: usize = 256 * 1024;

/// Why a question to another machine got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    /// `ssh` itself failed: redacted, bounded, and whether a terminal page there would get past
    /// it (a host key or a password to answer).
    Ssh(BatchFailure),
    /// The machine answered, and the answer refuses the thread (csh, no CLI, no such folder).
    Refused(String),
}

impl RemoteError {
    /// What the user is told.
    pub fn reason(&self) -> &str {
        match self {
            Self::Ssh(failure) => &failure.reason,
            Self::Refused(why) => why,
        }
    }

    /// Whether opening a terminal page on the machine would let the user answer what stopped it.
    pub fn needs_terminal(&self) -> bool {
        matches!(self, Self::Ssh(failure) if failure.needs_terminal)
    }
}

/// How the app reaches `ssh`: the program (a resolved path, or a test's fake) and the exact
/// environment it runs in (`KEY=value` entries).
#[derive(Debug, Clone, Default)]
pub struct Ssh {
    pub program: String,
    pub env: Vec<String>,
}

impl Ssh {
    /// Runs `argv` (whose `argv[0]` is replaced by [`Self::program`]) and returns its stdout, or
    /// why it failed.
    fn run(&self, argv: &[String]) -> Result<String, RemoteError> {
        let Some((_, args)) = argv.split_first() else {
            return Err(RemoteError::Refused("empty command line".into()));
        };
        let mut command = std::process::Command::new(&self.program);
        command
            .args(args)
            .env_clear()
            .stdin(std::process::Stdio::null());
        command.envs(self.env.iter().filter_map(|entry| entry.split_once('=')));
        // Its own process group, so a `ProxyCommand` it started dies with it on timeout, and
        // at most `MAX_OUTPUT` of what the other machine prints is kept.
        let out =
            crate::exec::run_command_in_group_capped(command, "ssh", TIMEOUT_SECS, MAX_OUTPUT)
                .map_err(|why| {
                    RemoteError::Ssh(BatchFailure {
                        reason: agent_core::redact::redact(&why),
                        needs_terminal: false,
                    })
                })?;
        if !out.status.success() {
            return Err(RemoteError::Ssh(remote::batch_failure(
                out.status.code(),
                &String::from_utf8_lossy(&out.stderr),
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Probes kept per (machine, command).
static PROBES: Mutex<Option<HashMap<(String, String), RemoteProbe>>> = Mutex::new(None);

/// The kept probe of `cli` on `alias`, if one succeeded in this process.
pub fn cached_probe(alias: &str, cli: &str) -> Option<RemoteProbe> {
    PROBES
        .lock()
        .ok()?
        .as_ref()?
        .get(&(alias.to_owned(), cli.to_owned()))
        .cloned()
}

/// Forgets every kept probe of `alias` (it was removed or renamed in Settings).
pub fn forget(alias: &str) {
    if let Ok(mut probes) = PROBES.lock() {
        if let Some(map) = probes.as_mut() {
            map.retain(|(host, _), _| host != alias);
        }
    }
}

/// Where `cli` is on `alias`: the kept answer, else a probe now (kept when it succeeds).
pub fn probe(ssh: &Ssh, alias: &str, cli: &str) -> Result<RemoteProbe, RemoteError> {
    if let Some(found) = cached_probe(alias, cli) {
        return Ok(found);
    }
    let command = remote::probe_command(cli).map_err(RemoteError::Refused)?;
    let argv = remote::ssh_batch_argv(alias, command).map_err(RemoteError::Refused)?;
    let stdout = ssh.run(&argv)?;
    let found = remote::parse_probe(cli, &stdout).map_err(RemoteError::Refused)?;
    if let Ok(mut probes) = PROBES.lock() {
        probes
            .get_or_insert_with(HashMap::new)
            .insert((alias.to_owned(), cli.to_owned()), found.clone());
    }
    Ok(found)
}

/// The canonical absolute path of `dir` (as typed: `~` allowed) on `alias`. Asked only after a
/// probe there found a POSIX login shell (`probe`, the proof): the folder's quoting is POSIX.
pub fn check_folder(
    ssh: &Ssh,
    alias: &str,
    dir: &str,
    probe: &RemoteProbe,
) -> Result<String, RemoteError> {
    if !remote::posix_shell(&probe.shell) {
        return Err(RemoteError::Refused(
            "The login shell there is not a POSIX shell".to_owned(),
        ));
    }
    let command = remote::folder_command(dir).map_err(RemoteError::Refused)?;
    let argv = remote::ssh_batch_argv(alias, command).map_err(RemoteError::Refused)?;
    let stdout = ssh.run(&argv)?;
    remote::parse_folder(&stdout).map_err(RemoteError::Refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `ssh`: records its arguments, one per line, and then plays the "remote" side by
    /// running the last argument in a local `sh` with `$SHELL`, `PATH` and `HOME` set as a
    /// login there would have them. Nothing is ever connected to.
    fn fake_ssh(dir: &std::path::Path, home: &std::path::Path, bin: &std::path::Path) -> Ssh {
        let script = dir.join("ssh");
        let record = dir.join("argv");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n: > '{rec}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{rec}'; done\n\
                 for last in \"$@\"; do :; done\n\
                 exec env SHELL=/bin/sh HOME='{home}' PATH='{bin}:/usr/bin:/bin' sh -c \"$last\"\n",
                rec = record.display(),
                home = home.display(),
                bin = bin.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        Ssh {
            program: script.to_string_lossy().into_owned(),
            env: vec!["PATH=/usr/bin:/bin".into()],
        }
    }

    #[test]
    fn the_probe_and_folder_check_run_batch_ssh_and_keep_only_a_success() {
        if std::process::Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .is_err()
        {
            eprintln!("no sh; skipping");
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("home");
        let bin = base.path().join("bin");
        let work = home.join("my 'src'");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let ssh = fake_ssh(base.path(), &home, &bin);
        let alias = format!("box-{}", std::process::id());

        // No `claude` on the login PATH yet: refused, and nothing kept.
        let missing = probe(&ssh, &alias, "claude").unwrap_err();
        assert!(missing.reason().contains("not found"), "{missing:?}");
        assert!(cached_probe(&alias, "claude").is_none());

        let cli = bin.join("claude");
        std::fs::write(&cli, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let found = probe(&ssh, &alias, "claude").unwrap();
        assert_eq!(found.cli, cli.to_string_lossy());
        let argv = std::fs::read_to_string(base.path().join("argv")).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(argv[..2], ["-T", "-o"]);
        assert!(argv.contains(&"BatchMode=yes"));
        let dash = argv.iter().position(|a| *a == "--").unwrap();
        assert_eq!(argv[dash + 1], alias);

        // Kept: the next probe asks nothing (the fake would now fail if asked).
        std::fs::remove_file(&cli).unwrap();
        assert_eq!(probe(&ssh, &alias, "claude").unwrap(), found);
        forget(&alias);
        assert!(cached_probe(&alias, "claude").is_none());

        // The folder check gives the canonical path, and a missing folder fails.
        let canonical = std::fs::canonicalize(&work).unwrap();
        assert_eq!(
            check_folder(&ssh, &alias, "~/my 'src'", &found).unwrap(),
            canonical.to_string_lossy()
        );
        assert!(check_folder(&ssh, &alias, "/nonexistent/agent-kit", &found).is_err());
        assert!(check_folder(&ssh, "-oProxyCommand=x", "~", &found).is_err());
    }

    /// The folder check's quoting is POSIX: it is never sent to a machine whose login shell is
    /// not (fish would read `\'` inside quotes as an escaped quote), and ssh is never run.
    #[test]
    fn the_folder_check_needs_a_posix_login_shell() {
        let base = tempfile::tempdir().unwrap();
        let ran = base.path().join("ran");
        let script = base.path().join("ssh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\n: > '{}'\necho /srv\n", ran.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ssh = Ssh {
            program: script.to_string_lossy().into_owned(),
            env: Vec::new(),
        };
        for shell in [
            "/usr/bin/fish",
            "/usr/bin/nu",
            "/usr/bin/pwsh",
            "/bin/tcsh",
            "",
        ] {
            let probe = RemoteProbe {
                shell: shell.into(),
                path: "/usr/bin".into(),
                cli: "/usr/bin/claude".into(),
            };
            let refused = check_folder(&ssh, "box", "~/a'b", &probe).unwrap_err();
            assert!(refused.reason().contains("POSIX"), "{shell}");
        }
        assert!(!ran.exists(), "ssh never ran");
    }

    /// What the other machine prints is kept only up to [`MAX_OUTPUT`].
    #[test]
    fn a_flood_of_output_from_the_other_machine_is_capped() {
        let base = tempfile::tempdir().unwrap();
        let script = base.path().join("ssh");
        std::fs::write(
            &script,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 40000 ]; do echo 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; \
             i=$((i+1)); done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ssh = Ssh {
            program: script.to_string_lossy().into_owned(),
            env: vec!["PATH=/usr/bin:/bin".into()],
        };
        let out = ssh.run(&["ssh".to_owned()]).unwrap();
        assert!(out.len() <= MAX_OUTPUT, "{}", out.len());
        assert!(out.len() > MAX_OUTPUT / 2);
    }

    #[test]
    fn an_ssh_that_stops_at_a_host_key_offers_a_terminal() {
        let base = tempfile::tempdir().unwrap();
        let script = base.path().join("ssh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'Host key verification failed.' >&2\nexit 255\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ssh = Ssh {
            program: script.to_string_lossy().into_owned(),
            env: Vec::new(),
        };
        let failed = probe(&ssh, "unknown-key-box", "codex").unwrap_err();
        assert!(failed.needs_terminal());
        assert_eq!(failed.reason(), "Host key verification failed.");
        assert!(cached_probe("unknown-key-box", "codex").is_none());
    }
}
