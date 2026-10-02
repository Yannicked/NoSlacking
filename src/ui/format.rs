//! Formatting a draft: bold, italic, strike, code, code blocks and quotes,
//! put around the selection (or at the cursor) by a shortcut or the
//! composer's formatting bar.
//!
//! Everything here works in chars, as the text field's cursor does, and
//! is pure: the text and selection in, the new text and selection out.

use std::ops::Range;

/// A style the composer can apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Bold,
    Italic,
    Strike,
    Code,
    CodeBlock,
    Quote,
}

impl Format {
    /// The marker that goes on both sides, for the inline styles.
    fn marker(self) -> Option<&'static str> {
        match self {
            Self::Bold => Some("*"),
            Self::Italic => Some("_"),
            Self::Strike => Some("~"),
            Self::Code => Some("`"),
            Self::CodeBlock | Self::Quote => None,
        }
    }
}

/// Applies `format` to `selection` (char indices, in any order) of `text`.
/// Applying it again takes it off, so the shortcuts toggle like a word
/// processor's. Returns the new text and what to select in it.
pub fn apply(text: &str, selection: Range<usize>, format: Format) -> (String, Range<usize>) {
    let chars: Vec<char> = text.chars().collect();
    let start = selection.start.min(selection.end).min(chars.len());
    let end = selection.start.max(selection.end).min(chars.len());
    let multiline = chars[start..end].contains(&'\n');
    match format {
        // Code over several lines only reads as code in a block.
        Format::Code if multiline => block(&chars, start, end),
        Format::CodeBlock => block(&chars, start, end),
        Format::Quote => quote(&chars, start, end),
        _ => match format.marker() {
            Some(marker) => inline(&chars, start, end, marker),
            None => (text.to_owned(), start..end),
        },
    }
}

fn collect(parts: &[&[char]]) -> String {
    parts.iter().flat_map(|p| p.iter()).collect()
}

/// Wraps the selection in `marker`, or unwraps it when it already is.
fn inline(chars: &[char], start: usize, end: usize, marker: &str) -> (String, Range<usize>) {
    let marker: Vec<char> = marker.chars().collect();
    let m = marker.len();
    let before = &chars[..start];
    let after = &chars[end..];
    // Markers right outside the selection (or around the cursor): take
    // them away.
    if before.ends_with(&marker) && after.starts_with(&marker) {
        let text = collect(&[&chars[..start - m], &chars[start..end], &chars[end + m..]]);
        return (text, start - m..end - m);
    }
    // A selection that took its markers along.
    let inner = &chars[start..end];
    if inner.len() >= 2 * m && inner.starts_with(&marker) && inner.ends_with(&marker) {
        let text = collect(&[before, &inner[m..inner.len() - m], after]);
        return (text, start..end - 2 * m);
    }
    // Slack's markers must hug the text, so spaces at the selection's
    // edges stay outside them.
    let lead = inner.iter().take_while(|c| c.is_whitespace()).count();
    let trail = inner[lead..]
        .iter()
        .rev()
        .take_while(|c| c.is_whitespace())
        .count();
    let (word_start, word_end) = (start + lead, end - trail);
    let text = collect(&[
        &chars[..word_start],
        &marker,
        &chars[word_start..word_end],
        &marker,
        &chars[word_end..],
    ]);
    (text, word_start + m..word_end + m)
}

/// Fences the selection as a code block on lines of its own.
fn block(chars: &[char], start: usize, end: usize) -> (String, Range<usize>) {
    let fence: Vec<char> = "```".chars().collect();
    let inner = &chars[start..end];
    if inner.len() >= 6 && inner.starts_with(&fence) && inner.ends_with(&fence) {
        let body = &inner[3..inner.len() - 3];
        let body = body.strip_prefix(&['\n']).unwrap_or(body);
        let body = body.strip_suffix(&['\n']).unwrap_or(body);
        let text = collect(&[&chars[..start], body, &chars[end..]]);
        return (text, start..start + body.len());
    }
    let mut open: Vec<char> = Vec::new();
    if start > 0 && chars[start - 1] != '\n' {
        open.push('\n');
    }
    open.extend(&fence);
    open.push('\n');
    let mut close = vec!['\n'];
    close.extend(&fence);
    if chars.get(end).is_some_and(|c| *c != '\n') {
        close.push('\n');
    }
    let text = collect(&[&chars[..start], &open, inner, &close, &chars[end..]]);
    let at = start + open.len();
    (text, at..at + inner.len())
}

/// Starts every line the selection touches with `> `, or takes the marks
/// off when every one already has them.
fn quote(chars: &[char], start: usize, end: usize) -> (String, Range<usize>) {
    let first = chars[..start]
        .iter()
        .rposition(|c| *c == '\n')
        .map_or(0, |at| at + 1);
    let last = chars[end..]
        .iter()
        .position(|c| *c == '\n')
        .map_or(chars.len(), |at| end + at);
    let lines: Vec<&[char]> = chars[first..last].split(|c| *c == '\n').collect();
    let quoted = |line: &[char]| line.starts_with(&['>', ' ']);
    let unquote = lines.iter().all(|line| quoted(line));
    let mut out: Vec<char> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if unquote {
            out.extend(&line[2..]);
        } else {
            out.extend(['>', ' ']);
            out.extend(line.iter());
        }
    }
    let text = collect(&[&chars[..first], &out, &chars[last..]]);
    if start == end {
        // A cursor stays where it was in its line's text.
        let at = if unquote {
            start.saturating_sub(2).max(first)
        } else {
            start + 2
        };
        return (text, at..at);
    }
    (text, first..first + out.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies `format` to the part of `marked` between `[` and `]` (or at
    /// `|` for a cursor), and marks the new selection the same way.
    fn run(marked: &str, format: Format) -> String {
        let (text, range) = unmark(marked);
        let (out, selection) = apply(&text, range, format);
        mark(&out, selection)
    }

    fn unmark(marked: &str) -> (String, Range<usize>) {
        let mut text = String::new();
        let (mut start, mut end) = (0, 0);
        for c in marked.chars() {
            match c {
                '[' => start = text.chars().count(),
                ']' => end = text.chars().count(),
                '|' => {
                    start = text.chars().count();
                    end = start;
                }
                c => text.push(c),
            }
        }
        (text, start..end)
    }

    fn mark(text: &str, selection: Range<usize>) -> String {
        let chars: Vec<char> = text.chars().collect();
        if selection.is_empty() {
            return collect(&[&chars[..selection.start], &['|'], &chars[selection.start..]]);
        }
        collect(&[
            &chars[..selection.start],
            &['['],
            &chars[selection.clone()],
            &[']'],
            &chars[selection.end..],
        ])
    }

    #[test]
    fn styles_wrap_the_selection_and_toggle_off() {
        assert_eq!(run("say [hello] now", Format::Bold), "say *[hello]* now");
        assert_eq!(run("say *[hello]* now", Format::Bold), "say [hello] now");
        assert_eq!(run("say [*hello*] now", Format::Bold), "say [hello] now");
        assert_eq!(run("[ok]", Format::Italic), "_[ok]_");
        assert_eq!(run("[gone]", Format::Strike), "~[gone]~");
        assert_eq!(run("run [ls]", Format::Code), "run `[ls]`");
    }

    #[test]
    fn a_cursor_gets_a_pair_of_markers_to_type_into() {
        assert_eq!(run("a |b", Format::Bold), "a *|*b");
        assert_eq!(run("a *|*b", Format::Bold), "a |b");
        assert_eq!(run("|", Format::Code), "`|`");
    }

    #[test]
    fn spaces_at_the_edges_stay_outside_the_markers() {
        assert_eq!(run("a[ word ]b", Format::Bold), "a *[word]* b");
    }

    #[test]
    fn code_over_lines_becomes_a_block() {
        assert_eq!(run("see [a\nb]", Format::Code), "see \n```\n[a\nb]\n```");
        assert_eq!(run("|", Format::CodeBlock), "```\n|\n```");
        assert_eq!(run("[```\nx\n```]", Format::CodeBlock), "[x]");
        assert_eq!(run("[x] after", Format::CodeBlock), "```\n[x]\n```\n after");
    }

    #[test]
    fn quotes_mark_every_touched_line() {
        assert_eq!(
            run("one\nt[wo\nthr]ee\nfour", Format::Quote),
            "one\n[> two\n> three]\nfour"
        );
        assert_eq!(run("[> a\n> b]", Format::Quote), "[a\nb]");
        assert_eq!(run("|", Format::Quote), "> |");
        assert_eq!(run("> ab|c", Format::Quote), "ab|c");
    }

    #[test]
    fn wide_characters_count_as_one() {
        assert_eq!(run("日本[語]", Format::Bold), "日本*[語]*");
        assert_eq!(run("é|", Format::Italic), "é_|_");
    }

    #[test]
    fn selections_past_the_end_or_backwards_are_safe() {
        let (text, range) = apply("abc", Range { start: 9, end: 1 }, Format::Bold);
        assert_eq!(text, "a*bc*");
        assert_eq!(range, 2..4);
    }
}
