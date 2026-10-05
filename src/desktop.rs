//! What NoSlacking does on the desktop around its window: notifications,
//! and the settings that steer them.
//!
//! The settings live in one value of their own inside
//! [`crate::settings::Settings`], so new desktop options never touch the
//! rest of the settings file.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::model::ConversationKind;
use crate::notify::Level;

/// The desktop options, as saved in the settings file.
///
/// Every field has a default, so a file written before a field existed (or
/// one with a field that cannot be read) still loads.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DesktopSettings {
    /// Whether new messages may show a desktop notification at all.
    pub notifications: bool,
    /// Whether notifications play the desktop's sound.
    pub sound: bool,
    /// Words that notify wherever they are written, like a mention.
    pub keywords: Vec<String>,
    /// Your choice per conversation, by [`conversation_key`]. A missing
    /// entry follows Slack's preference, or the default for its kind.
    pub levels: BTreeMap<String, Level>,
    /// Conversations muted on this computer, by workspace: for sign-ins
    /// whose Slack preferences cannot be read or changed (your own app).
    pub muted: BTreeMap<String, BTreeSet<String>>,
    /// Whether to show the tray item, where the desktop has a tray.
    pub tray: bool,
    /// Whether closing the window keeps NoSlacking running in the tray.
    pub close_to_tray: bool,
    /// Whether NoSlacking starts when you log in (see [`crate::autostart`]).
    pub start_on_login: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            notifications: true,
            sound: true,
            keywords: Vec::new(),
            levels: BTreeMap::new(),
            muted: BTreeMap::new(),
            tray: true,
            // As Slack's own app: closing the window keeps you reachable.
            // Without a tray, closing quits all the same.
            close_to_tray: true,
            start_on_login: false,
        }
    }
}

impl DesktopSettings {
    /// Your own level for a conversation, if you set one here.
    pub fn level(&self, team: &str, channel: &str) -> Option<Level> {
        self.levels.get(&conversation_key(team, channel)).copied()
    }

    /// Sets (or with `None` forgets) your level for a conversation.
    pub fn set_level(&mut self, team: &str, channel: &str, level: Option<Level>) {
        let key = conversation_key(team, channel);
        match level {
            Some(level) => {
                self.levels.insert(key, level);
            }
            None => {
                self.levels.remove(&key);
            }
        }
    }

    /// What one workspace's views need of these settings.
    pub fn team_state(&self, team: &str) -> TeamState {
        let prefix = format!("{team}/");
        TeamState {
            levels: self
                .levels
                .iter()
                .filter_map(|(key, level)| Some((key.strip_prefix(&prefix)?.to_owned(), *level)))
                .collect(),
            local_muted: self
                .muted
                .get(team)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default(),
            ..TeamState::default()
        }
    }

    /// Forgets everything kept for a workspace, once it is signed out.
    pub fn forget_workspace(&mut self, team: &str) {
        let prefix = format!("{team}/");
        self.levels.retain(|key, _| !key.starts_with(&prefix));
        self.muted.remove(team);
    }

    /// Mutes or unmutes a conversation on this computer.
    pub fn set_muted(&mut self, team: &str, channel: &str, muted: bool) {
        let set = self.muted.entry(team.to_owned()).or_default();
        if muted {
            set.insert(channel.to_owned());
        } else {
            set.remove(channel);
        }
        if set.is_empty() {
            self.muted.remove(team);
        }
    }

    /// The keywords as typed in the settings: one per line or comma.
    pub fn keywords_text(&self) -> String {
        self.keywords.join(", ")
    }

    /// Reads keywords typed as a list, dropping empty ones and repeats.
    pub fn set_keywords_text(&mut self, text: &str) {
        self.keywords = parse_keywords(text);
    }
}

/// The desktop state of one workspace, kept with it so the sidebar can
/// show it without reaching for the settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TeamState {
    /// Your own levels, by conversation: this workspace's part of
    /// [`DesktopSettings::levels`], which stays the saved copy.
    pub levels: HashMap<String, Level>,
    /// Do Not Disturb, as Slack last said or as you set it here.
    pub dnd: crate::dnd::Dnd,
    /// Slack was asked for the stretch after the one that passed.
    pub dnd_asked: bool,
    /// Your notification preferences in Slack, for browser sessions, which
    /// can read and change them. `None` until they arrive, and for ever
    /// for sign-ins that cannot: mutes then stay on this computer.
    pub slack: Option<SlackPrefs>,
    /// Conversations muted on this computer: this workspace's part of
    /// [`DesktopSettings::muted`].
    pub local_muted: HashSet<String>,
}

/// What Slack keeps of your notification preferences, as far as this
/// client uses them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlackPrefs {
    /// Muted conversations: no notifications, and unread only for mentions.
    pub muted: HashSet<String>,
    /// Desktop notification levels chosen per conversation in Slack.
    pub levels: HashMap<String, Level>,
    /// Slack's own default for channels, when you changed it there.
    pub channel_default: Option<Level>,
    /// Your "My keywords" from Slack's preferences.
    pub keywords: Vec<String>,
}

impl TeamState {
    /// Your own level for a conversation, if you set one here.
    pub fn chosen(&self, channel: &str) -> Option<Level> {
        self.levels.get(channel).copied()
    }

    /// The level a conversation notifies at: yours, else Slack's for it,
    /// else Slack's default for channels, else the default for its kind.
    pub fn level(&self, channel: &str, kind: ConversationKind) -> Level {
        self.chosen(channel)
            .unwrap_or_else(|| self.default_level(channel, kind))
    }

    /// The level a conversation has without your choice here: Slack's for
    /// it, else Slack's default for channels, else the default for its
    /// kind.
    pub fn default_level(&self, channel: &str, kind: ConversationKind) -> Level {
        let slack = self.slack.as_ref();
        slack
            .and_then(|prefs| prefs.levels.get(channel).copied())
            .or_else(|| slack?.channel_default.filter(|_| !kind.is_dm()))
            .unwrap_or_else(|| Level::default_for(kind))
    }

    /// Whether a conversation is muted, in Slack or on this computer.
    pub fn is_muted(&self, channel: &str) -> bool {
        self.local_muted.contains(channel)
            || self
                .slack
                .as_ref()
                .is_some_and(|prefs| prefs.muted.contains(channel))
    }

    /// Whether muting goes to Slack (a browser session whose preferences
    /// arrived) rather than staying on this computer.
    pub fn mutes_in_slack(&self) -> bool {
        self.slack.is_some()
    }

    /// Slack's keywords, to add to your own.
    pub fn slack_keywords(&self) -> &[String] {
        self.slack.as_ref().map_or(&[], |prefs| &prefs.keywords)
    }
}

/// Splits typed keywords at commas and line breaks, trimmed, without empty
/// ones or repeats (ignoring case).
pub fn parse_keywords(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split([',', '\n']).map(str::trim) {
        if !word.is_empty() && !out.iter().any(|w| w.eq_ignore_ascii_case(word)) {
            out.push(word.to_owned());
        }
    }
    out
}

/// How a conversation is named in the settings file: `T123/C456`.
pub fn conversation_key(team: &str, channel: &str) -> String {
    format!("{team}/{channel}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_are_split_trimmed_and_deduplicated() {
        assert_eq!(
            parse_keywords("deploy, Outage\n outage ,, release\n"),
            ["deploy", "Outage", "release"]
        );
        assert!(parse_keywords(" , \n").is_empty());
    }

    #[test]
    fn levels_are_kept_per_workspace() {
        let mut settings = DesktopSettings::default();
        settings.set_level("T1", "C1", Some(Level::All));
        settings.set_level("T2", "C1", Some(Level::Nothing));
        assert_eq!(settings.level("T1", "C1"), Some(Level::All));
        let team = settings.team_state("T1");
        assert_eq!(team.chosen("C1"), Some(Level::All));
        assert_eq!(team.level("C2", ConversationKind::Channel), Level::Mentions);
        assert_eq!(team.level("D2", ConversationKind::Direct), Level::All);
        settings.forget_workspace("T1");
        assert_eq!(settings.level("T1", "C1"), None);
        assert_eq!(settings.level("T2", "C1"), Some(Level::Nothing));
        settings.set_level("T2", "C1", None);
        assert!(settings.levels.is_empty());
    }

    #[test]
    fn mutes_and_levels_combine_slack_and_this_computer() {
        let mut settings = DesktopSettings::default();
        settings.set_muted("T1", "C1", true);
        let mut team = settings.team_state("T1");
        assert!(team.is_muted("C1"));
        assert!(!team.mutes_in_slack());
        team.slack = Some(SlackPrefs {
            muted: HashSet::from(["C2".to_owned()]),
            levels: HashMap::from([("C3".to_owned(), Level::All)]),
            channel_default: Some(Level::Nothing),
            keywords: vec!["deploy".into()],
        });
        assert!(team.is_muted("C2") && team.is_muted("C1"));
        assert_eq!(team.level("C3", ConversationKind::Channel), Level::All);
        assert_eq!(team.level("C4", ConversationKind::Channel), Level::Nothing);
        assert_eq!(team.level("D4", ConversationKind::Direct), Level::All);
        team.levels.insert("C3".into(), Level::Mentions);
        assert_eq!(team.level("C3", ConversationKind::Channel), Level::Mentions);
        assert_eq!(team.slack_keywords(), ["deploy"]);
        settings.set_muted("T1", "C1", false);
        assert!(settings.muted.is_empty());
    }

    #[test]
    fn old_files_get_the_defaults() {
        let settings: DesktopSettings = serde_json::from_str(r#"{"sound": false}"#).expect("reads");
        assert!(settings.notifications);
        assert!(!settings.sound);
    }
}
