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

/// The decoded value of the first `key` in `query` (what follows a URL's
/// `?`, without its `#` fragment). `None` when the key is missing or its
/// value is not UTF-8 once decoded.
pub fn query_param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, value)| decode(value).ok())
        .map(Cow::into_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_params_are_found_and_decoded() {
        let query = "team=T1&thread_ts=1700000000.000100&q=a%20b%2Fc&bad=%FF&team=T2";
        assert_eq!(query_param(query, "team").as_deref(), Some("T1"));
        assert_eq!(
            query_param(query, "thread_ts").as_deref(),
            Some("1700000000.000100")
        );
        assert_eq!(query_param(query, "q").as_deref(), Some("a b/c"));
        assert_eq!(query_param(query, "bad"), None);
        assert_eq!(query_param(query, "missing"), None);
        assert_eq!(query_param("", "team"), None);
        assert_eq!(query_param("flag&x=1", "flag"), None, "a key needs a value");
    }

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
