//! Machines reached with the user's own `ssh`: the hosts listed in Settings, the folder on one,
//! and the exact `ssh` command lines the app runs for them.
//!
//! The app never speaks the ssh protocol and installs nothing on the other machine. It starts
//! the local `ssh` with an argv (never a local shell string), always with every kind of
//! forwarding off, so nothing listens on this machine and nothing of it is reachable from the
//! other one. The remote side gets one POSIX command line, `cd -- '<folder>' && exec <command>`,
//! every piece of user data in it single-quoted: the remote login shell expands nothing the user
//! typed, and writes nothing to the terminal before `exec`.

use serde::{Deserialize, Serialize};

/// The longest alias accepted, as a host name may be.
pub const MAX_ALIAS_LEN: usize = 255;
/// The longest label accepted.
pub const MAX_LABEL_LEN: usize = 255;
/// The longest remote folder accepted.
pub const MAX_DIR_LEN: usize = 4096;

/// Options every terminal page's `ssh` runs with, before `--` and the alias.
///
/// `-t` gives the remote command a terminal, as a terminal page needs. `BatchMode=no` lets ssh
/// ask in that terminal: a page is interactive, and the user is looking at it when it opens, so a
/// first connection's host-key question or a password prompt is answered there by the user, the
/// same as in any terminal. (A process with no terminal, such as a chat thread's agent, must
/// never wait on a prompt nobody can see, and runs with `BatchMode=yes` instead.)
/// `ClearAllForwardings=yes` overrides any `LocalForward`, `RemoteForward` or `DynamicForward`
/// in the user's ssh configuration, so the app opens no port on either side; agent, X11 and
/// local-command execution are off for the same reason, and so is `Tunnel` (a tun device, which
/// `ClearAllForwardings` does not cover). `RemoteCommand=none` keeps a configured remote command
/// from replacing the one given here.
pub const TTY_OPTIONS: &[&str] = &[
    "-t",
    "-o",
    "BatchMode=no",
    "-o",
    "ConnectTimeout=15",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "Tunnel=no",
    "-o",
    "RemoteCommand=none",
];

/// Options of the reachability check in Settings: never a prompt (it has no terminal), a short
/// connect timeout, and no forwarding of any kind.
pub const CHECK_OPTIONS: &[&str] = &[
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "Tunnel=no",
    "-o",
    "RemoteCommand=none",
];

/// A machine listed in Settings → General → Remote Machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHost {
    /// What `ssh` is given: a `Host` alias from the user's ssh configuration, a host name, or
    /// `user@host`. Checked by [`validate_alias`].
    pub alias: String,
    /// How the app names it; empty shows the alias.
    #[serde(default)]
    pub label: String,
}

impl RemoteHost {
    /// A host whose alias and label are both valid.
    pub fn new(alias: &str, label: &str) -> Result<Self, String> {
        let alias = alias.trim();
        validate_alias(alias)?;
        let label = label.trim();
        validate_label(label)?;
        Ok(Self {
            alias: alias.to_owned(),
            label: label.to_owned(),
        })
    }

    /// Whether the alias and label are valid as stored (a hand-edited file may hold anything).
    pub fn is_valid(&self) -> bool {
        validate_alias(&self.alias).is_ok() && validate_label(&self.label).is_ok()
    }

    /// The name shown for it: its label, else its alias.
    pub fn display_name(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.alias
        } else {
            &self.label
        }
    }
}

/// Whether `alias` may be handed to `ssh`: 1 to [`MAX_ALIAS_LEN`] characters of
/// `[A-Za-z0-9._@:-]`, not starting with `-` (which ssh would read as an option, `-oProxyCommand=`
/// above all). The `--` before it in every argv is a second guard, not the only one.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("Enter the machine's ssh name (a Host alias, host name or user@host)".into());
    }
    if alias.len() > MAX_ALIAS_LEN {
        return Err(format!("At most {MAX_ALIAS_LEN} characters"));
    }
    if alias.starts_with('-') {
        return Err("A name cannot start with “-”".into());
    }
    if let Some(bad) = alias
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | ':' | '-')))
    {
        return Err(if bad.is_whitespace() {
            "A name cannot hold spaces".into()
        } else {
            format!("A name cannot hold “{bad}”: use letters, digits and . _ @ : -")
        });
    }
    Ok(())
}

/// Whether `label` may be kept: at most [`MAX_LABEL_LEN`] characters, no control or invisible
/// characters.
pub fn validate_label(label: &str) -> Result<(), String> {
    if label.chars().count() > MAX_LABEL_LEN {
        return Err(format!("At most {MAX_LABEL_LEN} characters"));
    }
    if crate::text::has_control_or_bidi(label, &[]) {
        return Err("A label cannot hold control or invisible characters".into());
    }
    Ok(())
}

/// The folder on the other machine as typed: blank is `~`. An absolute path, `~`, `~/…` or a
/// path relative to the remote home; never `~user` (whose expansion the app does not do), nor a
/// control character, nor longer than [`MAX_DIR_LEN`].
pub fn parse_remote_dir(text: &str) -> Result<String, String> {
    let dir = text.trim();
    if dir.is_empty() {
        return Ok("~".to_owned());
    }
    if dir.len() > MAX_DIR_LEN {
        return Err(format!("At most {MAX_DIR_LEN} characters"));
    }
    if crate::text::has_control_or_bidi(dir, &[]) {
        return Err("A folder cannot hold control or invisible characters".into());
    }
    if dir.starts_with('~') && dir != "~" && !dir.starts_with("~/") {
        return Err("Only ~ and ~/… are understood, not ~user".into());
    }
    Ok(dir.to_owned())
}

/// POSIX single-quoting: nothing inside is expanded, and an embedded `'` is closed, escaped and
/// reopened.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `dir` as a quoted `cd` operand: `~` and `~/…` become `"$HOME"` and the quoted rest, since a
/// quoted `~` is not expanded.
fn cd_operand(dir: &str) -> String {
    match dir {
        "" | "~" => "\"$HOME\"".to_owned(),
        _ => match dir.strip_prefix("~/") {
            Some("") => "\"$HOME\"".to_owned(),
            Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
            None => sh_quote(dir),
        },
    }
}

/// The command line the remote login shell runs: `cd -- '<dir>' && exec <command>`, each element
/// of `command` quoted; with no command, the user's login shell there (`exec "$SHELL" -l`).
/// Nothing in it writes to the terminal before `exec`.
pub fn remote_command(dir: &str, command: &[String]) -> Result<String, String> {
    let dir = parse_remote_dir(dir)?;
    let run = if command.is_empty() {
        "\"$SHELL\" -l".to_owned()
    } else {
        command
            .iter()
            .map(|a| sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    };
    Ok(format!("cd -- {} && exec {run}", cd_operand(&dir)))
}

/// The argv of a terminal page on `alias`, in `dir` there, running `command` (empty: the login
/// shell): `ssh` + [`TTY_OPTIONS`] + `--` + the alias + the [`remote_command`] as one argument.
/// `argv[0]` is `ssh`; the caller may put its resolved path there.
pub fn ssh_tty_argv(alias: &str, dir: &str, command: &[String]) -> Result<Vec<String>, String> {
    validate_alias(alias)?;
    let remote = remote_command(dir, command)?;
    let mut argv = vec!["ssh".to_owned()];
    argv.extend(TTY_OPTIONS.iter().map(|s| (*s).to_owned()));
    argv.push("--".to_owned());
    argv.push(alias.to_owned());
    argv.push(remote);
    Ok(argv)
}

/// The argv of the reachability check: `ssh` + [`CHECK_OPTIONS`] + `--` + the alias + `true`.
pub fn ssh_check_argv(alias: &str) -> Result<Vec<String>, String> {
    validate_alias(alias)?;
    let mut argv = vec!["ssh".to_owned()];
    argv.extend(CHECK_OPTIONS.iter().map(|s| (*s).to_owned()));
    argv.push("--".to_owned());
    argv.push(alias.to_owned());
    argv.push("true".to_owned());
    Ok(argv)
}

/// Whether a link shown on a remote page may be opened on this machine: only `http://` and
/// `https://` with a host. Anything else (`file://`, `ssh:`, a custom handler's scheme) names
/// something on the other machine, or asks this one to run a handler the other machine chose.
pub fn remote_link_allowed(uri: &str) -> bool {
    let uri = uri.trim();
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    web && !rest.is_empty() && !rest.starts_with('/') && !crate::text::has_control_or_bidi(uri, &[])
}

/// What a remote page says when it refuses a link.
pub const REMOTE_LINK_REFUSED: &str =
    "Links to files on another machine cannot open here; only web links can";

/// A terminal page's title: `alias:folder`.
pub fn page_title(alias: &str, dir: &str) -> String {
    format!("{alias}:{dir}")
}

/// What the last reachability check of a machine said. Three states, never two: a machine not
/// checked (this session) is neither reachable nor not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RemoteCheck {
    #[default]
    NotChecked,
    Checking,
    Reachable,
    Unreachable(String),
}

impl RemoteCheck {
    /// The status line Settings shows.
    pub fn describe(&self) -> String {
        match self {
            RemoteCheck::NotChecked => "Not checked".to_owned(),
            RemoteCheck::Checking => "Checking…".to_owned(),
            RemoteCheck::Reachable => "Reachable".to_owned(),
            RemoteCheck::Unreachable(why) => format!("Not reachable: {why}"),
        }
    }
}

/// The check's answer from `ssh`'s exit code and standard error: exit 0 is reachable, anything
/// else is not, for the last line ssh wrote (bounded), or the exit code when it wrote nothing.
///
/// What ssh writes is not ssh's alone: a `ProxyCommand` or a proxy's error can print a
/// credential. It is redacted first, as a whole, then stripped of hidden characters, and only
/// then cut to its bound, so no secret is cut short of what masking recognises.
pub fn check_outcome(code: Option<i32>, stderr: &str) -> RemoteCheck {
    if code == Some(0) {
        return RemoteCheck::Reachable;
    }
    let stderr = crate::redact::redact(stderr);
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(|l| crate::text::truncate_bytes(&crate::text::without_hidden(l), 160).to_owned());
    RemoteCheck::Unreachable(match (line, code) {
        (Some(line), _) => line,
        (None, Some(code)) => format!("ssh exited with status {code}"),
        (None, None) => "ssh was stopped by a signal".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_refuses_leading_dash_and_metacharacters() {
        for good in [
            "box",
            "gpu.example.com",
            "me@192.0.2.7",
            "build_vm-2",
            "fe80::1",
            "a",
        ] {
            assert!(validate_alias(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-oProxyCommand=touch /tmp/x",
            "-v",
            "box;rm -rf ~",
            "box host",
            "box\n",
            "$(id)",
            "`id`",
            "box|cat",
            "box&",
            "a'b",
            "a\"b",
            "box/..",
            "*",
            "é",
        ] {
            assert!(validate_alias(bad).is_err(), "{bad:?}");
        }
        assert!(validate_alias(&"a".repeat(MAX_ALIAS_LEN)).is_ok());
        assert!(validate_alias(&"a".repeat(MAX_ALIAS_LEN + 1)).is_err());
        // The argv builders refuse what validation refuses.
        assert!(ssh_tty_argv("-oProxyCommand=x", "~", &[]).is_err());
        assert!(ssh_check_argv("box;id").is_err());
        assert!(RemoteHost::new("  box ", " Box ").is_ok_and(|h| h.alias == "box"));
        assert!(RemoteHost::new("box", "a\u{7}b").is_err());
    }

    #[test]
    fn remote_dir_accepts_home_forms_and_refuses_tilde_user() {
        assert_eq!(parse_remote_dir("  ").unwrap(), "~");
        assert_eq!(parse_remote_dir("~/src").unwrap(), "~/src");
        assert_eq!(parse_remote_dir("/srv/a b").unwrap(), "/srv/a b");
        assert!(parse_remote_dir("~root/x").is_err());
        assert!(parse_remote_dir("/a\nb").is_err());
        assert_eq!(cd_operand("~"), "\"$HOME\"");
        assert_eq!(cd_operand("~/"), "\"$HOME\"");
        assert_eq!(cd_operand("~/a'b"), r#""$HOME"/'a'\''b'"#);
    }

    #[test]
    fn ssh_argv_always_clears_forwardings() {
        let tty = ssh_tty_argv("box", "~", &[]).unwrap();
        let check = ssh_check_argv("box").unwrap();
        for argv in [&tty, &check] {
            assert_eq!(argv[0], "ssh");
            let joined = argv.join(" ");
            for option in [
                "ClearAllForwardings=yes",
                "ForwardAgent=no",
                "ForwardX11=no",
                "PermitLocalCommand=no",
                "Tunnel=no",
                "RemoteCommand=none",
            ] {
                assert!(joined.contains(&format!("-o {option}")), "{option}");
            }
            // No forwarding or listening flag at all.
            for flag in [
                "-L", "-R", "-D", "-A", "-X", "-Y", "-W", "-M", "-f", "-N", "-w", "-S", "-O",
            ] {
                assert!(!argv.iter().any(|a| a == flag), "{flag}");
            }
            // `--` comes right before the alias, so nothing after it is an option.
            let dash = argv.iter().position(|a| a == "--").unwrap();
            assert_eq!(argv[dash + 1], "box");
        }
        assert!(tty.iter().any(|a| a == "-t"));
        assert!(tty.iter().any(|a| a == "BatchMode=no"));
        assert!(check.iter().any(|a| a == "BatchMode=yes"));
        assert!(check.iter().any(|a| a == "ConnectTimeout=10"));
        assert_eq!(check.last().map(String::as_str), Some("true"));
        // The remote command is a single argument, the last.
        assert_eq!(tty.len(), TTY_OPTIONS.len() + 4);
        assert_eq!(tty.last().unwrap(), r#"cd -- "$HOME" && exec "$SHELL" -l"#);
    }

    /// Runs the remote command line in a local POSIX shell, as the remote login shell would.
    fn run_in_sh(line: &str, home: &std::path::Path) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(line)
            .env("HOME", home)
            .env("x", "EXPANDED")
            .output()
            .expect("run sh");
        assert!(
            out.status.success(),
            "{line}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn ssh_tty_argv_quotes_dir_and_command_for_a_posix_login_shell() {
        if std::process::Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .is_err()
        {
            eprintln!("no sh; skipping");
            return;
        }
        let home = std::env::temp_dir().join(format!("agent-core-remote-{}", std::process::id()));
        let tricky = "a b 'c' $x \"d\" `e` ;f";
        let dir = home.join(tricky);
        std::fs::create_dir_all(&dir).unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        let command: Vec<String> = [
            "sh",
            "-c",
            "pwd -P; for a in \"$@\"; do printf '[%s]' \"$a\"; done",
            "-",
            "one two",
            "$x",
            "it's",
            "\"q\"",
            "`id`",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

        let absolute = ssh_tty_argv("box", dir.to_str().unwrap(), &command).unwrap();
        let out = run_in_sh(absolute.last().unwrap(), &home);
        let expected_args = "[one two][$x][it's][\"q\"][`id`]";
        assert_eq!(
            out,
            format!("{}\n{expected_args}", canonical.display()),
            "absolute folder"
        );

        let under_home = ssh_tty_argv("box", &format!("~/{tricky}"), &command).unwrap();
        let out = run_in_sh(under_home.last().unwrap(), &home);
        assert_eq!(out, format!("{}\n{expected_args}", canonical.display()));

        // A folder that does not exist runs nothing.
        let missing = ssh_tty_argv("box", "/nonexistent/agent-core", &command).unwrap();
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(missing.last().unwrap())
            .output()
            .unwrap();
        assert!(!status.status.success());
        assert!(status.stdout.is_empty());

        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn check_outcome_has_three_states_and_a_reason() {
        assert_eq!(RemoteCheck::default().describe(), "Not checked");
        assert_eq!(check_outcome(Some(0), "warning"), RemoteCheck::Reachable);
        assert_eq!(
            check_outcome(Some(255), "debug\nssh: Could not resolve hostname box\n\n"),
            RemoteCheck::Unreachable("ssh: Could not resolve hostname box".into())
        );
        assert_eq!(
            check_outcome(Some(255), "").describe(),
            "Not reachable: ssh exited with status 255"
        );
        assert!(matches!(
            check_outcome(None, ""),
            RemoteCheck::Unreachable(_)
        ));
    }

    /// A credential a ProxyCommand printed is masked before the reason is cut to its bound, even
    /// when the bound falls inside it.
    #[test]
    fn check_outcome_masks_a_secret_before_cutting_the_reason() {
        let token = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        let prefix = format!("proxy: {} ", "x".repeat(142));
        assert_eq!(
            prefix.len(),
            150,
            "the token starts before the 160-byte bound"
        );
        let stderr = format!("debug1: hello\n{prefix}{token} rejected\n");
        let RemoteCheck::Unreachable(why) = check_outcome(Some(255), &stderr) else {
            panic!("a failed check is unreachable");
        };
        assert!(why.len() <= 160, "{}", why.len());
        assert!(!why.contains("ghp_abc"), "{why}");
        assert!(!why.contains("abcdefghij"), "{why}");
        assert!(why.starts_with("proxy: "), "{why}");
        // A whole secret on its own line is masked too.
        let RemoteCheck::Unreachable(why) = check_outcome(Some(255), &format!("auth {token}"))
        else {
            panic!("a failed check is unreachable");
        };
        assert!(!why.contains(token), "{why}");
    }

    #[test]
    fn a_remote_page_opens_only_web_links() {
        for ok in [
            "https://example.com/a?b=c",
            "http://192.0.2.7:8080/",
            "HTTPS://Example.com",
        ] {
            assert!(remote_link_allowed(ok), "{ok}");
        }
        for refused in [
            "file:///etc/passwd",
            "file://box/home/me/.ssh/id_ed25519",
            "FILE:///tmp/x",
            "ssh://box",
            "mailto:me@example.com",
            "javascript:alert(1)",
            "vscode://file/etc/passwd",
            "smb://server/share",
            "https:///etc/passwd",
            "https://",
            "/etc/passwd",
            "example.com",
            "",
            "https://exa\u{202e}mple.com",
        ] {
            assert!(!remote_link_allowed(refused), "{refused:?}");
        }
    }

    #[test]
    fn display_name_falls_back_to_the_alias() {
        let host = RemoteHost::new("box", "").unwrap();
        assert_eq!(host.display_name(), "box");
        let host = RemoteHost::new("box", "GPU box").unwrap();
        assert_eq!(host.display_name(), "GPU box");
        assert_eq!(page_title("box", "~/src"), "box:~/src");
    }
}
