//! What the interface draws about people beyond their names: whether
//! they are around.

use egui::{Color32, Rect, Stroke, Vec2};

use crate::i18n::t;
use crate::people::Presence;
use crate::theme::Palette;

/// Slack's "active" green. The same in both palettes, as in Slack: it
/// means one thing everywhere.
pub const ACTIVE: Color32 = Color32::from_rgb(0x2b, 0xac, 0x76);

/// Paints a presence dot on the lower right corner of an avatar at
/// `avatar`: filled green when active, a hollow ring when away, nothing
/// when unknown. `behind` is the colour around the avatar, which rings
/// the dot so it stands apart from the picture.
pub fn dot(
    painter: &egui::Painter,
    palette: &Palette,
    avatar: Rect,
    presence: Option<Presence>,
    behind: Color32,
) {
    let Some(presence) = presence else {
        return;
    };
    let radius = (avatar.width() * 0.2).clamp(3.5, 7.0);
    let center = avatar.right_bottom() - Vec2::splat(radius * 0.6);
    painter.circle_filled(center, radius + 1.5, behind);
    match presence {
        Presence::Active => {
            painter.circle_filled(center, radius, ACTIVE);
        }
        Presence::Away => {
            painter.circle_stroke(center, radius - 0.75, Stroke::new(1.5, palette.dim));
        }
    }
}

/// The word for a presence, for tooltips and screen readers.
pub fn word(presence: Presence) -> std::borrow::Cow<'static, str> {
    match presence {
        Presence::Active => t("Active"),
        Presence::Away => t("Away"),
    }
}
