//! HTML parsing and generation for Microsoft Teams messages.
//!
//! Teams messages carry formatted content as HTML (e.g. `<p>`, `<div>`,
//! `<b>`, `<i>`, `<a>`, `<at id="..">`, `<blockquote>`, `<pre>`).
//! This module converts Teams HTML into NoSlacking's [`crate::mrkdwn::Block`]
//! tree for display and converts plain/formatted text into Teams HTML for sending.

use crate::mrkdwn::{Block, Inline, Style};

/// Escapes special HTML characters in text.
pub fn escape_html(text: &str) -> String {
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

/// Decodes common HTML entities back to characters.
pub fn unescape_html(html: &str) -> String {
    html.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Converts a plain message or typed text into a Teams HTML payload.
pub fn text_to_teams_html(text: &str) -> String {
    let escaped = escape_html(text);
    if escaped.contains('\n') {
        let paragraphs: Vec<String> = escaped
            .split('\n')
            .map(|line| {
                if line.is_empty() {
                    "<p>&nbsp;</p>".to_string()
                } else {
                    format!("<p>{}</p>", line)
                }
            })
            .collect();
        paragraphs.join("")
    } else {
        format!("<p>{}</p>", escaped)
    }
}

/// Extracts plain text from Teams HTML by stripping tags and unescaping entities.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();

    for c in html.chars() {
        if c == '<' {
            in_tag = true;
            tag_buf.clear();
        } else if c == '>' {
            in_tag = false;
            let tag_lower = tag_buf.trim().to_ascii_lowercase();
            let is_block_end = tag_lower.starts_with("/p")
                || tag_lower.starts_with("/div")
                || tag_lower.starts_with("/li")
                || tag_lower.starts_with("br")
                || tag_lower.starts_with("/h");
            if is_block_end && !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() {
                out.push(' ');
            }
        } else if in_tag {
            tag_buf.push(c);
        } else {
            out.push(c);
        }
    }
    unescape_html(out.trim_end())
}

/// Parses Teams HTML into UI [`Block`]s.
pub fn html_to_blocks(html: &str) -> Vec<Block> {
    if html.is_empty() {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    let mut current_inlines = Vec::new();
    let mut in_quote = false;

    let mut style = Style::default();
    let mut href: Option<String> = None;
    let mut link_text = String::new();
    let mut in_link = false;
    let mut in_mention = false;
    let mut mention_id = String::new();
    let mut mention_text = String::new();

    let mut i = 0;
    let chars: Vec<char> = html.chars().collect();

    while i < chars.len() {
        if chars[i] == '<' {
            let start = i;
            while i < chars.len() && chars[i] != '>' {
                i += 1;
            }
            if i < chars.len() && chars[i] == '>' {
                i += 1;
                let tag: String = chars[start..i].iter().collect();
                let lower = tag.to_ascii_lowercase();

                match tag_name(&lower) {
                    "p" | "div" => {
                        if !current_inlines.is_empty() {
                            flush_inlines(&mut blocks, &mut current_inlines, in_quote);
                        }
                    }
                    "/p" | "/div" => {
                        flush_inlines(&mut blocks, &mut current_inlines, in_quote);
                    }
                    "blockquote" => {
                        flush_inlines(&mut blocks, &mut current_inlines, in_quote);
                        in_quote = true;
                    }
                    "/blockquote" => {
                        flush_inlines(&mut blocks, &mut current_inlines, in_quote);
                        in_quote = false;
                    }
                    "pre" => {
                        flush_inlines(&mut blocks, &mut current_inlines, in_quote);
                        let mut pre_text = String::new();
                        while i < chars.len() {
                            let window: String =
                                chars[i..std::cmp::min(i + 6, chars.len())].iter().collect();
                            if window.eq_ignore_ascii_case("</pre>") {
                                i += 6;
                                break;
                            }
                            pre_text.push(chars[i]);
                            i += 1;
                        }
                        let cleaned = strip_tags(&pre_text);
                        let trimmed = cleaned.trim();
                        if !trimmed.is_empty() {
                            blocks.push(Block::Preformatted(trimmed.to_string()));
                        }
                        continue;
                    }
                    "b" | "strong" => {
                        style.bold = true;
                    }
                    "/b" | "/strong" => {
                        style.bold = false;
                    }
                    "i" | "em" => {
                        style.italic = true;
                    }
                    "/i" | "/em" => {
                        style.italic = false;
                    }
                    "s" | "strike" | "del" => {
                        style.strike = true;
                    }
                    "/s" | "/strike" | "/del" => {
                        style.strike = false;
                    }
                    "code" => {
                        let mut code_text = String::new();
                        while i < chars.len() {
                            let window: String =
                                chars[i..std::cmp::min(i + 7, chars.len())].iter().collect();
                            if window.eq_ignore_ascii_case("</code>") {
                                i += 7;
                                break;
                            }
                            code_text.push(chars[i]);
                            i += 1;
                        }
                        current_inlines.push(Inline::Code(unescape_html(&code_text)));
                    }
                    "a" => {
                        in_link = true;
                        link_text.clear();
                        href = extract_attribute(&tag, "href");
                    }
                    "/a" => {
                        in_link = false;
                        if let Some(url) = href.take() {
                            current_inlines.push(Inline::Link {
                                url: unescape_html(&url),
                                label: if link_text.is_empty() {
                                    None
                                } else {
                                    Some(unescape_html(&link_text))
                                },
                                style,
                            });
                        }
                        link_text.clear();
                    }
                    "at" => {
                        in_mention = true;
                        mention_text.clear();
                        mention_id = extract_attribute(&tag, "id").unwrap_or_default();
                    }
                    "/at" => {
                        in_mention = false;
                        let label = if mention_text.is_empty() {
                            None
                        } else {
                            Some(unescape_html(mention_text.trim_start_matches('@')))
                        };
                        current_inlines.push(Inline::User {
                            id: mention_id.clone(),
                            label,
                        });
                        mention_id.clear();
                        mention_text.clear();
                    }
                    "br" => {
                        current_inlines.push(Inline::Newline);
                    }
                    _ => {}
                }
                continue;
            }
        }

        let ch = chars[i];
        if in_link {
            link_text.push(ch);
        } else if in_mention {
            mention_text.push(ch);
        } else {
            // Text run
            let mut text_buf = String::new();
            text_buf.push(ch);
            i += 1;
            while i < chars.len() && chars[i] != '<' {
                text_buf.push(chars[i]);
                i += 1;
            }
            let unescaped = unescape_html(&text_buf);
            if !unescaped.is_empty() {
                current_inlines.push(Inline::Text(unescaped, style));
            }
            continue;
        }

        i += 1;
    }

    flush_inlines(&mut blocks, &mut current_inlines, in_quote);

    if blocks.is_empty() {
        let plain = strip_tags(html);
        if !plain.trim().is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text(
                plain,
                Style::default(),
            )]));
        }
    }

    blocks
}

fn flush_inlines(blocks: &mut Vec<Block>, inlines: &mut Vec<Inline>, in_quote: bool) {
    if inlines.is_empty() {
        return;
    }
    let taken = std::mem::take(inlines);
    if in_quote {
        blocks.push(Block::Quote(taken));
    } else {
        blocks.push(Block::Paragraph(taken));
    }
}

fn extract_attribute(tag: &str, attr: &str) -> Option<String> {
    let lower_tag = tag.to_ascii_lowercase();
    let pattern = format!("{}=\"", attr);
    if let Some(pos) = lower_tag.find(&pattern) {
        let start = pos + pattern.len();
        if let Some(end) = tag[start..].find('"') {
            return Some(tag[start..start + end].to_string());
        }
    }
    let pattern_single = format!("{}='", attr);
    if let Some(pos) = lower_tag.find(&pattern_single) {
        let start = pos + pattern_single.len();
        if let Some(end) = tag[start..].find('\'') {
            return Some(tag[start..start + end].to_string());
        }
    }
    None
}

fn tag_name(tag: &str) -> &str {
    let s = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end_matches('/');
    s.split_whitespace().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_escapes_into_teams_html() {
        let html = text_to_teams_html("Hello <world> & \"teams\"");
        assert_eq!(html, "<p>Hello &lt;world&gt; &amp; &quot;teams&quot;</p>");
    }

    #[test]
    fn multiline_text_becomes_multiple_paragraphs() {
        let html = text_to_teams_html("Line 1\nLine 2");
        assert_eq!(html, "<p>Line 1</p><p>Line 2</p>");
    }

    #[test]
    fn strips_tags_accurately() {
        let text = strip_tags("<p>Hello <b>Bob</b>, visit <a href=\"https://x.y\">link</a>!</p>");
        assert_eq!(text, "Hello Bob, visit link!");
    }

    #[test]
    fn parses_formatted_teams_html() {
        let html = "<p>Hello <b>bold</b> and <i>italic</i> and <code>inline code</code></p>";
        let blocks = html_to_blocks(html);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 6);
                assert_eq!(inlines[0], Inline::Text("Hello ".into(), Style::default()));
                assert_eq!(
                    inlines[1],
                    Inline::Text(
                        "bold".into(),
                        Style {
                            bold: true,
                            ..Default::default()
                        }
                    )
                );
                assert_eq!(inlines[2], Inline::Text(" and ".into(), Style::default()));
                assert_eq!(
                    inlines[3],
                    Inline::Text(
                        "italic".into(),
                        Style {
                            italic: true,
                            ..Default::default()
                        }
                    )
                );
                assert_eq!(inlines[4], Inline::Text(" and ".into(), Style::default()));
                assert_eq!(inlines[5], Inline::Code("inline code".into()));
            }
            _ => panic!("expected paragraph"),
        }
    }

    #[test]
    fn parses_mentions_and_links() {
        let html = "<p>Hey <at id=\"8:orgid:123\">@Alice</at>, check <a href=\"https://example.com\">this</a></p>";
        let blocks = html_to_blocks(html);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 4);
                assert_eq!(
                    inlines[1],
                    Inline::User {
                        id: "8:orgid:123".into(),
                        label: Some("Alice".into()),
                    }
                );
                assert_eq!(
                    inlines[3],
                    Inline::Link {
                        url: "https://example.com".into(),
                        label: Some("this".into()),
                        style: Style::default(),
                    }
                );
            }
            _ => panic!("expected paragraph"),
        }
    }

    #[test]
    fn parses_preformatted_and_quotes() {
        let html = "<blockquote>A wise quote</blockquote><pre><code>fn main() {}</code></pre>";
        let blocks = html_to_blocks(html);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], Block::Quote(_)));
        assert_eq!(blocks[1], Block::Preformatted("fn main() {}".into()));
    }
}
