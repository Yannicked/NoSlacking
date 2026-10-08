//! Whole text files: decoded, split into lines and cut to the caps.

use super::{MAX_LINE_CHARS, MAX_LINES, Note, Text};
use crate::failure::Failure;

/// How much of the start is looked at to tell text from a binary file.
const SNIFF: usize = 8 * 1024;

/// The lines of a text file, and what was left out of them. A file with
/// NUL bytes near its start is not text.
pub(super) fn read(bytes: &[u8]) -> Result<(Text, Vec<Note>), Failure> {
    let mut notes = Vec::new();
    let decoded = decode(bytes, &mut notes)?;
    let mut lines = Vec::new();
    let mut long = 0;
    for line in decoded.split('\n') {
        if lines.len() == MAX_LINES {
            notes.push(Note::Lines { shown: MAX_LINES });
            break;
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (line, cut) = shown_line(line);
        if cut {
            long += 1;
        }
        lines.push(line);
    }
    // A file ending in a line end has no empty line after it.
    if lines.len() > 1 && lines.last().is_some_and(String::is_empty) && decoded.ends_with('\n') {
        lines.pop();
    }
    if long > 0 {
        notes.push(Note::LongLines { count: long });
    }
    Ok((Text { lines }, notes))
}

/// The text in `bytes`: UTF-16 when it starts with a byte order mark,
/// else UTF-8, with anything else shown as � and noted.
fn decode(bytes: &[u8], notes: &mut Vec<Note>) -> Result<String, Failure> {
    let utf16 = match bytes {
        [0xFF, 0xFE, rest @ ..] => Some((rest, true)),
        [0xFE, 0xFF, rest @ ..] => Some((rest, false)),
        _ => None,
    };
    if let Some((rest, little)) = utf16 {
        let units = rest.as_chunks::<2>().0.iter().map(|&pair| {
            if little {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        });
        let mut bad = false;
        let text: String = char::decode_utf16(units)
            .map(|c| {
                c.unwrap_or_else(|_| {
                    bad = true;
                    char::REPLACEMENT_CHARACTER
                })
            })
            .collect();
        if bad {
            notes.push(Note::NotUtf8);
        }
        return Ok(text);
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if bytes[..bytes.len().min(SNIFF)].contains(&0) {
        return Err(Failure::NotText);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_owned()),
        Err(_) => {
            notes.push(Note::NotUtf8);
            Ok(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

/// A line as shown: at most [`MAX_LINE_CHARS`] characters and an ellipsis
/// (a tab counting as one), then tabs as four spaces. True when it was cut.
fn shown_line(line: &str) -> (String, bool) {
    let (line, cut) = crate::text::ellipsize(line, MAX_LINE_CHARS + 1);
    (line.replace('\t', "    "), cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_split_into_lines() {
        let (text, notes) = read(b"\xEF\xBB\xBFfn main() {\r\n\tok\r\n}\n").expect("text");
        assert_eq!(text.lines, vec!["fn main() {", "    ok", "}"]);
        assert!(notes.is_empty());
        let (text, _) = read(b"").expect("empty");
        assert_eq!(text.lines, vec![""]);
    }

    #[test]
    fn utf16_with_a_byte_order_mark_is_read() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "héllo\nwörld".encode_utf16() {
            bytes.extend(unit.to_le_bytes());
        }
        let (text, notes) = read(&bytes).expect("utf-16");
        assert_eq!(text.lines, vec!["héllo", "wörld"]);
        assert!(notes.is_empty());
    }

    #[test]
    fn binary_files_are_not_text() {
        assert_eq!(read(b"\x7fELF\0\0\x01"), Err(Failure::NotText));
    }

    #[test]
    fn bad_bytes_are_replaced_and_noted() {
        let (text, notes) = read(b"caf\xe9\n").expect("latin-1 still shows");
        assert_eq!(text.lines, vec!["caf\u{FFFD}"]);
        assert_eq!(notes, vec![Note::NotUtf8]);
    }

    #[test]
    fn too_many_lines_and_long_lines_are_cut() {
        let many = "x\n".repeat(MAX_LINES + 10);
        let (text, notes) = read(many.as_bytes()).expect("many lines");
        assert_eq!(text.lines.len(), MAX_LINES);
        assert_eq!(notes, vec![Note::Lines { shown: MAX_LINES }]);

        let minified = "{\"a\":1}".repeat(MAX_LINE_CHARS);
        let (text, notes) = read(minified.as_bytes()).expect("one long line");
        assert_eq!(text.lines[0].chars().count(), MAX_LINE_CHARS + 1);
        assert_eq!(notes, vec![Note::LongLines { count: 1 }]);
    }
}
