//! A commit the user writes in the diff panel: checking its message, the prompt that asks the
//! thread's agent to draft one, and the command line the confirmation shows.
//!
//! Pure: the commit itself runs in `agent-kit`'s `commit`.

use crate::remote::sh_quote;

/// Past this many characters a subject line is flagged (never refused).
pub const SUBJECT_LIMIT: usize = 72;

/// At most this many paths are named in a draft prompt; the rest are counted.
pub const PROMPT_PATHS: usize = 20;

/// What the commit dialog says about a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCheck {
    /// Nothing but whitespace: git would refuse it, so Commit is off.
    pub empty: bool,
    /// The subject's length in characters, when it is over [`SUBJECT_LIMIT`].
    pub long_subject: Option<usize>,
    /// The message holds something [`crate::redact::redact`] would mask. It is committed as
    /// typed, but only after a second confirmation.
    pub looks_secret: bool,
}

/// Checks `message`, as typed.
pub fn check(message: &str) -> MessageCheck {
    let subject = subject(message);
    let length = subject.chars().count();
    MessageCheck {
        empty: message.trim().is_empty(),
        long_subject: (length > SUBJECT_LIMIT).then_some(length),
        looks_secret: crate::redact::redact(message) != message,
    }
}

/// The message's first non-blank line, trimmed.
pub fn subject(message: &str) -> &str {
    message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// The prompt "Draft Message" puts in the thread's composer: it names the files and never pastes
/// their diff, so it gives the agent nothing it cannot already read. The user reviews and sends it.
pub fn draft_prompt(paths: &[String]) -> String {
    let named: Vec<&str> = paths
        .iter()
        .take(PROMPT_PATHS)
        .map(String::as_str)
        .collect();
    let mut files = named.join(", ");
    let more = paths.len().saturating_sub(PROMPT_PATHS);
    if more > 0 {
        files.push_str(&format!(" and {more} more"));
    }
    format!(
        "Write a commit message for the uncommitted changes in {files}: a subject line of at most \
         {SUBJECT_LIMIT} characters, a blank line, then a short body if it helps. Reply with only \
         the message."
    )
}

/// `git` and `args` as one line a POSIX shell would read back as the same argument list: each
/// argument that is not plainly safe is single-quoted. For showing a command, never for running
/// one.
pub fn shown_command(args: &[String]) -> String {
    let mut line = String::from("git");
    for arg in args {
        line.push(' ');
        let plain = !arg.is_empty()
            && arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+,".contains(c));
        if plain {
            line.push_str(arg);
        } else {
            line.push_str(&sh_quote(arg));
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_check_flags_long_subject_and_secret_shaped_text() {
        let fine = check("Fix the thing\n\nIt was broken.");
        assert_eq!(
            fine,
            MessageCheck {
                empty: false,
                long_subject: None,
                looks_secret: false
            }
        );
        assert!(check("  \n\t\n").empty);
        assert!(check("").empty);

        let long = "x".repeat(SUBJECT_LIMIT + 1);
        assert_eq!(check(&long).long_subject, Some(SUBJECT_LIMIT + 1));
        assert_eq!(check(&"x".repeat(SUBJECT_LIMIT)).long_subject, None);
        // The subject is the first line that is not blank; the body may be long.
        let body = format!("\n\nShort\n\n{long}");
        assert_eq!(check(&body).long_subject, None);
        assert_eq!(subject(&body), "Short");
        // Characters, not bytes.
        assert_eq!(check(&"é".repeat(SUBJECT_LIMIT)).long_subject, None);

        let secret = "Add config\n\nkey: ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        assert!(check(secret).looks_secret);
    }

    #[test]
    fn the_draft_prompt_names_the_files_and_caps_the_list() {
        let prompt = draft_prompt(&["src/a.rs".into(), "README.md".into()]);
        assert!(prompt.contains("src/a.rs, README.md"));
        assert!(prompt.contains("Reply with only the message."));
        let many: Vec<String> = (0..PROMPT_PATHS + 3).map(|i| format!("f{i}")).collect();
        let prompt = draft_prompt(&many);
        assert!(prompt.contains(&format!("f{}", PROMPT_PATHS - 1)));
        assert!(!prompt.contains(&format!("f{}", PROMPT_PATHS)));
        assert!(prompt.contains("and 3 more"));
    }

    #[test]
    fn the_shown_command_quotes_what_a_shell_would_read_otherwise() {
        let args: Vec<String> = [
            "commit",
            "-m",
            "Fix it's $HOME `x`",
            "--",
            "src/a.rs",
            "-odd name",
            "",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            shown_command(&args),
            "git commit -m 'Fix it'\\''s $HOME `x`' -- src/a.rs '-odd name' ''"
        );
    }
}
