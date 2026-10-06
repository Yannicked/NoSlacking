//! Turning what you type into what Slack receives, and a sent message
//! back into text you can edit.

use crate::model::Message;
use crate::mrkdwn::{self, Block, Inline, Style};

/// What `@here`, `@channel` and `@everyone` become for Slack.
const BROADCASTS: [(&str, &str); 3] = [
    ("@here", "<!here>"),
    ("@channel", "<!channel>"),
    ("@everyone", "<!everyone>"),
];

/// What Slack receives for what you typed: markup characters escaped, and
/// picked mentions and broadcasts turned into Slack's own forms.
///
/// `mentions` pairs the text as typed with the markup it stands for. A
/// label only counts where it stands alone, so "@Ann" leaves "@Annabel"
/// be, and the text is read once from the start, so markup already put in
/// is never matched again.
pub fn to_wire(text: &str, mentions: &[(String, String)]) -> String {
    let escaped = mrkdwn::escape(text.trim_end());
    let mut forms: Vec<(String, &str)> = mentions
        .iter()
        .map(|(label, wire)| (mrkdwn::escape(label), wire.as_str()))
        .chain(
            BROADCASTS
                .iter()
                .map(|(typed, wire)| ((*typed).to_owned(), *wire)),
        )
        .filter(|(label, _)| !label.is_empty())
        .collect();
    // Longest first, so "@Ann Lee" wins over "@Ann".
    forms.sort_by_key(|(label, _)| std::cmp::Reverse(label.len()));
    let mut out = String::with_capacity(escaped.len());
    let mut previous = None;
    let mut rest = escaped.as_str();
    while let Some(c) = rest.chars().next() {
        let found = forms.iter().find(|(label, _)| {
            rest.starts_with(label.as_str()) && is_word_edge(rest[label.len()..].chars().next())
        });
        if is_word_edge(previous)
            && let Some((label, wire)) = found
        {
            out.push_str(wire);
            previous = label.chars().next_back();
            rest = &rest[label.len()..];
        } else {
            out.push(c);
            previous = Some(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Whether a typed label may start or end next to `c`.
fn is_word_edge(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_alphanumeric())
}

/// A piece of a message being made editable.
struct Piece {
    shown: String,
    /// The markup it came from, when typing `shown` alone would not bring
    /// it back.
    wire: Option<String>,
    /// What to show (and its markup) when `shown` is not unique in the
    /// text: [`to_wire`] would otherwise turn every copy into this link.
    fallback: Option<(String, String)>,
}

impl Piece {
    fn text(text: &str) -> Self {
        Self {
            shown: mrkdwn::unescape(text),
            wire: None,
            fallback: None,
        }
    }

    fn markup(shown: String, wire: String) -> Self {
        Self {
            shown,
            wire: Some(wire),
            fallback: None,
        }
    }
}

/// A sent message's text as you would type it, and the mentions that turn
/// it back into the same markup through [`to_wire`].
///
/// People, channels and user groups show as `@name`, `#name` and `@handle`.
/// A link shows its label when that is unique in the text, and its address
/// otherwise. `name_of` names a person (`'@'`), a channel (`'#'`) or a user
/// group (`'^'`, its handle) by id.
pub fn to_editable(
    wire: &str,
    name_of: impl Fn(char, &str) -> Option<String>,
) -> (String, Vec<(String, String)>) {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut rest = wire;
    while let Some(open) = rest.find('<') {
        pieces.push(Piece::text(&rest[..open]));
        let after = &rest[open + 1..];
        let inner = after
            .find('>')
            .map(|close| &after[..close])
            .filter(|inner| !inner.is_empty() && !inner.contains(['<', '\n']));
        let Some(inner) = inner else {
            // Not markup: Slack escapes a typed `<`, so keep it as it is.
            pieces.push(Piece::text("<"));
            rest = after;
            continue;
        };
        rest = &after[inner.len() + 1..];
        let raw = format!("<{inner}>");
        let (target, label) = match inner.split_once('|') {
            Some((target, label)) => (target, Some(mrkdwn::unescape(label))),
            None => (inner, None),
        };
        let label = label.filter(|l| !l.is_empty());
        let name = |sigil: char, id: &str| {
            name_of(sigil, id)
                .or_else(|| {
                    label
                        .as_deref()
                        .map(|l| l.trim_start_matches(sigil).to_owned())
                })
                .unwrap_or_else(|| id.to_owned())
        };
        let piece = if let Some(id) = target.strip_prefix('@') {
            Piece::markup(format!("@{}", name('@', id)), raw)
        } else if let Some(id) = target.strip_prefix('#') {
            Piece::markup(format!("#{}", name('#', id)), raw)
        } else if let Some(id) = target.strip_prefix("!subteam^") {
            // The handle Slack lists now, else the label it was sent with.
            let handle = name_of('^', id)
                .or_else(|| {
                    label
                        .as_deref()
                        .map(|l| l.trim_start_matches('@').to_owned())
                })
                .unwrap_or_else(|| id.to_owned());
            Piece::markup(format!("@{handle}"), raw)
        } else if let Some(command) = target.strip_prefix('!') {
            let word = command.split('^').next().unwrap_or(command);
            match BROADCASTS.iter().find(|(typed, _)| typed[1..] == *word) {
                // Typing these brings them back.
                Some((typed, _)) => Piece::text(typed),
                // Dates and the like: their label stands for them.
                None => Piece::markup(label.clone().unwrap_or_else(|| format!("@{word}")), raw),
            }
        } else {
            let url = mrkdwn::unescape(target);
            let bare = format!("<{target}>");
            match label.clone().filter(|l| *l != url) {
                Some(label) => Piece {
                    shown: label,
                    wire: Some(raw),
                    fallback: Some((url, bare)),
                },
                None => Piece::markup(url, bare),
            }
        };
        pieces.push(piece);
    }
    pieces.push(Piece::text(rest));
    let text: String = pieces.iter().map(|p| p.shown.as_str()).collect();
    let mut mentions: Vec<(String, String)> = Vec::new();
    for piece in &mut pieces {
        if let Some((url, bare)) = piece.fallback.take()
            && text.matches(piece.shown.as_str()).count() > 1
        {
            piece.shown = url;
            piece.wire = Some(bare);
        }
        if let Some(wire) = &piece.wire
            && !mentions.iter().any(|(label, _)| *label == piece.shown)
        {
            mentions.push((piece.shown.clone(), wire.clone()));
        }
    }
    let text = pieces.iter().map(|p| p.shown.as_str()).collect();
    (text, mentions)
}

/// The markup to edit a message from: written from its rich text when it
/// has some, which says what was meant (a list, a typed `*`) better than
/// its `text`, and its `text` otherwise. A message with a date, or with a
/// layout of more than rich text, keeps its `text`, which holds what the
/// rich text as read here would lose.
pub fn edit_source(message: &Message) -> String {
    match message.rich_text() {
        Some(blocks) if !message.uses_blocks() && !mrkdwn::has_commands(&message.text) => {
            to_mrkdwn(blocks)
        }
        _ => message.text.clone(),
    }
}

/// Blocks written back as mrkdwn, escaped as Slack's `text` is, so that
/// [`to_editable`] reads it as it reads a sent message: mentions, groups
/// and channels by id, styles as markers around the words, quotes as
/// `>` lines and lists as the lines the blocks already hold.
///
/// A style that starts inside a word (`un*seen*`) has no mrkdwn: Slack
/// reads markers only at a word's edge, so it comes back unstyled.
pub fn to_mrkdwn(blocks: &[Block]) -> String {
    let mut out = String::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            out.push('\n');
            // Quote lines next to each other are one quote.
            if matches!(
                (&blocks[index - 1], block),
                (Block::Quote(_), Block::Quote(_))
            ) {
                out.push('\n');
            }
        }
        match block {
            Block::Paragraph(inlines) => write_runs(inlines, Style::default(), &mut out),
            Block::Quote(inlines) => {
                let mut quoted = String::new();
                write_runs(inlines, Style::default(), &mut quoted);
                for (line, text) in quoted.split('\n').enumerate() {
                    if line > 0 {
                        out.push('\n');
                    }
                    out.push_str("&gt; ");
                    out.push_str(text);
                }
            }
            Block::Preformatted(code) => {
                out.push_str("```\n");
                out.push_str(&mrkdwn::escape(code));
                out.push_str("\n```");
            }
        }
    }
    out
}

/// The styles an inline carries; only text and links have any.
fn style_of(inline: &Inline) -> Style {
    match inline {
        Inline::Text(_, style) | Inline::Link { style, .. } => *style,
        _ => Style::default(),
    }
}

/// Writes inlines with their styles as markers. `open` holds the styles
/// already marked around them; each further style takes the longest run
/// that has it, so `*bold _both_*` comes back as it was written.
fn write_runs(inlines: &[Inline], open: Style, out: &mut String) {
    let mut at = 0;
    while at < inlines.len() {
        let style = style_of(&inlines[at]);
        let flags = [
            (style.bold && !open.bold, '*'),
            (style.italic && !open.italic, '_'),
            (style.strike && !open.strike, '~'),
        ];
        let Some(&(_, marker)) = flags.iter().find(|(new, _)| *new) else {
            write_one(&inlines[at], out);
            at += 1;
            continue;
        };
        let has = |inline: &Inline| {
            let style = style_of(inline);
            match marker {
                '*' => style.bold,
                '_' => style.italic,
                _ => style.strike,
            }
        };
        let end = at + inlines[at..].iter().take_while(|i| has(i)).count();
        let mut inner = open;
        match marker {
            '*' => inner.bold = true,
            '_' => inner.italic = true,
            _ => inner.strike = true,
        }
        let mut body = String::new();
        write_runs(&inlines[at..end], inner, &mut body);
        // Markers hug the words: spaces at the run's edges go outside.
        let core = body.trim();
        if core.is_empty() {
            out.push_str(&body);
        } else {
            let lead = body.len() - body.trim_start().len();
            out.push_str(&body[..lead]);
            out.push(marker);
            out.push_str(core);
            out.push(marker);
            out.push_str(&body[lead + core.len()..]);
        }
        at = end;
    }
}

/// One inline as mrkdwn, without its styles.
fn write_one(inline: &Inline, out: &mut String) {
    match inline {
        Inline::Text(text, _) => out.push_str(&mrkdwn::escape(text)),
        Inline::Code(code) => {
            out.push('`');
            out.push_str(&mrkdwn::escape(code));
            out.push('`');
        }
        Inline::Link { url, label, .. } => {
            out.push('<');
            out.push_str(&mrkdwn::escape(url));
            if let Some(label) = label {
                out.push('|');
                out.push_str(&mrkdwn::escape(label));
            }
            out.push('>');
        }
        Inline::User { id, .. } => out.push_str(&format!("<@{id}>")),
        Inline::Channel { id, .. } => out.push_str(&format!("<#{id}>")),
        Inline::Group { id, .. } => out.push_str(&format!("<!subteam^{id}>")),
        Inline::Broadcast(range) => out.push_str(&format!("<!{range}>")),
        Inline::Emoji(name) => out.push_str(&format!(":{name}:")),
        Inline::Newline => out.push('\n'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_becomes_slack_markup() {
        let mentions = vec![
            ("@Ann".to_owned(), "<@U1>".to_owned()),
            ("@Ann Lee".to_owned(), "<@U2>".to_owned()),
        ];
        assert_eq!(
            to_wire("hi @Ann Lee & @Ann <3 @here, not @heresy", &mentions),
            "hi <@U2> &amp; <@U1> &lt;3 <!here>, not @heresy"
        );
    }

    #[test]
    fn mention_labels_match_only_whole_words() {
        let mentions = vec![
            ("@Ann".to_owned(), "<@U1>".to_owned()),
            ("@Annabel".to_owned(), "<@U2>".to_owned()),
            ("@Zoë".to_owned(), "<@U3>".to_owned()),
        ];
        assert_eq!(
            to_wire("@Annabel, @Ann and @Annie", &mentions),
            "<@U2>, <@U1> and @Annie"
        );
        assert_eq!(
            to_wire("über @Zoë! ünd @Zoëy mail@Ann", &mentions),
            "über <@U3>! ünd @Zoëy mail@Ann"
        );
        // The boundary is read from the whole text, not from where the last
        // match ended.
        assert_eq!(to_wire("é@here @here", &[]), "é@here <!here>");
        assert_eq!(to_wire("@channel—@everyone", &[]), "<!channel>—<!everyone>");
    }

    #[test]
    fn inserted_markup_is_not_matched_again() {
        // A person called "U2" must not reach into `<@U2>`.
        let mentions = vec![
            ("@Bo".to_owned(), "<@U2>".to_owned()),
            ("@U2".to_owned(), "<@U9>".to_owned()),
        ];
        assert_eq!(to_wire("@Bo", &mentions), "<@U2>");
    }

    fn names(sigil: char, id: &str) -> Option<String> {
        match (sigil, id) {
            ('@', "U1") => Some("Ann Lee".into()),
            ('#', "C1") => Some("general".into()),
            ('^', "S2") => Some("ops".into()),
            _ => None,
        }
    }

    #[test]
    fn group_mentions_round_trip_next_to_people() {
        // As the composer records a picked group and a picked person.
        let mentions = vec![
            ("@ops".to_owned(), "<!subteam^S2|@ops>".to_owned()),
            ("@Ann Lee".to_owned(), "<@U1>".to_owned()),
        ];
        let wire = to_wire("@ops and @Ann Lee, not @opsy", &mentions);
        assert_eq!(wire, "<!subteam^S2|@ops> and <@U1>, not @opsy");
        let (text, again) = to_editable(&wire, names);
        assert_eq!(text, "@ops and @Ann Lee, not @opsy");
        assert_eq!(to_wire(&text, &again), wire);
        // A group sent without a label takes its handle from the list, and
        // an unknown one keeps its id; both go back out unchanged.
        let bare = "<!subteam^S2> <!subteam^S9>";
        let (text, again) = to_editable(bare, names);
        assert_eq!(text, "@ops @S9");
        assert_eq!(to_wire(&text, &again), bare);
    }

    #[test]
    fn edited_messages_keep_their_markup() {
        let wire = "hi <@U1> and <@U2|bob> in <#C1|general> &amp; <!here>: see \
                    <https://x.y/a?b=1&amp;c=2|the docs>, <https://x.y> or \
                    <mailto:a@x.y|a@x.y> &lt;3 <!subteam^S1|@design> ünï *bold*";
        let (text, mentions) = to_editable(wire, names);
        assert_eq!(
            text,
            "hi @Ann Lee and @bob in #general & @here: see the docs, https://x.y or \
             a@x.y <3 @design ünï *bold*"
        );
        assert_eq!(to_wire(&text, &mentions), wire);
    }

    #[test]
    fn edits_survive_changes_around_the_markup() {
        let (text, mentions) = to_editable("ping <@U1> about <#C9>", names);
        assert_eq!(text, "ping @Ann Lee about #C9");
        let changed = text.replace("ping", "hey") + " & <#C1>";
        assert_eq!(
            to_wire(&changed, &mentions),
            "hey <@U1> about <#C9> &amp; &lt;#C1&gt;"
        );
    }

    #[test]
    fn a_link_label_that_repeats_shows_the_address() {
        // "docs" appears as a word too; keeping the label would link both.
        let wire = "docs: <https://x.y|docs>";
        let (text, mentions) = to_editable(wire, names);
        assert_eq!(text, "docs: https://x.y");
        assert_eq!(to_wire(&text, &mentions), "docs: <https://x.y>");
    }

    /// Rich text as Slack sends it, read as the interface reads it.
    fn rich(json: &str) -> Vec<Block> {
        let block: serde_json::Value = serde_json::from_str(json).expect("parses");
        crate::slack::rich::blocks(&block)
    }

    /// Rich text → editable text → what is sent → rich text again.
    fn edited_unchanged(blocks: &[Block]) -> String {
        let (text, mentions) = to_editable(&to_mrkdwn(blocks), names);
        let wire = to_wire(&text, &mentions);
        let block = crate::slack::rich_out::rich_text(&wire)
            .unwrap_or_else(|skip| panic!("{wire:?}: {skip:?}"));
        assert_eq!(crate::slack::rich::blocks(&block), blocks, "{wire:?}");
        text
    }

    #[test]
    fn rich_text_edits_and_saves_unchanged() {
        let blocks = rich(
            r#"{"type":"rich_text","elements":[
                {"type":"rich_text_section","elements":[
                    {"type":"text","text":"Hi "},
                    {"type":"user","user_id":"U1"},
                    {"type":"text","text":", "},
                    {"type":"text","text":"bold ","style":{"bold":true}},
                    {"type":"text","text":"both","style":{"bold":true,"italic":true}},
                    {"type":"text","text":" in "},
                    {"type":"channel","channel_id":"C1"},
                    {"type":"text","text":" for "},
                    {"type":"usergroup","usergroup_id":"S2"},
                    {"type":"text","text":" "},
                    {"type":"broadcast","range":"here"},
                    {"type":"text","text":" & 2*3 < 7 "},
                    {"type":"emoji","name":"+1","unicode":"1f44d-1f3fc","skin_tone":3},
                    {"type":"text","text":"\nsee "},
                    {"type":"link","url":"https://x.y/?a=1&b=2","text":"the docs"},
                    {"type":"text","text":" or "},
                    {"type":"text","text":"cargo test","style":{"code":true}},
                    {"type":"text","text":"\n"}]},
                {"type":"rich_text_list","style":"ordered","indent":0,"border":0,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"one"}]},
                    {"type":"rich_text_section","elements":[
                        {"type":"text","text":"two","style":{"strike":true}}]}]},
                {"type":"rich_text_list","style":"bullet","indent":1,"border":0,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"inner"}]}]},
                {"type":"rich_text_quote","elements":[{"type":"text","text":"quoted\nlines"}]},
                {"type":"rich_text_preformatted","border":0,"elements":[
                    {"type":"text","text":"let x = a < b && c;"}]},
                {"type":"rich_text_section","elements":[{"type":"text","text":"bye"}]}
            ]}"#,
        );
        let text = edited_unchanged(&blocks);
        assert_eq!(
            text,
            "Hi @Ann Lee, *bold _both_* in #general for @ops @here & 2*3 < 7 \
             :+1::skin-tone-3:\nsee the docs or `cargo test`\n1. one\n2. ~two~\n    • inner\n\
             > quoted\n> lines\n```\nlet x = a < b && c;\n```\nbye"
        );
    }

    #[test]
    fn quotes_and_quoted_lists_edit_unchanged() {
        let blocks = rich(
            r#"{"type":"rich_text","elements":[
                {"type":"rich_text_quote","elements":[{"type":"text","text":"said"}]},
                {"type":"rich_text_list","style":"bullet","indent":0,"border":1,"elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"point"}]}]},
                {"type":"rich_text_section","elements":[{"type":"text","text":"after"}]},
                {"type":"rich_text_quote","elements":[{"type":"text","text":"one"}]},
                {"type":"rich_text_quote","elements":[{"type":"text","text":"two"}]}
            ]}"#,
        );
        let text = edited_unchanged(&blocks);
        assert_eq!(text, "> said\n> • point\nafter\n> one\n\n> two");
    }

    #[test]
    fn edits_start_from_rich_text_unless_it_would_lose_something() {
        let local = crate::model::Ts::new("local-1");
        let mut message =
            super::super::workspace::local_message("U1", &local, "*1.* fallback", &None, false);
        message.blocks.clear();
        assert_eq!(edit_source(&message), "*1.* fallback");
        message.blocks = vec![crate::model::KitBlock::RichText(
            rich(
                r#"{"type":"rich_text","elements":[{"type":"rich_text_list","style":"ordered",
                    "elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"a"}]}]}]}"#,
            )
            .into(),
        )];
        assert_eq!(edit_source(&message), "1. a");
        message.text = "on <!date^1700000000^{date}|Nov 14>".into();
        assert_eq!(edit_source(&message), message.text);
    }

    #[test]
    fn stray_angle_brackets_stay_text() {
        let (text, mentions) = to_editable("a < b <> c", names);
        assert_eq!(text, "a < b <> c");
        assert!(mentions.is_empty());
    }
}
