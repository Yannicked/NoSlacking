//! Slack's `rich_text` blocks, read into the blocks the interface draws.
//!
//! What people write in Slack's own apps arrives twice: as mrkdwn in
//! `text`, and as a `rich_text` block that already says which part is an
//! emoji, a mention, a link or bold. Reading the block keeps Slack's own
//! answer instead of guessing it again from the mrkdwn: `:tada:1000` is an
//! emoji and a number because Slack says so, and a `*` someone typed stays
//! a star.
//!
//! The text in a `rich_text` block is not escaped the way `text` is, so it
//! is taken as it stands. Anything this does not know is left out, as the
//! mrkdwn parser leaves unknown markup as text: the block is never worse
//! than what it replaces.

use serde_json::Value;

use crate::mrkdwn::{Block, Inline, Style, is_openable};

/// The blocks of one `rich_text` block, in reading order.
pub fn blocks(block: &Value) -> Vec<Block> {
    let mut out = Vec::new();
    let mut paragraph = Vec::new();
    // Whether the last block is a list inside a quote, which the quote or
    // quoted list right after it continues.
    let mut quoted_list = false;
    for element in elements(block) {
        let bordered = kind(element) == "rich_text_list"
            && element.get("border").and_then(Value::as_u64).unwrap_or(0) > 0;
        match kind(element) {
            "rich_text_section" => {
                inlines(element, &mut paragraph);
            }
            // A list with a border is quoted: Slack draws the quote's bar
            // beside it.
            "rich_text_list" if bordered => {
                flush(&mut paragraph, &mut out);
                let mut quote = Vec::new();
                list(element, &mut quote);
                trim(&mut quote);
                push_quote(&mut out, quote, true);
            }
            "rich_text_list" => list(element, &mut paragraph),
            "rich_text_quote" => {
                flush(&mut paragraph, &mut out);
                let mut quote = Vec::new();
                inlines(element, &mut quote);
                trim(&mut quote);
                push_quote(&mut out, quote, quoted_list);
            }
            "rich_text_preformatted" => {
                flush(&mut paragraph, &mut out);
                let code = plain(element);
                let code = code.trim_matches('\n');
                if !code.is_empty() {
                    out.push(Block::Preformatted(code.to_owned()));
                }
            }
            _ => {}
        }
        quoted_list = bordered;
    }
    flush(&mut paragraph, &mut out);
    out
}

/// Adds a quote, continuing the quote just before it when `join` says the
/// two are one: a quote and the quoted list that follows it.
fn push_quote(out: &mut Vec<Block>, quote: Vec<Inline>, join: bool) {
    if quote.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(Block::Quote(previous)) if join => {
            previous.push(Inline::Newline);
            previous.extend(quote);
        }
        _ => out.push(Block::Quote(quote)),
    }
}

fn elements(node: &Value) -> impl Iterator<Item = &Value> {
    node.get("elements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn kind(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

fn str_at<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get(key).and_then(Value::as_str)
}

/// Ends the paragraph being built, as the mrkdwn parser ends one: without
/// the line breaks around it.
fn flush(paragraph: &mut Vec<Inline>, out: &mut Vec<Block>) {
    trim(paragraph);
    if !paragraph.is_empty() {
        out.push(Block::Paragraph(std::mem::take(paragraph)));
    }
}

/// Drops line breaks at both ends; a section after a quote often starts
/// with the break that ended the quote.
fn trim(inlines: &mut Vec<Inline>) {
    while matches!(inlines.last(), Some(Inline::Newline)) {
        inlines.pop();
    }
    let lead = inlines
        .iter()
        .take_while(|inline| matches!(inline, Inline::Newline))
        .count();
    inlines.drain(..lead);
}

/// A list, one item to a line, marked and indented as Slack's apps show it.
/// The interface has no list block, and the mrkdwn of the same message is
/// also lines of text.
fn list(list: &Value, paragraph: &mut Vec<Inline>) {
    if !matches!(paragraph.last(), None | Some(Inline::Newline)) {
        paragraph.push(Inline::Newline);
    }
    let ordered = str_at(list, "style") == Some("ordered");
    let indent = list.get("indent").and_then(Value::as_u64).unwrap_or(0);
    // Bounded, so counting on from a wild offset cannot overflow.
    let first = list
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u64::from(u32::MAX))
        + 1;
    let pad = "    ".repeat(usize::try_from(indent).unwrap_or(0).min(8));
    for (number, item) in (first..).zip(elements(list)) {
        let marker = if ordered {
            format!("{pad}{}. ", ordinal(number, indent))
        } else {
            format!("{pad}• ")
        };
        push(paragraph, marker, Style::default());
        inlines(item, paragraph);
        paragraph.push(Inline::Newline);
    }
}

/// An ordered list item's number as Slack writes it at an indent: `1.`,
/// then `a.` one level in, then `i.`, and round again.
pub(super) fn ordinal(number: u64, indent: u64) -> String {
    match indent % 3 {
        1 => letters(number),
        2 => roman(number).unwrap_or_else(|| number.to_string()),
        _ => number.to_string(),
    }
}

/// `1` is `a`, `26` is `z`, `27` is `aa`: letters as a spreadsheet counts
/// its columns, so every number has one.
fn letters(mut number: u64) -> String {
    let mut out = Vec::new();
    while number > 0 {
        number -= 1;
        out.push(char::from(b'a' + u8::try_from(number % 26).unwrap_or(0)));
        number /= 26;
    }
    out.iter().rev().collect()
}

/// Lowercase Roman numerals, which stop at 3999; past that, none.
fn roman(mut number: u64) -> Option<String> {
    const NUMERALS: [(u64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    if !(1..4000).contains(&number) {
        return None;
    }
    let mut out = String::new();
    for (value, numeral) in NUMERALS {
        while number >= value {
            out.push_str(numeral);
            number -= value;
        }
    }
    Some(out)
}

/// A section's (or quote's) inline elements.
fn inlines(node: &Value, out: &mut Vec<Inline>) {
    for element in elements(node) {
        let style = style(element);
        match kind(element) {
            "text" => {
                let text = str_at(element, "text").unwrap_or("");
                if element.pointer("/style/code").and_then(Value::as_bool) == Some(true) {
                    out.push(Inline::Code(text.to_owned()));
                } else {
                    text_lines(out, text, style);
                }
            }
            "link" => {
                let url = str_at(element, "url").unwrap_or("").to_owned();
                let label = str_at(element, "text")
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
                if is_openable(&url) {
                    out.push(Inline::Link { url, label, style });
                } else {
                    // As in mrkdwn: a `file:` or drive path would run what
                    // it names when clicked, so it shows as text.
                    text_lines(out, label.as_deref().unwrap_or(&url), style);
                }
            }
            "emoji" => {
                if let Some(name) = str_at(element, "name").filter(|name| !name.is_empty()) {
                    // An emoji newer than the table still shows, as the
                    // characters Slack gave alongside its name.
                    match emoji_unicode(element) {
                        Some(text) if crate::emoji::unicode(name, None).is_none() => {
                            push(out, text, style);
                        }
                        _ => out.push(Inline::Emoji(emoji_name(name, element))),
                    }
                }
            }
            "user" => {
                if let Some(id) = str_at(element, "user_id") {
                    out.push(Inline::User {
                        id: id.to_owned(),
                        label: None,
                    });
                }
            }
            "usergroup" => {
                if let Some(id) = str_at(element, "usergroup_id") {
                    out.push(Inline::Group {
                        id: id.to_owned(),
                        label: None,
                    });
                }
            }
            "channel" => {
                if let Some(id) = str_at(element, "channel_id") {
                    out.push(Inline::Channel {
                        id: id.to_owned(),
                        label: None,
                    });
                }
            }
            "broadcast" => {
                // Only these notify a whole channel, as in mrkdwn.
                if let Some(range @ ("here" | "channel" | "everyone")) = str_at(element, "range") {
                    out.push(Inline::Broadcast(range.to_owned()));
                }
            }
            // A date shows the fallback Slack wrote for clients that cannot
            // format it; a colour shows its value.
            "date" => text_lines(out, str_at(element, "fallback").unwrap_or(""), style),
            "color" => text_lines(out, str_at(element, "value").unwrap_or(""), style),
            _ => {}
        }
    }
}

/// The name the emoji table knows, with Slack's skin tone (2 to 6) the
/// way mrkdwn writes it: `+1::skin-tone-3`.
fn emoji_name(name: &str, element: &Value) -> String {
    match element.get("skin_tone").and_then(Value::as_u64) {
        Some(tone @ 2..=6) => format!("{name}::skin-tone-{tone}"),
        _ => name.to_owned(),
    }
}

/// The characters in an emoji element's `unicode` field, which Slack
/// writes as code points in hex joined by dashes (`1f44d-1f3fc`), skin tone
/// included.
fn emoji_unicode(element: &Value) -> Option<String> {
    let field = str_at(element, "unicode").filter(|field| !field.is_empty())?;
    field
        .split('-')
        .map(|point| u32::from_str_radix(point, 16).ok().and_then(char::from_u32))
        .collect()
}

fn style(element: &Value) -> Style {
    let flag = |name: &str| {
        element
            .get("style")
            .and_then(|style| style.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Style {
        bold: flag("bold"),
        italic: flag("italic"),
        strike: flag("strike"),
    }
}

/// Text with its line breaks as [`Inline::Newline`], merged into the run
/// before it when the style matches.
fn text_lines(out: &mut Vec<Inline>, text: &str, style: Style) {
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push(Inline::Newline);
        }
        if !line.is_empty() {
            push(out, line.to_owned(), style);
        }
    }
}

fn push(out: &mut Vec<Inline>, text: String, style: Style) {
    match out.last_mut() {
        Some(Inline::Text(previous, previous_style)) if *previous_style == style => {
            previous.push_str(&text);
        }
        _ => out.push(Inline::Text(text, style)),
    }
}

/// A preformatted block's text: links as their address or label, emoji
/// as their code, the way Slack shows code.
fn plain(node: &Value) -> String {
    let mut out = String::new();
    for element in elements(node) {
        match kind(element) {
            "text" => out.push_str(str_at(element, "text").unwrap_or("")),
            "link" => out.push_str(
                str_at(element, "text")
                    .filter(|text| !text.is_empty())
                    .or_else(|| str_at(element, "url"))
                    .unwrap_or(""),
            ),
            "emoji" => {
                if let Some(name) = str_at(element, "name") {
                    out.push(':');
                    out.push_str(&emoji_name(name, element));
                    out.push(':');
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(json: &str) -> Vec<Block> {
        let block: Value = serde_json::from_str(json).expect("parses");
        blocks(&block)
    }

    fn text(text: &str) -> Inline {
        Inline::Text(text.into(), Style::default())
    }

    fn section(elements: &str) -> String {
        format!(
            r#"{{"type":"rich_text","elements":[{{"type":"rich_text_section","elements":[{elements}]}}]}}"#
        )
    }

    #[test]
    fn emoji_are_what_slack_says_they_are() {
        let blocks = read(&section(
            r#"{"type":"emoji","name":"large_blue_square","unicode":"1f7e6"},
               {"type":"text","text":"1000 at 10:30:00 "},
               {"type":"emoji","name":"+1","skin_tone":3}"#,
        ));
        assert_eq!(
            blocks,
            [Block::Paragraph(vec![
                Inline::Emoji("large_blue_square".into()),
                text("1000 at 10:30:00 "),
                Inline::Emoji("+1::skin-tone-3".into()),
            ])]
        );
    }

    #[test]
    fn newer_emoji_show_their_unicode_and_code_keeps_tones() {
        let blocks = read(&section(
            r#"{"type":"emoji","name":"face_shaking_from_the_future","unicode":"1fae8"},
               {"type":"emoji","name":"+1","unicode":"1f44d-1f3fc","skin_tone":3},
               {"type":"emoji","name":"broken","unicode":"zz"}"#,
        ));
        assert_eq!(
            blocks,
            [Block::Paragraph(vec![
                text("\u{1fae8}"),
                Inline::Emoji("+1::skin-tone-3".into()),
                Inline::Emoji("broken".into()),
            ])]
        );
        let code = read(
            r#"{"type":"rich_text","elements":[{"type":"rich_text_preformatted","elements":[
                {"type":"text","text":"ok "},
                {"type":"emoji","name":"wave","skin_tone":5}]}]}"#,
        );
        assert_eq!(code, [Block::Preformatted("ok :wave::skin-tone-5:".into())]);
    }

    #[test]
    fn typed_markup_and_brackets_stay_text() {
        let blocks = read(&section(
            r#"{"type":"text","text":"2 * 3 < 7 & *not bold*"}"#,
        ));
        assert_eq!(
            blocks,
            [Block::Paragraph(vec![text("2 * 3 < 7 & *not bold*")])]
        );
    }

    #[test]
    fn styles_mentions_and_links_come_through() {
        let blocks = read(&section(
            r#"{"type":"text","text":"hi","style":{"bold":true}},
               {"type":"text","text":" "},
               {"type":"user","user_id":"U1"},
               {"type":"text","text":" see "},
               {"type":"link","url":"https://x.y","text":"this","style":{"italic":true}},
               {"type":"text","text":" and "},
               {"type":"text","text":"x = 1","style":{"code":true}},
               {"type":"broadcast","range":"here"},
               {"type":"usergroup","usergroup_id":"S1"},
               {"type":"channel","channel_id":"C1"}"#,
        ));
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let italic = Style {
            italic: true,
            ..Style::default()
        };
        assert_eq!(
            blocks,
            [Block::Paragraph(vec![
                Inline::Text("hi".into(), bold),
                text(" "),
                Inline::User {
                    id: "U1".into(),
                    label: None
                },
                text(" see "),
                Inline::Link {
                    url: "https://x.y".into(),
                    label: Some("this".into()),
                    style: italic
                },
                text(" and "),
                Inline::Code("x = 1".into()),
                Inline::Broadcast("here".into()),
                Inline::Group {
                    id: "S1".into(),
                    label: None
                },
                Inline::Channel {
                    id: "C1".into(),
                    label: None
                },
            ])]
        );
    }

    #[test]
    fn unsafe_links_and_unknown_broadcasts_are_not_live() {
        let blocks = read(&section(
            r#"{"type":"link","url":"file:///etc/passwd"},
               {"type":"broadcast","range":"nobody"}"#,
        ));
        assert_eq!(blocks, [Block::Paragraph(vec![text("file:///etc/passwd")])]);
    }

    #[test]
    fn lines_quotes_lists_and_code_keep_their_shape() {
        let blocks = read(
            r#"{"type":"rich_text","elements":[
                {"type":"rich_text_section","elements":[{"type":"text","text":"one\ntwo\n"}]},
                {"type":"rich_text_list","style":"ordered","indent":0,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"first"}]},
                    {"type":"rich_text_section","elements":[{"type":"text","text":"second"}]}]},
                {"type":"rich_text_list","style":"bullet","indent":1,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"inner"}]}]},
                {"type":"rich_text_quote","elements":[{"type":"text","text":"said"}]},
                {"type":"rich_text_section","elements":[{"type":"text","text":"\nafter"}]},
                {"type":"rich_text_preformatted","elements":[
                    {"type":"text","text":"let x = *y;\n"},
                    {"type":"link","url":"https://x.y"}]}
            ]}"#,
        );
        assert_eq!(
            blocks,
            [
                Block::Paragraph(vec![
                    text("one"),
                    Inline::Newline,
                    text("two"),
                    Inline::Newline,
                    text("1. first"),
                    Inline::Newline,
                    text("2. second"),
                    Inline::Newline,
                    text("    • inner"),
                ]),
                Block::Quote(vec![text("said")]),
                Block::Paragraph(vec![text("after")]),
                Block::Preformatted("let x = *y;\nhttps://x.y".into()),
            ]
        );
    }

    #[test]
    fn nested_ordered_lists_count_as_slack_does() {
        let blocks = read(
            r#"{"type":"rich_text","elements":[
                {"type":"rich_text_list","style":"ordered","indent":0,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"top"}]}]},
                {"type":"rich_text_list","style":"ordered","indent":1,"offset":1,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"b"}]}]},
                {"type":"rich_text_list","style":"ordered","indent":2,"offset":3,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"iv"}]}]},
                {"type":"rich_text_list","style":"ordered","indent":3,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"one"}]}]}
            ]}"#,
        );
        assert_eq!(
            blocks,
            [Block::Paragraph(vec![
                text("1. top"),
                Inline::Newline,
                text("    b. b"),
                Inline::Newline,
                text("        iv. iv"),
                Inline::Newline,
                text("            1. one"),
            ])]
        );
        assert_eq!(letters(26), "z");
        assert_eq!(letters(28), "ab");
        assert_eq!(roman(1994).as_deref(), Some("mcmxciv"));
        assert_eq!(ordinal(4000, 2), "4000");
    }

    #[test]
    fn lists_with_a_border_are_quoted() {
        let blocks = read(
            r#"{"type":"rich_text","elements":[
                {"type":"rich_text_quote","elements":[{"type":"text","text":"said"}]},
                {"type":"rich_text_list","style":"bullet","indent":0,"border":1,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"point"}]}]},
                {"type":"rich_text_section","elements":[{"type":"text","text":"after"}]},
                {"type":"rich_text_list","style":"ordered","border":1,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"alone"}]}]}
            ]}"#,
        );
        assert_eq!(
            blocks,
            [
                Block::Quote(vec![text("said"), Inline::Newline, text("• point")]),
                Block::Paragraph(vec![text("after")]),
                Block::Quote(vec![text("1. alone")]),
            ]
        );
    }

    #[test]
    fn dates_show_their_fallback_and_unknown_elements_nothing() {
        let blocks = read(&section(
            r#"{"type":"date","timestamp":1700000000,"format":"{date}","fallback":"Nov 14"},
               {"type":"something_new","text":"?"}"#,
        ));
        assert_eq!(blocks, [Block::Paragraph(vec![text("Nov 14")])]);
    }
}
