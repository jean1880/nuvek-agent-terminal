//! A thread's tags: what the user types, made into the one form that is stored, shown and
//! searched for (`#name` in the sidebar's search, [`crate::search_query::SearchQuery`]).
//!
//! A tag is one word: no whitespace, comma or control character, at most [`MAX_TAG_CHARS`]
//! characters, lowercased, with any leading `#` dropped. A thread holds at most [`MAX_TAGS`].
//! A tag that redaction would change is refused, never stored masked: a tag is shown on every
//! row, so it must never be a secret. A tag may have one of the theme's colours
//! ([`TagColour`]), named by its token so every colour scheme, the desktop's own included,
//! draws it in its own shade.

use std::fmt;

use crate::redact::redact;

/// Most characters a tag keeps.
pub const MAX_TAG_CHARS: usize = 32;
/// Most tags a thread holds.
pub const MAX_TAGS: usize = 8;

/// Why a tag (or a set of them) was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagError {
    /// Nothing left once trimmed and stripped of `#`.
    Empty,
    /// Longer than [`MAX_TAG_CHARS`] (the tag, as typed, for the message).
    TooLong(String),
    /// Holds whitespace, a comma or a control character.
    Invalid(String),
    /// Redaction would change it: it looks like a secret. Never echoed.
    LooksLikeSecret,
    /// More than [`MAX_TAGS`].
    TooMany(usize),
}

impl fmt::Display for TagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "A tag needs at least one character."),
            // The text is masked again here: a message never shows what redaction would hide.
            Self::TooLong(tag) => {
                write!(
                    f,
                    "“{}” is longer than {MAX_TAG_CHARS} characters.",
                    redact(tag)
                )
            }
            Self::Invalid(tag) => write!(
                f,
                "“{}” holds a character a tag cannot have.",
                redact(tag)
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect::<String>()
            ),
            Self::LooksLikeSecret => {
                write!(f, "A tag looks like a secret, so it is not kept.")
            }
            Self::TooMany(n) => write!(f, "{n} tags: a thread holds at most {MAX_TAGS}."),
        }
    }
}

impl std::error::Error for TagError {}

/// Whether `c` may never be part of a tag.
fn forbidden(c: char) -> bool {
    c.is_whitespace() || c.is_control() || c == ','
}

/// Whether redaction would change `typed`: whole, lowercased, or any run of it between the
/// characters a tag cannot have (a secret with a stray comma or control character in it).
fn looks_like_secret(typed: &str) -> bool {
    let changes = |s: &str| redact(s) != s;
    changes(typed)
        || changes(&typed.to_lowercase())
        || typed
            .split(forbidden)
            .filter(|part| !part.is_empty())
            .any(|part| changes(part) || changes(&part.to_lowercase()))
}

/// `input` as a stored tag: trimmed, any leading `#` dropped, lowercased. Refused when nothing
/// is left, when redaction would change it ([`TagError::LooksLikeSecret`], checked first, so no
/// other refusal ever echoes a secret), when it holds whitespace, a comma or a control
/// character, or when it is too long.
pub fn normalize_tag(input: &str) -> Result<String, TagError> {
    let typed = input.trim().trim_start_matches('#');
    if typed.is_empty() {
        return Err(TagError::Empty);
    }
    if looks_like_secret(typed) {
        return Err(TagError::LooksLikeSecret);
    }
    if typed.chars().any(forbidden) {
        return Err(TagError::Invalid(typed.to_owned()));
    }
    let tag = typed.to_lowercase();
    if tag.chars().count() > MAX_TAG_CHARS {
        return Err(TagError::TooLong(typed.to_owned()));
    }
    Ok(tag)
}

/// What the user typed into the tags entry: tags separated by commas, spaces or new lines, each
/// normalised ([`normalize_tag`]), the first of each name kept. A lone `#` is skipped. Refused
/// at the first bad tag; a list longer than [`MAX_TAGS`] is not refused here ([`tag_set`]
/// checks the whole set).
pub fn parse_tags(input: &str) -> Result<Vec<String>, TagError> {
    let mut tags: Vec<String> = Vec::new();
    for piece in input.split(|c: char| c == ',' || c.is_whitespace()) {
        if piece.trim_start_matches('#').is_empty() {
            continue;
        }
        let tag = normalize_tag(piece)?;
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    Ok(tags)
}

/// `tags` as a thread's set: each normalised, the first of each name kept, sorted, at most
/// [`MAX_TAGS`]. What the store writes, so whatever reaches it is checked again.
pub fn tag_set<S: AsRef<str>>(tags: &[S]) -> Result<Vec<String>, TagError> {
    let mut set: Vec<String> = Vec::new();
    for tag in tags {
        let tag = normalize_tag(tag.as_ref())?;
        if !set.contains(&tag) {
            set.push(tag);
        }
    }
    if set.len() > MAX_TAGS {
        return Err(TagError::TooMany(set.len()));
    }
    set.sort();
    Ok(set)
}

/// A tag's colour: one of the theme's tokens, never a literal colour, so the desktop's own
/// colours (`ThemeChoice::System`) draw it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TagColour {
    Accent,
    Success,
    Warning,
    Danger,
}

impl TagColour {
    /// Every colour, in the order a picker lists them (after "No colour").
    pub const ALL: [TagColour; 4] = [Self::Accent, Self::Success, Self::Warning, Self::Danger];

    /// The token name: what is stored, and the CSS class suffix (`tag-<key>`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Accent => "accent",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Danger => "danger",
        }
    }

    /// What a picker calls it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Accent => "Accent",
            Self::Success => "Green",
            Self::Warning => "Yellow",
            Self::Danger => "Red",
        }
    }

    /// The colour stored as `key`; `None` for anything else.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_splits_and_caps() {
        assert_eq!(normalize_tag("  #Review ").as_deref(), Ok("review"));
        assert_eq!(normalize_tag("##Spike").as_deref(), Ok("spike"));
        assert_eq!(normalize_tag("c#").as_deref(), Ok("c#"));
        assert_eq!(normalize_tag("Écrit").as_deref(), Ok("écrit"));
        assert_eq!(normalize_tag(" # "), Err(TagError::Empty));
        assert_eq!(normalize_tag(""), Err(TagError::Empty));
        assert!(matches!(normalize_tag("a b"), Err(TagError::Invalid(_))));
        assert!(matches!(normalize_tag("a,b"), Err(TagError::Invalid(_))));
        assert!(matches!(
            normalize_tag("a\u{1f}b"),
            Err(TagError::Invalid(_))
        ));
        assert!(matches!(normalize_tag("a\0"), Err(TagError::Invalid(_))));
        let longest = "x".repeat(MAX_TAG_CHARS);
        assert_eq!(normalize_tag(&longest).as_deref(), Ok(longest.as_str()));
        assert!(matches!(
            normalize_tag(&format!("{longest}y")),
            Err(TagError::TooLong(_))
        ));

        assert_eq!(
            parse_tags("Review, spike\nblocked  #review,,#").unwrap(),
            ["review", "spike", "blocked"]
        );
        assert_eq!(parse_tags("  , \n ").unwrap(), Vec::<String>::new());
        assert!(parse_tags("ok\u{7}bad").is_err());

        let nine: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
        assert_eq!(tag_set(&nine), Err(TagError::TooMany(9)));
        assert_eq!(tag_set(&nine[..8]).unwrap().len(), MAX_TAGS);
        assert_eq!(
            tag_set(&["b", "#A", "a", "B"]).unwrap(),
            ["a", "b"],
            "duplicates fold before the cap is counted"
        );
        assert!(tag_set(&["ok", "not ok"]).is_err());
    }

    #[test]
    fn a_tag_that_redaction_would_change_is_refused() {
        // A key that redaction masks, short enough to be a tag.
        let secret = "AKIAIOSFODNN7EXAMPLE";
        assert_ne!(
            redact(secret),
            secret,
            "the fixture must be one redact masks"
        );
        assert_eq!(normalize_tag(secret), Err(TagError::LooksLikeSecret));
        assert_eq!(
            parse_tags(&format!("fine, {secret}")),
            Err(TagError::LooksLikeSecret)
        );
        assert_eq!(tag_set(&[secret]), Err(TagError::LooksLikeSecret));
        assert!(
            !TagError::LooksLikeSecret.to_string().contains(secret),
            "the message never echoes it"
        );
        // With a character a tag cannot have, before, inside or after it, it is still refused as
        // a secret, never as an invalid tag that would echo it.
        for (case, typed) in [
            format!("{secret},"),
            format!("{secret}\u{7}"),
            format!("x {secret}"),
            format!("{secret}\u{1f}more"),
        ]
        .iter()
        .enumerate()
        {
            // The message names the case, never the fixture.
            assert_eq!(
                normalize_tag(typed),
                Err(TagError::LooksLikeSecret),
                "case {case}"
            );
            assert!(tag_set(&[typed.as_str()])
                .expect_err("refused")
                .to_string()
                .find(secret)
                .is_none());
        }
        // Every message is masked, whatever reaches it.
        for error in [
            TagError::Invalid(format!("{secret}\u{7}")),
            TagError::TooLong(format!("{secret}{}", "x".repeat(40))),
        ] {
            assert!(
                !error.to_string().contains(secret),
                "an error message echoes the fixture"
            );
        }
        // Plain words are kept.
        for word in ["review", "token", "password", "secret", "key"] {
            assert_eq!(normalize_tag(word).as_deref(), Ok(word));
        }
    }

    #[test]
    fn every_colour_round_trips_through_its_key() {
        // A new colour fails to compile here until it has a place in ALL.
        let place = |c: TagColour| match c {
            TagColour::Accent => 0,
            TagColour::Success => 1,
            TagColour::Warning => 2,
            TagColour::Danger => 3,
        };
        for (i, colour) in TagColour::ALL.into_iter().enumerate() {
            assert_eq!(place(colour), i);
            assert_eq!(TagColour::from_key(colour.key()), Some(colour));
            assert!(!colour.label().is_empty());
        }
        assert_eq!(TagColour::from_key("red"), None);
        assert_eq!(TagColour::from_key("#ff0000"), None);
        assert_eq!(TagColour::from_key(""), None);
    }

    #[test]
    fn messages_name_what_was_wrong() {
        assert!(TagError::TooLong("abc".into()).to_string().contains("abc"));
        assert!(TagError::TooMany(9).to_string().contains('8'));
        assert!(!TagError::Invalid("a\u{7}".into())
            .to_string()
            .contains('\u{7}'));
    }
}
