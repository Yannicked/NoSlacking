//! Percent-encoding for URL parts, on `percent-encoding`, which `url`
//! already brings: everything but the unreserved characters of RFC 3986
//! is escaped, as `encodeURIComponent` does.

use std::borrow::Cow;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

/// What stays as it is: letters, digits and `-` `_` `.` `~`.
const ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// `text` with everything but the unreserved characters escaped.
pub fn encode(text: &str) -> Cow<'_, str> {
    utf8_percent_encode(text, ESCAPED).into()
}

/// `text` with its escapes undone, or an error when they spell bytes that
/// are not UTF-8. A `+` stays a `+`.
pub fn decode(text: &str) -> Result<Cow<'_, str>, std::str::Utf8Error> {
    percent_decode_str(text).decode_utf8()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unreserved_characters_stay() {
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode("a b/c?d=e&f+g"), "a%20b%2Fc%3Fd%3De%26f%2Bg");
        assert_eq!(encode("é"), "%C3%A9");
    }

    #[test]
    fn decoding_undoes_encoding() {
        for text in ["", "a b/c?d=e&f+g", "émoji 🎉", "100%"] {
            assert_eq!(decode(&encode(text)).expect("UTF-8"), text);
        }
        assert_eq!(decode("a+b").expect("UTF-8"), "a+b");
        assert!(decode("%FF").is_err());
    }
}
