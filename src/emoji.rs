//! Slack's emoji: shortcodes (`:tada:`, `:+1::skin-tone-3:`) and each
//! workspace's custom emoji, which are images or aliases of other emoji.

use std::collections::HashMap;

/// What a shortcode stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    Unicode(String),
    /// A custom emoji's image URL.
    Image(String),
    Unknown,
}

include!("emoji_table.rs");

/// Slack's own aliases that are in neither table, to the names they mean.
const SLACK_NAMES: &[(&str, &str)] = &[
    ("simple_smile", "slightly_smiling_face"),
    ("thumbsup_all", "+1"),
    ("slack", "speech_balloon"),
    ("party_popper", "tada"),
];

/// A workspace's custom emoji, from `emoji.list`.
#[derive(Clone, Debug, Default)]
pub struct EmojiSet {
    custom: HashMap<String, String>,
}

impl EmojiSet {
    pub fn new(custom: HashMap<String, String>) -> Self {
        Self { custom }
    }

    pub fn custom_names(&self) -> impl Iterator<Item = (&str, &str)> {
        self.custom
            .iter()
            .filter(|(_, value)| !value.starts_with("alias:"))
            .map(|(name, url)| (name.as_str(), url.as_str()))
    }

    pub fn resolve(&self, name: &str) -> Resolved {
        let (base, tone) = split_tone(name);
        let mut current = base;
        // Aliases can chain; never follow a loop.
        for _ in 0..4 {
            match self.custom.get(current) {
                Some(value) => match value.strip_prefix("alias:") {
                    Some(target) => current = target,
                    None => return Resolved::Image(value.clone()),
                },
                None => break,
            }
        }
        match unicode(current, tone) {
            Some(text) => Resolved::Unicode(text),
            None => Resolved::Unknown,
        }
    }
}

/// `+1::skin-tone-3` is `+1` at the third tone (Slack counts 2 to 6).
pub fn split_tone(name: &str) -> (&str, Option<u8>) {
    match name.split_once("::skin-tone-") {
        Some((base, tone)) => (base, tone.parse().ok()),
        None => (name, None),
    }
}

/// The Unicode for a standard shortcode. An emoji newer than the `emojis`
/// crate still shows, without its skin tones.
pub fn unicode(name: &str, tone: Option<u8>) -> Option<String> {
    match standard(name) {
        Some(emoji) => Some(with_tone(emoji, tone.unwrap_or(0)).to_owned()),
        None => slack_emoji(alias(name)).map(str::to_owned),
    }
}

/// `name`, or what a Slack-only alias of it means.
fn alias(name: &str) -> &str {
    SLACK_NAMES
        .iter()
        .find(|(slack, _)| *slack == name)
        .map_or(name, |(_, meant)| meant)
}

/// The emoji Slack's table gives `name`.
fn slack_emoji(name: &str) -> Option<&'static str> {
    NAMES
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| NAMES[i].1)
}

/// Slack's names for `emoji`, the one Slack writes first; empty for an
/// emoji Slack's table lacks.
pub fn names(emoji: &emojis::Emoji) -> &'static [&'static str] {
    // Skin tones and variation selectors are not part of the name.
    let key: String = emoji
        .with_skin_tone(emojis::SkinTone::Default)
        .unwrap_or(emoji)
        .as_str()
        .chars()
        .filter(|&c| c != '\u{fe0f}')
        .collect();
    BY_EMOJI
        .binary_search_by(|(e, _)| (*e).cmp(key.as_str()))
        .map_or(&[], |i| BY_EMOJI[i].1)
}

/// The shortcode to send for `emoji`: Slack's own name when it has one,
/// since other Slack clients only know those, else GitHub's.
pub fn shortcode(emoji: &'static emojis::Emoji) -> Option<&'static str> {
    names(emoji).first().copied().or_else(|| emoji.shortcode())
}

/// Every Slack shortcode, for autocomplete.
pub fn all_names() -> impl Iterator<Item = &'static str> {
    NAMES.iter().map(|(name, _)| *name)
}

/// How many emoji the picker's "Recently used" row remembers.
pub const RECENT_MAX: usize = 24;

/// Slack's skin tones run from 2 (light) to 6 (dark); anything else is the
/// default yellow.
pub fn valid_tone(tone: u8) -> Option<u8> {
    (2..=6).contains(&tone).then_some(tone)
}

/// The `emojis` crate's name for Slack's tone number.
fn skin_tone(tone: u8) -> Option<emojis::SkinTone> {
    Some(match tone {
        2 => emojis::SkinTone::Light,
        3 => emojis::SkinTone::MediumLight,
        4 => emojis::SkinTone::Medium,
        5 => emojis::SkinTone::MediumDark,
        6 => emojis::SkinTone::Dark,
        _ => return None,
    })
}

/// The standard emoji a shortcode names: Slack's table first, then
/// GitHub's names (which `emojis` follows) for the ones Slack spells alike.
pub fn standard(name: &str) -> Option<&'static emojis::Emoji> {
    let name = alias(name);
    slack_emoji(name)
        .and_then(emojis::get)
        .or_else(|| emojis::get_by_shortcode(name))
}

/// Whether the emoji comes in skin tones (`:+1:` does, `:tada:` not).
pub fn has_tones(name: &str) -> bool {
    standard(name).is_some_and(|e| e.skin_tones().is_some())
}

/// `name` at your skin tone, as Slack writes it (`+1::skin-tone-3`), when
/// the emoji has tones and the name carries none yet.
pub fn toned(name: &str, tone: u8) -> String {
    match valid_tone(tone) {
        Some(tone) if !name.contains("::skin-tone-") && has_tones(name) => {
            format!("{name}::skin-tone-{tone}")
        }
        _ => name.to_owned(),
    }
}

/// The emoji drawn for a standard emoji at a tone, for the picker.
pub fn with_tone(emoji: &'static emojis::Emoji, tone: u8) -> &'static str {
    skin_tone(tone)
        .and_then(|tone| emoji.with_skin_tone(tone))
        .unwrap_or(emoji)
        .as_str()
}

/// Gives every `:shortcode:` in wire text that has tones your skin tone,
/// except in code, where `:+1:` is just text, and in `<…>` forms, where it
/// is part of a link or a label.
pub fn tone_shortcodes(wire: &str, tone: u8) -> String {
    if valid_tone(tone).is_none() || !wire.contains(':') {
        return wire.to_owned();
    }
    let mut out = String::with_capacity(wire.len());
    let mut shields = crate::mrkdwn::shielded(wire).into_iter().peekable();
    let mut rest = wire;
    while let Some(c) = rest.chars().next() {
        let at = wire.len() - rest.len();
        while shields.next_if(|&(_, end)| end < at).is_some() {}
        if let Some((_, end)) = shields.next_if(|&(start, _)| start <= at) {
            // Shields are whole characters, so this cuts at a boundary.
            let len = end + 1 - at;
            out.push_str(&rest[..len]);
            rest = &rest[len..];
            continue;
        }
        if c == ':' {
            let after = &rest[1..];
            let name_len = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '\'')))
                .unwrap_or(after.len());
            let name = &after[..name_len];
            let closed = after[name_len..].starts_with(':');
            let before_ok = out
                .chars()
                .next_back()
                .is_none_or(|p| !p.is_ascii_alphanumeric());
            let already = after[name_len..].starts_with("::skin-tone-");
            if closed && !name.is_empty() && before_ok && !already && has_tones(name) {
                out.push(':');
                out.push_str(&toned(name, tone));
                out.push(':');
                rest = &after[name_len + 1..];
                continue;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The emoji in a message's wire text, by name without their tone, in
/// order and once each, for "Recently used".
pub fn used_in(wire: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for block in crate::mrkdwn::parse(wire) {
        if let crate::mrkdwn::Block::Paragraph(inlines) | crate::mrkdwn::Block::Quote(inlines) =
            block
        {
            for inline in inlines {
                if let crate::mrkdwn::Inline::Emoji(name) = inline {
                    let base = split_tone(&name).0.to_owned();
                    if !found.contains(&base) {
                        found.push(base);
                    }
                }
            }
        }
    }
    found
}

/// Puts `names` at the front of the recently used list, newest first,
/// without repeats, keeping at most [`RECENT_MAX`].
pub fn remember(recent: &mut Vec<String>, names: &[String]) {
    for name in names.iter().rev() {
        let base = split_tone(name).0;
        recent.retain(|r| r != base);
        recent.insert(0, base.to_owned());
    }
    recent.truncate(RECENT_MAX);
}

/// A standard emoji group's name in the interface language, for the
/// picker's headings.
pub fn group_name(group: emojis::Group) -> std::borrow::Cow<'static, str> {
    use crate::i18n::t;
    use emojis::Group;
    match group {
        Group::SmileysAndEmotion => t("Smileys & Emotion"),
        Group::PeopleAndBody => t("People & Body"),
        Group::AnimalsAndNature => t("Animals & Nature"),
        Group::FoodAndDrink => t("Food & Drink"),
        Group::TravelAndPlaces => t("Travel & Places"),
        Group::Activities => t("Activities"),
        Group::Objects => t("Objects"),
        Group::Symbols => t("Symbols"),
        Group::Flags => t("Flags"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_names_resolve_and_are_what_we_send() {
        // Slack's name, which GitHub's table spells `green_circle`.
        assert_eq!(unicode("large_green_circle", None).as_deref(), Some("🟢"));
        assert_eq!(unicode("thumbsup", Some(3)).as_deref(), Some("👍🏼"));
        // GitHub-only spellings still read, and Slack-only aliases too.
        assert_eq!(unicode("green_circle", None).as_deref(), Some("🟢"));
        assert_eq!(unicode("simple_smile", None).as_deref(), Some("🙂"));
        let circle = emojis::get("🟢").expect("in emojis");
        assert_eq!(shortcode(circle), Some("large_green_circle"));
        let heart = emojis::get("❤️").expect("in emojis");
        assert_eq!(shortcode(heart), Some("heart"));
        let thumbs = emojis::get("👍🏽").expect("toned");
        assert_eq!(names(thumbs), ["+1", "thumbsup"]);
        assert!(all_names().any(|n| n == "large_green_circle"));
    }

    #[test]
    fn the_name_table_is_sorted_for_lookup() {
        assert!(NAMES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(BY_EMOJI.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn standard_names_tones_and_slack_spellings() {
        let set = EmojiSet::default();
        assert_eq!(set.resolve("tada"), Resolved::Unicode("🎉".into()));
        assert_eq!(set.resolve("+1"), Resolved::Unicode("👍".into()));
        assert_eq!(
            set.resolve("+1::skin-tone-6"),
            Resolved::Unicode("👍🏿".into())
        );
        assert_eq!(set.resolve("simple_smile"), Resolved::Unicode("🙂".into()));
        // Names both tables share need no entry of their own.
        assert_eq!(
            set.resolve("upside_down_face"),
            Resolved::Unicode("🙃".into())
        );
        assert_eq!(
            set.resolve("heavy_check_mark"),
            Resolved::Unicode("✔️".into())
        );
        assert_eq!(set.resolve("no_such_thing"), Resolved::Unknown);
    }

    #[test]
    fn your_tone_goes_on_emoji_that_have_tones() {
        assert_eq!(toned("+1", 3), "+1::skin-tone-3");
        assert_eq!(toned("wave", 6), "wave::skin-tone-6");
        assert_eq!(toned("tada", 3), "tada");
        assert_eq!(toned("+1", 0), "+1");
        assert_eq!(toned("+1", 9), "+1");
        assert_eq!(toned("+1::skin-tone-2", 5), "+1::skin-tone-2");
        assert_eq!(
            tone_shortcodes(":+1: ok :tada: `:+1:` :wave::skin-tone-2: a:+1:", 4),
            ":+1::skin-tone-4: ok :tada: `:+1:` :wave::skin-tone-2: a:+1:"
        );
        assert_eq!(tone_shortcodes(":+1:", 1), ":+1:");
    }

    #[test]
    fn your_tone_stays_out_of_code_and_links() {
        assert_eq!(
            tone_shortcodes("``a ` :+1: b`` :+1:", 2),
            "``a ` :+1: b`` :+1::skin-tone-2:"
        );
        assert_eq!(
            tone_shortcodes("<https://x.y/:wave:/|:+1:> :wave:", 3),
            "<https://x.y/:wave:/|:+1:> :wave::skin-tone-3:"
        );
        assert_eq!(
            tone_shortcodes("```\n:+1:\n``` &gt; `:+1:` :+1:", 4),
            "```\n:+1:\n``` &gt; `:+1:` :+1::skin-tone-4:"
        );
        // A lone tick opens nothing, so what follows still gets the tone.
        assert_eq!(tone_shortcodes("it`s :+1:", 5), "it`s :+1::skin-tone-5:");
    }

    #[test]
    fn recently_used_emoji_lead_without_repeats() {
        assert_eq!(
            used_in(":+1::skin-tone-3: and :tada: :+1: `:eyes:`"),
            ["+1", "tada"]
        );
        let mut recent = vec!["eyes".to_owned(), "tada".to_owned()];
        remember(
            &mut recent,
            &["tada".to_owned(), "+1::skin-tone-2".to_owned()],
        );
        assert_eq!(recent, ["tada", "+1", "eyes"]);
        let many: Vec<String> = (0..30).map(|n| format!("e{n}")).collect();
        remember(&mut recent, &many);
        assert_eq!(recent.len(), RECENT_MAX);
        assert_eq!(recent[0], "e0");
    }

    #[test]
    fn custom_emoji_and_alias_chains() {
        let set = EmojiSet::new(HashMap::from([
            (
                "parrot".to_owned(),
                "https://emoji.slack-edge.com/parrot.gif".to_owned(),
            ),
            ("party".to_owned(), "alias:parrot".to_owned()),
            ("yay".to_owned(), "alias:tada".to_owned()),
            ("ouroboros".to_owned(), "alias:ouroboros".to_owned()),
        ]));
        assert_eq!(
            set.resolve("party"),
            Resolved::Image("https://emoji.slack-edge.com/parrot.gif".into())
        );
        assert_eq!(set.resolve("yay"), Resolved::Unicode("🎉".into()));
        assert_eq!(set.resolve("ouroboros"), Resolved::Unknown);
    }
}
