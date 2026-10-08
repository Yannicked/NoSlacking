//! The command palette: the quick switcher (Ctrl+K) with `>` typed first
//! lists commands instead of conversations, as VS Code's does.
//!
//! Each command is something the app does already, from a menu or a
//! shortcut; the palette only finds it by name. What a command does can
//! hang on where you are (away or active, the theme), so the list is made
//! from a [`State`].

use crate::i18n::{t, tf};
use crate::model::Action;
use crate::sidebar::HideInactive;

/// What starts a command search in the switcher.
pub const PREFIX: char = '>';

/// What the commands' words and effects depend on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// You show as away in the open workspace.
    pub away: bool,
    /// "Always show as active" is on.
    pub stay_active: bool,
    /// The palette in use is a dark one.
    pub dark: bool,
    /// After how long quiet conversations are hidden now.
    pub hide_inactive: HideInactive,
}

/// One command of the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    MarkAllRead,
    SetStatus,
    /// Away, or active again, whichever you are not.
    ToggleAway,
    ToggleStayActive,
    ToggleTheme,
    Settings,
    Shortcuts,
    NewMessage,
    Browse,
    Search,
    /// Pauses notifications for an hour.
    Snooze,
    /// Moves "Hide inactive conversations" on to its next choice.
    CycleHideInactive,
}

impl Command {
    /// Every command, in the order an empty search lists them.
    pub const ALL: [Self; 12] = [
        Self::NewMessage,
        Self::Search,
        Self::Browse,
        Self::MarkAllRead,
        Self::SetStatus,
        Self::ToggleAway,
        Self::ToggleStayActive,
        Self::Snooze,
        Self::ToggleTheme,
        Self::CycleHideInactive,
        Self::Settings,
        Self::Shortcuts,
    ];

    /// What the palette calls it, as things stand.
    pub fn label(self, state: &State) -> String {
        match self {
            Self::MarkAllRead => t("Mark all as read").into_owned(),
            Self::SetStatus => t("Set a status…").into_owned(),
            Self::ToggleAway if state.away => t("Set yourself as active").into_owned(),
            Self::ToggleAway => t("Set yourself as away").into_owned(),
            Self::ToggleStayActive if state.stay_active => {
                t("Stop always showing as active").into_owned()
            }
            Self::ToggleStayActive => t("Always show as active").into_owned(),
            Self::ToggleTheme if state.dark => t("Switch to the light theme").into_owned(),
            Self::ToggleTheme => t("Switch to the dark theme").into_owned(),
            Self::Settings => t("Open settings").into_owned(),
            Self::Shortcuts => t("Keyboard shortcuts").into_owned(),
            Self::NewMessage => t("New message").into_owned(),
            Self::Browse => t("Browse channels").into_owned(),
            Self::Search => t("Search messages and files").into_owned(),
            Self::Snooze => t("Pause notifications for 1 hour").into_owned(),
            Self::CycleHideInactive => tf(
                "Hide inactive conversations: {choice}",
                &[("choice", &next_hide(state.hide_inactive).label())],
            ),
        }
    }

    /// The line of the shortcut sheet whose keys also do this, by its
    /// (English) label, for showing the keys beside the command.
    pub fn shortcut(self) -> Option<&'static str> {
        match self {
            Self::NewMessage => Some("New message"),
            Self::Browse => Some("Browse channels"),
            Self::Search => Some("Search messages and files"),
            Self::Settings => Some("Settings"),
            Self::Shortcuts => Some("Keyboard shortcuts"),
            _ => None,
        }
    }

    /// The action that carries it out, as the menus send it.
    pub fn action(self, state: &State) -> Action {
        use crate::people::Action as People;
        match self {
            Self::MarkAllRead => Action::Views(crate::views::Action::MarkAllRead),
            Self::SetStatus => Action::People(People::EditStatus),
            Self::ToggleAway => Action::People(People::SetAway(!state.away)),
            Self::ToggleStayActive => Action::People(People::StayActive(!state.stay_active)),
            Self::ToggleTheme => Action::SetAppearance(if state.dark {
                crate::settings::Appearance::Light
            } else {
                crate::settings::Appearance::Dark
            }),
            Self::Settings => Action::ShowSettings,
            Self::Shortcuts => Action::ShowShortcuts,
            Self::NewMessage => Action::Convos(crate::convos::Action::NewMessage),
            Self::Browse => Action::Convos(crate::convos::Action::Browse),
            Self::Search => Action::OpenSearch,
            Self::Snooze => Action::Snooze(Some(crate::dnd::Snooze::Hour1)),
            Self::CycleHideInactive => Action::HideInactive(next_hide(state.hide_inactive)),
        }
    }
}

/// The choice of "Hide inactive conversations" after `now`, round again
/// to Off after the longest.
pub fn next_hide(now: HideInactive) -> HideInactive {
    let all = HideInactive::ALL;
    let at = all.iter().position(|h| *h == now).unwrap_or(0);
    all[(at + 1) % all.len()]
}

/// The command search in what was typed in the switcher, if it is one:
/// what follows the `>`.
pub fn query(typed: &str) -> Option<&str> {
    typed.trim_start().strip_prefix(PREFIX).map(str::trim)
}

/// How well `label` matches `needle` (both lower case), lower is better:
/// it starts with it, a word starts with it, it holds it, or its letters
/// come in order with others between (so "mar" finds "Mark all as read"
/// and "kbs" finds "Keyboard shortcuts").
fn score(label: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() || label.starts_with(needle) {
        return Some(0);
    }
    if label
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(needle))
    {
        return Some(1);
    }
    if label.contains(needle) {
        return Some(2);
    }
    let mut letters = label.chars();
    needle
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|wanted| letters.any(|c| c == wanted))
        .then_some(3)
}

/// The commands whose names match `needle`, best first, the palette's own
/// order among equals.
pub fn matching(needle: &str, state: &State) -> Vec<Command> {
    let needle = needle.trim().to_lowercase();
    let mut found: Vec<(u8, usize, Command)> = Command::ALL
        .iter()
        .enumerate()
        .filter_map(|(at, &command)| {
            score(&command.label(state).to_lowercase(), &needle).map(|s| (s, at, command))
        })
        .collect();
    found.sort_by_key(|&(s, at, _)| (s, at));
    found.into_iter().map(|(_, _, command)| command).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_greater_than_sign_starts_a_command_search() {
        assert_eq!(query(">mark"), Some("mark"));
        assert_eq!(query("  > theme "), Some("theme"));
        assert_eq!(query(">"), Some(""));
        assert_eq!(query("general"), None);
        assert_eq!(query("a>b"), None);
    }

    #[test]
    fn commands_match_by_start_word_and_letters_in_order() {
        let state = State::default();
        assert_eq!(matching("", &state), Command::ALL.to_vec());
        assert_eq!(matching("mark", &state)[0], Command::MarkAllRead);
        assert_eq!(matching("THEME", &state), vec![Command::ToggleTheme]);
        assert_eq!(matching("kbs", &state), vec![Command::Shortcuts]);
        assert_eq!(matching("away", &state)[0], Command::ToggleAway);
        assert!(matching("zzz", &state).is_empty());
        // A word's start beats letters strewn about.
        let set = matching("set", &state);
        assert_eq!(set.first(), Some(&Command::SetStatus));
    }

    #[test]
    fn the_words_follow_where_you_are() {
        let mut state = State::default();
        assert_eq!(Command::ToggleAway.label(&state), "Set yourself as away");
        state.away = true;
        assert_eq!(Command::ToggleAway.label(&state), "Set yourself as active");
        assert_eq!(matching("active", &state)[0], Command::ToggleAway);
        state.dark = true;
        assert_eq!(
            Command::ToggleTheme.label(&state),
            "Switch to the light theme"
        );
        state.hide_inactive = HideInactive::Month;
        assert_eq!(
            Command::CycleHideInactive.label(&state),
            "Hide inactive conversations: After 3 months"
        );
    }

    #[test]
    fn each_command_sends_what_its_menu_sends() {
        use crate::people::Action as People;
        let state = State {
            away: true,
            stay_active: false,
            dark: true,
            hide_inactive: HideInactive::ThreeMonths,
        };
        let action = |command: Command| command.action(&state);
        assert!(matches!(
            action(Command::MarkAllRead),
            Action::Views(crate::views::Action::MarkAllRead)
        ));
        assert!(matches!(
            action(Command::SetStatus),
            Action::People(People::EditStatus)
        ));
        assert!(matches!(
            action(Command::ToggleAway),
            Action::People(People::SetAway(false))
        ));
        assert!(matches!(
            action(Command::ToggleStayActive),
            Action::People(People::StayActive(true))
        ));
        assert!(matches!(
            action(Command::ToggleTheme),
            Action::SetAppearance(crate::settings::Appearance::Light)
        ));
        assert!(matches!(action(Command::Settings), Action::ShowSettings));
        assert!(matches!(action(Command::Shortcuts), Action::ShowShortcuts));
        assert!(matches!(
            action(Command::NewMessage),
            Action::Convos(crate::convos::Action::NewMessage)
        ));
        assert!(matches!(
            action(Command::Browse),
            Action::Convos(crate::convos::Action::Browse)
        ));
        assert!(matches!(action(Command::Search), Action::OpenSearch));
        assert!(matches!(
            action(Command::Snooze),
            Action::Snooze(Some(crate::dnd::Snooze::Hour1))
        ));
        assert!(matches!(
            action(Command::CycleHideInactive),
            Action::HideInactive(HideInactive::Off)
        ));
    }

    #[test]
    fn hiding_inactive_conversations_goes_round() {
        let mut choice = HideInactive::Off;
        let mut seen = Vec::new();
        for _ in 0..4 {
            choice = next_hide(choice);
            seen.push(choice);
        }
        assert_eq!(
            seen,
            [
                HideInactive::Week,
                HideInactive::Month,
                HideInactive::ThreeMonths,
                HideInactive::Off
            ]
        );
    }
}
