//! Turning what you type into what Slack receives, and a sent message
//! back into text you can edit.

use crate::mrkdwn;

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
/// People and channels show as `@name` and `#name`. A link shows its label
/// when that is unique in the text, and its address otherwise. `name_of`
/// names a person (`'@'`) or a channel (`'#'`) by id.
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
        } else if let Some(command) = target.strip_prefix('!') {
            let word = command.split('^').next().unwrap_or(command);
            match BROADCASTS.iter().find(|(typed, _)| typed[1..] == *word) {
                // Typing these brings them back.
                Some((typed, _)) => Piece::text(typed),
                // User groups and dates: their label stands for them.
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
            _ => None,
        }
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

    #[test]
    fn stray_angle_brackets_stay_text() {
        let (text, mentions) = to_editable("a < b <> c", names);
        assert_eq!(text, "a < b <> c");
        assert!(mentions.is_empty());
    }
}
