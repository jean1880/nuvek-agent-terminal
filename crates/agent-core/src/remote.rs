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
/// above all), with at most one `@`, and then a user and a host that are both non-empty and
/// neither starting with `-` (`me@-v` would hand ssh `-v` as the host). The `--` before it in
/// every argv is a second guard, not the only one.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    validate_alias_chars(alias)?;
    if let Some((user, host)) = alias.split_once('@') {
        if host.contains('@') {
            return Err("A name holds at most one “@”".into());
        }
        if user.is_empty() || host.is_empty() {
            return Err("Write user@host with both a user and a host".into());
        }
        if user.starts_with('-') || host.starts_with('-') {
            return Err("Neither the user nor the host can start with “-”".into());
        }
    }
    Ok(())
}

/// [`validate_alias`]'s length and character rules.
fn validate_alias_chars(alias: &str) -> Result<(), String> {
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

/// Whether `alias` may be reached: it is valid and listed in Settings → General → Remote
/// Machines (`hosts`) as stored now. A thread keeps its alias for life, so one made on a machine
/// since removed (or renamed) from the list is refused, never connected to.
pub fn listed(hosts: &[RemoteHost], alias: &str) -> Result<(), String> {
    validate_alias(alias)?;
    if hosts.iter().any(|h| h.alias == alias && h.is_valid()) {
        Ok(())
    } else {
        Err(format!(
            "{alias} is not listed in Settings → General → Remote Machines; add it there to \
             connect"
        ))
    }
}

/// Whether a link shown on a remote page may be opened on this machine: only `http://` and
/// `https://` with a host. Anything else (`file://`, `ssh:`, a custom handler's scheme) names
/// something on the other machine, or asks this one to run a handler the other machine chose.
///
/// Judged as given, never trimmed, since the caller launches exactly this string: no ASCII
/// whitespace anywhere, no control, bidi or otherwise hidden character, at most
/// [`MAX_LINK_LEN`] bytes. The authority (after `://`, up to the first `/`, `?` or `#`) must
/// name a host, must not start with `:` (a port with no host), and must hold no `@`: neither
/// `user:pass@` nor `https://trusted.example@other.example`, which reads as one host and goes
/// to the other.
pub fn remote_link_allowed(uri: &str) -> bool {
    if uri.len() > MAX_LINK_LEN
        || uri.bytes().any(|b| b.is_ascii_whitespace())
        || crate::text::has_hidden(uri)
        || crate::text::has_control_or_bidi(uri, &[])
    {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.starts_with(':') && !authority.contains('@')
}

/// Options of every `ssh` that has no terminal: a chat thread's agent, the probe of the agent
/// there and the folder check. `-T` asks for no terminal; `BatchMode=yes` makes ssh fail rather
/// than wait on a host-key question or a password nobody can see (the user answers those in a
/// terminal page instead); the forwarding options are [`TTY_OPTIONS`]'s, for the same reasons.
pub const BATCH_OPTIONS: &[&str] = &[
    "-T",
    "-o",
    "BatchMode=yes",
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

/// What starts a thread's folder in the store when it is on another machine.
pub const THREAD_SCHEME: &str = "ssh://";

/// The longest `PATH` a probe may report.
pub const MAX_PATH_LEN: usize = 8192;

/// A folder on another machine as a thread keeps it: the listed alias and the folder's
/// canonical absolute path there (what `pwd -P` said when the thread was made).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteFolder {
    alias: String,
    dir: String,
}

impl RemoteFolder {
    /// A folder whose alias is valid and whose path is absolute, free of control and invisible
    /// characters and at most [`MAX_DIR_LEN`] long.
    pub fn new(alias: &str, dir: &str) -> Result<Self, String> {
        validate_alias(alias)?;
        validate_absolute(dir)?;
        Ok(Self {
            alias: alias.to_owned(),
            dir: dir.to_owned(),
        })
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// What the store's `cwd` column holds: `ssh://<alias><dir>` (the path starts with `/`).
    pub fn cwd(&self) -> String {
        format!("{THREAD_SCHEME}{}{}", self.alias, self.dir)
    }

    /// The name shown for it, in the sidebar and the thread's subtitle: `alias:/path`.
    pub fn title(&self) -> String {
        page_title(&self.alias, &self.dir)
    }
}

/// Whether `dir` is a usable absolute folder on another machine.
fn validate_absolute(dir: &str) -> Result<(), String> {
    if !dir.starts_with('/') {
        return Err("The folder there must be an absolute path".into());
    }
    if dir.len() > MAX_DIR_LEN {
        return Err(format!("At most {MAX_DIR_LEN} characters"));
    }
    if crate::text::has_control_or_bidi(dir, &[]) || crate::text::has_hidden(dir) {
        return Err("A folder cannot hold control or invisible characters".into());
    }
    Ok(())
}

/// A thread's folder, read from its stored `cwd` at every boundary that receives a path: a
/// folder on this machine, or one on another machine reached with `ssh`. Code that takes a
/// thread's folder must match both, so nothing local is ever done with a remote path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ThreadFolder {
    Local(String),
    Remote(RemoteFolder),
}

impl ThreadFolder {
    /// Reads a stored `cwd`. Anything not starting with [`THREAD_SCHEME`] is a local folder
    /// (as every thread stored before remote threads was); an `ssh://` value that does not hold
    /// a valid alias and absolute path is an error, never taken for a local path.
    pub fn parse(cwd: &str) -> Result<Self, String> {
        let Some(rest) = cwd.strip_prefix(THREAD_SCHEME) else {
            return Ok(Self::Local(cwd.to_owned()));
        };
        let Some(slash) = rest.find('/') else {
            return Err("A folder on another machine names no path".into());
        };
        let (alias, dir) = rest.split_at(slash);
        RemoteFolder::new(alias, dir).map(Self::Remote)
    }

    /// The folder on another machine, if it is one.
    pub fn remote(&self) -> Option<&RemoteFolder> {
        match self {
            Self::Remote(folder) => Some(folder),
            Self::Local(_) => None,
        }
    }

    /// What the store keeps for it.
    pub fn cwd(&self) -> String {
        match self {
            Self::Local(dir) => dir.clone(),
            Self::Remote(folder) => folder.cwd(),
        }
    }
}

/// Whether a stored `cwd` names a folder on another machine (valid or not): such a value is
/// never a local path, whatever else is wrong with it.
pub fn is_remote_cwd(cwd: &str) -> bool {
    cwd.starts_with(THREAD_SCHEME)
}

/// The agent CLI a thread on another machine runs for `driver`, or why it cannot run there:
/// Claude and Codex only. agy reads its transcript and media from this machine's disk, and the
/// Local agent is this app's own runner, which is not on the other machine.
pub fn chat_cli(driver: crate::adapter::Driver) -> Result<&'static str, String> {
    use crate::adapter::Driver;
    match driver {
        Driver::Claude | Driver::Codex => Ok(driver.info().default_command),
        Driver::Agy => Err(
            "Antigravity cannot run a thread on another machine: the app reads its transcript \
             and images from this machine's disk."
                .into(),
        ),
        Driver::Local => Err(
            "The Local agent cannot run a thread on another machine: it is this app's own \
             runner, which is not installed there."
                .into(),
        ),
    }
}

/// The buttons a remote approval card keeps: Allow once and Deny. Session and always rules
/// are judged against this machine's files and folders, so none is offered, kept or applied for
/// a call on another machine.
pub fn approval_options(options: &mut Vec<crate::event::Decision>) {
    use crate::event::Decision;
    options.retain(|d| matches!(d, Decision::Allow | Decision::Deny));
    if !options.contains(&Decision::Deny) {
        options.push(Decision::Deny);
    }
}

/// Why Codex on another machine takes no images: it is given an image as a file path, read
/// there, and the image is a file on this machine.
pub const CODEX_IMAGES_REFUSED: &str =
    "Codex on another machine can't take images: it reads an image from a file path, and the \
     image is on this machine.";

/// Whether an agent on another machine takes images with a prompt: Claude does (they go as
/// base64 on its input), Codex does not ([`CODEX_IMAGES_REFUSED`]).
pub fn takes_images(driver: crate::adapter::Driver) -> Result<(), &'static str> {
    match driver {
        crate::adapter::Driver::Codex => Err(CODEX_IMAGES_REFUSED),
        _ => Ok(()),
    }
}

/// What a remote thread says when its `ssh` exits 255 (ssh's own failure: no connection, an
/// unknown host key, a refused login), rather than the agent's.
pub fn unreachable_notice(alias: &str) -> String {
    format!("Could not reach {alias}.")
}

/// `ssh` exits with 255 when it fails itself, never for the remote command.
pub fn ssh_failed(code: Option<i32>) -> bool {
    code == Some(255)
}

/// What the probe of an agent on another machine found: the login shell there, the `PATH` a
/// login shell has, and the agent CLI's absolute path on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProbe {
    pub shell: String,
    pub path: String,
    pub cli: String,
}

const PROBE_SHELL: &str = "__agent_terminal_shell=";
const PROBE_PATH: &str = "__agent_terminal_path=";
const PROBE_CLI: &str = "__agent_terminal_cli=";
const PROBE_QUOTE: &str = "__agent_terminal_quote=";

/// The quoting canary's value: [`sh_quote`] of it is what the login shell itself parses first in
/// the probe, so it comes back exactly only from a shell that reads that quoting as POSIX does.
/// The `'` is closed, escaped and reopened (`'\''`); the `\\` stays two backslashes inside single
/// quotes, where fish reads it as one (and `\'` as a quote that does not close).
const QUOTE_CANARY: &str = r"a'b c\\d";

/// The login shells a thread on another machine runs under, by name: POSIX shells only. The
/// account's login shell parses the thread's command line (`cd -- '…' && exec …`), and the
/// quoting that keeps every value literal ([`sh_quote`]) holds only for a POSIX shell: in fish,
/// `\'` inside single quotes is an escaped quote, so a folder, path or argument could close the
/// quote and run there. csh, tcsh, fish, nu, elvish, xonsh, pwsh and any shell not listed are
/// refused.
pub const POSIX_SHELLS: &[&str] = &["sh", "bash", "zsh", "ksh", "dash", "ash", "mksh", "yash"];

/// Whether `shell` (a path, as `$SHELL` names it) is one of [`POSIX_SHELLS`].
pub fn posix_shell(shell: &str) -> bool {
    let name = shell.rsplit('/').next().unwrap_or_default();
    !shell.is_empty() && POSIX_SHELLS.contains(&name)
}

/// The remote command of the probe for `cli` (a bare command name). Read by whatever login
/// shell the account has, so it is written to parse the same in csh: one line, with no `!` and
/// no `$` outside single quotes. The login shell itself first prints the quoting canary
/// ([`QUOTE_CANARY`], quoted with [`sh_quote`] as every value on a thread's command line is), so
/// the probe learns how the shell that will read those lines quotes, whatever `$SHELL` says. Then
/// a single-quoted `/bin/sh` script with no `'` in it says the login shell, stops there for any
/// shell not in [`POSIX_SHELLS`], and otherwise asks a login shell (`-l`, never interactive, its
/// input empty) for its `PATH` and where `cli` is on it. Markers set the answers apart from
/// anything a profile prints.
pub fn probe_command(cli: &str) -> Result<String, String> {
    if cli.is_empty()
        || !cli
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        || cli.starts_with('-')
    {
        return Err(format!("“{cli}” is not a command name"));
    }
    Ok(format!(
        "printf '%s\\n' {canary}; \
         exec /bin/sh -c 'printf \"{PROBE_SHELL}%s\\n\" \"$SHELL\"; case \"${{SHELL##*/}}\" in \
         {shells}) ;; *) exit 0;; esac; exec \"$SHELL\" -lc \"printf \\\"{PROBE_PATH}%s\\\\n\\\" \
         \\\"\\$PATH\\\"; printf \\\"{PROBE_CLI}%s\\\\n\\\" \\\"\\$(command -v {cli})\\\"\" \
         </dev/null'",
        canary = sh_quote(&format!("{PROBE_QUOTE}{QUOTE_CANARY}")),
        shells = POSIX_SHELLS.join("|")
    ))
}

/// What the probe printed, as a [`RemoteProbe`], or why the agent cannot run there: a login
/// shell not in [`POSIX_SHELLS`] (the thread's command line is POSIX), a quoting canary that did
/// not come back exactly (the shell parsing the command line does not quote as POSIX does,
/// whatever `$SHELL` names), no `PATH`, or `cli` not found on it (or not a plain absolute path).
/// What the other machine printed is redacted before it is quoted.
pub fn parse_probe(cli: &str, stdout: &str) -> Result<RemoteProbe, String> {
    let field = |marker: &str| {
        stdout
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(marker))
            .map(str::to_owned)
    };
    let shell = field(PROBE_SHELL)
        .ok_or_else(|| "The other machine did not say what its login shell is".to_owned())?;
    if !posix_shell(&shell) {
        let name = crate::text::truncate_bytes(
            &crate::text::without_hidden(&crate::redact::redact(
                shell.rsplit('/').next().unwrap_or_default(),
            )),
            60,
        )
        .to_owned();
        return Err(format!(
            "The login shell there is “{name}”; a thread on another machine needs a POSIX \
             login shell ({})",
            POSIX_SHELLS.join(", ")
        ));
    }
    if field(PROBE_QUOTE).as_deref() != Some(QUOTE_CANARY) {
        return Err(format!(
            "The shell that reads commands there does not quote as a POSIX shell does; a thread \
             on another machine needs a POSIX login shell ({})",
            POSIX_SHELLS.join(", ")
        ));
    }
    let path = field(PROBE_PATH).unwrap_or_default();
    if path.is_empty() || path.len() > MAX_PATH_LEN || crate::text::has_control_or_bidi(&path, &[])
    {
        return Err("The login shell there reported no usable PATH".into());
    }
    let found = field(PROBE_CLI).unwrap_or_default();
    if found.is_empty() {
        return Err(format!(
            "{cli} was not found on the login shell's PATH there; install it, or put it on \
             the PATH a login shell gets"
        ));
    }
    if !plain_cli_path(&found) {
        let shown = crate::text::truncate_bytes(
            &crate::text::without_hidden(&crate::redact::redact(&found)),
            120,
        )
        .to_owned();
        return Err(format!("{cli} there is not a program file ({shown})"));
    }
    Ok(RemoteProbe {
        shell: crate::text::without_hidden(&shell),
        path,
        cli: found,
    })
}

/// Whether a probed CLI path can follow `env` in the command line: absolute, bounded, with no
/// control or invisible character and no `=` (which `env` would take for an assignment).
fn plain_cli_path(cli: &str) -> bool {
    cli.starts_with('/')
        && cli.len() <= MAX_DIR_LEN
        && !cli.contains('=')
        && !crate::text::has_control_or_bidi(cli, &[])
        && !crate::text::has_hidden(cli)
}

/// Whether an adapter argument (a model, a session id, a flag) can go on the command line
/// there: no control, bidi or invisible character, a NUL above all, which no argv can carry.
/// A model or session id the other machine reported comes back as one of these.
pub fn plain_arg(arg: &str) -> bool {
    !crate::text::has_control_or_bidi(arg, &[]) && !crate::text::has_hidden(arg)
}

/// The remote command of the folder check: `cd -- '<dir>' && pwd -P` (the folder as typed,
/// `~` allowed), which prints the canonical absolute path a thread then keeps.
pub fn folder_command(dir: &str) -> Result<String, String> {
    let dir = parse_remote_dir(dir)?;
    Ok(format!("cd -- {} && pwd -P", cd_operand(&dir)))
}

/// The folder the check printed: its last line, which must be an absolute path.
pub fn parse_folder(stdout: &str) -> Result<String, String> {
    let line = stdout.lines().rev().find(|l| !l.is_empty()).unwrap_or("");
    validate_absolute(line).map_err(|_| "The folder there could not be read".to_owned())?;
    Ok(line.to_owned())
}

/// The argv of an `ssh` with no terminal ([`BATCH_OPTIONS`]) running `remote` (one POSIX line)
/// on `alias`.
pub fn ssh_batch_argv(alias: &str, remote: String) -> Result<Vec<String>, String> {
    validate_alias(alias)?;
    let mut argv = vec!["ssh".to_owned()];
    argv.extend(BATCH_OPTIONS.iter().map(|s| (*s).to_owned()));
    argv.push("--".to_owned());
    argv.push(alias.to_owned());
    argv.push(remote);
    Ok(argv)
}

/// The argv of a chat thread's agent on another machine: `ssh` + [`BATCH_OPTIONS`] + `--` + the
/// alias + `cd -- '<dir>' && exec env PATH='<probed>' '<cli>' '<arg>'…`, every value quoted.
/// `args` are the adapter's arguments after its program, which the probed CLI replaces. Refused
/// unless the probe found a POSIX login shell ([`posix_shell`]: the quoting is POSIX), a plain
/// CLI path and a usable `PATH`, and every argument is [`plain_arg`].
pub fn ssh_chat_argv(
    folder: &RemoteFolder,
    probe: &RemoteProbe,
    args: &[String],
) -> Result<Vec<String>, String> {
    if !posix_shell(&probe.shell) {
        return Err("The login shell there is not a POSIX shell".into());
    }
    if !plain_cli_path(&probe.cli) {
        return Err("The agent's path there is not a plain absolute path".into());
    }
    if probe.path.is_empty()
        || probe.path.len() > MAX_PATH_LEN
        || crate::text::has_control_or_bidi(&probe.path, &[])
    {
        return Err("The PATH there is not usable".into());
    }
    if !args.iter().all(|a| plain_arg(a)) {
        return Err("An argument holds a control or invisible character".into());
    }
    let mut line = format!(
        "cd -- {} && exec env {} {}",
        sh_quote(&folder.dir),
        sh_quote(&format!("PATH={}", probe.path)),
        sh_quote(&probe.cli)
    );
    for arg in args {
        line.push(' ');
        line.push_str(&sh_quote(arg));
    }
    ssh_batch_argv(&folder.alias, line)
}

/// Why a batch `ssh` failed, read from its exit and standard error, and whether a terminal
/// page there would get past it (the user answering a host-key question or a password, which a
/// batch `ssh` refuses to ask).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchFailure {
    pub reason: String,
    pub needs_terminal: bool,
}

/// A batch `ssh`'s failure: the redacted, bounded reason [`check_outcome`] gives, and whether
/// ssh stopped at something only a person can answer.
///
/// The standard error of a batch `ssh` also carries what the command there wrote (the agent CLI,
/// the remote shell), which can say anything, "Permission denied" included. So for ssh's own
/// failure (exit 255) both the reason and whether a terminal would help are read only from the
/// lines ssh itself writes ([`ssh_own_line`]); for any other exit, the command there failed and
/// no terminal is offered.
pub fn batch_failure(code: Option<i32>, stderr: &str) -> BatchFailure {
    // Called for a failure: a zero exit with nothing to show still reads as one.
    let code = if code == Some(0) { Some(1) } else { code };
    if !ssh_failed(code) {
        let reason = match check_outcome(code, stderr) {
            RemoteCheck::Unreachable(why) => why,
            _ => "ssh failed".to_owned(),
        };
        return BatchFailure {
            reason,
            needs_terminal: false,
        };
    }
    let own: Vec<&str> = stderr.lines().filter(|l| ssh_own_line(l)).collect();
    let own = own.join("\n");
    let reason = match check_outcome(code, &own) {
        RemoteCheck::Unreachable(why) => why,
        _ => "ssh failed".to_owned(),
    };
    let lower = own.to_ascii_lowercase();
    let needs_terminal = [
        "host key verification failed",
        "remote host identification has changed",
        " host key is known for ",
        ": permission denied (",
        "no more authentication methods",
        "passphrase",
        "keyboard-interactive",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    BatchFailure {
        reason,
        needs_terminal,
    }
}

/// The starts of the lines OpenSSH writes itself when it fails (lower case), beside those it
/// prefixes with `ssh:`.
const SSH_LINE_STARTS: &[&str] = &[
    "host key verification failed",
    "@@@@@@@@",
    "@    warning: remote host identification has changed",
    "it is also possible that a host key has just been changed",
    "received disconnect from ",
    "disconnected from ",
    "connection closed by ",
    "connection reset by ",
    "connection timed out during banner exchange",
    "kex_exchange_identification:",
    "ssh_exchange_identification:",
    "banner exchange:",
    "unable to negotiate with ",
    "too many authentication failures",
    "no more authentication methods",
    "load key ",
    "bad owner or permissions on ",
];

/// Whether a line of a batch `ssh`'s standard error is ssh's own (`ssh: …`, or one of the
/// messages OpenSSH writes unprefixed: [`SSH_LINE_STARTS`], `<user>@<host>: Permission denied
/// (…)`, `No <type> host key is known for …`, a configuration file's error), never the command's
/// there.
pub fn ssh_own_line(line: &str) -> bool {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("ssh: ") || SSH_LINE_STARTS.iter().any(|s| lower.starts_with(s)) {
        return true;
    }
    if lower.starts_with("no ") && lower.contains(" host key is known for ") {
        return true;
    }
    // `<user>@<host>: Permission denied (publickey,password).`: the destination is one word with
    // an `@`, and the parentheses hold only authentication method names (not, say, a program's
    // `Permission denied (os error 13)`).
    if let Some((who, rest)) = line.split_once(": ") {
        let one_word = !who.is_empty() && !who.contains(char::is_whitespace);
        let methods = rest
            .strip_prefix("Permission denied (")
            .and_then(|r| r.strip_suffix(")."));
        if one_word
            && who.contains('@')
            && methods.is_some_and(|m| {
                !m.is_empty()
                    && m.chars()
                        .all(|c| c.is_ascii_lowercase() || matches!(c, ',' | '-' | '@' | '.'))
            })
        {
            return true;
        }
        // `/home/me/.ssh/config: line 3: Bad configuration option: x`, and its `terminating`.
        if one_word
            && (who.starts_with('/') || who.starts_with('~'))
            && rest
                .to_ascii_lowercase()
                .contains("bad configuration option")
        {
            return true;
        }
    }
    false
}

/// What a thread on another machine has of each part of a local thread. The one list docs,
/// controls and tests read; [`support`] has no wildcard, so a new part does not compile until it
/// says what a remote thread does with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Subsystem {
    Agent,
    Stop,
    Approvals,
    SessionRules,
    AlwaysRules,
    Checkpoints,
    DiffPanel,
    Undo,
    Worktree,
    Fork,
    HandoffBrief,
    SessionPickers,
    ProfileEnvFile,
    ModelProvider,
    Delegation,
    Images,
    FileLinks,
    WorkspaceFiles,
    VendorProbe,
    Drawer,
    IdleSuspend,
}

impl Subsystem {
    pub const ALL: [Subsystem; 21] = [
        Subsystem::Agent,
        Subsystem::Stop,
        Subsystem::Approvals,
        Subsystem::SessionRules,
        Subsystem::AlwaysRules,
        Subsystem::Checkpoints,
        Subsystem::DiffPanel,
        Subsystem::Undo,
        Subsystem::Worktree,
        Subsystem::Fork,
        Subsystem::HandoffBrief,
        Subsystem::SessionPickers,
        Subsystem::ProfileEnvFile,
        Subsystem::ModelProvider,
        Subsystem::Delegation,
        Subsystem::Images,
        Subsystem::FileLinks,
        Subsystem::WorkspaceFiles,
        Subsystem::VendorProbe,
        Subsystem::Drawer,
        Subsystem::IdleSuspend,
    ];
}

/// How a thread on another machine has a [`Subsystem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// As a local thread has it.
    Full,
    /// Has it, differently: what differs.
    Degraded(&'static str),
    /// Does not have it: why, as its control's tooltip says.
    Unavailable(&'static str),
}

/// What a control on a remote thread says when it is insensitive.
pub const NOT_ON_REMOTE: &str = "Not available for a folder on another machine";

/// How a thread on another machine has `part`.
pub fn support(part: Subsystem) -> Support {
    match part {
        Subsystem::Agent => Support::Degraded(
            "Claude and Codex only, run with ssh in batch mode by their absolute path and the \
             PATH a login shell has there",
        ),
        Subsystem::Stop => Support::Degraded(
            "the agent's own interrupt over its input; a tool it started there may keep running",
        ),
        Subsystem::Approvals => Support::Degraded("Allow once and Deny only"),
        Subsystem::SessionRules | Subsystem::AlwaysRules => Support::Unavailable(
            "rules are judged against this machine's files, so none is offered or applied",
        ),
        Subsystem::Checkpoints
        | Subsystem::DiffPanel
        | Subsystem::Undo
        | Subsystem::Worktree
        | Subsystem::HandoffBrief
        | Subsystem::SessionPickers
        | Subsystem::WorkspaceFiles => Support::Unavailable(NOT_ON_REMOTE),
        Subsystem::Fork => {
            Support::Degraded("a fork keeps the folder there and takes no baseline to compare")
        }
        Subsystem::ProfileEnvFile | Subsystem::ModelProvider => {
            Support::Unavailable("a profile's environment file and model server are this machine's")
        }
        Subsystem::Delegation => {
            Support::Unavailable("sub-agents run on this machine, not beside the remote agent")
        }
        Subsystem::Images => Support::Degraded(CODEX_IMAGES_REFUSED),
        Subsystem::FileLinks => Support::Degraded("only web links open; a path names a file there"),
        Subsystem::VendorProbe => Support::Unavailable(
            "this machine's account and usage say nothing of the account there",
        ),
        Subsystem::Drawer => Support::Degraded("an ssh terminal in the same folder there"),
        Subsystem::IdleSuspend => Support::Full,
    }
}

/// The longest link a remote page opens.
pub const MAX_LINK_LEN: usize = 4096;

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
            "HTTPS://Example.com/path",
            "https://example.com#top",
            "https://example.com/a@b",
        ] {
            assert!(remote_link_allowed(ok), "{ok}");
        }
        let long = format!("https://example.com/{}", "a".repeat(5_000));
        assert!(long.len() > MAX_LINK_LEN);
        assert!(!remote_link_allowed(&long), "a 5,000-byte link");
        for refused in [
            "\nhttps://example.com",
            "https://example.com\n",
            "https://example.com/a b",
            " https://example.com",
            "https://example.com\t",
            "https://?q",
            "https://#f",
            "https://:80",
            "https://:80/x",
            "https://user@",
            "https://user:pass@example.com/",
            "https://trusted.com@evil.example",
            "https://example.com\u{a0}/x",
            "https://exa\u{3164}mple.com",
            "https://exa\u{200b}mple.com",
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

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn probe() -> RemoteProbe {
        RemoteProbe {
            shell: "/bin/bash".into(),
            path: "/home/me/.local/bin:/usr/bin".into(),
            cli: "/home/me/.local/bin/claude".into(),
        }
    }

    #[test]
    fn a_chat_process_runs_batch_ssh_with_no_forwarding_and_dashdash_before_the_host() {
        let folder = RemoteFolder::new("box", "/srv/app").unwrap();
        let argv = ssh_chat_argv(&folder, &probe(), &strings(&["--verbose"])).unwrap();
        let mut expected = strings(&[
            "ssh",
            "-T",
            "-o",
            "BatchMode=yes",
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
            "--",
            "box",
        ]);
        expected.push(
            "cd -- '/srv/app' && exec env 'PATH=/home/me/.local/bin:/usr/bin' \
             '/home/me/.local/bin/claude' '--verbose'"
                .to_owned(),
        );
        assert_eq!(argv, expected);
        // Never a terminal, never a prompt, never a forwarding flag.
        for flag in [
            "-t", "-tt", "-L", "-R", "-D", "-A", "-X", "-Y", "-W", "-M", "-f", "-N", "-w",
        ] {
            assert!(!argv.iter().any(|a| a == flag), "{flag}");
        }
        assert!(!argv.iter().any(|a| a == "BatchMode=no"));
        // The probe and the folder check take the same options.
        let check = ssh_batch_argv("box", folder_command("~").unwrap()).unwrap();
        assert_eq!(check[..check.len() - 1], expected[..expected.len() - 1]);
        // An alias ssh would read as an option is refused before any argv exists.
        assert!(RemoteFolder::new("-oProxyCommand=x", "/srv").is_err());
        assert!(ssh_batch_argv("-v", "true".into()).is_err());
    }

    #[test]
    fn a_chat_command_quotes_hostile_folders_paths_and_arguments() {
        if std::process::Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .is_err()
        {
            eprintln!("no sh; skipping");
            return;
        }
        let base = std::env::temp_dir().join(format!("agent-core-chat-{}", std::process::id()));
        let tricky = "a b 'c' $x \"d\" `e` ;f && $(id) |g";
        let dir = base.join(tricky);
        std::fs::create_dir_all(&dir).unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        // A fake "cli" that prints where it runs, its PATH and its arguments.
        let cli = base.join("it's a cli");
        std::fs::write(
            &cli,
            "#!/bin/sh\npwd -P\nprintf '%s\\n' \"$PATH\"\nfor a in \"$@\"; do printf '[%s]' \"$a\"; done\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let folder = RemoteFolder::new("box", canonical.to_str().unwrap()).unwrap();
        let probe = RemoteProbe {
            shell: "/bin/sh".into(),
            path: "/usr/bin:/bin:/odd 'dir' $x".into(),
            cli: cli.to_str().unwrap().into(),
        };
        let args = strings(&["one two", "$x", "it's", "\"q\"", "`id`", "$(id)", "a;b"]);
        let argv = ssh_chat_argv(&folder, &probe, &args).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(argv.last().unwrap())
            .env("x", "EXPANDED")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!(
                "{}\n/usr/bin:/bin:/odd 'dir' $x\n[one two][$x][it's][\"q\"][`id`][$(id)][a;b]",
                canonical.display()
            )
        );
        std::fs::remove_dir_all(&base).unwrap();

        // A probe that reported something unusable never makes a command line.
        let mut bad = probe.clone();
        bad.cli = "claude".into();
        assert!(ssh_chat_argv(&folder, &bad, &[]).is_err());
        let mut bad = probe;
        bad.path = "/usr/bin\n:/x".into();
        assert!(ssh_chat_argv(&folder, &bad, &[]).is_err());
    }

    #[test]
    fn a_thread_folder_round_trips_and_a_bad_remote_one_is_never_local() {
        let folder = ThreadFolder::parse("ssh://box/srv/my app").unwrap();
        let remote = folder.remote().unwrap();
        assert_eq!((remote.alias(), remote.dir()), ("box", "/srv/my app"));
        assert_eq!(folder.cwd(), "ssh://box/srv/my app");
        assert_eq!(remote.title(), "box:/srv/my app");
        assert_eq!(
            ThreadFolder::parse("ssh://me@fe80::1/").unwrap().cwd(),
            "ssh://me@fe80::1/"
        );
        assert_eq!(
            ThreadFolder::parse("/home/me/src").unwrap(),
            ThreadFolder::Local("/home/me/src".into())
        );
        for bad in [
            "ssh://",
            "ssh://box",
            "ssh:///srv",
            "ssh://-oProxyCommand=x/srv",
            "ssh://box;id/srv",
            "ssh://box/srv\nx",
            "ssh://box/a\u{202e}b",
        ] {
            assert!(ThreadFolder::parse(bad).is_err(), "{bad:?}");
            assert!(is_remote_cwd(bad), "{bad:?}");
        }
        assert!(!is_remote_cwd("/srv"));
        assert!(RemoteFolder::new("box", "~/src").is_err(), "only absolute");
    }

    /// Only a POSIX login shell runs a thread's command line: fish reads `\'` inside single
    /// quotes as an escaped quote, so a value could close [`sh_quote`]'s quoting there.
    #[test]
    fn only_a_posix_login_shell_is_accepted() {
        let reply = |shell: &str| {
            format!(
                "{}{PROBE_SHELL}{shell}\n{PROBE_PATH}/usr/bin\n{PROBE_CLI}/usr/bin/claude\n",
                canary_line()
            )
        };
        for ok in [
            "/bin/sh",
            "/bin/bash",
            "/usr/bin/zsh",
            "/bin/dash",
            "/bin/ksh",
            "/bin/ash",
        ] {
            assert!(parse_probe("claude", &reply(ok)).is_ok(), "{ok}");
        }
        for refused in [
            "/usr/bin/fish",
            "/usr/bin/nu",
            "/usr/bin/pwsh",
            "/usr/bin/elvish",
            "/usr/bin/xonsh",
            "/bin/csh",
            "/bin/tcsh",
            "/opt/bin/mysh",
            "",
        ] {
            let why = parse_probe("claude", &reply(refused)).unwrap_err();
            assert!(why.contains("POSIX"), "{refused}: {why}");
        }
        // A probe naming such a shell never makes a command line, however it was made.
        let folder = RemoteFolder::new("box", "/srv").unwrap();
        for shell in [
            "/usr/bin/fish",
            "/usr/bin/nu",
            "/usr/bin/pwsh",
            "/opt/bin/unknown",
        ] {
            let probe = RemoteProbe {
                shell: shell.into(),
                ..probe()
            };
            assert!(ssh_chat_argv(&folder, &probe, &[]).is_err(), "{shell}");
        }
        // The probe script itself stops for anything not listed (checked in a local sh).
        if std::process::Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .is_ok()
        {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(probe_command("claude").unwrap())
                .env("SHELL", "/usr/bin/fish")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(stdout.contains(PROBE_SHELL), "{stdout}");
            assert!(!stdout.contains(PROBE_PATH), "fish is never run: {stdout}");
        }
    }

    /// A CLI path with `=` would be read by `env` as an assignment, and an argument with a NUL
    /// or a control character (a model or session id the other machine reported) can be no argv.
    #[test]
    fn a_cli_with_an_equals_sign_or_an_argument_with_a_control_character_is_refused() {
        let folder = RemoteFolder::new("box", "/srv").unwrap();
        let assigned = RemoteProbe {
            cli: "/opt/A=B/claude".into(),
            ..probe()
        };
        assert!(ssh_chat_argv(&folder, &assigned, &[]).is_err());
        let reply = format!(
            "{}{PROBE_SHELL}/bin/bash\n{PROBE_PATH}/usr/bin\n{PROBE_CLI}/x=y/codex\n",
            canary_line()
        );
        assert!(parse_probe("codex", &reply)
            .unwrap_err()
            .contains("not a program file"));
        for bad in [
            "sess\0ion",
            "model\nname",
            "a\u{202e}b",
            "a\u{200b}b",
            "x\u{1b}[2J",
        ] {
            assert!(
                ssh_chat_argv(&folder, &probe(), &[bad.to_owned()]).is_err(),
                "{bad:?}"
            );
            assert!(!plain_arg(bad));
        }
        assert!(ssh_chat_argv(&folder, &probe(), &["--model".into(), "opus".into()]).is_ok());
    }

    #[test]
    fn an_alias_holds_at_most_one_at_with_a_user_and_a_host_neither_an_option() {
        for bad in [
            "me@-v",
            "@host",
            "user@",
            "a@b@c",
            "-u@host",
            "me@-oProxyCommand=x",
        ] {
            assert!(validate_alias(bad).is_err(), "{bad}");
            assert!(ssh_batch_argv(bad, "true".into()).is_err(), "{bad}");
        }
        for ok in ["me@box", "me@fe80::1", "box", "me.name@host-1.example"] {
            assert!(validate_alias(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn only_a_listed_machine_is_reached() {
        let hosts = vec![
            RemoteHost::new("box", "").unwrap(),
            RemoteHost {
                alias: "-oProxyCommand=x".into(),
                label: String::new(),
            },
        ];
        assert!(listed(&hosts, "box").is_ok());
        assert!(listed(&hosts, "other").unwrap_err().contains("not listed"));
        assert!(
            listed(&hosts, "-oProxyCommand=x").is_err(),
            "invalid even if stored"
        );
        assert!(listed(&[], "box").is_err());
    }

    #[test]
    fn only_claude_and_codex_run_on_another_machine() {
        use crate::adapter::Driver;
        assert_eq!(chat_cli(Driver::Claude), Ok("claude"));
        assert_eq!(chat_cli(Driver::Codex), Ok("codex"));
        assert!(chat_cli(Driver::Agy).unwrap_err().contains("Antigravity"));
        assert!(chat_cli(Driver::Local).unwrap_err().contains("Local agent"));
        assert!(takes_images(Driver::Claude).is_ok());
        assert_eq!(takes_images(Driver::Codex), Err(CODEX_IMAGES_REFUSED));
    }

    #[test]
    fn a_remote_card_offers_allow_once_and_deny_only() {
        use crate::event::Decision;
        let mut options = vec![
            Decision::Allow,
            Decision::AllowForSession,
            Decision::AllowAlways,
            Decision::Deny,
            Decision::Cancel,
        ];
        approval_options(&mut options);
        assert_eq!(options, vec![Decision::Allow, Decision::Deny]);
        let mut options = vec![Decision::Allow, Decision::Cancel];
        approval_options(&mut options);
        assert_eq!(options, vec![Decision::Allow, Decision::Deny]);
    }

    #[test]
    fn the_probe_parses_in_sh_and_refuses_csh_a_missing_cli_and_junk() {
        assert!(probe_command("claude").is_ok());
        for bad in ["", "-x", "a b", "a;b", "$(id)", "a'b"] {
            assert!(probe_command(bad).is_err(), "{bad:?}");
        }
        let command = probe_command("claude").unwrap();
        assert!(!command.contains('!'));
        // The login shell first prints the canary as `sh_quote` quotes it; then the script, inside
        // whose outer single quotes there is no other single quote (csh reads it too).
        let canary = format!(
            "printf '%s\\n' {}; ",
            sh_quote(&format!("{PROBE_QUOTE}{QUOTE_CANARY}"))
        );
        assert_eq!(
            canary,
            r"printf '%s\n' '__agent_terminal_quote=a'\''b c\\d'; "
        );
        let inner = command
            .strip_prefix(canary.as_str())
            .and_then(|r| r.strip_prefix("exec /bin/sh -c '"))
            .and_then(|r| r.strip_suffix('\''))
            .unwrap();
        assert!(!inner.contains('\''));
        // Run as a POSIX login shell would, with a fake `claude` on the login PATH.
        if std::process::Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .is_ok()
        {
            let base =
                std::env::temp_dir().join(format!("agent-core-probe-{}", std::process::id()));
            let bin = base.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let fake = bin.join("claude");
            std::fs::write(&fake, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = format!("{}:/usr/bin:/bin", bin.display());
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .env("SHELL", "/bin/sh")
                .env("PATH", &path)
                .env("HOME", &base)
                .env_remove("ENV")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let found = parse_probe("claude", &stdout).unwrap();
            assert_eq!(found.cli, fake.to_str().unwrap());
            assert_eq!(found.shell, "/bin/sh");
            assert!(found.path.starts_with(&bin.display().to_string()));
            std::fs::remove_dir_all(&base).unwrap();
        }
        let quote = canary_line();
        let ok = format!(
            "motd\n{quote}{PROBE_SHELL}/usr/bin/zsh\n{PROBE_PATH}/usr/bin\n{PROBE_CLI}/usr/bin/codex\n"
        );
        assert_eq!(parse_probe("codex", &ok).unwrap().cli, "/usr/bin/codex");
        let csh = format!("{quote}{PROBE_SHELL}/bin/tcsh\n");
        assert!(parse_probe("codex", &csh).unwrap_err().contains("tcsh"));
        let missing = format!("{quote}{PROBE_SHELL}/bin/bash\n{PROBE_PATH}/usr/bin\n{PROBE_CLI}\n");
        assert!(parse_probe("codex", &missing)
            .unwrap_err()
            .contains("not found"));
        let alias = format!(
            "{quote}{PROBE_SHELL}/bin/bash\n{PROBE_PATH}/usr/bin\n{PROBE_CLI}alias codex=x\n"
        );
        assert!(parse_probe("codex", &alias).is_err());
        assert!(parse_probe("codex", "").is_err());
    }

    /// What a POSIX login shell prints for the probe's quoting canary.
    fn canary_line() -> String {
        format!("{PROBE_QUOTE}{QUOTE_CANARY}\n")
    }

    /// `$SHELL` is only what the account says: the canary is what the shell that parses the
    /// command line printed. fish reads `\\` inside single quotes as one backslash (and would
    /// read `\'` as a quote that does not close), so its transcript is refused even under a
    /// `$SHELL` naming bash; so is a transcript with no canary, or a changed one.
    #[test]
    fn the_probe_refuses_a_shell_that_does_not_quote_as_posix_does() {
        let rest =
            format!("{PROBE_SHELL}/bin/bash\n{PROBE_PATH}/usr/bin\n{PROBE_CLI}/usr/bin/claude\n");
        assert!(parse_probe("claude", &format!("{}{rest}", canary_line())).is_ok());
        let fish = format!("{PROBE_QUOTE}a'b c\\d\n{rest}");
        let why = parse_probe("claude", &fish).unwrap_err();
        assert!(why.contains("does not quote as a POSIX shell"), "{why}");
        for other in [
            String::new(),
            format!("{PROBE_QUOTE}\n"),
            format!("{PROBE_QUOTE}a'\\''b c\\\\d\n"),
            format!("{PROBE_QUOTE}ab c\\\\d\n"),
            format!("{PROBE_QUOTE}{QUOTE_CANARY} \n"),
        ] {
            assert!(
                parse_probe("claude", &format!("{other}{rest}")).is_err(),
                "{other:?}"
            );
        }
        // A real POSIX shell parsing the probe's first command prints the canary exactly.
        if std::process::Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .is_ok()
        {
            let command = probe_command("claude").unwrap();
            let first = command.split_once("; exec ").unwrap().0;
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(first)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), canary_line());
        }
    }

    #[test]
    fn the_folder_check_prints_the_canonical_path() {
        assert_eq!(
            folder_command("~/src").unwrap(),
            r#"cd -- "$HOME"/'src' && pwd -P"#
        );
        assert!(folder_command("~root").is_err());
        assert_eq!(parse_folder("motd\n/srv/app\n").unwrap(), "/srv/app");
        assert!(parse_folder("relative\n").is_err());
        assert!(parse_folder("").is_err());
    }

    #[test]
    fn a_batch_failure_says_when_a_terminal_would_get_past_it() {
        let key = batch_failure(Some(255), "Host key verification failed.\n");
        assert!(key.needs_terminal);
        assert_eq!(key.reason, "Host key verification failed.");
        assert!(batch_failure(Some(255), "me@box: Permission denied (publickey).").needs_terminal);
        let dns = batch_failure(Some(255), "ssh: Could not resolve hostname box");
        assert!(!dns.needs_terminal);
        let token = "ghp_abcdefghijklmnopqrstuvwxyz0123";
        assert!(!batch_failure(Some(1), &format!("proxy {token}"))
            .reason
            .contains(token));
        assert!(ssh_failed(Some(255)));
        assert!(!ssh_failed(Some(1)));
        assert_eq!(unreachable_notice("box"), "Could not reach box.");
    }

    /// What the agent CLI or the shell there wrote to the same standard error never decides why
    /// ssh failed, nor whether a terminal would help: only ssh's own lines do.
    #[test]
    fn a_batch_failure_reads_only_the_lines_ssh_writes_itself() {
        // The remote CLI saying "Permission denied" and a fake key prompt, then exiting 255.
        let cli = "Error: /srv/app/.env: Permission denied (os error 13)\n\
                   claude: Permission denied\n\
                   Enter passphrase for key: host key verification failed\n";
        let failure = batch_failure(Some(255), cli);
        assert!(!failure.needs_terminal, "{failure:?}");
        assert_eq!(failure.reason, "ssh exited with status 255");
        // The same text from a command that failed otherwise offers no terminal either.
        assert!(!batch_failure(Some(1), cli).needs_terminal);
        // ssh's own line after the CLI's: it alone is the reason, and it is answered there.
        let mixed =
            format!("{cli}me@box: Permission denied (publickey,password).\nlast cli line\n");
        let failure = batch_failure(Some(255), &mixed);
        assert!(failure.needs_terminal);
        assert_eq!(
            failure.reason,
            "me@box: Permission denied (publickey,password)."
        );
        let dns = batch_failure(
            Some(255),
            "codex: retrying\nssh: Could not resolve hostname box: Name or service not known\n",
        );
        assert!(!dns.needs_terminal);
        assert!(dns.reason.starts_with("ssh: Could not resolve"), "{dns:?}");
        for own in [
            "ssh: connect to host box port 22: Connection refused",
            "Host key verification failed.",
            "No ED25519 host key is known for box and you have requested strict checking.",
            "Connection closed by 192.0.2.7 port 22",
            "kex_exchange_identification: read: Connection reset by peer",
            "/home/me/.ssh/config: line 3: Bad configuration option: frob",
        ] {
            assert!(ssh_own_line(own), "{own}");
        }
        for theirs in [
            "Permission denied",
            "Error: Permission denied (os error 13)",
            "claude: Permission denied (publickey).",
            "the host key verification failed for npm",
            "",
        ] {
            assert!(!ssh_own_line(theirs), "{theirs}");
        }
    }

    /// `Subsystem::ALL` is hand-maintained: `next` has no wildcard, so a new part does not
    /// compile until it is chained in here, and walking the chain must give exactly `ALL`.
    #[test]
    fn every_subsystem_says_what_a_remote_thread_has() {
        use Subsystem::*;
        fn next(s: Subsystem) -> Option<Subsystem> {
            match s {
                Agent => Some(Stop),
                Stop => Some(Approvals),
                Approvals => Some(SessionRules),
                SessionRules => Some(AlwaysRules),
                AlwaysRules => Some(Checkpoints),
                Checkpoints => Some(DiffPanel),
                DiffPanel => Some(Undo),
                Undo => Some(Worktree),
                Worktree => Some(Fork),
                Fork => Some(HandoffBrief),
                HandoffBrief => Some(SessionPickers),
                SessionPickers => Some(ProfileEnvFile),
                ProfileEnvFile => Some(ModelProvider),
                ModelProvider => Some(Delegation),
                Delegation => Some(Images),
                Images => Some(FileLinks),
                FileLinks => Some(WorkspaceFiles),
                WorkspaceFiles => Some(VendorProbe),
                VendorProbe => Some(Drawer),
                Drawer => Some(IdleSuspend),
                IdleSuspend => None,
            }
        }
        let mut walked = vec![Agent];
        while let Some(n) = next(*walked.last().unwrap()) {
            walked.push(n);
        }
        assert_eq!(walked, Subsystem::ALL);
        for part in Subsystem::ALL {
            match support(part) {
                Support::Full => {}
                Support::Degraded(why) | Support::Unavailable(why) => {
                    assert!(!why.is_empty(), "{part:?}")
                }
            }
        }
        for local_only in [
            Checkpoints,
            DiffPanel,
            Undo,
            Worktree,
            HandoffBrief,
            SessionPickers,
        ] {
            assert_eq!(support(local_only), Support::Unavailable(NOT_ON_REMOTE));
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
