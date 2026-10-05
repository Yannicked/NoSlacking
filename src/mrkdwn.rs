//! Slack's message markup ("mrkdwn"), parsed into blocks of styled runs.
//!
//! Slack escapes `&`, `<` and `>` in message text and uses angle brackets
//! for everything special: `<@U123>` mentions, `<#C123|general>` channels,
//! `<!here>`, `<https://x.y|label>` links. Inside text, `*bold*`,
//! `_italic_`, `~strike~` and `` `code` `` style runs, and ```` ``` ````
//! fences preformatted blocks. Lines starting with `>` are quotes, and a
//! fence opened on a quoted line stays in its quote.
//!
//! [`parse`] never fails: anything it does not recognise stays text.

use std::collections::HashMap;

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

/// Whether `url` is safe to hand to the system opener: web pages and mail
/// only. Other schemes (`file:`, `smb:`, `C:\…`) can launch programs.
pub fn is_openable(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => rest.starts_with("//") && rest.len() > 2,
        "mailto" => !rest.is_empty(),
        _ => false,
    }
}

/// Escapes what Slack treats as markup in text a person typed.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Parses a message's text into blocks.
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
                        let line_start = before.rfind('\n').map_or(0, |at| at + 1);
                        rest = match quote_marker(&before[line_start..]) {
                            Some(lead) => {
                                lines(&before[..line_start], &mut blocks);
                                quoted_fence(lead, &after[..end], &after[end + 3..], &mut blocks)
                            }
                            None => {
                                lines(before, &mut blocks);
                                let code = after[..end].trim_matches('\n');
                                blocks.push(Block::Preformatted(unescape(code)));
                                after[end + 3..].trim_start_matches('\n')
                            }
                        };
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
    join_quotes(blocks)
}

/// A line's text after its quote marker, if it is quoted.
fn quote_marker(line: &str) -> Option<&str> {
    line.strip_prefix("&gt; ")
        .or_else(|| line.strip_prefix("&gt;"))
        .or_else(|| line.strip_prefix("> "))
}

/// A fence opened on a quoted line (`> ```code```  `): the code stays in
/// the quote, a line of code at a time, instead of the quote being lost.
/// `lead` is the quoted text before the fence, `code` what the fence holds
/// and `after` the text after it. Returns what is left to parse.
fn quoted_fence<'a>(lead: &str, code: &str, after: &'a str, blocks: &mut Vec<Block>) -> &'a str {
    let mut quote = Vec::new();
    inline(lead, Style::default(), &mut quote);
    // Each line of a quoted block carries its own marker after the first.
    let code: Vec<&str> = code
        .split('\n')
        .map(|line| quote_marker(line).unwrap_or(line))
        .collect();
    let first = code
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(code.len());
    let last = code
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(first, |l| l + 1);
    for line in &code[first..last] {
        if !quote.is_empty() {
            quote.push(Inline::Newline);
        }
        if !line.is_empty() {
            quote.push(Inline::Code(unescape(line)));
        }
    }
    // Text after the fence on the same line is still quoted.
    let (tail, rest) = after.split_once('\n').unwrap_or((after, ""));
    if !tail.trim().is_empty() {
        quote.push(Inline::Newline);
        inline(tail.trim_start(), Style::default(), &mut quote);
    }
    if !quote.is_empty() {
        blocks.push(Block::Quote(quote));
    }
    rest
}

/// Joins quotes that follow each other, which only a quoted fence leaves
/// apart: the lines before it, the fence and the lines after are one quote.
fn join_quotes(blocks: Vec<Block>) -> Vec<Block> {
    let mut joined: Vec<Block> = Vec::with_capacity(blocks.len());
    for block in blocks {
        match (joined.last_mut(), block) {
            (Some(Block::Quote(previous)), Block::Quote(next)) => {
                previous.push(Inline::Newline);
                previous.extend(next);
            }
            (_, block) => joined.push(block),
        }
    }
    joined
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
        let quoted = quote_marker(line);
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

/// Whether a style marker next to `c` stands at the edge of a word: the
/// line's ends, spaces, and anything that is not a letter or digit in any
/// script, so `“*bold*”` and `「*太字*」` work as `"*bold*"` does.
/// Combining accents belong to the letter before them.
fn is_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace() || !(c.is_alphanumeric() || is_combining(c)))
}

/// The common blocks of combining marks, which are not alphanumeric but
/// are part of a word.
fn is_combining(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Whether an emoji code may start after `c`: not glued to a Latin letter
/// or a digit, so the `:30:` in `10:30:00` stays text. Other scripts write
/// no spaces between words, so emoji may follow them directly. Only the
/// start is checked: Slack draws `:large_green_square:999`, and a time
/// already fails here, at the digit before its first colon.
fn emoji_edge(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_ascii_alphanumeric())
}

/// Where one line's markup characters are, found in a single pass. Every
/// question the parser asks ("where is the next `>`?") is then a binary
/// search instead of a scan of the rest of the line, which kept a long
/// line of unclosed markers (`*a *a *a …`) from taking quadratic time.
struct Marks {
    /// `<` (and line breaks): a `<…>` form must not contain one.
    opens: Vec<usize>,
    /// `>`, which closes a `<…>` form.
    closes: Vec<usize>,
    /// Backticks.
    ticks: Vec<usize>,
    /// Where each run of two or more backticks starts, by its length.
    runs: HashMap<usize, Vec<usize>>,
    /// For `*`, `_` and `~`, the places that can close a styled run: the
    /// marker hugs the text before it and a word boundary follows.
    closers: [Vec<usize>; 3],
}

impl Marks {
    fn new(text: &str) -> Self {
        let mut marks = Self {
            opens: Vec::new(),
            closes: Vec::new(),
            ticks: Vec::new(),
            runs: HashMap::new(),
            closers: [Vec::new(), Vec::new(), Vec::new()],
        };
        let mut previous: Option<char> = None;
        // The ticks seen in a row so far.
        let mut run = 0;
        let mut chars = text.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            match c {
                '<' | '\n' => marks.opens.push(at),
                '>' => marks.closes.push(at),
                '`' => {
                    marks.ticks.push(at);
                    run += 1;
                    // The run's last tick records where it started.
                    if chars.peek().is_none_or(|&(_, next)| next != '`') {
                        if run > 1 {
                            marks.runs.entry(run).or_default().push(at + 1 - run);
                        }
                        run = 0;
                    }
                }
                '*' | '_' | '~' => {
                    let hugs = previous.is_some_and(|p| !p.is_whitespace());
                    if hugs && is_boundary(chars.peek().map(|&(_, next)| next)) {
                        marks.closers[marker_index(c)].push(at);
                    }
                }
                _ => {}
            }
            previous = Some(c);
        }
        marks
    }
}

/// The first position in a sorted list at or after `from`.
fn next_at(positions: &[usize], from: usize) -> Option<usize> {
    positions
        .get(positions.partition_point(|&p| p < from))
        .copied()
}

fn marker_index(marker: char) -> usize {
    match marker {
        '*' => 0,
        '_' => 1,
        _ => 2,
    }
}

/// Parses one line of inline markup into `out`.
fn inline(text: &str, style: Style, out: &mut Vec<Inline>) {
    let marks = Marks::new(text);
    let mut plain_start = 0;
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        let c = bytes[i];
        let before = text[..i].chars().next_back();
        let consumed = match c {
            b'<' => next_at(&marks.closes, i + 1)
                // Nothing between the brackets may open another form.
                .filter(|&end| next_at(&marks.opens, i + 1).is_none_or(|open| open > end))
                .and_then(|end| {
                    // Adjacent runs of one style merge, so flushing early is
                    // harmless when the bracket turns out to be text.
                    flush(out, text, plain_start, i, style);
                    plain_start = i;
                    special(&text[i + 1..end], style, out).then_some(end + 1 - i)
                }),
            b'`' => match code(text, i, &marks) {
                Code::Span { inner, len } => {
                    flush(out, text, plain_start, i, style);
                    out.push(Inline::Code(unescape(inner)));
                    Some(len)
                }
                Code::Text { len } => {
                    // A run of backticks with nothing to close it is text,
                    // all of it: its second tick must not open a span.
                    i += len;
                    continue;
                }
                Code::None => None,
            },
            b'*' | b'_' | b'~' if is_boundary(before) => {
                styled(text, i, &marks, style).map(|(inner, inner_style, len)| {
                    flush(out, text, plain_start, i, style);
                    inline(inner, inner_style, out);
                    len
                })
            }
            b':' if emoji_edge(before) => emoji(&text[i..]).map(|(name, len)| {
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

/// What is between the brackets of a `<...>` form, which holds no `<` or
/// line break. Returns whether it was one; if not, it stays text.
fn special(inner: &str, style: Style, out: &mut Vec<Inline>) -> bool {
    if inner.is_empty() {
        return false;
    }
    // An empty label (`<@U1|>`) is no label: the name is looked up instead.
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(unescape(label)).filter(|l| !l.is_empty())),
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
        let name = command.split('^').next().unwrap_or(command);
        match (name, command.strip_prefix("subteam^")) {
            (_, Some(id)) => Inline::Group {
                id: id.to_owned(),
                label,
            },
            // Only these notify a whole channel; drawing anything else
            // (`<!foo>`) as a broadcast would claim a ping that never was.
            ("here" | "channel" | "everyone", _) => Inline::Broadcast(name.to_owned()),
            // A date (`<!date^1700000000^{date}|Nov 14>`) and commands this
            // client does not know show their fallback text; with none,
            // what was written stays as it is.
            _ => {
                let Some(label) = label else {
                    return false;
                };
                push_plain(out, label, style);
                return true;
            }
        }
    } else if target.contains(':') {
        let url = unescape(target);
        if !is_openable(&url) {
            // A `file:`, `smb:` or drive path would run whatever it names
            // when clicked; show what was written instead of a link.
            push_plain(out, label.unwrap_or(url), style);
            return true;
        }
        Inline::Link { url, label, style }
    } else {
        return false;
    };
    out.push(item);
    true
}

/// Text that is already unescaped, merged into the run before it when the
/// style matches.
fn push_plain(out: &mut Vec<Inline>, text: String, style: Style) {
    match out.last_mut() {
        Some(Inline::Text(previous, previous_style)) if *previous_style == style => {
            previous.push_str(&text);
        }
        _ => out.push(Inline::Text(text, style)),
    }
}

/// What a backtick starts.
enum Code<'a> {
    /// A code span: what it holds, and the bytes it takes with its ticks.
    Span { inner: &'a str, len: usize },
    /// A run of `len` ticks that nothing closes.
    Text { len: usize },
    /// A single tick that nothing closes.
    None,
}

/// The code span opened by the backticks at `text[at]`.
fn code<'a>(text: &'a str, at: usize, marks: &Marks) -> Code<'a> {
    let run = text.as_bytes()[at..]
        .iter()
        .take_while(|&&b| b == b'`')
        .count();
    if run == 1 {
        // One tick closes at the next tick, whatever follows it.
        return match next_at(&marks.ticks, at + 1) {
            Some(end) => Code::Span {
                inner: &text[at + 1..end],
                len: end + 1 - at,
            },
            None => Code::None,
        };
    }
    // ``code with a ` inside`` closes at a run as long as its opener.
    let close = marks
        .runs
        .get(&run)
        .and_then(|starts| next_at(starts, at + run));
    let Some(end) = close else {
        return Code::Text { len: run };
    };
    let inner = &text[at + run..end];
    // One space may pad each side, so a span can start or end with a tick:
    // `` `x` `` is `x` in backticks.
    let inner = match inner.strip_prefix(' ').and_then(|s| s.strip_suffix(' ')) {
        Some(padded) if !padded.trim().is_empty() => padded,
        _ => inner,
    };
    Code::Span {
        inner,
        len: end + run - at,
    }
}

/// A styled run opened by the marker at `text[at]`: the inner text, its
/// style and the bytes consumed. The closer must hug the text and end at a
/// word boundary, on the same line.
fn styled<'a>(
    text: &'a str,
    at: usize,
    marks: &Marks,
    style: Style,
) -> Option<(&'a str, Style, usize)> {
    let marker = char::from(text.as_bytes()[at]);
    let after = &text[at + 1..];
    if after.starts_with(char::is_whitespace) || after.starts_with(marker) {
        return None;
    }
    // The first closer past the opener; it cannot be right after it, as
    // that was just ruled out, so the inner text is never empty.
    let close = next_at(&marks.closers[marker_index(marker)], at + 1)?;
    let mut style = style;
    match marker {
        '*' => style.bold = true,
        '_' => style.italic = true,
        _ => style.strike = true,
    }
    Some((&text[at + 1..close], style, close + 1 - at))
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

/// Parsed texts kept between frames, so an immediate-mode view need not
/// parse every message on every frame.
///
/// Keyed by the text itself, so an edited message is parsed afresh and two
/// texts can never share an entry. Call [`ParseCache::sweep`] once a frame:
/// it drops what was not asked for since the last sweep, so the cache
/// holds about what is on screen.
#[derive(Debug, Default)]
pub struct ParseCache {
    entries: HashMap<String, (std::sync::Arc<[Block]>, bool)>,
}

impl ParseCache {
    /// The blocks of `text`, parsed now or on an earlier frame.
    pub fn get(&mut self, text: &str) -> std::sync::Arc<[Block]> {
        if let Some((blocks, used)) = self.entries.get_mut(text) {
            *used = true;
            return blocks.clone();
        }
        let blocks: std::sync::Arc<[Block]> = parse(text).into();
        self.entries.insert(text.to_owned(), (blocks.clone(), true));
        blocks
    }

    /// Forgets the texts not asked for since the last sweep.
    pub fn sweep(&mut self) {
        self.entries.retain(|_, (_, used)| std::mem::take(used));
    }

    /// How many texts are kept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is kept.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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
                        Inline::Emoji(name) => {
                            // The tone is not part of the name the tables know.
                            let (base, tone) = crate::emoji::split_tone(name);
                            match crate::emoji::unicode(base, tone) {
                                Some(unicode) => out.push_str(&unicode),
                                None => out.push_str(&format!(":{name}:")),
                            }
                        }
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
    fn only_web_and_mail_links_are_links() {
        assert!(is_openable("https://x.y/a"));
        assert!(is_openable("HTTP://x.y"));
        assert!(is_openable("mailto:a@b.c"));
        for unsafe_url in [
            "C:\\Users\\Public\\x.exe",
            "file:///etc/passwd",
            "smb://host/share",
            "javascript:alert(1)",
            "https:",
            "https://",
            "nothing",
        ] {
            assert!(!is_openable(unsafe_url), "{unsafe_url}");
        }
        assert_eq!(
            parse("<C:\\x.exe|report.pdf> and <file:///etc/passwd>"),
            [Block::Paragraph(vec![text(
                "report.pdf and file:///etc/passwd"
            )])]
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
    fn plain_text_keeps_skin_tones() {
        assert_eq!(
            plain(":+1::skin-tone-2: :wave::skin-tone-6:", |_| None),
            "👍🏻 👋🏿"
        );
        assert_eq!(
            plain(":nope::skin-tone-2:", |_| None),
            ":nope::skin-tone-2:"
        );
    }

    /// Long lines of markup that never closes. Each used to make every
    /// marker scan the rest of the line: 40 KB of `*a ` took about 600 ms
    /// in a release build.
    fn pathological() -> Vec<(&'static str, String)> {
        const SIZE: usize = 40 * 1024;
        let repeat = |unit: &str| unit.repeat(SIZE / unit.len());
        vec![
            ("bold", repeat("*a ")),
            ("italic", repeat("_a ")),
            ("strike", repeat("~a ")),
            ("mixed", repeat("*a _b ~c ")),
            ("brackets", repeat("<a ")),
            ("nested brackets", format!("{}>", repeat("<"))),
            ("ticks", repeat("``a")),
            ("colons", repeat(":a b")),
            ("tones", repeat(":a::skin-tone-")),
            ("quotes", repeat("&gt; *a\n")),
            ("fences", repeat("``` *a ")),
            ("quoted fences", repeat("&gt; ```a\n")),
            ("tick runs", repeat("`` ```a ")),
        ]
    }

    #[test]
    fn long_unclosed_markup_parses_in_linear_time() {
        // A generous budget for an unoptimized build on a busy machine;
        // the quadratic parser took many seconds here.
        let budget = std::time::Duration::from_secs(1);
        for (name, text) in pathological() {
            let start = std::time::Instant::now();
            let blocks = parse(&text);
            let took = start.elapsed();
            assert!(!blocks.is_empty(), "{name}");
            assert!(took < budget, "{name}: {took:?} for {} bytes", text.len());
        }
    }

    fn paragraph(text: &str) -> Vec<Inline> {
        match parse(text).as_slice() {
            [Block::Paragraph(inlines)] => inlines.clone(),
            other => panic!("one paragraph from {text:?}, got {other:?}"),
        }
    }

    #[test]
    fn only_known_broadcasts_are_broadcasts() {
        assert_eq!(
            paragraph("<!here> <!channel> <!everyone|everyone>"),
            [
                Inline::Broadcast("here".into()),
                text(" "),
                Inline::Broadcast("channel".into()),
                text(" "),
                Inline::Broadcast("everyone".into()),
            ]
        );
        assert_eq!(paragraph("hi <!foo>"), [text("hi <!foo>")]);
        assert_eq!(paragraph("hi <!foo|bar>!"), [text("hi bar!")]);
    }

    #[test]
    fn groups_dates_and_unclosed_brackets() {
        assert_eq!(
            paragraph("<!subteam^S1|@design> and <!subteam^S2>"),
            [
                Inline::Group {
                    id: "S1".into(),
                    label: Some("@design".into())
                },
                text(" and "),
                Inline::Group {
                    id: "S2".into(),
                    label: None
                },
            ]
        );
        assert_eq!(
            paragraph("due <!date^1700000000^{date_short}|Nov 14, 2023> ok"),
            [text("due Nov 14, 2023 ok")]
        );
        for unclosed in ["a <@U1", "<", "a <b <@U1>", "<<<", "x <#C1|gen"] {
            let inlines = paragraph(unclosed);
            assert!(
                inlines
                    .iter()
                    .all(|i| matches!(i, Inline::Text(..) | Inline::User { .. })),
                "{unclosed:?}: {inlines:?}"
            );
        }
        assert_eq!(paragraph("a <@U1"), [text("a <@U1")]);
        assert_eq!(
            paragraph("a <b <@U1>"),
            [
                text("a <b "),
                Inline::User {
                    id: "U1".into(),
                    label: None
                }
            ]
        );
    }

    #[test]
    fn user_and_channel_labels() {
        assert_eq!(
            paragraph("<@U1|ann> <@U2|> <#C1|> <!subteam^S1|>"),
            [
                Inline::User {
                    id: "U1".into(),
                    label: Some("ann".into())
                },
                text(" "),
                Inline::User {
                    id: "U2".into(),
                    label: None
                },
                text(" "),
                Inline::Channel {
                    id: "C1".into(),
                    label: None
                },
                text(" "),
                Inline::Group {
                    id: "S1".into(),
                    label: None
                },
            ]
        );
        assert_eq!(
            paragraph("<@U1|a &amp; b>"),
            [Inline::User {
                id: "U1".into(),
                label: Some("a & b".into())
            }]
        );
    }

    #[test]
    fn word_boundaries_know_other_scripts() {
        assert_eq!(paragraph("“*bold*”"), [text("“"), bold("bold"), text("”")]);
        assert_eq!(
            paragraph("「*太字*」です"),
            [text("「"), bold("太字"), text("」です")]
        );
        assert_eq!(paragraph("日本*太字*です"), [text("日本*太字*です")]);
        // An accent written as a combining mark is part of its letter.
        assert_eq!(paragraph("cafe\u{301}*x*"), [text("cafe\u{301}*x*")]);
        assert_eq!(paragraph("naïve_word_here"), [text("naïve_word_here")]);
    }

    #[test]
    fn double_backticks_hold_single_ones() {
        assert_eq!(
            paragraph("run ``a ` b`` now"),
            [text("run "), Inline::Code("a ` b".into()), text(" now")]
        );
        assert_eq!(paragraph("`` `x` ``"), [Inline::Code("`x`".into())]);
        assert_eq!(paragraph("``unclosed"), [text("``unclosed")]);
        assert_eq!(paragraph("``a`b"), [text("``a`b")]);
    }

    #[test]
    fn a_quoted_fence_stays_quoted() {
        assert_eq!(
            parse("&gt; ```let x = 1;```"),
            [Block::Quote(vec![Inline::Code("let x = 1;".into())])]
        );
        assert_eq!(
            parse("&gt; look\n&gt; ```\n&gt; a\n&gt; b\n&gt; ```\n&gt; after\nplain"),
            [
                Block::Quote(vec![
                    text("look"),
                    Inline::Newline,
                    Inline::Code("a".into()),
                    Inline::Newline,
                    Inline::Code("b".into()),
                    Inline::Newline,
                    text("after"),
                ]),
                Block::Paragraph(vec![text("plain")]),
            ]
        );
        assert_eq!(
            parse("&gt; see ```x``` there"),
            [Block::Quote(vec![
                text("see "),
                Inline::Newline,
                Inline::Code("x".into()),
                Inline::Newline,
                text("there"),
            ])]
        );
        // An unquoted fence after a quote still ends it.
        assert_eq!(
            parse("&gt; q\n```x```"),
            [
                Block::Quote(vec![text("q")]),
                Block::Preformatted("x".into())
            ]
        );
    }

    #[test]
    fn emoji_need_their_own_word() {
        assert_eq!(paragraph("at 10:30:00 ok"), [text("at 10:30:00 ok")]);
        assert_eq!(paragraph("key:value:x"), [text("key:value:x")]);
        assert_eq!(
            paragraph("ok:tada: (:tada:) すごい:tada:"),
            [
                text("ok:tada: ("),
                Inline::Emoji("tada".into()),
                text(") すごい"),
                Inline::Emoji("tada".into()),
            ]
        );
        assert_eq!(
            paragraph(":tada::tada:"),
            [Inline::Emoji("tada".into()), Inline::Emoji("tada".into())]
        );
    }

    #[test]
    fn emoji_may_run_into_the_text_after_them() {
        assert_eq!(
            paragraph(":large_blue_square:1000 :large_green_square:999"),
            [
                Inline::Emoji("large_blue_square".into()),
                text("1000 "),
                Inline::Emoji("large_green_square".into()),
                text("999"),
            ]
        );
        assert_eq!(
            paragraph("(:tada:ok)"),
            [text("("), Inline::Emoji("tada".into()), text("ok)")]
        );
    }

    /// Pieces that trip parsers: every kind of marker, half-open forms,
    /// escapes, and characters wider than a byte.
    const FRAGMENTS: &[&str] = &[
        "*",
        "_",
        "~",
        "`",
        "``",
        "```",
        "<",
        ">",
        "|",
        "@",
        "#",
        "!",
        ":",
        "^",
        "&",
        ";",
        "&gt;",
        "&gt; ",
        "&lt;",
        "&amp;",
        "&am",
        "> ",
        " ",
        "\n",
        "\t",
        "a",
        "Z",
        "9",
        "10:30",
        "é",
        "e\u{301}",
        "日本",
        "「",
        "」",
        "“",
        "”",
        "。",
        "😀",
        "👍🏽",
        "\u{200d}",
        "<@U1>",
        "<@U1|>",
        "<@",
        "<#C1|gen>",
        "<#",
        "<!here>",
        "<!foo>",
        "<!",
        "<!subteam^S1|@x>",
        "<!subteam^",
        "<!date^1^{date}|d>",
        "<!date^",
        "<https://x.y|l>",
        "<https://",
        "<file:///x>",
        "<mailto:a@b.c>",
        ":tada:",
        ":+1::skin-tone-2:",
        "::skin-tone-",
        ":a:",
    ];

    /// A small deterministic generator (xorshift), so a failure always
    /// reproduces.
    fn generated(count: usize) -> impl Iterator<Item = String> {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..count).map(move |_| {
            let pieces = next() % 24;
            (0..pieces)
                .map(|_| FRAGMENTS[(next() % FRAGMENTS.len() as u64) as usize])
                .collect()
        })
    }

    #[test]
    fn parsing_never_panics() {
        for input in generated(50_000) {
            let blocks = parse(&input);
            let _ = only_emoji(&blocks);
            let _ = plain(&input, |_| Some("@someone".into()));
            for block in &blocks {
                if let Block::Paragraph(inlines) | Block::Quote(inlines) = block {
                    assert!(
                        !inlines
                            .iter()
                            .any(|i| matches!(i, Inline::Text(t, _) if t.is_empty())),
                        "no empty runs from {input:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_parse_cache_keeps_what_is_used() {
        let mut cache = ParseCache::default();
        let first = cache.get("*hi*");
        assert_eq!(&*first, parse("*hi*").as_slice());
        assert!(
            std::sync::Arc::ptr_eq(&first, &cache.get("*hi*")),
            "parsed once"
        );
        cache.get("other");
        cache.sweep();
        assert_eq!(cache.len(), 2, "both were used before the sweep");
        cache.get("*hi*");
        cache.sweep();
        assert_eq!(cache.len(), 1, "unused since the last sweep");
        cache.sweep();
        assert!(cache.is_empty());
    }

    #[test]
    fn escaping_round_trips() {
        assert_eq!(unescape(&escape("a < b & c > d")), "a < b & c > d");
    }
}
