//! The microphone's mute button in the call bar, beside Leave while the
//! huddle is live, and in the call window's controls: one self-contained
//! widget, drawn in either's [`Look`].
//!
//! Muted it is a quiet button with a red, struck-through microphone;
//! live it is filled with the huddle's green and a white microphone, so
//! an open microphone is never missed and never looks like Leave's red.
//! Cmd+Shift+Space toggles it (Slack's own chord) wherever the button
//! shows: the window that has the focus takes it.

use super::call_bar::{Lit, Look, Toggle, toggle_control};
use super::shortcuts::MIC;
use crate::huddle_mic::{Mic, MicAction};
use crate::i18n::{t, tf};
use crate::theme::{self, Palette};

/// What a click or the chord asks in state `mic`.
pub fn toggled(mic: Mic) -> MicAction {
    match mic {
        Mic::Muted => MicAction::Unmute,
        Mic::Opening | Mic::Live => MicAction::Mute,
    }
}

/// Draws the button for `mic` in `look`; returns what was asked, by a
/// click or the chord.
pub fn mute_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    mic: Mic,
    look: Look,
) -> Option<MicAction> {
    let shortcut = MIC.spelled();
    let (icon, label, tip) = match mic {
        Mic::Muted => (
            theme::Icon::MicOff,
            t("Unmute"),
            tf(
                "Your microphone is off. Unmute to talk ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
        Mic::Opening => (
            theme::Icon::Mic,
            t("Turning on…"),
            tf(
                "Opening your microphone. Mute ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
        Mic::Live => (
            theme::Icon::Mic,
            t("Mute"),
            tf(
                "Your microphone is on: everyone hears you. Mute ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
    };
    let lit = match mic {
        Mic::Live => Lit::On,
        Mic::Opening => Lit::Pending,
        Mic::Muted => Lit::Off { red: true },
    };
    let toggle = Toggle {
        icon,
        label: label.into_owned(),
        tip,
        lit,
        chord: MIC,
    };
    let (_, asked) = toggle_control(ui, palette, look, toggle);
    asked.then(|| toggled(mic))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_button_and_the_chord_flip_the_microphone() {
        assert_eq!(toggled(Mic::Muted), MicAction::Unmute);
        assert_eq!(toggled(Mic::Live), MicAction::Mute);
        // Opening can be called off.
        assert_eq!(toggled(Mic::Opening), MicAction::Mute);
    }

    #[test]
    fn the_chord_is_on_the_sheet() {
        assert_eq!(
            super::super::shortcuts::keys_of("Mute / unmute the microphone"),
            Some(MIC.text)
        );
    }
}
