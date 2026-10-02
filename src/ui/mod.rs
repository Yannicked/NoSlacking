//! The interface: a workspace rail, the conversation list, the open
//! conversation and, when one is open, its thread.

mod composer;
mod conversation;
mod keys;
mod login;
mod message;
mod overlays;
mod rich;
mod settings;
mod sidebar;
mod thread;

use egui::{Color32, CornerRadius, Rect, Sense, Vec2};

use crate::app::{App, Page};
use crate::theme::{self, Palette};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    keys::global(app, ui.ctx());
    match app.page {
        Page::SignIn => login::show(app, ui),
        Page::Settings => {
            sidebar::rail(app, ui);
            settings::show(app, ui);
        }
        Page::Main => {
            sidebar::rail(app, ui);
            sidebar::show(app, ui);
            if app.thread.is_some() {
                thread::show(app, ui);
            }
            conversation::show(app, ui);
        }
    }
    overlays::show(app, ui.ctx());
}

/// A rounded square picture, or coloured initials until there is one.
pub fn avatar(
    ui: &mut egui::Ui,
    url: Option<&str>,
    name: &str,
    seed: &str,
    size: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    paint_avatar(ui, rect, url, name, seed);
    response
}

pub fn paint_avatar(ui: &egui::Ui, rect: Rect, url: Option<&str>, name: &str, seed: &str) {
    let radius = CornerRadius::same((rect.width() * 0.22).round() as u8);
    let placeholder = || {
        ui.painter()
            .rect_filled(rect, radius, theme::identity_color(seed));
        let initial: String = name
            .chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().collect())
            .unwrap_or_else(|| "?".to_owned());
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial,
            theme::semibold(rect.height() * 0.48),
            Color32::WHITE,
        );
    };
    match url {
        Some(url) if !url.is_empty() => {
            let image = egui::Image::new(url.to_owned()).corner_radius(radius);
            match image.load_for_size(ui.ctx(), rect.size()) {
                Ok(egui::load::TexturePoll::Ready { .. }) => image.paint_at(ui, rect),
                _ => placeholder(),
            }
        }
        _ => placeholder(),
    }
}

/// A pill with a count, for unread mentions.
pub fn badge(ui: &mut egui::Ui, palette: &Palette, count: u32) {
    let text = if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    };
    let galley = ui
        .painter()
        .layout_no_wrap(text, theme::bold(11.0), Color32::WHITE);
    let size = Vec2::new((galley.size().x + 12.0).max(20.0), 18.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(9), palette.badge);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// A quiet label above a group of rows or fields.
pub fn section_label(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(theme::semibold(12.0))
            .color(palette.dim),
    );
}

/// The local time of day, "14:03", whatever the date: the day separators
/// above the messages already say which day it is.
pub fn short_time(ts: &crate::model::Ts) -> String {
    ts.zoned()
        .map(|z| z.strftime("%H:%M").to_string())
        .unwrap_or_default()
}

/// The heading of a day in a conversation.
pub fn day_label(ts: &crate::model::Ts) -> String {
    use crate::i18n::t;
    let Some(zoned) = ts.zoned() else {
        return String::new();
    };
    let today = jiff::Zoned::now().date();
    let date = zoned.date();
    if date == today {
        t("Today").into_owned()
    } else if today.yesterday().ok() == Some(date) {
        t("Yesterday").into_owned()
    } else {
        long_date(&t, date, date.year() != today.year())
    }
}

/// The full moment of a message, for the tooltip over its time:
/// "Monday, March 3, 2025 at 14:03:12".
pub fn full_time(ts: &crate::model::Ts) -> Option<String> {
    let zoned = ts.zoned()?;
    let date = long_date(&crate::i18n::t, zoned.date(), true);
    let time = zoned.strftime("%H:%M:%S").to_string();
    Some(crate::i18n::tf(
        "{date} at {time}",
        &[("date", &date), ("time", &time)],
    ))
}

/// A translator: [`crate::i18n::t`], or in tests a fixed language.
type Translate<'a> = &'a dyn Fn(&'static str) -> std::borrow::Cow<'static, str>;

/// "Monday, March 3", with ", 2025" when asked. The names and the order
/// come from the catalog, since strftime only knows English.
fn long_date(t: Translate<'_>, date: jiff::civil::Date, with_year: bool) -> String {
    let pattern = if with_year {
        t("{weekday}, {month} {day}, {year}")
    } else {
        t("{weekday}, {month} {day}")
    };
    crate::i18n::fill(
        &pattern,
        &[
            ("weekday", &weekday_name(t, date.weekday())),
            ("month", &month_name(t, date.month())),
            ("day", &date.day().to_string()),
            ("year", &date.year().to_string()),
        ],
    )
}

fn weekday_name(t: Translate<'_>, day: jiff::civil::Weekday) -> String {
    use jiff::civil::Weekday;
    match day {
        Weekday::Monday => t("Monday"),
        Weekday::Tuesday => t("Tuesday"),
        Weekday::Wednesday => t("Wednesday"),
        Weekday::Thursday => t("Thursday"),
        Weekday::Friday => t("Friday"),
        Weekday::Saturday => t("Saturday"),
        Weekday::Sunday => t("Sunday"),
    }
    .into_owned()
}

fn month_name(t: Translate<'_>, month: i8) -> String {
    match month {
        1 => t("January"),
        2 => t("February"),
        3 => t("March"),
        4 => t("April"),
        5 => t("May"),
        6 => t("June"),
        7 => t("July"),
        8 => t("August"),
        9 => t("September"),
        10 => t("October"),
        11 => t("November"),
        _ => t("December"),
    }
    .into_owned()
}

/// "5 minutes ago" for thread summaries.
pub fn relative(ts: &crate::model::Ts) -> String {
    use crate::i18n::{t, tn};
    let Some(seconds) = ts.seconds() else {
        return String::new();
    };
    let now = jiff::Timestamp::now().as_second();
    let ago = (now - seconds).max(0);
    let minutes = (ago / 60) as u32;
    let hours = (ago / 3600) as u32;
    let days = (ago / 86_400) as u32;
    if ago < 60 {
        t("just now").into_owned()
    } else if minutes < 60 {
        tn("{count} minute ago", "{count} minutes ago", minutes)
    } else if hours < 24 {
        tn("{count} hour ago", "{count} hours ago", hours)
    } else {
        tn("{count} day ago", "{count} days ago", days)
    }
}

/// A human file size.
pub fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// The URI an image of `team` loads by: public URLs as they are, files
/// through the authenticated loader.
pub fn image_uri(team: &str, url: &str) -> String {
    if crate::slack::client::is_slack_file_url(url) {
        crate::images::authed(team, url)
    } else {
        url.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_follow_the_language() {
        let date = jiff::civil::date(2025, 3, 3);
        let english = |s: &'static str| std::borrow::Cow::Borrowed(s);
        assert_eq!(long_date(&english, date, false), "Monday, March 3");
        assert_eq!(long_date(&english, date, true), "Monday, March 3, 2025");
        let dutch = |s: &'static str| fastframe_i18n::gettext(crate::i18n::Locale::Dutch, s);
        assert_eq!(long_date(&dutch, date, false), "maandag 3 maart");
        assert_eq!(long_date(&dutch, date, true), "maandag 3 maart 2025");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(file_size(512), "512 B");
        assert_eq!(file_size(48_213), "47.1 KB");
        assert_eq!(file_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn files_need_the_token_and_avatars_do_not() {
        assert_eq!(
            image_uri("T1", "https://files.slack.com/files-pri/a.png"),
            "nsauth:T1:https://files.slack.com/files-pri/a.png"
        );
        assert_eq!(
            image_uri("T1", "https://avatars.slack-edge.com/a.png"),
            "https://avatars.slack-edge.com/a.png"
        );
        assert_eq!(
            image_uri("T1", "https://evil.example/x.png?files.slack.com"),
            "https://evil.example/x.png?files.slack.com"
        );
        assert_eq!(image_uri("T1", "bytes://a.png"), "bytes://a.png");
    }
}
