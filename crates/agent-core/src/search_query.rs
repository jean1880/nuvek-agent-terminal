//! What the user types into message search, made into an FTS5 query that cannot fail to parse.
//!
//! Each word becomes a quoted phrase (a `"` in it doubled), so FTS5 syntax typed by the user
//! (`AND`, `OR`, `NOT`, `NEAR`, `*`, `^`, `-`, parentheses, a `column:` filter) is only text. The
//! phrases are joined by spaces, which FTS5 reads as AND, and the last one is a prefix, so a word
//! still being typed matches. A word with no letter or digit in it is dropped: the tokenizer would
//! find nothing in it to look for.

/// Most words a query keeps; the rest are ignored.
pub const MAX_TERMS: usize = 16;
/// Most characters a word keeps; the rest are ignored.
pub const MAX_TERM_CHARS: usize = 64;

/// `input` as an FTS5 `MATCH` expression; `None` when it holds nothing to look for.
pub fn compile(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
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
        for input in ["", "   ", "* - \" ( ) ^", "\u{2122} ..."] {
            assert_eq!(compile(input), None, "{input:?}");
        }
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
