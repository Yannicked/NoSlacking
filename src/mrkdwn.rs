//! Slack's message markup ("mrkdwn"), parsed into blocks of styled runs.
//!
//! Slack escapes `&`, `<` and `>` in message text and uses angle brackets
//! for everything special: `<@U123>` mentions, `<#C123|general>` channels,
//! `<!here>`, `<https://x.y|label>` links. Inside text, `*bold*`,
//! `_italic_`, `~strike~` and `` `code` `` style runs, and ```` ``` ````
//! fences preformatted blocks. Lines starting with `>` are quotes.
//!
//! [`parse`] never fails: anything it does not recognise stays text.

/// How a run of text is styled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inline {
    Text(String, Style),
    Code(String),
    Link {
        url: String,
        label: Option<String>,
        style: Style,
    },
    User {
        id: String,
        label: Option<String>,
    },
    Channel {
        id: String,
        label: Option<String>,
    },
    /// `@here`, `@channel`, `@everyone`.
    Broadcast(String),
    /// A user group, with Slack's label (`@design`).
    Group {
        id: String,
        label: Option<String>,
    },
    Emoji(String),
    Newline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Quote(Vec<Inline>),
    Preformatted(String),
}

/// `&amp;`, `&lt;` and `&gt;` back to characters.
pub fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Escapes what Slack treats as markup in text a person typed.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn parse(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find("```") {
            Some(start) => {
                let (before, after) = rest.split_at(start);
                let after = &after[3..];
                match after.find("```") {
                    Some(end) => {
                        lines(before, &mut blocks);
                        let code = after[..end].trim_matches('\n');
                        blocks.push(Block::Preformatted(unescape(code)));
                        rest = after[end + 3..].trim_start_matches('\n');
                    }
                    None => {
                        lines(rest, &mut blocks);
                        rest = "";
                    }
                }
            }
            None => {
                lines(rest, &mut blocks);
                rest = "";
            }
        }
    }
    blocks
}

/// Groups lines into paragraphs and quotes.
fn lines(text: &str, blocks: &mut Vec<Block>) {
    if text.is_empty() {
        return;
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut paragraph: Vec<Inline> = Vec::new();
    let mut quote: Vec<Inline> = Vec::new();
    for line in text.split('\n') {
        let quoted = line
            .strip_prefix("&gt; ")
            .or_else(|| line.strip_prefix("&gt;"))
            .or_else(|| line.strip_prefix("> "));
        match quoted {
            Some(inner) => {
                if !paragraph.is_empty() {
                    trim_newline(&mut paragraph);
                    blocks.push(Block::Paragraph(std::mem::take(&mut paragraph)));
                }
                if !quote.is_empty() {
                    quote.push(Inline::Newline);
                }
                inline(inner, Style::default(), &mut quote);
            }
            None => {
                if !quote.is_empty() {
                    blocks.push(Block::Quote(std::mem::take(&mut quote)));
                }
                if !paragraph.is_empty() {
                    paragraph.push(Inline::Newline);
                }
                inline(line, Style::default(), &mut paragraph);
            }
        }
    }
    if !quote.is_empty() {
        blocks.push(Block::Quote(quote));
    }
    if !paragraph.is_empty() {
        trim_newline(&mut paragraph);
        blocks.push(Block::Paragraph(paragraph));
    }
}

fn trim_newline(inlines: &mut Vec<Inline>) {
    while matches!(inlines.last(), Some(Inline::Newline)) {
        inlines.pop();
    }
}

fn push_text(out: &mut Vec<Inline>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    let text = unescape(text);
    if let Some(Inline::Text(previous, previous_style)) = out.last_mut()
        && *previous_style == style
    {
        previous.push_str(&text);
        return;
    }
    out.push(Inline::Text(text, style));
}

fn is_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace() || c.is_ascii_punctuation())
}

/// Parses one line of inline markup into `out`.
fn inline(text: &str, style: Style, out: &mut Vec<Inline>) {
    let mut plain_start = 0;
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        let c = bytes[i];
        let before = text[..i].chars().next_back();
        let consumed = match c {
            b'<' => {
                // Adjacent runs of one style merge, so flushing early is
                // harmless when the bracket turns out to be text.
                flush(out, text, plain_start, i, style);
                plain_start = i;
                special(&text[i..], style, out)
            }
            b'`' => text[i + 1..].find('`').filter(|&end| end > 0).map(|end| {
                flush(out, text, plain_start, i, style);
                out.push(Inline::Code(unescape(&text[i + 1..i + 1 + end])));
                end + 2
            }),
            b'*' | b'_' | b'~' if is_boundary(before) => {
                styled(&text[i..], c as char, style).map(|(inner, inner_style, len)| {
                    flush(out, text, plain_start, i, style);
                    inline(inner, inner_style, out);
                    len
                })
            }
            b':' => emoji(&text[i..]).map(|(name, len)| {
                flush(out, text, plain_start, i, style);
                out.push(Inline::Emoji(name.to_owned()));
                len
            }),
            _ => None,
        };
        match consumed {
            Some(len) => {
                i += len;
                plain_start = i;
            }
            None => i += text[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    flush(out, text, plain_start, text.len(), style);
}

fn flush(out: &mut Vec<Inline>, text: &str, start: usize, end: usize, style: Style) {
    if start < end {
        push_text(out, &text[start..end], style);
    }
}

/// `<...>` forms. Returns the bytes consumed.
fn special(text: &str, style: Style, out: &mut Vec<Inline>) -> Option<usize> {
    let end = text.find('>')?;
    let inner = &text[1..end];
    if inner.is_empty() || inner.contains('<') || inner.contains('\n') {
        return None;
    }
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(unescape(label))),
        None => (inner, None),
    };
    let item = if let Some(id) = target.strip_prefix('@') {
        Inline::User {
            id: id.to_owned(),
            label,
        }
    } else if let Some(id) = target.strip_prefix('#') {
        Inline::Channel {
            id: id.to_owned(),
            label,
        }
    } else if let Some(command) = target.strip_prefix('!') {
        if let Some(id) = command.strip_prefix("subteam^") {
            Inline::Group {
                id: id.to_owned(),
                label,
            }
        } else if command.starts_with("date^") {
            Inline::Text(label.unwrap_or_default(), style)
        } else {
            Inline::Broadcast(command.split('^').next().unwrap_or(command).to_owned())
        }
    } else if target.contains(':') {
        Inline::Link {
            url: unescape(target),
            label: label.filter(|l| !l.is_empty()),
            style,
        }
    } else {
        return None;
    };
    out.push(item);
    Some(end + 1)
}

/// A styled run starting at `text[0] == marker`: the inner text, its style
/// and the bytes consumed. The closer must hug the text and end at a word
/// boundary, on the same line.
fn styled(text: &str, marker: char, style: Style) -> Option<(&str, Style, usize)> {
    let after = &text[1..];
    if after.starts_with(char::is_whitespace) || after.starts_with(marker) {
        return None;
    }
    let mut search = 0;
    while let Some(offset) = after[search..].find(marker) {
        let at = search + offset;
        let inner = &after[..at];
        let next = after[at + 1..].chars().next();
        if !inner.is_empty() && !inner.ends_with(char::is_whitespace) && is_boundary(next) {
            let mut style = style;
            match marker {
                '*' => style.bold = true,
                '_' => style.italic = true,
                _ => style.strike = true,
            }
            return Some((inner, style, at + 2));
        }
        search = at + 1;
    }
    None
}

/// `:name:` at the start of `text`: the name and the bytes consumed.
fn emoji(text: &str) -> Option<(&str, usize)> {
    let after = &text[1..];
    let mut end = after.find(':')?;
    // A skin tone is part of the name: `:+1::skin-tone-2:`.
    if after[end..].starts_with("::skin-tone-")
        && let Some(close) = after[end + 2..].find(':')
    {
        end = end + 2 + close;
    }
    let name = &after[..end];
    let valid = !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '\'' | ':'));
    valid.then_some((name, end + 2))
}

/// Whether a message is only emoji (and whitespace), drawn large like Slack.
pub fn only_emoji(blocks: &[Block]) -> bool {
    let [Block::Paragraph(inlines)] = blocks else {
        return false;
    };
    let mut count = 0;
    for inline in inlines {
        match inline {
            Inline::Emoji(_) => count += 1,
            Inline::Text(text, _) if text.trim().is_empty() => {}
            Inline::Text(text, _) if fastframe_emoji::only_emoji(text.trim()).is_some() => {
                count += 1
            }
            _ => return false,
        }
    }
    (1..=23).contains(&count)
}

/// Plain text for previews and notifications: markup dropped, mentions and
/// links as their labels.
pub fn plain(text: &str, name_of: impl Fn(&Inline) -> Option<String>) -> String {
    let mut out = String::new();
    for block in parse(text) {
        if !out.is_empty() {
            out.push(' ');
        }
        match block {
            Block::Preformatted(code) => out.push_str(&code),
            Block::Paragraph(inlines) | Block::Quote(inlines) => {
                for inline in &inlines {
                    match inline {
                        Inline::Text(text, _) | Inline::Code(text) => out.push_str(text),
                        Inline::Newline => out.push(' '),
                        Inline::Link { url, label, .. } => {
                            out.push_str(label.as_deref().unwrap_or(url));
                        }
                        Inline::Broadcast(name) => {
                            out.push('@');
                            out.push_str(name);
                        }
                        Inline::Emoji(name) => match crate::emoji::unicode(name, None) {
                            Some(unicode) => out.push_str(&unicode),
                            None => out.push_str(&format!(":{name}:")),
                        },
                        other => out.push_str(&name_of(other).unwrap_or_default()),
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text(s.into(), Style::default())
    }

    fn bold(s: &str) -> Inline {
        Inline::Text(
            s.into(),
            Style {
                bold: true,
                ..Style::default()
            },
        )
    }

    #[test]
    fn plain_text_stays_one_run_and_is_unescaped() {
        assert_eq!(
            parse("a &lt;b&gt; &amp; c"),
            [Block::Paragraph(vec![text("a <b> & c")])]
        );
    }

    #[test]
    fn styles_need_word_boundaries() {
        assert_eq!(
            parse("this is *bold* and 2*3*4"),
            [Block::Paragraph(vec![
                text("this is "),
                bold("bold"),
                text(" and 2*3*4")
            ])]
        );
        assert_eq!(
            parse("snake_case_name"),
            [Block::Paragraph(vec![text("snake_case_name")])]
        );
        assert_eq!(
            parse("* not a list"),
            [Block::Paragraph(vec![text("* not a list")])]
        );
    }

    #[test]
    fn styles_nest() {
        let Block::Paragraph(inlines) = &parse("*bold _both_*")[0] else {
            panic!("paragraph");
        };
        assert_eq!(
            inlines[1],
            Inline::Text(
                "both".into(),
                Style {
                    bold: true,
                    italic: true,
                    strike: false
                }
            )
        );
    }

    #[test]
    fn angle_brackets_make_mentions_channels_and_links() {
        assert_eq!(
            parse(
                "hi <@U1> in <#C1|general>, <!here> see <https://x.y/?a=1&amp;b=2|this> or <mailto:a@b.c>"
            ),
            [Block::Paragraph(vec![
                text("hi "),
                Inline::User {
                    id: "U1".into(),
                    label: None
                },
                text(" in "),
                Inline::Channel {
                    id: "C1".into(),
                    label: Some("general".into())
                },
                text(", "),
                Inline::Broadcast("here".into()),
                text(" see "),
                Inline::Link {
                    url: "https://x.y/?a=1&b=2".into(),
                    label: Some("this".into()),
                    style: Style::default()
                },
                text(" or "),
                Inline::Link {
                    url: "mailto:a@b.c".into(),
                    label: None,
                    style: Style::default()
                },
            ])]
        );
        assert_eq!(
            parse("a < b > c"),
            [Block::Paragraph(vec![text("a < b > c")])]
        );
    }

    #[test]
    fn code_emoji_and_tones() {
        assert_eq!(
            parse("run `ls *` now :+1::skin-tone-2: :tada:"),
            [Block::Paragraph(vec![
                text("run "),
                Inline::Code("ls *".into()),
                text(" now "),
                Inline::Emoji("+1::skin-tone-2".into()),
                text(" "),
                Inline::Emoji("tada".into()),
            ])]
        );
        assert_eq!(
            parse("at 10:30 ok"),
            [Block::Paragraph(vec![text("at 10:30 ok")])]
        );
    }

    #[test]
    fn fences_and_quotes_make_blocks() {
        assert_eq!(
            parse("look:\n```\nfn main() {}\n```\n&gt; quoted *line*\n&gt; two\nafter"),
            [
                Block::Paragraph(vec![text("look:")]),
                Block::Preformatted("fn main() {}".into()),
                Block::Quote(vec![
                    text("quoted "),
                    bold("line"),
                    Inline::Newline,
                    text("two")
                ]),
                Block::Paragraph(vec![text("after")]),
            ]
        );
        assert_eq!(
            parse("```unterminated"),
            [Block::Paragraph(vec![text("```unterminated")])]
        );
    }

    #[test]
    fn emoji_only_messages_are_recognised() {
        assert!(only_emoji(&parse(":tada: :tada:")));
        assert!(!only_emoji(&parse(":tada: yay")));
        assert!(!only_emoji(&parse("")));
    }

    #[test]
    fn plain_text_for_previews() {
        assert_eq!(
            plain("*hi* <@U1> :tada: <https://x.y|link>", |_| Some(
                "@Ann".into()
            )),
            "hi @Ann 🎉 link"
        );
    }

    #[test]
    fn escaping_round_trips() {
        assert_eq!(unescape(&escape("a < b & c > d")), "a < b & c > d");
    }
}
