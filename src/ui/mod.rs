//! The interface: a workspace rail, the conversation list, the open
//! conversation and, when one is open, its thread.

mod add_emoji;
mod browse;
#[cfg(feature = "huddle-audio")]
mod call_bar;
mod composer;
mod context;
mod conversation;
mod desktop;
mod details;
mod format;
mod hooks;
mod keys;
mod lightbox;
mod login;
mod message;
pub use message::plain_text;
mod overlays;
mod people;
mod rich;
mod rows;
mod search;
mod selection;
mod settings;
mod share;
mod shortcuts;
mod sidebar;
mod thread;
mod viewer;
mod views;

use egui::{Color32, CornerRadius, Rect, Sense, Vec2};

use crate::app::{App, Page};
use crate::theme::{self, Palette};

/// Where [`show`] leaves [`App::channels_with_drafts`] for the sidebar's
/// rows.
pub fn drafts_id() -> egui::Id {
    egui::Id::new("channels-with-drafts")
}

/// Where [`show`] leaves [`App::quick_reactions`] for the message
/// toolbars to read, which are drawn far from the settings.
pub fn quick_reactions_id() -> egui::Id {
    egui::Id::new("quick-reactions")
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let quick = std::sync::Arc::new(app.quick_reactions());
    ui.data_mut(|d| d.insert_temp(quick_reactions_id(), quick));
    // Gathered again only when the drafts or the workspace change.
    let team = app.active_workspace().map(|w| w.info.team_id.as_str());
    let revision = app.drafts.revision();
    let stale = ui.data_mut(|d| {
        let kept = d.get_temp::<std::sync::Arc<std::collections::HashSet<String>>>(drafts_id());
        let seen = d.get_temp_mut_or_default::<(Option<String>, u64)>(drafts_id().with("seen"));
        let stale = kept.is_none() || seen.1 != revision || seen.0.as_deref() != team;
        if stale {
            *seen = (team.map(str::to_owned), revision);
        }
        stale
    });
    if stale {
        let drafts = std::sync::Arc::new(
            team.map(|team| app.channels_with_drafts(team))
                .unwrap_or_default(),
        );
        ui.data_mut(|d| d.insert_temp(drafts_id(), drafts));
    }
    let view_open = app.views.open.is_some();
    ui.data_mut(|d| d.insert_temp(views::open_id(), view_open));
    let saved = std::sync::Arc::new(
        app.active_team()
            .and_then(|team| app.views.team(&team))
            .map(|v| v.saved_keys.clone())
            .unwrap_or_default(),
    );
    ui.data_mut(|d| d.insert_temp(views::saved_id(), saved));
    // First, so Esc leaves a selected message before it closes the thread.
    selection::keys(app, ui.ctx());
    keys::global(app, ui.ctx());
    browse::keys(app, ui.ctx());
    views::keys(app, ui.ctx());
    match app.page {
        Page::SignIn => login::show(app, ui),
        Page::Settings => {
            sidebar::rail(app, ui);
            // The settings have no sidebar: the call bar stays in sight
            // at their foot.
            #[cfg(feature = "huddle-audio")]
            call_bar::panel(
                ui,
                "settings-call-bar",
                &app.palette,
                app.huddles.listening.as_ref(),
                &app.workspaces,
                true,
                &mut app.actions,
            );
            settings::show(app, ui);
        }
        Page::Main => {
            sidebar::rail(app, ui);
            sidebar::show(app, ui);
            if app.thread.is_some() {
                thread::show(app, ui);
            } else if app.convos.details.is_some() && app.views.open.is_none() {
                details::show(app, ui);
            }
            if app.views.open.is_some() {
                views::show(app, ui);
            } else {
                conversation::show(app, ui);
            }
        }
    }
    overlays::show(app, ui.ctx());
    search::show(app, ui.ctx());
    browse::show(app, ui.ctx());
    views::dialog(app, ui.ctx());
    // Before the sweep, so what the pop-outs draw stays parsed.
    app.show_popouts(&ui.ctx().clone());
    rich::end_frame();
}

/// A pop-out window's contents: its conversation, made the active one for
/// the while (see [`crate::app::Popout`]).
pub fn popout(app: &mut App, ui: &mut egui::Ui) {
    conversation::show(app, ui);
}

/// The picture at `uri` in a box of exactly `size`, before it has loaded
/// as after, so whatever is below it stays put when it arrives. The box is
/// worked out from the size Slack gave (or a fixed one when it gave none);
/// the picture is drawn in it as [`contain`] says, in case it turns out
/// another shape.
pub fn picture(
    ui: &mut egui::Ui,
    uri: String,
    size: Vec2,
    radius: CornerRadius,
    sense: Sense,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let image = egui::Image::new(uri)
            .corner_radius(radius)
            .show_loading_spinner(false);
        let poll = image.load_for_size(ui.ctx(), size);
        if let Ok(egui::load::TexturePoll::Pending { .. }) = poll {
            // A faint box with a small spinner: egui's own fills the whole
            // box, which is loud for a large picture.
            ui.painter()
                .rect_filled(rect, radius, ui.visuals().faint_bg_color);
            let side = rect.width().min(rect.height()).min(24.0);
            egui::Spinner::new()
                .size(side)
                .paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(side)));
        }
        let loaded = poll.ok().and_then(|poll| poll.size());
        image.paint_at(ui, contain(rect, loaded));
    }
    response
}

/// Where a picture `loaded` big (unknown while it loads) is drawn in the
/// box `rect`: shrunk to fit it, never grown past its own size, and
/// centred. A picture the shape the box was made for fills it exactly.
pub fn contain(rect: Rect, loaded: Option<Vec2>) -> Rect {
    let Some(loaded) = loaded.filter(|s| s.x > 0.0 && s.y > 0.0) else {
        return rect;
    };
    let scale = (rect.width() / loaded.x)
        .min(rect.height() / loaded.y)
        .min(1.0);
    Rect::from_center_size(rect.center(), loaded * scale)
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
    theme::describe(&response, egui::WidgetType::Image, name);
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

/// A moment given in seconds since the epoch, as a list shows it: "Today
/// at 14:03", "Monday, March 3 at 09:00".
pub fn moment_label(seconds: i64) -> String {
    let ts = crate::model::Ts::new(format!("{seconds}.000000"));
    crate::i18n::tf(
        "{date} at {time}",
        &[("date", &day_label(&ts)), ("time", &short_time(&ts))],
    )
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

    fn boxed(size: Vec2) -> Rect {
        Rect::from_min_size(egui::pos2(10.0, 20.0), size)
    }

    #[test]
    fn a_picture_the_shape_slack_gave_fills_its_box() {
        let rect = boxed(Vec2::new(400.0, 225.0));
        assert_eq!(contain(rect, None), rect, "held open while it loads");
        assert_eq!(contain(rect, Some(Vec2::new(1280.0, 720.0))), rect);
        assert_eq!(contain(rect, Some(Vec2::new(400.0, 225.0))), rect);
        assert_eq!(contain(rect, Some(Vec2::ZERO)), rect, "a broken size");
    }

    #[test]
    fn a_picture_of_another_shape_stays_inside_its_box() {
        let rect = boxed(Vec2::new(400.0, 225.0));
        // Taller than the box: as tall, narrower, centred.
        let tall = contain(rect, Some(Vec2::new(300.0, 450.0)));
        assert_eq!(tall.size(), Vec2::new(150.0, 225.0));
        assert_eq!(tall.center(), rect.center());
        // Smaller than the box: its own size, never blown up.
        let small = contain(rect, Some(Vec2::new(64.0, 64.0)));
        assert_eq!(small.size(), Vec2::splat(64.0));
        assert!(rect.contains_rect(small));
    }

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
