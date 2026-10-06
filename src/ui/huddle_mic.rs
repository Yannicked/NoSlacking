//! The microphone's mute button in a huddle (the `huddle-audio`
//! feature): one self-contained widget, placed for now beside "Leave" in
//! the conversation's header.
//!
//! Muted it is a plain button with a struck-through microphone; live it
//! is filled red, so an open microphone is never missed. Cmd+Shift+Space
//! (Slack's own chord for it) toggles it wherever the widget shows.

use egui::{Key, Modifiers, RichText};

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

/// Draws the button for `mic`; returns what was asked, by a click or the
/// chord.
pub fn mute_button(ui: &mut egui::Ui, palette: &Palette, mic: Mic) -> Option<MicAction> {
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
    let live = mic == Mic::Live;
    let (ink, fill) = if live {
        (egui::Color32::WHITE, Some(palette.danger))
    } else {
        (palette.text, None)
    };
    let mut button = egui::Button::image_and_text(
        icon.image(if live { ink } else { palette.secondary }, 14.0),
        RichText::new(label).font(theme::regular(12.5)).color(ink),
    )
    .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL + 2));
    if let Some(fill) = fill {
        button = button.fill(fill);
    }
    let response = ui
        .add(button)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip);
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
