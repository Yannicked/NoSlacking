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
    for element in elements(block) {
        match kind(element) {
            "rich_text_section" => {
                inlines(element, &mut paragraph);
            }
            "rich_text_list" => list(element, &mut paragraph),
            "rich_text_quote" => {
                flush(&mut paragraph, &mut out);
                let mut quote = Vec::new();
                inlines(element, &mut quote);
                trim(&mut quote);
                if !quote.is_empty() {
                    out.push(Block::Quote(quote));
                }
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
    }
    flush(&mut paragraph, &mut out);
    out
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
    let first = list.get("offset").and_then(Value::as_u64).unwrap_or(0) + 1;
    let pad = "    ".repeat(usize::try_from(indent).unwrap_or(0).min(8));
    for (number, item) in (first..).zip(elements(list)) {
        let marker = if ordered {
            format!("{pad}{number}. ")
        } else {
            format!("{pad}• ")
        };
        push(paragraph, marker, Style::default());
        inlines(item, paragraph);
        paragraph.push(Inline::Newline);
    }
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
                    out.push(Inline::Emoji(emoji_name(name, element)));
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
                    out.push_str(name);
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
    fn dates_show_their_fallback_and_unknown_elements_nothing() {
        let blocks = read(&section(
            r#"{"type":"date","timestamp":1700000000,"format":"{date}","fallback":"Nov 14"},
               {"type":"something_new","text":"?"}"#,
        ));
        assert_eq!(blocks, [Block::Paragraph(vec![text("Nov 14")])]);
    }
}
