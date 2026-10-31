//! JSON text masking that preserves formatting and visits original field names.

use serde_json::Value;
use std::ops::Range;

pub struct MaskedJson {
    pub text: String,
    pub keys_changed: bool,
    pub spelling_changed: bool,
    /// Original quoted token spans, in source order, for checking unmasked source text.
    pub strings: Vec<Range<usize>>,
}

fn is_sequence(text: &str) -> Result<bool, &'static str> {
    if !text.trim_start().starts_with(['{', '[']) {
        return Ok(false);
    }
    let mut found = false;
    for value in serde_json::Deserializer::from_str(text).into_iter::<Value>() {
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                // serde_json exposes the recursion error through Display, not a typed accessor.
                // A resource refusal must never turn encoded JSON into unstructured text.
                if error.to_string().starts_with("recursion limit exceeded") {
                    return Err("JSON text exceeds its masking depth.");
                }
                return Ok(false);
            }
        };
        if !matches!(value, Value::Object(_) | Value::Array(_)) {
            return Ok(false);
        }
        found = true;
    }
    Ok(found)
}

/// Mask string tokens in a valid sequence of JSON objects/arrays (including NDJSON). Other text
/// returns `None`. Unchanged tokens, whitespace and punctuation are copied verbatim. The
/// callback receives the sensitive enclosing field, decoded text, and whether this is a key.
pub fn mask_json(
    text: &str,
    parent: Option<&str>,
    mut mask: impl FnMut(Option<&str>, &str, bool) -> String,
) -> Result<Option<MaskedJson>, &'static str> {
    if !is_sequence(text)? {
        return Ok(None);
    }
    struct Walker<'a, F> {
        text: &'a str,
        at: usize,
        copied: usize,
        out: String,
        keys_changed: bool,
        spelling_changed: bool,
        strings: Vec<Range<usize>>,
        mask: F,
    }
    impl<F: FnMut(Option<&str>, &str, bool) -> String> Walker<'_, F> {
        fn blanks(&mut self) {
            while self
                .text
                .as_bytes()
                .get(self.at)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.at += 1;
            }
        }
        fn string(&mut self, parent: Option<&str>, key: bool) -> Result<String, &'static str> {
            let start = self.at;
            self.at += 1;
            while let Some(byte) = self.text.as_bytes().get(self.at) {
                match byte {
                    b'\\' => self.at += 2,
                    b'"' => {
                        self.at += 1;
                        break;
                    }
                    _ => self.at += 1,
                }
            }
            let original = self
                .text
                .get(start..self.at)
                .ok_or("Invalid JSON text span.")?;
            let decoded: String =
                serde_json::from_str(original).map_err(|_| "Invalid JSON string.")?;
            self.strings.push(start..self.at);
            let safe = (self.mask)(parent, &decoded, key);
            if safe != decoded {
                // Re-encoding noncanonical escapes could change a shell/file string outside
                // the masked value. Strict callers must refuse this case.
                self.spelling_changed |= serde_json::to_string(&decoded)
                    .map_err(|_| "Cannot encode original JSON string.")?
                    != original;
                self.keys_changed |= key;
                self.out.push_str(&self.text[self.copied..start]);
                self.out.push_str(
                    &serde_json::to_string(&safe)
                        .map_err(|_| "Cannot encode masked JSON string.")?,
                );
                self.copied = self.at;
            }
            Ok(decoded)
        }
        fn value(&mut self, parent: Option<&str>, depth: usize) -> Result<(), &'static str> {
            if depth > 128 {
                return Err("JSON text exceeds its masking depth.");
            }
            self.blanks();
            match self.text.as_bytes().get(self.at) {
                Some(b'"') => {
                    self.string(parent, false)?;
                }
                Some(b'{') => {
                    self.at += 1;
                    self.blanks();
                    if self.text.as_bytes().get(self.at) == Some(&b'}') {
                        self.at += 1;
                        return Ok(());
                    }
                    loop {
                        self.blanks();
                        let key = self.string(None, true)?;
                        self.blanks();
                        self.at += 1; // colon, verified by the JSON deserializer
                        let enclosing = parent
                            .filter(|p| super::is_sensitive_name(p))
                            .unwrap_or(&key);
                        self.value(Some(enclosing), depth + 1)?;
                        self.blanks();
                        let end = self.text.as_bytes().get(self.at) == Some(&b'}');
                        self.at += 1;
                        if end {
                            break;
                        }
                    }
                }
                Some(b'[') => {
                    self.at += 1;
                    self.blanks();
                    if self.text.as_bytes().get(self.at) == Some(&b']') {
                        self.at += 1;
                        return Ok(());
                    }
                    loop {
                        self.value(parent, depth + 1)?;
                        self.blanks();
                        let end = self.text.as_bytes().get(self.at) == Some(&b']');
                        self.at += 1;
                        if end {
                            break;
                        }
                    }
                }
                Some(_) => {
                    while self
                        .text
                        .as_bytes()
                        .get(self.at)
                        .is_some_and(|b| !b.is_ascii_whitespace() && !b",]}[{\"".contains(b))
                    {
                        self.at += 1;
                    }
                }
                None => return Err("Missing JSON text value."),
            }
            Ok(())
        }
    }
    let mut walker = Walker {
        text,
        at: 0,
        copied: 0,
        out: String::new(),
        keys_changed: false,
        spelling_changed: false,
        strings: Vec::new(),
        mask: &mut mask,
    };
    walker.blanks();
    while walker.at < text.len() {
        let before = walker.at;
        walker.value(parent, 1)?;
        if walker.at <= before {
            return Err("JSON masking made no progress.");
        }
        walker.blanks();
    }
    walker.out.push_str(&text[walker.copied..]);
    if !is_sequence(&walker.out)? {
        return Err("Masking changed JSON structure.");
    }
    Ok(Some(MaskedJson {
        text: walker.out,
        keys_changed: walker.keys_changed,
        spelling_changed: walker.spelling_changed,
        strings: walker.strings,
    }))
}

/// Persistent JSON text uses the same sensitive-ancestor rule as structured event values.
pub fn redact_text(parent: Option<&str>, text: &str) -> String {
    redact_text_at(parent, text, 0, &mut (32 * 1024 * 1024))
}
fn redact_text_at(parent: Option<&str>, text: &str, depth: usize, remaining: &mut usize) -> String {
    if depth > 128 || text.len() > *remaining {
        return "[JSON masking limit exceeded.]".into();
    }
    *remaining -= text.len();
    match mask_json(text, parent, |key, value, is_key| {
        if is_key {
            super::redact(value)
        } else {
            redact_text_at(key, value, depth + 1, remaining)
        }
    }) {
        Ok(Some(masked)) => masked.text,
        Ok(None) => {
            parent.map_or_else(|| super::redact(text), |key| super::redact_keyed(key, text))
        }
        Err(_) => "[Sensitive JSON text withheld.]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adjacent_json_values_progress_and_deep_json_fails_closed() {
        for input in ["[]{}", "{\"tokens\":[\"private-1111\"]}[]"] {
            assert_eq!(
                redact_text(None, input),
                input.replace("private-1111", "****1111")
            );
        }
        let deep = format!(
            "{{\"tokens\":{}\"private-1111\"{}}}",
            "[".repeat(130),
            "]".repeat(130)
        );
        assert!(mask_json(&deep, None, |_, value, _| value.into()).is_err());
        assert_eq!(redact_text(None, &deep), "[Sensitive JSON text withheld.]");
    }
    #[test]
    fn primitive_roots_are_plain_text_and_changed_escape_spellings_are_flagged() {
        for input in [
            r#""\u0065cho a\/b""#,
            "123",
            "true",
            "null",
            "{}\n\"plain\"",
            "0[]",
            "true{}",
            "1\"text\"",
        ] {
            assert!(
                mask_json(input, None, |_, _, _| panic!("primitive interpreted"))
                    .unwrap()
                    .is_none()
            );
        }
        let input = r#"{"value":"\u0061 a\/b secret","untouched":"\u0062"}"#;
        let masked = mask_json(input, None, |_, value, _| {
            value.replace("secret", "[REDACTED]")
        })
        .unwrap()
        .unwrap();
        assert!(masked.spelling_changed);
        assert!(masked.text.contains(r#""untouched":"\u0062""#));
    }
    #[test]
    fn json_sequences_preserve_formatting_duplicates_and_escapes_outside_masked_values() {
        let input = " {\"name\":\"\\u96ea\", \"name\":\"kept\",\"tokens\":[\"array-private-1111\", [\"nested-private-2222\"]]}\n{\"ordinary\": \"visible\"} ";
        let expected = input
            .replace("array-private-1111", "****1111")
            .replace("nested-private-2222", "****2222");
        assert_eq!(redact_text(None, input), expected);
        let plain = " {\"name\":\"\\u96ea\", \"name\":\"kept\"}\n[1, 2] ";
        assert_eq!(redact_text(None, plain), plain);
    }
    #[test]
    fn non_json_shell_text_is_not_interpreted_and_json_key_changes_are_reported() {
        for text in ["[ -d . ] && echo present", "{ echo present; } # note"] {
            assert!(mask_json(text, None, |_, value, _| value.into())
                .unwrap()
                .is_none());
        }
        let masked = mask_json(
            "{\"escaped\\u005fkey\": \"visible\"}",
            None,
            |_, value, key| {
                if key {
                    "[REDACTED]".into()
                } else {
                    value.into()
                }
            },
        )
        .unwrap()
        .unwrap();
        assert!(masked.keys_changed);
        assert_eq!(masked.text, "{\"[REDACTED]\": \"visible\"}");
    }
}
