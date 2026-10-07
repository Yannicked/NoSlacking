//! The microphone's mute button in the call bar, beside Leave while the
//! huddle is live, and in the call window's controls: one self-contained
//! widget, drawn in either's [`Look`].
//!
//! Muted it is a quiet button with a red, struck-through microphone;
//! live it is filled with the huddle's green and a white microphone, so
//! an open microphone is never missed and never looks like Leave's red.
//! Cmd+Shift+Space toggles it (Slack's own chord) wherever the button
//! shows: the window that has the focus takes it.

use egui::{Color32, Key, Modifiers};

use super::call_bar::{Look, control};
use super::people::ACTIVE;
use super::shortcuts::spell;
use crate::huddle_mic::{Mic, MicAction};
use crate::i18n::{t, tf};
use crate::theme::{self, Palette};

/// The chord that toggles the microphone, as the shortcut sheet lists it.
pub const TOGGLE: &str = "Cmd+Shift+Space";

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
    let shortcut = spell(TOGGLE, cfg!(target_os = "macos"));
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
    let (fill, ink, icon_ink) = match mic {
        Mic::Live => (ACTIVE, Color32::WHITE, Color32::WHITE),
        Mic::Opening => (palette.surface_hover, palette.secondary, palette.secondary),
        Mic::Muted => (palette.surface_hover, palette.text, palette.danger),
    };
    let response = control(ui, look, (icon, icon_ink), &label, ink, fill).on_hover_text(tip);
    let chord =
        ui.input_mut(|input| input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Space));
    (response.clicked() || chord).then(|| toggled(mic))
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
            Some(TOGGLE)
        );
    }
}
