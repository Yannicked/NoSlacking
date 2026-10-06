//! The small player on voice clip and sound file cards: a play/pause
//! button, how far it got, and a click on the waveform or bar to seek.
//! What plays and how is [`crate::audio`]'s; this only draws it and asks.

use egui::{CornerRadius, Rect, Vec2};

use crate::audio::{Now, Phase, Request, Track};
use crate::i18n::tf;
use crate::model::{Action, File};
use crate::theme::{Icon, Palette};

/// What plays now, if it is `file` in `team`.
pub(super) fn now(ui: &egui::Ui, team: &str, file: &File) -> Option<Now> {
    crate::audio::now_for(ui.ctx(), team, &file.id)
}

/// The round button filling `rect`: play, pause, a spinner while the
/// sound loads, or a warning when it could not play here.
pub(super) fn button(
    ui: &egui::Ui,
    palette: &Palette,
    rect: Rect,
    hovered: bool,
    now: Option<&Now>,
) {
    let fill = if hovered {
        palette.accent
    } else {
        palette.accent.gamma_multiply(0.85)
    };
    let painter = ui.painter();
    painter.circle_filled(rect.center(), rect.width() / 2.0, fill);
    let center = rect.center();
    match now.map(|n| n.phase) {
        Some(Phase::Loading) => {
            egui::Spinner::new()
                .size(18.0)
                .color(palette.on_accent)
                .paint_at(ui, Rect::from_center_size(center, Vec2::splat(18.0)));
        }
        Some(Phase::Playing) => {
            for dx in [-4.0, 4.0] {
                painter.rect_filled(
                    Rect::from_center_size(center + Vec2::new(dx, 0.0), Vec2::new(4.0, 14.0)),
                    CornerRadius::same(1),
                    palette.on_accent,
                );
            }
        }
        Some(Phase::Failed(_)) => {
            Icon::CircleAlert
                .image(palette.on_accent, 18.0)
                .paint_at(ui, Rect::from_center_size(center, Vec2::splat(18.0)));
        }
        Some(Phase::Paused) | None => {
            Icon::Play.image(palette.on_accent, 18.0).paint_at(
                ui,
                Rect::from_center_size(center + Vec2::new(1.5, 0.0), Vec2::splat(18.0)),
            );
        }
    }
}

/// What the button says it does, for the tooltip and screen readers.
pub(super) fn tip(file: &File, now: Option<&Now>) -> String {
    let name = [("name", file.name.as_str())];
    match now.map(|n| n.phase) {
        Some(Phase::Playing) => tf("Pause {name}", &name),
        Some(Phase::Loading) => tf("Stop loading {name}", &name),
        Some(Phase::Failed(why)) => why.message().unwrap_or_else(|| tf("Play {name}", &name)),
        Some(Phase::Paused) | None => tf("Play {name}", &name),
    }
}

/// The time to show: how far in and how long while it is in hand, else
/// how long it lasts, if known.
pub(super) fn time(now: Option<&Now>, duration_ms: Option<u64>) -> Option<String> {
    match now {
        Some(now) if !matches!(now.phase, Phase::Failed(_)) => {
            Some(crate::audio::time_text(now.position, now.duration))
        }
        _ => duration_ms.map(crate::model::duration_text),
    }
}

/// How far along the bar to draw as played, 0 to 1.
pub(super) fn progress(now: Option<&Now>) -> Option<f32> {
    now.filter(|n| !matches!(n.phase, Phase::Failed(_)))
        .map(Now::fraction)
}

/// Asks to play or pause `track`, or to seek when the click landed in
/// `seek` (the waveform or bar), at that point along it.
pub(super) fn clicked(
    response: &egui::Response,
    track: Track,
    seek: Rect,
    actions: &mut Vec<Action>,
) {
    let at = response
        .interact_pointer_pos()
        .filter(|pos| seek.contains(*pos));
    let request = match at {
        Some(pos) => Request::Seek {
            track,
            fraction: crate::audio::fraction_at(pos.x, seek.left(), seek.right()),
        },
        None => Request::Toggle(track),
    };
    actions.push(Action::Audio(request));
}

/// A thin bar from `left` to `right` on `y`, played up to `fraction`.
pub(super) fn bar(ui: &egui::Ui, palette: &Palette, left: f32, right: f32, y: f32, fraction: f32) {
    let rail = Rect::from_min_max(
        egui::pos2(left, y - 2.0),
        egui::pos2(right.max(left), y + 2.0),
    );
    let painter = ui.painter();
    painter.rect_filled(rail, CornerRadius::same(2), palette.outline);
    let played = Rect::from_min_max(
        rail.min,
        egui::pos2(left + (right - left).max(0.0) * fraction, rail.max.y),
    );
    painter.rect_filled(played, CornerRadius::same(2), palette.accent);
    painter.circle_filled(egui::pos2(played.right(), y), 5.0, palette.accent);
}
