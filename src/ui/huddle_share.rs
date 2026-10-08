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

use egui::{Color32, RichText};

use super::call_bar::{LEAVE, Lit, Look, Toggle, toggle_control};
use super::people::ACTIVE;
use super::shortcuts::SHARE;
use crate::huddle_share::{ShareAction, Sharing, Source, SourceKind};
use crate::i18n::{t, tf};
use crate::theme::{self, Icon, Palette};

/// What a click or the chord asks in state `sharing`.
pub fn toggled(sharing: Sharing) -> ShareAction {
    match sharing {
        Sharing::Off => ShareAction::Start,
        Sharing::Choosing => ShareAction::CancelPick,
        Sharing::Starting | Sharing::On => ShareAction::Stop,
    }
}

/// Draws the Share button for `sharing` in `look`; returns what was
/// asked, by a click, its menu or the chord.
pub fn share_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    sharing: Sharing,
    look: Look,
) -> Option<ShareAction> {
    let shortcut = SHARE.spelled();
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
    let lit = match sharing {
        Sharing::On => Lit::On,
        Sharing::Choosing | Sharing::Starting => Lit::Pending,
        Sharing::Off => Lit::Off { red: false },
    };
    let toggle = Toggle {
        icon,
        label: label.into_owned(),
        tip,
        lit,
        chord: SHARE,
    };
    let (response, pressed) = toggle_control(ui, palette, look, toggle);
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
    pressed.then(|| toggled(sharing))
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
                            // The helper lists cameras only when asked
                            // for them, never as something to share.
                            SourceKind::Camera => Icon::Video,
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
            Some(SHARE.text)
        );
        let taken: Vec<&str> = super::super::shortcuts::GROUPS
            .iter()
            .flat_map(|g| g.shortcuts)
            .flat_map(|s| s.keys.iter().chain(s.also))
            .copied()
            .filter(|k| *k == SHARE.text)
            .collect();
        assert_eq!(taken, [SHARE.text], "one line has the chord");
        assert_eq!(
            super::super::shortcuts::spell(SHARE.text, false),
            "Ctrl+Shift+E"
        );
        assert_eq!(super::super::shortcuts::spell(SHARE.text, true), "⇧⌘E");
    }
}
