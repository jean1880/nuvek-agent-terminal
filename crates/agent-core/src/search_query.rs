//! What the user types into the sidebar's search: plain words, and `#tag` words.
//!
//! [`SearchQuery::parse`] is the one grammar. Words are split at whitespace (and NUL). A word
//! that starts with `#` is a tag term: it matches a thread holding that tag (lowercased, every
//! leading `#` dropped, as tags are stored: [`crate::thread_tag`]), and several are ANDed. A lone
//! `#` is a tag still being typed and means nothing yet. Every other word is plain text, matched
//! on titles, folders and tags in the sidebar and searched for in messages.
//!
//! [`compile`] makes the plain words (never a tag term) into an FTS5 query that cannot fail to
//! parse. Each word becomes a quoted phrase (a `"` in it doubled), so FTS5 syntax typed by the
//! user (`AND`, `OR`, `NOT`, `NEAR`, `*`, `^`, `-`, parentheses, a `column:` filter) is only text.
//! The phrases are joined by spaces, which FTS5 reads as AND, and the last one is a prefix, so a
//! word still being typed matches. A word with no letter or digit in it is dropped: the tokenizer
//! would find nothing in it to look for. A NUL separates words like a space: FTS5's query parser
//! stops at one, so a NUL inside a phrase would leave it unterminated.

/// Most words a query keeps; the rest are ignored.
pub const MAX_TERMS: usize = 16;
/// Most characters a word keeps; the rest are ignored.
pub const MAX_TERM_CHARS: usize = 64;

/// A parsed search: its plain words, in order, and its tag terms.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    /// The plain words, as typed.
    pub words: Vec<String>,
    /// The tag terms, lowercased and without their `#`, each once, in order.
    pub tags: Vec<String>,
}

impl SearchQuery {
    /// `input` split into plain words and `#tag` terms.
    pub fn parse(input: &str) -> Self {
        let mut query = Self::default();
        for word in input
            .split(|c: char| c.is_whitespace() || c == '\0')
            .filter(|word| !word.is_empty())
        {
            if word.starts_with('#') {
                let tag = word.trim_start_matches('#').to_lowercase();
                if !tag.is_empty() && !query.tags.contains(&tag) {
                    query.tags.push(tag);
                }
            } else {
                query.words.push(word.to_owned());
            }
        }
        query
    }

    /// The plain words, joined by single spaces: what is matched as text, and what message
    /// search looks for.
    pub fn text(&self) -> String {
        self.words.join(" ")
    }

    /// Nothing to filter by: no word and no tag.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.tags.is_empty()
    }
}

/// `input`'s plain words ([`SearchQuery::parse`]; tag terms are left out) as an FTS5 `MATCH`
/// expression; `None` when they hold nothing to look for.
pub fn compile(input: &str) -> Option<String> {
    let terms: Vec<String> = SearchQuery::parse(input)
        .words
        .into_iter()
        .map(|word| word.chars().take(MAX_TERM_CHARS).collect::<String>())
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .take(MAX_TERMS)
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        return None;
    }
    Some(format!("{}*", terms.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_become_quoted_phrases_and_the_last_is_a_prefix() {
        assert_eq!(
            compile("cargo test").as_deref(),
            Some("\"cargo\" \"test\"*")
        );
        assert_eq!(compile("  hello  ").as_deref(), Some("\"hello\"*"));
    }

    #[test]
    fn fts_syntax_is_only_text() {
        assert_eq!(
            compile("a AND b OR NOT c").as_deref(),
            Some("\"a\" \"AND\" \"b\" \"OR\" \"NOT\" \"c\"*")
        );
        assert_eq!(
            compile("NEAR(x y) text:z -w ^v").as_deref(),
            Some("\"NEAR(x\" \"y)\" \"text:z\" \"-w\" \"^v\"*")
        );
        assert_eq!(
            compile("say \"hi\"").as_deref(),
            Some("\"say\" \"\"\"hi\"\"\"*")
        );
    }

    #[test]
    fn nothing_to_look_for_is_none() {
        for input in ["", "   ", "* - \" ( ) ^", "\u{2122} ...", "\0", "\0\0 \0"] {
            assert_eq!(compile(input), None, "{input:?}");
        }
    }

    #[test]
    fn a_nul_separates_words_and_never_reaches_the_query() {
        assert_eq!(compile("ab\0cd").as_deref(), Some("\"ab\" \"cd\"*"));
        assert_eq!(compile("\"x\0").as_deref(), Some("\"\"\"x\"*"));
        assert!(!compile("a\0\"b\0").expect("words").contains('\0'));
    }

    #[test]
    fn hash_words_are_tag_terms_and_the_rest_is_text() {
        let q = SearchQuery::parse("  fix #Review the\tbuild ##spike #review # ");
        assert_eq!(q.words, ["fix", "the", "build"]);
        assert_eq!(q.tags, ["review", "spike"], "lowercased, once each");
        assert_eq!(q.text(), "fix the build");
        assert!(!q.is_empty());

        let only = SearchQuery::parse("#blocked");
        assert!(only.words.is_empty() && only.text().is_empty());
        assert_eq!(only.tags, ["blocked"]);
        assert!(
            SearchQuery::parse("  #  ").is_empty(),
            "a lone # means nothing yet"
        );
        assert!(SearchQuery::parse("").is_empty());
        // A `#` inside a word is text.
        assert_eq!(SearchQuery::parse("c# issue#4").words, ["c#", "issue#4"]);
    }

    #[test]
    fn tag_terms_never_reach_message_search() {
        assert_eq!(
            compile("#review deploy #spike").as_deref(),
            Some("\"deploy\"*")
        );
        assert_eq!(compile("#review"), None, "tags alone search no message");
        assert_eq!(compile("#review #spike #"), None);
        assert_eq!(
            compile("a #b c").as_deref(),
            Some("\"a\" \"c\"*"),
            "the last plain word is the prefix"
        );
    }

    #[test]
    fn unicode_words_are_kept() {
        assert_eq!(compile("café 東京").as_deref(), Some("\"café\" \"東京\"*"));
    }

    #[test]
    fn long_input_is_bounded() {
        let many = "word ".repeat(100);
        let q = compile(&many).expect("words");
        assert_eq!(q.matches("\"word\"").count(), MAX_TERMS);
        let long = "x".repeat(1000);
        let q = compile(&long).expect("a word");
        assert_eq!(q.len(), MAX_TERM_CHARS + 3, "{q}");
    }
}
