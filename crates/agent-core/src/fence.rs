//! Markdown framing for text of unknown content: a code fence the text cannot close, and a
//! block quote. Shared by everything that puts agent, terminal or file text into a document.

/// `text` between two code fences, each longer than any run of backticks inside it.
///
/// The guarantee: whatever `text` holds, it can never close its own fence early, because the
/// fence is at least three backticks and always longer than the longest run in the text (a
/// shorter or equal run does not close a fence in CommonMark). The text itself is kept
/// verbatim; a trailing newline is not added or removed, so a caller that wants none trims it.
pub fn fenced(text: &str) -> String {
    let mut fence = "```".to_string();
    while text.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}\n{text}\n{fence}")
}

/// `text` as a Markdown block quote: every line, an empty one included, gets a `> ` prefix.
///
/// Lines are split as `str::lines` does, so a trailing newline adds no empty quoted line and
/// an empty `text` gives an empty string. The quote holds no fence; a fence inside `text`
/// stays a quoted line and cannot end the quote.
pub fn quote(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The longest run of backticks in `s`.
    fn longest_run(s: &str) -> usize {
        s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
    }

    #[test]
    fn plain_text_gets_a_three_backtick_fence() {
        assert_eq!(fenced("hello"), "```\nhello\n```");
    }

    #[test]
    fn a_fence_in_the_content_is_outlived_by_the_outer_one() {
        assert_eq!(fenced("```\nlast output"), "````\n```\nlast output\n````");
    }

    #[test]
    fn three_four_and_ten_backticks_cannot_close_the_fence() {
        for n in [3usize, 4, 10] {
            let inner = "`".repeat(n);
            let text = format!("a\n{inner}\nb");
            let out = fenced(&text);
            let fence = "`".repeat(n + 1);
            assert_eq!(out, format!("{fence}\n{text}\n{fence}"));
            // The outer fence is strictly longer than anything inside it.
            let body = &out[fence.len()..out.len() - fence.len()];
            assert!(longest_run(body) < fence.len());
        }
    }

    #[test]
    fn a_short_run_does_not_lengthen_the_fence() {
        assert_eq!(fenced("a `` b"), "```\na `` b\n```");
    }

    #[test]
    fn an_empty_string_is_an_empty_fenced_block() {
        assert_eq!(fenced(""), "```\n\n```");
    }

    #[test]
    fn content_without_a_final_newline_is_closed_on_its_own_line() {
        assert_eq!(fenced("a\nb"), "```\na\nb\n```");
    }

    #[test]
    fn a_final_newline_in_the_content_is_kept() {
        assert_eq!(fenced("a\n"), "```\na\n\n```");
    }

    #[test]
    fn quote_prefixes_every_line_and_keeps_empty_ones() {
        assert_eq!(quote("a\n\nb"), "> a\n> \n> b");
    }

    #[test]
    fn quote_of_an_empty_string_is_empty() {
        assert_eq!(quote(""), "");
    }

    #[test]
    fn quote_ignores_a_final_newline() {
        assert_eq!(quote("a\n"), "> a");
        assert_eq!(quote("a"), "> a");
    }

    #[test]
    fn quote_leaves_a_fence_inside_the_quoted_line() {
        assert_eq!(quote("```\nx"), "> ```\n> x");
    }
}
