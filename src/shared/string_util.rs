pub(crate) trait CharExt {
    fn is_regex_special_char(&self) -> bool;
}

pub(crate) trait StringExt {
    fn escape_regex_chars(&self) -> String;
    fn from_utf8_lossy_as_owned(v: Vec<u8>) -> String;
    fn trim_end_in_place(&mut self);
}

impl StringExt for String {
    fn escape_regex_chars(&self) -> String {
        let mut buf = String::with_capacity(self.len());
        for char in self.chars() {
            if char.is_regex_special_char() {
                buf.push('\\');
            }
            buf.push(char);
        }
        buf
    }

    fn from_utf8_lossy_as_owned(v: Vec<u8>) -> String {
        if let std::borrow::Cow::Owned(string) = String::from_utf8_lossy(&v) {
            string
        } else {
            // SAFETY: `String::from_utf8_lossy`'s guaranteec valid utf8 when a borrowed
            // variant is returned. Owned value, meaning invalid utf8, is handled above.
            unsafe { String::from_utf8_unchecked(v) }
        }
    }

    fn trim_end_in_place(&mut self) {
        let trimmed_len = str::trim_end(self).len();
        if trimmed_len < self.len() {
            self.truncate(trimmed_len);
        }
    }
}

/// Shorten `value` to at most `max_chars` characters, appending `…` when
/// something was dropped. Character-aware on purpose: a byte-index
/// truncation (`str::split_at`) panics when the cut lands inside a
/// multi-byte character, which a torrent or track title can easily do.
pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    match value.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &value[..cut]),
        None => value.to_owned(),
    }
}

impl CharExt for char {
    fn is_regex_special_char(&self) -> bool {
        matches!(
            self,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        )
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_chars;

    #[test]
    fn a_short_string_is_unchanged() {
        assert_eq!(truncate_chars("short", 40), "short");
    }

    #[test]
    fn exactly_at_the_limit_is_unchanged() {
        let value = "a".repeat(40);
        assert_eq!(truncate_chars(&value, 40), value);
    }

    #[test]
    fn ascii_is_truncated_at_the_limit_with_an_ellipsis() {
        let value = "a".repeat(60);
        assert_eq!(truncate_chars(&value, 40), format!("{}…", "a".repeat(40)));
    }

    #[test]
    fn a_cut_inside_a_multibyte_character_does_not_panic() {
        // Byte 40 lands inside the 'é' (its bytes are at 39..41), which is
        // exactly the input that used to abort in the downloads modal.
        let value = format!("{}é{}", "a".repeat(39), "b".repeat(10));
        assert_eq!(truncate_chars(&value, 40), format!("{}é…", "a".repeat(39)));
    }
}
