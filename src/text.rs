//! Small text helpers shared across the app.

use std::borrow::Cow;

/// `text` in at most `max_chars` characters: when it is longer, cut, with
/// the space before the cut dropped and an ellipsis as its last
/// character. True when it was cut.
pub fn ellipsize(text: &str, max_chars: usize) -> (Cow<'_, str>, bool) {
    if text.char_indices().nth(max_chars).is_none() {
        return (Cow::Borrowed(text), false);
    }
    let end = text
        .char_indices()
        .nth(max_chars.saturating_sub(1))
        .map_or(text.len(), |(at, _)| at);
    (Cow::Owned(format!("{}…", text[..end].trim_end())), true)
}

/// `text` safe inside an XML or HTML element or a quoted attribute: `&`,
/// `<`, `>` and both quotes become entities, so nothing in it can close
/// the element or attribute it sits in.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// `bytes` as lowercase hex, two digits each: how digests and random ids
/// are written down.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The bytes `text` spells in hex, either case; `None` unless it is pairs
/// of hex digits and nothing else.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| {
        char::from(c)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|&[high, low]| Some(digit(high)? << 4 | digit(low)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsize_cuts_only_what_does_not_fit() {
        assert_eq!(ellipsize("short", 5), (Cow::Borrowed("short"), false));
        assert_eq!(ellipsize("", 0), (Cow::Borrowed(""), false));
        assert_eq!(ellipsize("one two three", 8).0, "one two…");
        assert_eq!(ellipsize("ééééé", 3), (Cow::Owned("éé…".into()), true));
        assert_eq!(ellipsize("ab", 1).0, "…");
        assert_eq!(ellipsize("ab", 0).0, "…");
    }

    #[test]
    fn unhex_undoes_hex() {
        let bytes = [0x00, 0x0f, 0xa0, 0xff];
        assert_eq!(unhex(&hex(&bytes)).as_deref(), Some(&bytes[..]));
        assert_eq!(unhex("ABcd").as_deref(), Some(&[0xab, 0xcd][..]));
        assert_eq!(unhex("").as_deref(), Some(&[][..]));
        assert_eq!(unhex("abc"), None, "odd length");
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("+1"), None);
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn xml_escape_covers_markup_and_quotes() {
        assert_eq!(
            xml_escape(r#"<a href="x">Tom & Jerry's</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&#39;s&lt;/a&gt;"
        );
        assert_eq!(xml_escape("plain 👍"), "plain 👍");
    }
}
