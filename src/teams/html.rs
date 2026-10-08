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

/// What a sent message carries: its HTML, and the people it mentions,
/// each numbered as its `<span itemid>` in the HTML.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub html: String,
    pub mentions: Vec<SentMention>,
}

/// One person a sent message mentions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentMention {
    /// Its number, as the span's `itemid`.
    pub item: usize,
    /// Their MRI.
    pub mri: String,
    /// The name the mention shows.
    pub name: String,
}

impl Outgoing {
    /// The message's `properties.mentions`: a JSON array as text, as the
    /// web client writes it (recorded on a received message).
    pub fn mentions_json(&self) -> String {
        let list: Vec<serde_json::Value> = self
            .mentions
            .iter()
            .map(|m| {
                serde_json::json!({
                    "@type": "http://schema.skype.com/Mention",
                    "itemid": m.item,
                    "mri": m.mri,
                    "mentionType": "person",
                    "displayName": m.name,
                })
            })
            .collect();
        serde_json::Value::Array(list).to_string()
    }
}

/// Turns what the interface sends (Slack's markup: `&amp;`-escaped text,
/// `*bold*`, `_italic_`, `~strike~`, `` `code` ``, code blocks, `>`
/// quotes, `<@id|name>` mentions, `<url|label>` links, `<!here>`) into a
/// Teams message, as the Teams composer writes one: each line a
/// paragraph, styles as `<b>`, `<i>`, `<s>`, `<code>`, a code block as
/// `<pre>`, a quote as `<blockquote>`, a mention as a
/// `<span itemtype="http://schema.skype.com/Mention">` with its entry in
/// [`Outgoing::mentions`], a link as `<a>`, an emoji as itself. Teams has
/// no channel links or broadcasts: those keep only their words.
pub fn wire_to_teams(wire: &str) -> Outgoing {
    let mut mentions: Vec<SentMention> = Vec::new();
    let mut html = String::new();
    for block in crate::mrkdwn::parse(wire) {
        match block {
            Block::Paragraph(inlines) => html.push_str(&lines_html(&inlines, &mut mentions)),
            Block::Quote(inlines) => {
                html.push_str("<blockquote>");
                html.push_str(&lines_html(&inlines, &mut mentions));
                html.push_str("</blockquote>");
            }
            Block::Preformatted(code) => {
                html.push_str("<pre>");
                html.push_str(&escape_html(&code));
                html.push_str("</pre>");
            }
        }
    }
    if html.is_empty() {
        html.push_str("<p>&nbsp;</p>");
    }
    Outgoing { html, mentions }
}

/// A run of inlines as paragraphs, one per line; an empty line keeps its
/// place.
fn lines_html(inlines: &[Inline], mentions: &mut Vec<SentMention>) -> String {
    let mut out = String::new();
    for line in inlines.split(|i| *i == Inline::Newline) {
        let text: String = line.iter().map(|i| inline_html(i, mentions)).collect();
        if text.is_empty() {
            out.push_str("<p>&nbsp;</p>");
        } else {
            out.push_str(&format!("<p>{text}</p>"));
        }
    }
    out
}

/// One inline as Teams HTML.
fn inline_html(inline: &Inline, mentions: &mut Vec<SentMention>) -> String {
    match inline {
        Inline::Text(text, style) => styled(&escape_html(text), *style),
        Inline::Code(code) => format!("<code>{}</code>", escape_html(code)),
        Inline::Link { url, label, style } => {
            let text = label.as_deref().unwrap_or(url);
            styled(
                &format!("<a href=\"{}\">{}</a>", escape_html(url), escape_html(text)),
                *style,
            )
        }
        Inline::User { id, label } => {
            let name = label.as_deref().unwrap_or(id);
            let name = name.strip_prefix('@').unwrap_or(name).to_owned();
            let item = mentions.len();
            let span = format!(
                "<span itemtype=\"http://schema.skype.com/Mention\" itemscope=\"\" itemid=\"{item}\">{}</span>",
                escape_html(&name)
            );
            mentions.push(SentMention {
                item,
                mri: crate::teams::client::user_mri(id),
                name,
            });
            span
        }
        Inline::Channel { id, label } => {
            escape_html(&format!("#{}", label.as_deref().unwrap_or(id)))
        }
        Inline::Broadcast(name) => escape_html(&format!("@{name}")),
        Inline::Group { id, label } => escape_html(label.as_deref().unwrap_or(id)),
        Inline::Emoji(name) => {
            crate::emoji::unicode(name, None).unwrap_or_else(|| format!(":{name}:"))
        }
        Inline::Newline => String::new(),
    }
}

/// `html` in `style`'s tags.
fn styled(html: &str, style: Style) -> String {
    let mut out = html.to_owned();
    if style.strike {
        out = format!("<s>{out}</s>");
    }
    if style.italic {
        out = format!("<i>{out}</i>");
    }
    if style.bold {
        out = format!("<b>{out}</b>");
    }
    out
}

/// Puts people into a parsed message's mentions: Teams numbers each one
/// (`itemid`, `<at id>`) and says whom it means in the message's
/// `properties.mentions`, here `people` (number, user id). The web client
/// gives each word of a name its own mention; those side by side, apart
/// only by spaces, become one.
pub fn resolve_mentions(blocks: Vec<Block>, people: &[(String, String)]) -> Vec<Block> {
    let resolve = |inlines: Vec<Inline>| -> Vec<Inline> {
        let mut out: Vec<Inline> = Vec::with_capacity(inlines.len());
        for inline in inlines {
            let Inline::User { id, label } = inline else {
                out.push(inline);
                continue;
            };
            let id = people
                .iter()
                .find(|(item, _)| *item == id)
                .map_or(id, |(_, user)| user.clone());
            // The same person just before, with only spaces between.
            let spaces = matches!(out.last(), Some(Inline::Text(t, _)) if t.trim().is_empty());
            let before = out.len() - usize::from(spaces);
            if let Some(Inline::User {
                id: previous,
                label: Some(name),
            }) = before.checked_sub(1).and_then(|at| out.get_mut(at))
                && *previous == id
                && let Some(more) = &label
            {
                name.push(' ');
                name.push_str(more);
                out.truncate(before);
                continue;
            }
            out.push(Inline::User { id, label });
        }
        out
    };
    blocks
        .into_iter()
        .map(|block| match block {
            Block::Paragraph(inlines) => Block::Paragraph(resolve(inlines)),
            Block::Quote(inlines) => Block::Quote(resolve(inlines)),
            other @ Block::Preformatted(_) => other,
        })
        .collect()
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
            // An emoji is an image whose `alt` is the emoji itself.
            if tag_lower.starts_with("img")
                && let Some(alt) = emoji_alt(&tag_buf)
            {
                out.push_str(&alt);
            }
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
    // Spans open, and how deep the mention's own span is: a mention is a
    // `<span itemtype="http://schema.skype.com/Mention">`, other spans
    // only style.
    let mut spans = 0usize;
    let mut mention_span: Option<usize> = None;

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
                    "span" => {
                        spans += 1;
                        if mention_span.is_none() && lower.contains("schema.skype.com/mention") {
                            mention_span = Some(spans);
                            in_mention = true;
                            mention_text.clear();
                            mention_id = extract_attribute(&tag, "itemid").unwrap_or_default();
                        }
                    }
                    "/span" => {
                        if mention_span == Some(spans) {
                            mention_span = None;
                            in_mention = false;
                            let label = (!mention_text.is_empty())
                                .then(|| unescape_html(mention_text.trim_start_matches('@')));
                            current_inlines.push(Inline::User {
                                id: std::mem::take(&mut mention_id),
                                label,
                            });
                            mention_text.clear();
                        }
                        spans = spans.saturating_sub(1);
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
                    "img" => {
                        if let Some(alt) = emoji_alt(&tag) {
                            current_inlines.push(Inline::Text(alt, style));
                        }
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

/// The emoji an `<img>` stands for: Teams writes emoji as images of the
/// Emoji type whose `alt` is the character itself.
fn emoji_alt(tag: &str) -> Option<String> {
    if !tag.contains("schema.skype.com/Emoji") {
        return None;
    }
    extract_attribute(tag, "alt")
        .map(|alt| unescape_html(&alt))
        .filter(|alt| !alt.is_empty())
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
    fn the_interfaces_markup_goes_out_as_teams_html() {
        let sent = wire_to_teams(
            "hi &amp; <@live:ana|Ana de Wit>, see <https://x.y/?a=1&amp;b=2|the site>\n@here <!here>",
        );
        assert_eq!(
            sent.html,
            "<p>hi &amp; <span itemtype=\"http://schema.skype.com/Mention\" itemscope=\"\" \
             itemid=\"0\">Ana de Wit</span>, see <a href=\"https://x.y/?a=1&amp;b=2\">the \
             site</a></p><p>@here @here</p>"
        );
        assert_eq!(
            sent.mentions,
            [SentMention {
                item: 0,
                mri: "8:live:ana".into(),
                name: "Ana de Wit".into()
            }]
        );
        let json: serde_json::Value =
            serde_json::from_str(&sent.mentions_json()).expect("the mentions are JSON");
        assert_eq!(json[0]["mri"], "8:live:ana");
        assert_eq!(json[0]["itemid"], 0);
        assert_eq!(json[0]["mentionType"], "person");
        assert_eq!(wire_to_teams("plain").mentions_json(), "[]");
    }

    #[test]
    fn styles_quotes_and_code_go_out_as_teams_formatting() {
        let sent =
            wire_to_teams("*bold* _it_ ~gone~ `x &lt; y` :wave:\n&gt; quoted\n```let a = 1;```");
        assert!(sent.html.contains("<b>bold</b>"), "{}", sent.html);
        assert!(sent.html.contains("<i>it</i>"), "{}", sent.html);
        assert!(sent.html.contains("<s>gone</s>"), "{}", sent.html);
        assert!(sent.html.contains("<code>x &lt; y</code>"), "{}", sent.html);
        assert!(sent.html.contains('👋'), "{}", sent.html);
        assert!(
            sent.html.contains("<blockquote><p>quoted</p></blockquote>"),
            "{}",
            sent.html
        );
        assert!(sent.html.contains("<pre>let a = 1;</pre>"), "{}", sent.html);
        assert_eq!(wire_to_teams("").html, "<p>&nbsp;</p>");
    }

    #[test]
    fn a_name_mentioned_word_by_word_is_one_mention() {
        // As the web client writes it (recorded): one span per word.
        let html = "<p><span itemtype=\"http://schema.skype.com/Mention\" itemscope=\"\" \
                    itemid=\"0\">Ana</span>&nbsp;<span itemtype=\"http://schema.skype.com/Mention\" \
                    itemscope=\"\" itemid=\"1\">de Wit</span>&nbsp;hello</p>";
        let people = [
            ("0".to_owned(), "live:ana".to_owned()),
            ("1".to_owned(), "live:ana".to_owned()),
        ];
        let blocks = resolve_mentions(html_to_blocks(html), &people);
        let Some(Block::Paragraph(inlines)) = blocks.first() else {
            panic!("a paragraph: {blocks:?}");
        };
        assert_eq!(
            inlines.first(),
            Some(&Inline::User {
                id: "live:ana".into(),
                label: Some("Ana de Wit".into())
            })
        );
        assert!(
            !inlines[1..]
                .iter()
                .any(|i| matches!(i, Inline::User { .. })),
            "{inlines:?}"
        );
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

    #[test]
    fn emoji_images_read_as_their_emoji() {
        let html = r#"<p>so <img itemscope="" itemtype="http://schema.skype.com/Emoji" itemid="sad" src="https://statics.teams.cdn.office.net/x" title="Sad" alt="🙁" style="width:20px"> today</p>"#;
        assert_eq!(strip_tags(html), "so 🙁 today");
        let blocks = html_to_blocks(html);
        let text: String = match blocks.first() {
            Some(Block::Paragraph(inlines)) => inlines
                .iter()
                .filter_map(|i| match i {
                    Inline::Text(t, _) => Some(t.as_str()),
                    _ => None,
                })
                .collect(),
            other => panic!("expected a paragraph, got {other:?}"),
        };
        assert_eq!(text, "so 🙁 today");
        // A picture is not an emoji: nothing stands in for it here.
        let picture = r#"<p><img itemtype="http://schema.skype.com/AMSImage" alt="image" src="https://x"></p>"#;
        assert_eq!(strip_tags(picture), "");
    }
}
