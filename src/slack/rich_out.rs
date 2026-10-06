//! Slack's `rich_text` block written from a message's mrkdwn: the reverse
//! of [`super::rich`], and what Slack's own composer sends beside `text`.
//!
//! With the block, Slack shows what was meant instead of guessing it again
//! from the mrkdwn: a list is a list, a mention is a mention. The block is
//! built from the parsed mrkdwn ([`mrkdwn::parse`]) and read back through
//! [`rich::blocks`] before it goes: if the reading differs from what was
//! parsed, a simpler block is tried, and when none reads back the same, the
//! message goes as `text` alone, as it always did. A message is never lost
//! or changed by the block.
//!
//! Text in a `rich_text` block is not escaped the way `text` is: `&`, `<`
//! and `>` go as they are.

use serde_json::{Map, Value, json};

use super::rich;
use crate::mrkdwn::{self, Block, Inline, Style};

/// Whether messages go out with a `rich_text` block beside their text.
/// The one switch: should Slack refuse the blocks, make this `false` and
/// every message goes as mrkdwn `text` alone again, sent, edited and
/// scheduled.
const SEND_RICH_TEXT: bool = true;

/// The deepest list Slack's apps indent, and the reader shows.
const MAX_INDENT: usize = 8;

/// Why a message goes without a `rich_text` block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Sending rich text is switched off ([`SEND_RICH_TEXT`]).
    Off,
    /// Nothing but white space: there is nothing to lay out.
    Empty,
    /// The text holds markup the blocks have no element for, such as a
    /// date (`<!date^…>`), which would go as its label only.
    Commands,
    /// No block read back as what the text says.
    Unfaithful,
}

/// The `blocks` parameter for a message whose mrkdwn is `wire`: a JSON
/// array holding its `rich_text` block. `None` sends `text` alone; why is
/// logged when it is not simply an empty or switched-off case.
pub fn blocks_param(wire: &str) -> Option<String> {
    let block = match rich_text(wire) {
        Ok(block) => block,
        Err(Skip::Off | Skip::Empty) => return None,
        Err(reason) => {
            // The reason only: the text is the person's message.
            log::warn!("Sending a message as text alone: {reason:?}");
            return None;
        }
    };
    match serde_json::to_string(&[block]) {
        Ok(param) => Some(param),
        Err(error) => {
            log::warn!("Sending a message as text alone: {error}");
            None
        }
    }
}

/// What Slack will show for `wire` sent with its block, as the interface
/// draws a `rich_text` block: given to the copy that shows until Slack's
/// own comes, so nothing moves when it does.
pub fn layout(wire: &str) -> Option<Vec<Block>> {
    let block = rich_text(wire).ok()?;
    let blocks = rich::blocks(&block);
    (!blocks.is_empty()).then_some(blocks)
}

/// The `rich_text` block for a message whose mrkdwn is `wire`.
pub fn rich_text(wire: &str) -> Result<Value, Skip> {
    if !SEND_RICH_TEXT {
        return Err(Skip::Off);
    }
    if mrkdwn::has_commands(wire) {
        return Err(Skip::Commands);
    }
    faithful(&mrkdwn::parse(wire))
}

/// Where lists are written as lists; elsewhere their lines stay text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lists {
    Everywhere,
    /// Not in quotes, where a quoted list joins the quote around it in
    /// ways a blank line cannot always survive.
    OutsideQuotes,
    Nowhere,
}

/// The first block, from the richest down, that reads back as `parsed`.
fn faithful(parsed: &[Block]) -> Result<Value, Skip> {
    let expected = normalise(parsed);
    if expected.iter().all(is_blank) {
        return Err(Skip::Empty);
    }
    [Lists::Everywhere, Lists::OutsideQuotes, Lists::Nowhere]
        .into_iter()
        .map(|lists| encode(parsed, lists))
        .find(|block| normalise(&rich::blocks(block)) == expected)
        .ok_or(Skip::Unfaithful)
}

/// Blocks as both readers agree on them: mrkdwn's labels on mentions are
/// not in rich text (the interface names them by id either way), line
/// breaks at a block's ends are not drawn, and empty blocks are none.
fn normalise(blocks: &[Block]) -> Vec<Block> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Paragraph(inlines) => normal_inlines(inlines).map(Block::Paragraph),
            Block::Quote(inlines) => normal_inlines(inlines).map(Block::Quote),
            Block::Preformatted(code) => {
                (!code.is_empty()).then(|| Block::Preformatted(code.clone()))
            }
        })
        .collect()
}

fn normal_inlines(inlines: &[Inline]) -> Option<Vec<Inline>> {
    let mut out: Vec<Inline> = Vec::new();
    for inline in inlines {
        let inline = match inline {
            Inline::Text(text, _) if text.is_empty() => continue,
            Inline::Text(text, style) => {
                if let Some(Inline::Text(previous, previous_style)) = out.last_mut()
                    && previous_style == style
                {
                    previous.push_str(text);
                    continue;
                }
                Inline::Text(text.clone(), *style)
            }
            Inline::User { id, .. } => Inline::User {
                id: id.clone(),
                label: None,
            },
            Inline::Channel { id, .. } => Inline::Channel {
                id: id.clone(),
                label: None,
            },
            Inline::Group { id, .. } => Inline::Group {
                id: id.clone(),
                label: None,
            },
            other => other.clone(),
        };
        out.push(inline);
    }
    while matches!(out.last(), Some(Inline::Newline)) {
        out.pop();
    }
    let lead = out
        .iter()
        .take_while(|inline| matches!(inline, Inline::Newline))
        .count();
    out.drain(..lead);
    (!out.is_empty()).then_some(out)
}

/// Whether a block holds nothing but white space.
fn is_blank(block: &Block) -> bool {
    match block {
        Block::Paragraph(inlines) | Block::Quote(inlines) => inlines.iter().all(|inline| {
            matches!(inline, Inline::Newline)
                || matches!(inline, Inline::Text(text, _) if text.trim().is_empty())
        }),
        Block::Preformatted(code) => code.trim().is_empty(),
    }
}

/// The `rich_text` block for `blocks`, with lists where `lists` says.
fn encode(blocks: &[Block], lists: Lists) -> Value {
    let mut elements = Vec::new();
    for block in blocks {
        match block {
            Block::Paragraph(inlines) => {
                paragraph(inlines, lists != Lists::Nowhere, &mut elements);
            }
            Block::Quote(inlines) => quote(inlines, lists == Lists::Everywhere, &mut elements),
            Block::Preformatted(code) if code.is_empty() => {}
            Block::Preformatted(code) => elements.push(json!({
                "type": "rich_text_preformatted",
                "elements": [{"type": "text", "text": code}],
                "border": 0,
            })),
        }
    }
    json!({"type": "rich_text", "elements": elements})
}

/// A line of a paragraph or quote: text, or an item of a list.
enum Line<'a> {
    Text(&'a [Inline]),
    Item(Item),
}

/// A list item: `• point` or `2. point`, indented four spaces a level, as
/// [`rich::blocks`] writes them.
struct Item {
    ordered: bool,
    indent: usize,
    /// Its number, for an ordered item.
    number: u64,
    content: Vec<Inline>,
}

/// A paragraph: its runs of text lines as sections, its runs of items as
/// lists.
fn paragraph(inlines: &[Inline], lists: bool, out: &mut Vec<Value>) {
    let lines = lines(inlines, lists);
    let mut rest = lines.as_slice();
    while !rest.is_empty() {
        let text = rest
            .iter()
            .take_while(|line| matches!(line, Line::Text(_)))
            .count();
        if text > 0 {
            let mut elements = Vec::new();
            join_lines(&rest[..text], &mut elements);
            // Before a list, the section ends its last line, as Slack's
            // does; a blank line before the list is then a second break.
            if text < rest.len() {
                push_text(&mut elements, "\n", Style::default());
            }
            if !elements.is_empty() {
                out.push(json!({"type": "rich_text_section", "elements": elements}));
            }
            rest = &rest[text..];
        }
        let items = items_at_start(rest);
        push_lists(&items, 0, out);
        rest = &rest[items.len()..];
    }
}

/// A quote: its runs of text lines as quotes, its runs of items as lists
/// with a border, which Slack draws inside the quote.
fn quote(inlines: &[Inline], lists: bool, out: &mut Vec<Value>) {
    let lines = lines(inlines, lists);
    let mut rest = lines.as_slice();
    while !rest.is_empty() {
        let text = rest
            .iter()
            .take_while(|line| matches!(line, Line::Text(_)))
            .count();
        if text > 0 {
            let mut elements = Vec::new();
            join_lines(&rest[..text], &mut elements);
            if !elements.is_empty() {
                out.push(json!({"type": "rich_text_quote", "elements": elements}));
            }
            rest = &rest[text..];
        }
        let items = items_at_start(rest);
        push_lists(&items, 1, out);
        rest = &rest[items.len()..];
    }
}

/// The items a run of lines starts with.
fn items_at_start<'a>(lines: &'a [Line<'a>]) -> Vec<&'a Item> {
    lines
        .iter()
        .map_while(|line| match line {
            Line::Item(item) => Some(item),
            Line::Text(_) => None,
        })
        .collect()
}

/// Text lines as inline elements, a line break between each two.
fn join_lines(lines: &[Line<'_>], elements: &mut Vec<Value>) {
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            push_text(elements, "\n", Style::default());
        }
        if let Line::Text(inlines) = line {
            inline_elements(inlines, elements);
        }
    }
}

/// Splits `inlines` at its line breaks and, with `lists`, finds the list
/// items among the lines.
///
/// An ordered item counts only when it starts its list (`1.`, `a.`, `i.`)
/// or goes on from the item before it at its level, so a line that merely
/// starts with a year ("2024. What a year") stays text.
fn lines(inlines: &[Inline], lists: bool) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    // The last number at each level of the list being read.
    let mut counters = [None::<u64>; MAX_INDENT + 1];
    for line in inlines.split(|inline| matches!(inline, Inline::Newline)) {
        let found = if lists { item(line) } else { None };
        let Some(item) = found else {
            counters = [None; MAX_INDENT + 1];
            out.push(Line::Text(line));
            continue;
        };
        // Deeper levels start again under a new item.
        counters[item.indent + 1..].fill(None);
        let goes_on = item.number == 1 || counters[item.indent] == item.number.checked_sub(1);
        if item.ordered && !goes_on {
            counters = [None; MAX_INDENT + 1];
            out.push(Line::Text(line));
            continue;
        }
        counters[item.indent] = item.ordered.then_some(item.number);
        out.push(Line::Item(item));
    }
    out
}

/// The list item a line is, if it starts with a marker the way
/// [`rich::blocks`] writes one and has something after it.
fn item(line: &[Inline]) -> Option<Item> {
    let (Inline::Text(first, style), after) = line.split_first()? else {
        return None;
    };
    if *style != Style::default() {
        return None;
    }
    let spaces = first.len() - first.trim_start_matches(' ').len();
    let indent = spaces / 4;
    if spaces % 4 != 0 || indent > MAX_INDENT {
        return None;
    }
    let marked = &first[spaces..];
    let (ordered, number, text) = match marked.strip_prefix("• ") {
        Some(text) => (false, 0, text),
        None => {
            let (token, text) = marked.split_once(". ")?;
            (true, number_of(token, indent)?, text)
        }
    };
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(Inline::Text(text.to_owned(), Style::default()));
    }
    content.extend(after.iter().cloned());
    let something = content
        .iter()
        .any(|inline| !matches!(inline, Inline::Text(text, _) if text.trim().is_empty()));
    something.then_some(Item {
        ordered,
        indent,
        number,
        content,
    })
}

/// The number an ordered item's marker stands for at `indent`: digits,
/// then letters one level in, then Roman numerals, as Slack counts.
fn number_of(token: &str, indent: usize) -> Option<u64> {
    // Long enough for the longest Roman numeral, short enough that no
    // count can overflow.
    if token.is_empty() || token.len() > 16 {
        return None;
    }
    let number = match indent % 3 {
        1 if token.len() <= 6 => token.bytes().try_fold(0u64, |number, b| {
            b.is_ascii_lowercase()
                .then(|| number * 26 + u64::from(b - b'a') + 1)
        })?,
        2 => from_roman(token)?,
        0 if token.len() <= 10 => token.parse().ok()?,
        _ => return None,
    };
    // Only the one way of writing each number: not `01`, not `+1`.
    let fits = (1..=u64::from(u32::MAX)).contains(&number);
    let indent = u64::try_from(indent).ok()?;
    (fits && rich::ordinal(number, indent) == token).then_some(number)
}

/// A Roman numeral's value; a badly formed one is caught when
/// [`number_of`] writes the value again.
fn from_roman(token: &str) -> Option<u64> {
    let value = |c: char| match c {
        'i' => Some(1),
        'v' => Some(5),
        'x' => Some(10),
        'l' => Some(50),
        'c' => Some(100),
        'd' => Some(500),
        'm' => Some(1000),
        _ => None,
    };
    let values: Vec<i64> = token.chars().map(value).collect::<Option<_>>()?;
    let mut total = 0i64;
    for (index, &v) in values.iter().enumerate() {
        // A smaller numeral before a larger one is taken away: `iv`.
        match values.get(index + 1) {
            Some(&next) if next > v => total -= v,
            _ => total += v,
        }
    }
    u64::try_from(total).ok()
}

/// List items as `rich_text_list` elements: one per run of items of one
/// kind and level, numbered on from `offset` when a list goes on after a
/// deeper one.
fn push_lists(items: &[&Item], border: u8, out: &mut Vec<Value>) {
    let mut current: Option<(&Item, u64, Vec<Value>)> = None;
    for item in items {
        let mut section = Vec::new();
        inline_elements(&item.content, &mut section);
        let section = json!({"type": "rich_text_section", "elements": section});
        if let Some((first, next, sections)) = &mut current
            && first.ordered == item.ordered
            && first.indent == item.indent
            && (!item.ordered || item.number == *next)
        {
            sections.push(section);
            *next += 1;
            continue;
        }
        if let Some((first, _, sections)) = current.take() {
            out.push(list(first, sections, border));
        }
        current = Some((item, item.number + 1, vec![section]));
    }
    if let Some((first, _, sections)) = current {
        out.push(list(first, sections, border));
    }
}

fn list(first: &Item, sections: Vec<Value>, border: u8) -> Value {
    let mut list = json!({
        "type": "rich_text_list",
        "style": if first.ordered { "ordered" } else { "bullet" },
        "indent": first.indent,
        "border": border,
        "elements": sections,
    });
    if first.ordered && first.number > 1 {
        list["offset"] = json!(first.number - 1);
    }
    list
}

/// One line's inlines as rich text elements.
fn inline_elements(inlines: &[Inline], out: &mut Vec<Value>) {
    for inline in inlines {
        match inline {
            Inline::Text(text, style) => push_text(out, text, *style),
            Inline::Newline => push_text(out, "\n", Style::default()),
            // Code runs stay apart: two spans side by side are two.
            Inline::Code(code) => out.push(json!({
                "type": "text",
                "text": code,
                "style": {"code": true},
            })),
            Inline::Link { url, label, style } => {
                let mut link = json!({"type": "link", "url": url});
                if let Some(label) = label {
                    link["text"] = json!(label);
                }
                if let Some(style) = style_json(*style) {
                    link["style"] = style;
                }
                out.push(link);
            }
            Inline::User { id, .. } => out.push(json!({"type": "user", "user_id": id})),
            Inline::Channel { id, .. } => {
                out.push(json!({"type": "channel", "channel_id": id}));
            }
            Inline::Group { id, .. } => {
                out.push(json!({"type": "usergroup", "usergroup_id": id}));
            }
            Inline::Broadcast(range) => out.push(json!({"type": "broadcast", "range": range})),
            Inline::Emoji(name) => out.push(emoji(name)),
        }
    }
}

/// An emoji element as Slack writes one: its name, its skin tone (2 to 6)
/// apart, and for a standard emoji its characters as hex code points
/// joined by dashes (`1f44d-1f3fc`).
fn emoji(name: &str) -> Value {
    let (base, tone) = crate::emoji::split_tone(name);
    let (name, tone) = match tone {
        Some(tone @ 2..=6) => (base, Some(tone)),
        _ => (name, None),
    };
    let mut element = json!({"type": "emoji", "name": name});
    if crate::emoji::unicode(name, None).is_some()
        && let Some(characters) = crate::emoji::unicode(name, tone)
    {
        let points: Vec<String> = characters
            .chars()
            .map(|c| format!("{:x}", u32::from(c)))
            .collect();
        element["unicode"] = json!(points.join("-"));
    }
    if let Some(tone) = tone {
        element["skin_tone"] = json!(tone);
    }
    element
}

/// A run of text, merged into the text element before it when that has
/// the same style, as Slack writes a paragraph as one element.
fn push_text(out: &mut Vec<Value>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    let style = style_json(style);
    if let Some(last) = out.last_mut()
        && last.get("type").and_then(Value::as_str) == Some("text")
        && last.get("style") == style.as_ref()
        && let Some(Value::String(previous)) = last.get_mut("text")
    {
        previous.push_str(text);
        return;
    }
    let mut element = json!({"type": "text", "text": text});
    if let Some(style) = style {
        element["style"] = style;
    }
    out.push(element);
}

/// The `style` of a text or link element: only the flags that are on, and
/// none at all for plain text.
fn style_json(style: Style) -> Option<Value> {
    let mut flags = Map::new();
    for (name, on) in [
        ("bold", style.bold),
        ("italic", style.italic),
        ("strike", style.strike),
    ] {
        if on {
            flags.insert(name.to_owned(), Value::Bool(true));
        }
    }
    (!flags.is_empty()).then_some(Value::Object(flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(wire: &str) -> Value {
        rich_text(wire).unwrap_or_else(|skip| panic!("{wire:?}: {skip:?}"))
    }

    /// The elements of the block for `wire`.
    fn elements(wire: &str) -> Vec<Value> {
        block(wire)["elements"]
            .as_array()
            .cloned()
            .expect("elements")
    }

    fn section(elements: Value) -> Value {
        json!({"type": "rich_text_section", "elements": elements})
    }

    fn text(text: &str) -> Inline {
        Inline::Text(text.into(), Style::default())
    }

    fn kinds(wire: &str) -> Vec<String> {
        elements(wire)
            .iter()
            .map(|element| element["type"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    /// Asserts the block for `wire` reads back as `wire` parses.
    fn round_trips(wire: &str) {
        let read = rich::blocks(&block(wire));
        assert_eq!(
            normalise(&read),
            normalise(&mrkdwn::parse(wire)),
            "{wire:?}"
        );
    }

    #[test]
    fn every_inline_element_has_slacks_shape() {
        let wire = "*hi* _it_ ~no~ `x = 1` <@U1> <#C1|general> <!subteam^S1|@ops> \
                    <!here> <https://x.y/?a=1&amp;b=2|the docs> <https://x.y> \
                    :tada: :+1::skin-tone-3: :partyparrot:";
        assert_eq!(
            elements(wire),
            [section(json!([
                {"type": "text", "text": "hi", "style": {"bold": true}},
                {"type": "text", "text": " "},
                {"type": "text", "text": "it", "style": {"italic": true}},
                {"type": "text", "text": " "},
                {"type": "text", "text": "no", "style": {"strike": true}},
                {"type": "text", "text": " "},
                {"type": "text", "text": "x = 1", "style": {"code": true}},
                {"type": "text", "text": " "},
                {"type": "user", "user_id": "U1"},
                {"type": "text", "text": " "},
                {"type": "channel", "channel_id": "C1"},
                {"type": "text", "text": " "},
                {"type": "usergroup", "usergroup_id": "S1"},
                {"type": "text", "text": " "},
                {"type": "broadcast", "range": "here"},
                {"type": "text", "text": " "},
                {"type": "link", "url": "https://x.y/?a=1&b=2", "text": "the docs"},
                {"type": "text", "text": " "},
                {"type": "link", "url": "https://x.y"},
                {"type": "text", "text": " "},
                {"type": "emoji", "name": "tada", "unicode": "1f389"},
                {"type": "text", "text": " "},
                {"type": "emoji", "name": "+1", "unicode": "1f44d-1f3fc", "skin_tone": 3},
                {"type": "text", "text": " "},
                {"type": "emoji", "name": "partyparrot"},
            ]))]
        );
        round_trips(wire);
    }

    #[test]
    fn styles_combine_and_links_keep_theirs() {
        let wire = "*bold _both ~all~_* _<https://x.y|see>_";
        assert_eq!(
            elements(wire),
            [section(json!([
                {"type": "text", "text": "bold ", "style": {"bold": true}},
                {"type": "text", "text": "both ", "style": {"bold": true, "italic": true}},
                {"type": "text", "text": "all",
                    "style": {"bold": true, "italic": true, "strike": true}},
                {"type": "text", "text": " "},
                {"type": "link", "url": "https://x.y", "text": "see", "style": {"italic": true}},
            ]))]
        );
        round_trips(wire);
    }

    #[test]
    fn text_goes_unescaped_and_markup_that_is_not_stays_text() {
        let wire = "2*3*4 snake_case_name a &lt; b &amp;&amp; c &gt; d 10:30:00 *not closed";
        assert_eq!(
            elements(wire),
            [section(json!([{
                "type": "text",
                "text": "2*3*4 snake_case_name a < b && c > d 10:30:00 *not closed",
            }]))]
        );
        round_trips(wire);
        assert_eq!(
            elements("&lt;"),
            [section(json!([{"type": "text", "text": "<"}]))]
        );
    }

    #[test]
    fn lines_are_one_text_element() {
        assert_eq!(
            elements("one\ntwo\n\nfour"),
            [section(
                json!([{"type": "text", "text": "one\ntwo\n\nfour"}])
            )]
        );
    }

    #[test]
    fn lists_nest_count_on_and_sit_between_lines() {
        let wire = "Plan:\n\n1. *first*\n    a. inner\n    b. more\n        i. deep\n\
                    2. second\n    • dot\n• loose\nafter";
        let item = |text: &str| section(json!([{"type": "text", "text": text}]));
        assert_eq!(
            elements(wire),
            [
                section(json!([{"type": "text", "text": "Plan:\n\n"}])),
                json!({"type": "rich_text_list", "style": "ordered", "indent": 0, "border": 0,
                    "elements": [section(json!([
                        {"type": "text", "text": "first", "style": {"bold": true}}]))]}),
                json!({"type": "rich_text_list", "style": "ordered", "indent": 1, "border": 0,
                    "elements": [item("inner"), item("more")]}),
                json!({"type": "rich_text_list", "style": "ordered", "indent": 2, "border": 0,
                    "elements": [item("deep")]}),
                json!({"type": "rich_text_list", "style": "ordered", "indent": 0, "border": 0,
                    "offset": 1, "elements": [item("second")]}),
                json!({"type": "rich_text_list", "style": "bullet", "indent": 1, "border": 0,
                    "elements": [item("dot")]}),
                json!({"type": "rich_text_list", "style": "bullet", "indent": 0, "border": 0,
                    "elements": [item("loose")]}),
                item("after"),
            ]
        );
        round_trips(wire);
    }

    #[test]
    fn numbers_that_start_no_list_stay_text() {
        // A year, a skipped number, a marker with nothing after it, a
        // marker Slack does not write and a padded number.
        for (wire, lists) in [
            ("2024. What a year", 0),
            ("1. one\n3. three", 1),
            ("• ", 0),
            ("- dash", 0),
            ("01. zero", 0),
            ("  • two spaces", 0),
        ] {
            let found = kinds(wire)
                .iter()
                .filter(|kind| *kind == "rich_text_list")
                .count();
            assert_eq!(found, lists, "{wire:?}");
            round_trips(wire);
        }
        assert_eq!(number_of("iv", 2), Some(4));
        assert_eq!(number_of("mcmxciv", 2), Some(1994));
        assert_eq!(number_of("iiii", 2), None);
        assert_eq!(number_of("ab", 1), Some(28));
        assert_eq!(number_of("+1", 0), None);
    }

    #[test]
    fn quotes_code_and_quoted_lists() {
        let wire =
            "&gt; said *this*\n&gt; • a point\n&gt; and more\n```\nlet x = a &lt; b;\n```\nend";
        assert_eq!(
            elements(wire),
            [
                json!({"type": "rich_text_quote", "elements": [
                    {"type": "text", "text": "said "},
                    {"type": "text", "text": "this", "style": {"bold": true}}]}),
                json!({"type": "rich_text_list", "style": "bullet", "indent": 0, "border": 1,
                    "elements": [section(json!([{"type": "text", "text": "a point"}]))]}),
                json!({"type": "rich_text_quote",
                    "elements": [{"type": "text", "text": "and more"}]}),
                json!({"type": "rich_text_preformatted", "border": 0,
                    "elements": [{"type": "text", "text": "let x = a < b;"}]}),
                section(json!([{"type": "text", "text": "end"}])),
            ]
        );
        round_trips(wire);
        // Two quotes a blank line apart stay two.
        assert_eq!(kinds("&gt; a\n\n&gt; b"), ["rich_text_quote"; 2]);
        round_trips("&gt; a\n\n&gt; b");
        round_trips("&gt; ```quoted code```\nplain");
    }

    #[test]
    fn a_quoted_list_that_would_lose_a_line_stays_text() {
        // The blank quoted line before the item would not survive the
        // quote and the list being joined, so the item stays a line.
        let wire = "&gt; a\n&gt;\n&gt; • b";
        assert_eq!(kinds(wire), ["rich_text_quote"]);
        round_trips(wire);
    }

    #[test]
    fn blocks_that_cannot_read_back_alike_send_text_alone() {
        // Two paragraphs side by side read back as one.
        let parsed = [
            Block::Paragraph(vec![text("a")]),
            Block::Paragraph(vec![text("b")]),
        ];
        assert_eq!(faithful(&parsed), Err(Skip::Unfaithful));
        let date = "due <!date^1700000000^{date}|Nov 14>";
        assert_eq!(rich_text(date), Err(Skip::Commands));
        assert_eq!(blocks_param(date), None);
        assert_eq!(layout(date), None);
    }

    #[test]
    fn empty_and_blank_messages_have_no_block() {
        for wire in ["", " ", "\n\n", "  \n \t", "```\n\n```"] {
            assert_eq!(rich_text(wire), Err(Skip::Empty), "{wire:?}");
            assert_eq!(blocks_param(wire), None);
            assert_eq!(layout(wire), None);
        }
    }

    #[test]
    fn the_parameter_is_an_array_of_the_block() {
        let param = blocks_param("*hi*").expect("a block");
        let parsed: Value = serde_json::from_str(&param).expect("json");
        assert_eq!(parsed, json!([block("*hi*")]));
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        assert_eq!(
            layout("*hi* <@U1|ann>"),
            Some(vec![Block::Paragraph(vec![
                Inline::Text("hi".into(), bold),
                text(" "),
                Inline::User {
                    id: "U1".into(),
                    label: None
                },
            ])])
        );
    }

    #[test]
    fn typical_messages_round_trip() {
        for wire in [
            "hi <@U1>, see <https://x.y|this> :tada:",
            "*Release* notes:\n• one\n• two `code`\n    • nested\n\nThanks <!channel>",
            "&gt; quoted _line_\n&gt; two\nreply",
            "look:\n```\nfn main() {}\n```\nok",
            "1. a\n2. b\n3. c",
            ":+1::skin-tone-6: ~gone~ <mailto:a@x.y>",
            "日本語 *太字* と :tada:",
        ] {
            round_trips(wire);
        }
    }
}
