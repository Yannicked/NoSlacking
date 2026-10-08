//! The device pickers: a menu for each kind in Settings → Huddles, and
//! the small arrow beside Mute (the microphone and the speaker) and
//! beside Video (the camera) in the call bar and the call window, as
//! calls have them, to switch without leaving the call.
//!
//! Every picker lists the system's default first, then the devices there
//! are, and a remembered device that is not connected as such
//! ([`crate::devices::entries`]). Opening one asks the worker for the
//! devices again, so one plugged in a moment ago shows; choosing one
//! pushes [`devices::Action::Choose`], which the app remembers and the
//! worker uses at once.

use egui::{CornerRadius, RichText, Vec2};

use super::call_bar::Look;
use crate::devices::{self, Chosen, Kind, Listing};
use crate::i18n::t;
use crate::model::Action;
use crate::theme::{self, Icon, Palette};

/// What the pickers show: what is chosen and what there is.
#[derive(Clone, Copy, Debug)]
pub struct Pickers<'a> {
    /// The devices chosen.
    pub chosen: &'a Chosen,
    /// The devices last listed.
    pub lists: &'a devices::State,
}

/// What a picker's name is: "Microphone", "Speaker", "Camera".
pub fn kind_label(kind: Kind) -> String {
    match kind {
        Kind::Microphone => t("Microphone"),
        Kind::Speaker => t("Speaker"),
        Kind::Camera => t("Camera"),
    }
    .into_owned()
}

/// Asks for the devices of `kinds` again when the popup `id` has just
/// opened (it was closed last frame).
fn refresh_on_open(
    ui: &egui::Ui,
    id: egui::Id,
    open: bool,
    kinds: &[Kind],
    actions: &mut Vec<Action>,
) {
    let seen = id.with("was-open");
    let was = ui.data(|d| d.get_temp::<bool>(seen)).unwrap_or(false);
    if open && !was {
        for kind in kinds {
            actions.push(Action::Devices(devices::Action::Refresh(*kind)));
        }
    }
    if open != was {
        ui.data_mut(|d| d.insert_temp(seen, open));
    }
}

/// The lines of a picker for `kind`, as selectable rows; then, while
/// nothing is listed, that it is looking, or why it cannot.
fn lines(
    ui: &mut egui::Ui,
    palette: &Palette,
    kind: Kind,
    pickers: Pickers<'_>,
    actions: &mut Vec<Action>,
) -> bool {
    let listing = pickers.lists.listing(kind);
    let mut chose = false;
    for entry in devices::entries(kind, pickers.chosen.get(kind), listing.devices()) {
        let label = devices::label(kind, &entry);
        // A missing device reads quieter, but not on the selection's
        // fill, where quiet would be unreadable.
        let color = if entry.absent && !entry.selected {
            palette.secondary
        } else {
            palette.text
        };
        let text = RichText::new(&label)
            .font(theme::regular(13.5))
            .color(color);
        let response = ui.add(egui::Button::selectable(entry.selected, text).truncate());
        let response = if entry.absent {
            response.on_hover_text(t(
                "Not connected now: the system default is used until it is back",
            ))
        } else {
            response.on_hover_text(&label)
        };
        if response.clicked() {
            actions.push(Action::Devices(devices::Action::Choose {
                kind,
                choice: entry.choice,
            }));
            chose = true;
        }
    }
    status(ui, palette, listing);
    chose
}

/// While nothing is listed: that it is looking, or why it cannot.
fn status(ui: &mut egui::Ui, palette: &Palette, listing: &Listing) {
    if let Some(failure) = listing.failure() {
        ui.add(
            egui::Label::new(
                RichText::new(failure.sentence())
                    .font(theme::regular(12.0))
                    .color(palette.secondary),
            )
            .wrap(),
        );
    } else if listing.asking && listing.devices().is_none() {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0).color(palette.secondary));
            ui.label(
                RichText::new(t("Looking for devices…"))
                    .font(theme::regular(12.0))
                    .color(palette.secondary),
            );
        });
    }
}

/// The text a picker's button shows: what is in use.
fn selected_label(kind: Kind, pickers: Pickers<'_>) -> String {
    let listing = pickers.lists.listing(kind);
    devices::entries(kind, pickers.chosen.get(kind), listing.devices())
        .into_iter()
        .find(|e| e.selected)
        .map_or_else(
            || devices::default_label(kind),
            |e| devices::label(kind, &e),
        )
}

/// A picker for `kind` in Settings: a menu button showing what is in
/// use, its popup the devices, asked for again each time it opens.
pub fn picker(
    ui: &mut egui::Ui,
    palette: &Palette,
    kind: Kind,
    pickers: Pickers<'_>,
    name: egui::Id,
    actions: &mut Vec<Action>,
) {
    let id = devices::popup_id("settings", kind);
    let label = selected_label(kind, pickers);
    let button = ui
        .add(
            egui::Button::new(
                RichText::new(&label)
                    .font(theme::regular(13.5))
                    .color(palette.text),
            )
            .right_text(Icon::ChevronDown.image(palette.secondary, 12.0))
            .truncate()
            .fill(palette.surface)
            .stroke(egui::Stroke::new(1.0, palette.outline))
            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
            .min_size(Vec2::new(260.0, 30.0)),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .labelled_by(name);
    theme::describe(&button, egui::WidgetType::ComboBox, &label);
    let shown = egui::Popup::menu(&button)
        .id(id)
        .width(button.rect.width())
        .show(|ui| {
            ui.set_min_width(button.rect.width() - 12.0);
            if lines(ui, palette, kind, pickers, actions) {
                ui.close();
            }
        });
    refresh_on_open(ui, id, shown.is_some(), &[kind], actions);
}

/// The small arrow beside Mute (the microphone and the speaker) or
/// Video (the camera) in the call bar and the call window, in `look`:
/// a menu of the devices of `kinds`, each under its name, and a way to
/// the settings. `place` keeps the bar's and the window's apart.
pub fn menu_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    look: Look,
    place: &str,
    kinds: &[Kind],
    pickers: Pickers<'_>,
    actions: &mut Vec<Action>,
) {
    let Some(first) = kinds.first().copied() else {
        return;
    };
    let id = devices::menu_id(place, first);
    let open = egui::Popup::is_id_open(ui.ctx(), id);
    let tip = match first {
        Kind::Camera => t("Choose your camera"),
        Kind::Microphone | Kind::Speaker => t("Choose your microphone and speaker"),
    };
    let fill = if open {
        palette.surface_active
    } else {
        palette.surface_hover
    };
    let image = Icon::ChevronUp.image(palette.secondary, look.icon - 2.0);
    // Narrow: the arrow goes with its button, and the call bar is short
    // of room.
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().button_padding.x = 2.0;
            ui.add(
                egui::Button::image(image)
                    .fill(fill)
                    .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
                    .min_size(Vec2::new((look.height * 0.62).round(), look.height)),
            )
        })
        .inner
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip.as_ref());
    theme::describe(&response, egui::WidgetType::Button, &tip);
    let shown = egui::Popup::menu(&response)
        .id(id)
        .align(egui::RectAlign::TOP_START)
        .align_alternatives(&[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START])
        .gap(4.0)
        .show(|ui| {
            ui.set_min_width(240.0);
            ui.set_max_width(320.0);
            let mut chose = false;
            for (n, kind) in kinds.iter().enumerate() {
                if n > 0 {
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    let icon = match kind {
                        Kind::Microphone => Icon::Mic,
                        Kind::Speaker => Icon::Volume,
                        Kind::Camera => Icon::Video,
                    };
                    ui.add(icon.image(palette.secondary, 13.0));
                    ui.label(
                        RichText::new(kind_label(*kind))
                            .font(theme::semibold(12.5))
                            .color(palette.secondary),
                    );
                });
                chose |= lines(ui, palette, *kind, pickers, actions);
            }
            ui.separator();
            if ui
                .add(egui::Button::new(
                    RichText::new(t("Device settings…"))
                        .font(theme::regular(13.0))
                        .color(palette.text),
                ))
                .clicked()
            {
                actions.push(Action::ShowSettings);
                chose = true;
            }
            if chose {
                ui.close();
            }
        });
    refresh_on_open(ui, id, shown.is_some(), kinds, actions);
}

/// Which kinds the arrow beside Mute and beside Video offer.
pub const MIC_MENU: [Kind; 2] = [Kind::Microphone, Kind::Speaker];
/// The arrow beside Video's.
#[cfg(feature = "huddle-camera")]
pub const CAMERA_MENU: [Kind; 1] = [Kind::Camera];
