//! Sharing your screen from the call bar (the `huddle-share` feature):
//! the Share button beside the camera's while the huddle is live (and in
//! the call window's controls, in their [`Look`]), the row saying you
//! are sharing with Stop sharing, and, where the system has no dialog of
//! its own, the screens and windows to choose from. Self-contained, so
//! the call bar and the call window only place them.
//!
//! Off it is a quiet button with a screen; on it is filled with the
//! huddle's green, as the camera is, so a share that is on is never
//! missed, and the row above the buttons says so in words. Cmd+Shift+E
//! starts or stops it (Teams' chord; Slack's desktop app has none of its
//! own) wherever the button shows. Its menu (a right click) chooses
//! something else to share.

use egui::{Color32, Key, Modifiers, RichText};

use super::call_bar::{LEAVE, Look, control};
use super::people::ACTIVE;
use super::shortcuts::spell;
use crate::huddle_share::{ShareAction, Sharing, Source, SourceKind};
use crate::i18n::{t, tf};
use crate::theme::{self, Icon, Palette};

/// The chord that starts and stops sharing, as the shortcut sheet lists
/// it.
pub const TOGGLE: &str = "Cmd+Shift+E";

/// What a click or the chord asks in state `sharing`.
pub fn toggled(sharing: Sharing) -> ShareAction {
    match sharing {
        Sharing::Off => ShareAction::Start,
        Sharing::Choosing => ShareAction::CancelPick,
        Sharing::Starting | Sharing::On => ShareAction::Stop,
    }
}

/// Whether the chord was pressed in this window's input this frame.
fn chord(ui: &egui::Ui) -> bool {
    ui.input_mut(|input| input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::E))
}

/// Draws the Share button for `sharing` in `look`; returns what was
/// asked, by a click, its menu or the chord.
pub fn share_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    sharing: Sharing,
    look: Look,
) -> Option<ShareAction> {
    let shortcut = spell(TOGGLE, cfg!(target_os = "macos"));
    let (icon, label, tip) = match sharing {
        Sharing::Off => (
            Icon::ScreenShare,
            t("Share"),
            tf("Share your screen ({shortcut})", &[("shortcut", &shortcut)]),
        ),
        Sharing::Choosing | Sharing::Starting => (
            Icon::ScreenShare,
            t("Share"),
            tf(
                "Starting to share your screen. Cancel ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
        Sharing::On => (
            Icon::ScreenShareOff,
            t("Sharing"),
            tf(
                "You are sharing your screen: everyone sees it. Stop sharing ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
    };
    let (fill, ink, icon_ink) = match sharing {
        Sharing::On => (ACTIVE, Color32::WHITE, Color32::WHITE),
        Sharing::Choosing | Sharing::Starting => {
            (palette.surface_hover, palette.secondary, palette.secondary)
        }
        Sharing::Off => (palette.surface_hover, palette.text, palette.text),
    };
    let response = control(ui, look, (icon, icon_ink), &label, ink, fill).on_hover_text(tip);
    let mut asked = None;
    response.context_menu(|ui| {
        if ui.button(t("Share something else…")).clicked() {
            asked = Some(ShareAction::ChooseAgain);
            ui.close();
        }
        if sharing == Sharing::On && ui.button(t("Stop sharing")).clicked() {
            asked = Some(ShareAction::Stop);
            ui.close();
        }
    });
    if asked.is_some() {
        return asked;
    }
    (response.clicked() || chord(ui)).then(|| toggled(sharing))
}

/// The row above the buttons while sharing or starting to: what is going
/// on, and Stop sharing (or Cancel). Nothing while off.
pub fn sharing_row(ui: &mut egui::Ui, palette: &Palette, sharing: Sharing) -> Option<ShareAction> {
    let (text, button) = match sharing {
        Sharing::On => (t("You are sharing your screen"), t("Stop sharing")),
        Sharing::Starting => (t("Starting to share your screen…"), t("Cancel")),
        Sharing::Off | Sharing::Choosing => return None,
    };
    let mut asked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let fill = if sharing == Sharing::On {
                LEAVE
            } else {
                palette.surface_hover
            };
            let ink = if sharing == Sharing::On {
                Color32::WHITE
            } else {
                palette.text
            };
            let stop = ui
                .add(
                    egui::Button::new(RichText::new(&*button).font(theme::medium(12.5)).color(ink))
                        .fill(fill)
                        .corner_radius(theme::RADIUS_SMALL + 2)
                        .min_size(egui::Vec2::new(0.0, 24.0)),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if stop.clicked() {
                asked = Some(ShareAction::Stop);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if sharing == Sharing::On {
                    ui.add(Icon::ScreenShare.image(ACTIVE, 14.0));
                } else {
                    ui.add(egui::Spinner::new().size(12.0).color(palette.secondary));
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(&*text)
                            .font(theme::medium(12.0))
                            .color(palette.text),
                    )
                    .wrap(),
                );
            });
        });
    });
    asked
}

/// The screens and windows to choose from, where the system has no
/// dialog of its own: one line each, and Cancel.
pub fn picker(ui: &mut egui::Ui, palette: &Palette, sources: &[Source]) -> Option<ShareAction> {
    let mut asked = None;
    egui::Frame::new()
        .fill(palette.surface_hover)
        .corner_radius(theme::RADIUS)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(t("Choose what to share"))
                            .font(theme::semibold(12.5))
                            .color(palette.text),
                    )
                    .truncate(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, palette, Icon::X, 12.0, &t("Cancel")).clicked() {
                        asked = Some(ShareAction::CancelPick);
                    }
                });
            });
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for source in sources {
                        let icon = match source.kind {
                            SourceKind::Screen => Icon::Monitor,
                            SourceKind::Window => Icon::AppWindow,
                        };
                        let line = ui
                            .add(
                                egui::Button::image_and_text(
                                    icon.image(palette.secondary, 14.0),
                                    RichText::new(&source.name)
                                        .font(theme::regular(12.5))
                                        .color(palette.text),
                                )
                                .frame(false)
                                .truncate(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(&source.name);
                        if line.clicked() {
                            asked = Some(ShareAction::Pick(source.id.clone()));
                        }
                    }
                });
        });
    asked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_button_and_its_chord_toggle_the_share() {
        assert_eq!(toggled(Sharing::Off), ShareAction::Start);
        assert_eq!(toggled(Sharing::Starting), ShareAction::Stop);
        assert_eq!(toggled(Sharing::On), ShareAction::Stop);
        assert_eq!(toggled(Sharing::Choosing), ShareAction::CancelPick);
        // On the sheet, and clashing with no other line of it.
        assert_eq!(
            super::super::shortcuts::keys_of("Share your screen / stop sharing"),
            Some(TOGGLE)
        );
        let taken: Vec<&str> = super::super::shortcuts::GROUPS
            .iter()
            .flat_map(|g| g.shortcuts)
            .flat_map(|s| s.keys.iter().chain(s.also))
            .copied()
            .filter(|k| *k == TOGGLE)
            .collect();
        assert_eq!(taken, [TOGGLE], "one line has the chord");
        assert_eq!(spell(TOGGLE, false), "Ctrl+Shift+E");
        assert_eq!(spell(TOGGLE, true), "⇧⌘E");
    }
}
