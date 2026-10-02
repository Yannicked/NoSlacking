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

/// Slack's names for emoji that GitHub's gemoji table (which `emojis`
/// follows) spells differently.
/// Names the two tables share are looked up directly and need no entry.
const SLACK_NAMES: &[(&str, &str)] = &[
    ("simple_smile", "slightly_smiling_face"),
    ("thumbsup_all", "+1"),
    ("slack", "speech_balloon"),
    ("white_frowning_face", "frowning_face"),
    ("face_with_rolling_eyes", "roll_eyes"),
    ("hugging_face", "hugs"),
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
fn split_tone(name: &str) -> (&str, Option<u8>) {
    match name.split_once("::skin-tone-") {
        Some((base, tone)) => (base, tone.parse().ok()),
        None => (name, None),
    }
}

/// The Unicode for a standard shortcode.
pub fn unicode(name: &str, tone: Option<u8>) -> Option<String> {
    let name = SLACK_NAMES
        .iter()
        .find(|(slack, _)| *slack == name)
        .map_or(name, |(_, gemoji)| gemoji);
    let emoji = emojis::get_by_shortcode(name)?;
    let toned = tone.and_then(|tone| {
        let tone = match tone {
            2 => emojis::SkinTone::Light,
            3 => emojis::SkinTone::MediumLight,
            4 => emojis::SkinTone::Medium,
            5 => emojis::SkinTone::MediumDark,
            6 => emojis::SkinTone::Dark,
            _ => return None,
        };
        emoji.with_skin_tone(tone)
    });
    Some(toned.unwrap_or(emoji).as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
