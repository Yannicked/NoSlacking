//! A message's files: pictures and videos inline (or behind a
//! placeholder until asked for), and a card for anything else: the first
//! lines of a text file, a voice clip's waveform, or a name and size.
//!
//! Every card is drawn into a box of a height known before it is drawn
//! (see [`card_height`]), so a list of messages never jumps as they come
//! into view.

use egui::{CornerRadius, Rect, RichText, Sense, Stroke, Vec2};

use super::{PLACEHOLDER, Row, player};
use crate::i18n::{t, tf, tn};
use crate::model::{Action, File, Media, Message, More, TextPreview};
use crate::theme::{self, Icon, Palette};

/// The space inside a card, around what it shows.
const PAD: f32 = 10.0;
/// The icon or play button at a card's left, and the height of its first
/// row.
const ICON: f32 = 36.0;
/// A card with only its first row: an icon, a name and a size.
pub(super) const CARD: f32 = PAD * 2.0 + ICON;
/// How wide a card is at most, in a wide column.
const CARD_WIDTH: f32 = 360.0;
/// How wide a text file's card is at most: code wants more room.
const TEXT_WIDTH: f32 = 520.0;
/// How many of a text preview's lines are shown at most.
pub(super) const PREVIEW_LINES: usize = 8;
/// One line of a text preview.
const LINE: f32 = 16.0;
/// The space inside a text preview's box.
const BOX_PAD: f32 = 6.0;
/// The line under a preview saying how much more there is.
const MORE: f32 = 20.0;
/// The line under a voice clip with the start of its transcript.
const TRANSCRIPT: f32 = 20.0;
/// A waveform bar's width and the step from one bar to the next.
const BAR: f32 = 3.0;
const BAR_STEP: f32 = 5.0;
/// A square icon button, as [`theme::icon_button`] draws one at 16.
const BUTTON: f32 = 28.0;

pub(super) fn file_view(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    file: &File,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let team = &row.workspace.info.team_id;
    // Deleted, or being deleted: Slack's own words in its place.
    if file.deleted || !row.workspace.shows_file(&file.id) {
        ui.label(
            RichText::new(t("This file was deleted."))
                .font(theme::regular(13.0))
                .italics()
                .color(palette.dim),
        );
        return;
    }
    let deletable = file.deletable_by(&row.workspace.info.user_id);
    if file.is_image()
        && let Some(thumb) = &file.thumb
    {
        let size = thumb_size(file, ui.available_width());
        let uri = crate::ui::image_uri(team, thumb);
        if !shows(ui, row, &uri) {
            placeholder(ui, row, &uri, &file.name);
            return;
        }
        let response = crate::ui::picture(
            ui,
            uri.clone(),
            size,
            CornerRadius::same(theme::RADIUS),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::ZoomIn)
        .on_hover_text(&file.name);
        // In the thread panel the viewer steps through the thread's
        // pictures; a parent is its own thread.
        let thread = row.in_thread.then(|| {
            message
                .thread_ts
                .clone()
                .unwrap_or_else(|| message.ts.clone())
        });
        if response.hovered() {
            crate::ui::context::hover(
                ui,
                crate::ui::context::Target::Image {
                    channel: row.channel.to_owned(),
                    thread: thread.clone(),
                    ts: message.ts.clone(),
                    file: file.id.clone(),
                    name: file.name.clone(),
                    download: file
                        .url_private
                        .clone()
                        .or_else(|| file.download_url.clone()),
                    permalink: file.permalink.clone(),
                    copy: file
                        .url_private
                        .iter()
                        .map(|full| crate::ui::image_uri(team, full))
                        .chain([uri.clone()])
                        .collect(),
                    deletable,
                },
            );
        }
        if response.clicked() {
            actions.push(Action::ViewImage {
                channel: row.channel.to_owned(),
                thread,
                ts: message.ts.clone(),
                file: file.id.clone(),
            });
        }
        return;
    }
    let poster = file
        .poster
        .as_deref()
        .map(|poster| crate::ui::image_uri(team, poster));
    if let Some(uri) = &poster
        && !shows(ui, row, uri)
    {
        placeholder(ui, row, uri, &file.name);
    } else if let Some(uri) = poster {
        still(ui, file, uri, deletable, actions);
    }
    let rect = if is_voice(file) {
        voice_card(ui, palette, team, file, actions)
    } else if file.media().is_some() {
        media_card(ui, palette, team, file, actions)
    } else if let Some(preview) = &file.preview {
        text_card(ui, palette, file, preview, actions)
    } else {
        file_card(ui, palette, file, actions)
    };
    if ui.rect_contains_pointer(rect) {
        hover_file(ui, file, deletable);
    }
}

/// A video's frame or a document's first page, with a play button and
/// the length on a video. Clicking it plays the video, opens the PDF (or
/// Slack's PDF of an Office document), or else downloads the file.
fn still(ui: &mut egui::Ui, file: &File, uri: String, deletable: bool, actions: &mut Vec<Action>) {
    let size = poster_size(file, ui.available_width());
    let response = crate::ui::picture(
        ui,
        uri,
        size,
        CornerRadius::same(theme::RADIUS),
        Sense::click(),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand);
    let (tip, action) = still_action(file);
    if file.media() == Some(Media::Video) {
        play_badge(ui, response.rect.center(), response.hovered());
        if let Some(ms) = file.duration_ms {
            duration_badge(ui, response.rect, &crate::model::duration_text(ms));
        }
    }
    if response.hovered() {
        hover_file(ui, file, deletable);
    }
    theme::describe(&response, egui::WidgetType::Button, &tip);
    if response.on_hover_text(&tip).clicked()
        && let Some(action) = action
    {
        actions.push(action);
    }
}

/// What clicking a file's still says and does: play what plays, open
/// what opens as a PDF, download the rest. The system only opens a
/// player's files and PDFs from here (see `backend::files::open_file`),
/// so a spreadsheet's still must not try to open the spreadsheet.
fn still_action(file: &File) -> (String, Option<Action>) {
    let open = |(url, name)| Action::OpenFile { url, name };
    if file.media().is_some() {
        (
            tf("Play {name}", &[("name", &file.name)]),
            file.player().map(open),
        )
    } else if let Some(pdf) = file.as_pdf() {
        (
            tf("Open {name} as a PDF", &[("name", &file.name)]),
            Some(open(pdf)),
        )
    } else if file.is_pdf() {
        (
            tf("Open {name}", &[("name", &file.name)]),
            file.url_private
                .clone()
                .or_else(|| file.download_url.clone())
                .map(|url| open((url, file.name.clone()))),
        )
    } else {
        (
            tf("Download {name}", &[("name", &file.name)]),
            download(file),
        )
    }
}

/// Saving `file` in the downloads folder.
fn download(file: &File) -> Option<Action> {
    file.download_url
        .clone()
        .or_else(|| file.url_private.clone())
        .map(|url| Action::Download {
            url,
            name: file.name.clone(),
        })
}

/// Whether `file` is a voice clip drawn as a waveform: one recorded in
/// Slack, or any sound Slack measured the loudness of.
fn is_voice(file: &File) -> bool {
    file.media() == Some(Media::Audio) && (file.voice || !file.wave.is_empty())
}

/// How tall `file`'s card is, under its still if it has one: exactly as
/// it is drawn, known before it is drawn.
pub(super) fn card_height(file: &File) -> f32 {
    if is_voice(file) {
        voice_height(file)
    } else if file.media().is_some() {
        CARD
    } else if let Some(preview) = &file.preview {
        text_height(preview)
    } else {
        CARD
    }
}

/// A text file's card height: its first row, the preview's lines, and the
/// line saying how much more there is.
fn text_height(preview: &TextPreview) -> f32 {
    let (lines, more) = preview.shown(PREVIEW_LINES);
    let more = if more == More::Nothing { 0.0 } else { MORE };
    PAD + ICON + PAD + preview_box_height(lines.len()) + more + PAD
}

/// The box holding `lines` lines of a preview.
fn preview_box_height(lines: usize) -> f32 {
    BOX_PAD * 2.0 + lines.max(1) as f32 * LINE
}

/// A voice clip's card height: the waveform's row, and the transcript's
/// start when Slack has one.
fn voice_height(file: &File) -> f32 {
    CARD + if file.transcript.is_some() {
        TRANSCRIPT
    } else {
        0.0
    }
}

/// A card's box: `height` tall, as wide as `max` allows in this column,
/// clickable as a whole. Buttons drawn on it later take their own clicks.
fn card_box(ui: &mut egui::Ui, palette: &Palette, max: f32, height: f32) -> (Rect, egui::Response) {
    let width = ui.available_width().min(max).max(160.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let fill = if response.hovered() {
        palette.surface_hover
    } else {
        palette.surface
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(theme::RADIUS),
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    theme::focus_ring(ui, &response, palette, theme::RADIUS);
    (rect, response)
}

/// `text` on one line, cut with an ellipsis where it would pass `width`.
fn one_line(
    ui: &egui::Ui,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    ui.painter().layout_job(job)
}

/// A card's first row inside `row`: the file icon, the name over its size
/// and kind, ending at `right`.
fn card_title(ui: &egui::Ui, palette: &Palette, file: &File, row: Rect, right: f32) {
    let icon = Rect::from_min_size(row.min, Vec2::splat(ICON));
    ui.painter().rect_filled(
        icon,
        CornerRadius::same(6),
        palette.accent.gamma_multiply(0.2),
    );
    let glyph = if file.preview.is_some() {
        Icon::Code
    } else {
        Icon::FileText
    };
    glyph
        .image(palette.accent, 20.0)
        .paint_at(ui, Rect::from_center_size(icon.center(), Vec2::splat(20.0)));
    name_and_detail(ui, palette, file, icon.right() + 10.0, right, row.top());
}

/// The file's name over its size and kind, from `left` to `right`.
fn name_and_detail(ui: &egui::Ui, palette: &Palette, file: &File, left: f32, right: f32, top: f32) {
    let width = right - left;
    let name = one_line(ui, &file.name, theme::semibold(14.0), palette.text, width);
    let detail = one_line(
        ui,
        &file_detail(file),
        theme::regular(12.0),
        palette.secondary,
        width,
    );
    let name_top = top + (ICON - name.size().y - detail.size().y - 1.0) / 2.0;
    let detail_top = name_top + name.size().y + 1.0;
    ui.painter()
        .galley(egui::pos2(left, name_top), name, palette.text);
    ui.painter()
        .galley(egui::pos2(left, detail_top), detail, palette.secondary);
}

/// An icon button at `rect`, drawn over a card.
fn card_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    rect: Rect,
    icon: Icon,
    tip: &str,
) -> egui::Response {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    theme::icon_button(&mut child, palette, icon, 16.0, tip)
}

/// A small labelled button ending at `right`, centred on `y`, drawn over a
/// card: for an action an icon would not say clearly.
fn text_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    right: f32,
    y: f32,
    label: &str,
    id: egui::Id,
) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::medium(12.0), palette.text);
    let size = galley.size() + Vec2::new(16.0, 8.0);
    let rect = Rect::from_min_size(egui::pos2(right - size.x, y - size.y / 2.0), size);
    let response = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = if response.hovered() {
        palette.surface_active
    } else {
        palette.window
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS_SMALL),
        fill,
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, palette.text);
    theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
    theme::describe(&response, egui::WidgetType::Button, label);
    response
}

/// Any other file: its name, size and kind; clicking it downloads it.
/// An Office document Slack made a PDF of also offers to open that.
fn file_card(ui: &mut egui::Ui, palette: &Palette, file: &File, actions: &mut Vec<Action>) -> Rect {
    let (rect, response) = card_box(ui, palette, CARD_WIDTH, CARD);
    let row = Rect::from_min_size(
        rect.min + Vec2::splat(PAD),
        Vec2::new(rect.width() - PAD * 2.0, ICON),
    );
    let mut right = view_button(ui, palette, file, row.right(), row.center().y, actions);
    if let Some(pdf) = file.as_pdf() {
        let label = t("Open as PDF");
        let button = text_button(
            ui,
            palette,
            right,
            row.center().y,
            &label,
            egui::Id::new(("open-as-pdf", &file.id)),
        );
        right = button.rect.left() - 8.0;
        if button
            .on_hover_text(tf("Open a PDF of {name}", &[("name", &file.name)]))
            .clicked()
        {
            actions.push(Action::OpenFile {
                url: pdf.0,
                name: pdf.1,
            });
        }
    }
    card_title(ui, palette, file, row, right);
    download_on_click(file, response, actions);
    rect
}

/// Opening `file` in the app's own viewer, when it opens there.
fn view(file: &File) -> Option<Action> {
    let kind = crate::viewer::kind(
        &file.name,
        &file.filetype,
        &file.mimetype,
        file.preview.is_some(),
    )?;
    let url = file
        .url_private
        .clone()
        .or_else(|| file.download_url.clone())?;
    Some(Action::ViewFile {
        url,
        name: file.name.clone(),
        filetype: file.filetype.clone(),
        kind,
        size: file.size,
    })
}

/// A "View" button ending at `right` on a card's first row, for a file
/// the viewer opens. Returns where whatever is left of it must end.
fn view_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    file: &File,
    right: f32,
    y: f32,
    actions: &mut Vec<Action>,
) -> f32 {
    let Some(action) = view(file) else {
        return right;
    };
    let button = text_button(
        ui,
        palette,
        right,
        y,
        &t("View"),
        egui::Id::new(("view-file", &file.id)),
    );
    let left = button.rect.left() - 8.0;
    if button
        .on_hover_text(tf("Show {name} here", &[("name", &file.name)]))
        .clicked()
    {
        actions.push(action);
    }
    left
}

/// Makes a card's own click save the file, as a file card always has.
fn download_on_click(file: &File, response: egui::Response, actions: &mut Vec<Action>) {
    theme::describe(
        &response,
        egui::WidgetType::Button,
        &tf("Download {name}", &[("name", &file.name)]),
    );
    if response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(t("Download"))
        .clicked()
        && let Some(action) = download(file)
    {
        actions.push(action);
    }
}

/// A snippet or text file: its first lines as Slack previews them, in
/// monospace and coloured by its kind, and how many more there are. Only
/// Slack's preview is shown: the file itself is never fetched for this.
fn text_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    file: &File,
    preview: &TextPreview,
    actions: &mut Vec<Action>,
) -> Rect {
    let (lines, more) = preview.shown(PREVIEW_LINES);
    let (rect, response) = card_box(ui, palette, TEXT_WIDTH, text_height(preview));
    let row = Rect::from_min_size(
        rect.min + Vec2::splat(PAD),
        Vec2::new(rect.width() - PAD * 2.0, ICON),
    );
    let button = Rect::from_center_size(
        egui::pos2(row.right() - BUTTON / 2.0, row.center().y),
        Vec2::splat(BUTTON),
    );
    let right = view_button(
        ui,
        palette,
        file,
        button.left() - 8.0,
        row.center().y,
        actions,
    );
    card_title(ui, palette, file, row, right);
    let code = Rect::from_min_size(
        egui::pos2(row.left(), row.bottom() + PAD),
        Vec2::new(row.width(), preview_box_height(lines.len())),
    );
    ui.painter().rect_filled(
        code,
        CornerRadius::same(theme::RADIUS_SMALL),
        palette.window,
    );
    let job = preview_job(palette, file, &lines.join("\n"));
    // Long lines are cut at the box's edge, never wrapped: the box keeps
    // the height it was given.
    let galley = ui.painter().layout_job(job);
    ui.painter().with_clip_rect(code.shrink(2.0)).galley(
        code.min + Vec2::splat(BOX_PAD),
        galley,
        palette.text,
    );
    let more = match more {
        More::Nothing => None,
        More::Lines(count) => Some(tn("{count} more line", "{count} more lines", count)),
        More::Unknown => Some(t("More in the file").into_owned()),
    };
    if let Some(more) = more {
        let galley = one_line(
            ui,
            &more,
            theme::regular(12.0),
            palette.secondary,
            code.width(),
        );
        let y = code.bottom() + (MORE - galley.size().y) / 2.0;
        ui.painter()
            .galley(egui::pos2(code.left(), y), galley, palette.secondary);
    }
    if card_button(ui, palette, button, Icon::Download, &t("Download")).clicked()
        && let Some(action) = download(file)
    {
        actions.push(action);
    }
    download_on_click(file, response, actions);
    rect
}

/// A preview's text laid out in monospace, one line every [`LINE`], and
/// coloured like a code block when its kind names a language.
fn preview_job(palette: &Palette, file: &File, text: &str) -> egui::text::LayoutJob {
    let font = theme::mono(12.0);
    let format = |color| {
        let mut format = egui::TextFormat::simple(font.clone(), color);
        format.line_height = Some(LINE);
        format
    };
    #[cfg(feature = "highlight")]
    if let Some(language) = preview_language(file) {
        let mut job = egui::text::LayoutJob::default();
        for (range, kind) in crate::highlight::highlight(language, text) {
            let mut run = format(crate::ui::rich::code_color(palette, palette.text, kind));
            run.italics = kind == crate::highlight::Kind::Comment;
            job.append(text.get(range).unwrap_or_default(), 0.0, run);
        }
        return job;
    }
    #[cfg(not(feature = "highlight"))]
    let _ = file;
    egui::text::LayoutJob::single_section(text.to_owned(), format(palette.text))
}

/// The language a text file is in: from Slack's kind for it, else its
/// name's extension.
#[cfg(feature = "highlight")]
fn preview_language(file: &File) -> Option<&'static crate::highlight::Language> {
    crate::highlight::language(&file.filetype).or_else(|| {
        let (_, ext) = file.name.rsplit_once('.')?;
        crate::highlight::language(ext)
    })
}

/// A voice clip: a play button, its waveform and length, and the start of
/// what was said when Slack wrote it down. It plays in the app (see
/// `player`): the waveform fills in as it plays and a click on it seeks.
fn voice_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    team: &str,
    file: &File,
    actions: &mut Vec<Action>,
) -> Rect {
    let (rect, response) = card_box(ui, palette, CARD_WIDTH, voice_height(file));
    let row = Rect::from_min_size(
        rect.min + Vec2::splat(PAD),
        Vec2::new(rect.width() - PAD * 2.0, ICON),
    );
    let now = player::now(ui, team, file);
    let tip = player::tip(file, now.as_ref());
    let play = Rect::from_min_size(row.min, Vec2::splat(ICON));
    player::button(ui, palette, play, response.hovered(), now.as_ref());
    let button = Rect::from_center_size(
        egui::pos2(row.right() - BUTTON / 2.0, row.center().y),
        Vec2::splat(BUTTON),
    );
    let mut right = button.left() - 8.0;
    // Measured as the widest the time can get, so the waveform keeps its
    // width as the seconds tick.
    if let Some(text) = player::time(now.as_ref(), file.duration_ms) {
        let widest = ui
            .painter()
            .layout_no_wrap(
                text.replace(|c: char| c.is_ascii_digit(), "0"),
                theme::regular(12.0),
                palette.secondary,
            )
            .size()
            .x;
        let galley = ui
            .painter()
            .layout_no_wrap(text, theme::regular(12.0), palette.secondary);
        right -= widest;
        ui.painter().galley(
            egui::pos2(
                right + widest - galley.size().x,
                row.center().y - galley.size().y / 2.0,
            ),
            galley,
            palette.secondary,
        );
        right -= 10.0;
    }
    let left = play.right() + 10.0;
    let bars = wave_bars(&file.wave, bar_count(right - left));
    let played = player::progress(now.as_ref()).map_or(0, |fraction| {
        crate::audio::played_bars(bars.len(), fraction)
    });
    let (done, ahead) = if now.is_some() {
        (palette.accent, palette.accent.gamma_multiply(0.35))
    } else {
        let idle = palette.accent.gamma_multiply(0.85);
        (idle, idle)
    };
    for (i, level) in bars.iter().enumerate() {
        let height = (level * (ICON - 8.0)).max(2.0);
        let x = left + i as f32 * BAR_STEP;
        let bar = Rect::from_center_size(
            egui::pos2(x + BAR / 2.0, row.center().y),
            Vec2::new(BAR, height),
        );
        let color = if i < played { done } else { ahead };
        ui.painter().rect_filled(bar, CornerRadius::same(1), color);
    }
    let wave = Rect::from_x_y_ranges(left..=right.max(left), row.y_range());
    if let Some(transcript) = &file.transcript {
        let galley = one_line(
            ui,
            &format!("\u{201c}{transcript}\u{201d}"),
            theme::regular(12.5),
            palette.secondary,
            row.right() - left,
        );
        let y = row.bottom() + (TRANSCRIPT - galley.size().y) / 2.0;
        ui.painter()
            .galley(egui::pos2(left, y), galley, palette.secondary);
    }
    if card_button(ui, palette, button, Icon::Download, &t("Download")).clicked()
        && let Some(action) = download(file)
    {
        actions.push(action);
    }
    theme::describe(&response, egui::WidgetType::Button, &tip);
    let response = response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(&tip);
    if response.clicked() {
        play_clicked(&response, file, wave, actions);
    }
    rect
}

/// A click on a sound's card: play, pause or seek in the app, or open it
/// in the system's player when there is nothing the app could play.
fn play_clicked(response: &egui::Response, file: &File, seek: Rect, actions: &mut Vec<Action>) {
    if let Some(track) = crate::audio::Track::of(file) {
        player::clicked(response, track, seek, actions);
    } else if let Some((url, name)) = file.player() {
        actions.push(Action::OpenFile { url, name });
    }
}

/// How many waveform bars fit in `width`.
fn bar_count(width: f32) -> usize {
    if width < BAR {
        0
    } else {
        ((width - BAR) / BAR_STEP).floor() as usize + 1
    }
}

/// A voice clip's loudness as `bars` bars from 0 to 1: Slack's samples
/// shared out over the bars, each bar as loud as the loudest sample it
/// covers (so a short loud word is not averaged away), or a sample
/// repeated when there are more bars than samples.
pub(super) fn wave_bars(samples: &[u8], bars: usize) -> Vec<f32> {
    if samples.is_empty() {
        return vec![0.0; bars];
    }
    let len = samples.len();
    (0..bars)
        .map(|i| {
            let start = (i * len / bars).min(len - 1);
            let end = ((i + 1) * len / bars).clamp(start + 1, len);
            let peak = samples
                .get(start..end)
                .and_then(|covered| covered.iter().max())
                .copied()
                .unwrap_or(0);
            f32::from(peak.min(100)) / 100.0
        })
        .collect()
}

/// Tells the message's right-click menu that `file` is under the pointer.
fn hover_file(ui: &egui::Ui, file: &File, deletable: bool) {
    crate::ui::context::hover(
        ui,
        crate::ui::context::Target::File {
            file: file.id.clone(),
            name: file.name.clone(),
            download: file
                .download_url
                .clone()
                .or_else(|| file.url_private.clone()),
            deletable,
        },
    );
}

/// Where it is remembered that a held-back picture was asked for.
fn reveal_id(uri: &str) -> egui::Id {
    egui::Id::new(("show-picture", uri))
}

/// Whether the picture at `uri` shows: always, unless pictures are held
/// back and this one has not been clicked yet.
pub(super) fn shows(ui: &egui::Ui, row: &Row<'_>, uri: &str) -> bool {
    row.look.inline_media || ui.data(|d| d.get_temp::<bool>(reveal_id(uri)).unwrap_or(false))
}

/// A short bar standing in for a held-back picture; clicking it shows the
/// picture (and only then is it fetched).
pub(super) fn placeholder(ui: &mut egui::Ui, row: &Row<'_>, uri: &str, name: &str) {
    let palette = row.palette;
    let width = ui.available_width().min(360.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, PLACEHOLDER), Sense::click());
    let fill = if response.hovered() {
        palette.surface_hover
    } else {
        palette.surface
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(theme::RADIUS),
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 18.0, rect.center().y),
        Vec2::splat(15.0),
    );
    Icon::Image
        .image(palette.secondary, 15.0)
        .paint_at(ui, icon);
    let label = if name.trim().is_empty() {
        t("Show the picture").into_owned()
    } else {
        tf("Show the picture: {name}", &[("name", name)])
    };
    let galley =
        ui.painter()
            .layout_no_wrap(label.clone(), theme::regular(13.0), palette.secondary);
    // One line: a long name is cut at the bar's end.
    ui.painter().with_clip_rect(rect.shrink(6.0)).galley(
        egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0),
        galley,
        palette.secondary,
    );
    theme::focus_ring(ui, &response, palette, theme::RADIUS);
    theme::describe(&response, egui::WidgetType::Button, &label);
    if response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        ui.data_mut(|d| d.insert_temp(reveal_id(uri), true));
    }
}

/// `size` scaled down to fit `max`, never up, and never smaller than a
/// speck; `fallback` when the size is not known.
pub(super) fn fit_within(size: Option<[f32; 2]>, max: Vec2, fallback: Vec2) -> Vec2 {
    let [w, h] = size
        .filter(|[w, h]| *w > 0.0 && *h > 0.0)
        .unwrap_or([fallback.x, fallback.y]);
    let scale = (max.x / w).min(max.y / h).min(1.0);
    Vec2::new(w * scale, h * scale).max(Vec2::splat(24.0))
}

/// How large a picture file is shown, in a column `width` wide: the size
/// of the thumbnail Slack gave, shrunk to fit, or a fixed box when Slack
/// gave none. The same before it has loaded as after.
pub(super) fn thumb_size(file: &File, width: f32) -> Vec2 {
    fit_within(
        file.thumb_size,
        Vec2::new(width.min(420.0), 320.0),
        Vec2::new(360.0, 240.0),
    )
}

/// How large a video's or PDF's still is shown, in a column `width` wide.
/// A page is shown smaller than a frame: it is there to recognise the
/// document, not to read it.
pub(super) fn poster_size(file: &File, width: f32) -> Vec2 {
    if file.media().is_some() {
        fit_within(
            file.poster_size,
            Vec2::new(width.min(400.0), 260.0),
            Vec2::new(400.0, 225.0),
        )
    } else {
        fit_within(
            file.poster_size,
            Vec2::new(width.min(240.0), 300.0),
            Vec2::new(212.0, 300.0),
        )
    }
}

/// A round play button painted over a picture, centred on `center`.
pub(super) fn play_badge(ui: &egui::Ui, center: egui::Pos2, hovered: bool) {
    let radius = 24.0;
    let fill = egui::Color32::from_black_alpha(if hovered { 200 } else { 150 });
    let painter = ui.painter();
    painter.circle_filled(center, radius, fill);
    // A triangle, nudged right so it looks centred.
    let c = center + Vec2::new(3.0, 0.0);
    painter.add(egui::Shape::convex_polygon(
        vec![
            c + Vec2::new(-8.0, -11.0),
            c + Vec2::new(11.0, 0.0),
            c + Vec2::new(-8.0, 11.0),
        ],
        egui::Color32::WHITE,
        Stroke::NONE,
    ));
}

/// A video's length in the corner of its still, as players show it.
fn duration_badge(ui: &egui::Ui, still: Rect, text: &str) {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), theme::medium(11.5), egui::Color32::WHITE);
    let size = galley.size() + Vec2::new(10.0, 4.0);
    let rect = Rect::from_min_size(still.max - size - Vec2::splat(6.0), size);
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(theme::RADIUS_SMALL),
        egui::Color32::from_black_alpha(170),
    );
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        egui::Color32::WHITE,
    );
}

/// "1.2 MB · MP4 · 4:39": a file's size, its kind, and how long it lasts.
fn file_detail(file: &File) -> String {
    let kind = if file.filetype.trim().is_empty() {
        file.mimetype.split('/').next_back().unwrap_or("")
    } else {
        file.filetype.trim()
    }
    .to_uppercase();
    let mut detail = format!("{} · {kind}", crate::ui::file_size(file.size));
    if let Some(ms) = file.duration_ms.filter(|_| file.media().is_some()) {
        detail.push_str(" · ");
        detail.push_str(&crate::model::duration_text(ms));
    }
    detail
}

/// A video or sound: a play button, its name, and a download button. A
/// video opens in the system's player; a sound plays in the app, with a
/// bar to follow and seek it while it is in hand. Returns where the card
/// is.
fn media_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    team: &str,
    file: &File,
    actions: &mut Vec<Action>,
) -> Rect {
    let (rect, response) = card_box(ui, palette, CARD_WIDTH, CARD);
    let row = Rect::from_min_size(
        rect.min + Vec2::splat(PAD),
        Vec2::new(rect.width() - PAD * 2.0, ICON),
    );
    let sound = file.media() == Some(Media::Audio);
    let now = player::now(ui, team, file).filter(|_| sound);
    let play = Rect::from_min_size(row.min, Vec2::splat(ICON));
    player::button(ui, palette, play, response.hovered(), now.as_ref());
    let button = Rect::from_center_size(
        egui::pos2(row.right() - BUTTON / 2.0, row.center().y),
        Vec2::splat(BUTTON),
    );
    let (left, right) = (play.right() + 10.0, button.left() - 8.0);
    let mut seek = Rect::NOTHING;
    match (
        player::progress(now.as_ref()),
        player::time(now.as_ref(), file.duration_ms),
    ) {
        (Some(fraction), Some(time)) => {
            let name = one_line(
                ui,
                &file.name,
                theme::semibold(14.0),
                palette.text,
                right - left,
            );
            let name_top = row.top() + 1.0;
            let below = name_top + name.size().y;
            ui.painter()
                .galley(egui::pos2(left, name_top), name, palette.text);
            let time = ui
                .painter()
                .layout_no_wrap(time, theme::regular(12.0), palette.secondary);
            let y = (below + row.bottom()) / 2.0;
            let bar_right = right - time.size().x - 10.0;
            ui.painter().galley(
                egui::pos2(right - time.size().x, y - time.size().y / 2.0),
                time,
                palette.secondary,
            );
            player::bar(ui, palette, left, bar_right, y, fraction);
            seek = Rect::from_x_y_ranges(left..=bar_right.max(left), below..=row.bottom());
        }
        _ => name_and_detail(ui, palette, file, left, right, row.top()),
    }
    if card_button(ui, palette, button, Icon::Download, &t("Download")).clicked()
        && let Some(action) = download(file)
    {
        actions.push(action);
    }
    let tip = if sound {
        player::tip(file, now.as_ref())
    } else {
        tf("Play {name}", &[("name", &file.name)])
    };
    theme::describe(&response, egui::WidgetType::Button, &tip);
    let hover = if sound {
        tip.clone()
    } else {
        t("Play in your media player").into_owned()
    };
    let response = response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(hover);
    if !response.clicked() {
        return rect;
    }
    if sound {
        play_clicked(&response, file, seek, actions);
    } else if let Some((url, name)) = file.player() {
        actions.push(Action::OpenFile { url, name });
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_shrink_to_fit_and_never_grow() {
        let max = Vec2::new(400.0, 300.0);
        let fallback = Vec2::new(400.0, 225.0);
        assert_eq!(
            fit_within(Some([1200.0, 600.0]), max, fallback),
            Vec2::new(400.0, 200.0)
        );
        assert_eq!(
            fit_within(Some([100.0, 50.0]), max, fallback),
            Vec2::new(100.0, 50.0)
        );
        assert_eq!(fit_within(None, max, fallback), fallback);
        assert_eq!(fit_within(Some([0.0, 10.0]), max, fallback), fallback);
        // A sliver stays big enough to see and press.
        assert_eq!(fit_within(Some([4000.0, 10.0]), max, fallback).y, 24.0);
    }

    #[test]
    fn a_picture_file_takes_the_same_room_loaded_or_not() {
        let file = File {
            mimetype: "image/png".into(),
            thumb: Some("https://files.slack.com/t720.png".into()),
            thumb_size: Some([540.0, 720.0]),
            ..File::default()
        };
        let size = thumb_size(&file, 800.0);
        assert_eq!(size, Vec2::new(240.0, 320.0));
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        // While it loads, and once the thumbnail Slack described arrives.
        assert_eq!(crate::ui::contain(rect, None), rect);
        assert_eq!(
            crate::ui::contain(rect, Some(Vec2::new(540.0, 720.0))),
            rect
        );
    }

    #[test]
    fn a_waveform_shares_slacks_samples_out_over_the_bars() {
        let samples: Vec<u8> = (0..100).collect();
        // Fewer bars: each the loudest of the samples it covers.
        let bars = wave_bars(&samples, 10);
        assert_eq!(bars.len(), 10);
        assert_eq!(bars.first(), Some(&0.09));
        assert_eq!(bars.last(), Some(&0.99));
        // More bars than samples: samples repeat, none is skipped.
        let bars = wave_bars(&[10, 100], 5);
        assert_eq!(bars, vec![0.1, 0.1, 0.1, 1.0, 1.0]);
        // A short loud word is not averaged away.
        let mut quiet = vec![5u8; 100];
        quiet[57] = 90;
        assert!(wave_bars(&quiet, 7).contains(&0.9));
        // No samples: a flat line; no room: no bars.
        assert_eq!(wave_bars(&[], 3), vec![0.0; 3]);
        assert!(wave_bars(&samples, 0).is_empty());
    }

    #[test]
    fn bars_fit_the_room_they_are_given() {
        assert_eq!(bar_count(0.0), 0);
        assert_eq!(bar_count(BAR), 1);
        assert_eq!(bar_count(BAR + BAR_STEP), 2);
        let width = 200.0;
        let bars = bar_count(width);
        assert!((bars - 1) as f32 * BAR_STEP + BAR <= width);
        assert!(bars as f32 * BAR_STEP + BAR > width);
    }

    /// Draws `draw` on a fresh frame `frames` times, in a column 700 wide,
    /// and returns the height of what it drew each time.
    fn drawn_heights(frames: usize, draw: impl Fn(&mut egui::Ui) -> Rect) -> Vec<f32> {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(700.0, 900.0),
            )),
            ..egui::RawInput::default()
        };
        (0..frames)
            .map(|_| {
                let mut height = 0.0;
                let mut out = ctx.run_ui(input(), |ui| height = draw(ui).height());
                out.textures_delta.clear();
                height
            })
            .collect()
    }

    fn snippet(text: &str, lines_more: Option<u32>) -> File {
        File {
            name: "deploy.rs".into(),
            filetype: "rust".into(),
            mimetype: "text/plain".into(),
            preview: Some(TextPreview {
                text: text.into(),
                lines_more,
                ..TextPreview::default()
            }),
            ..File::default()
        }
    }

    #[test]
    fn cards_take_the_height_they_are_guessed_at_from_the_first_frame() {
        let palette = Palette::dark();
        let long = (1..=30)
            .map(|n| format!("let line_{n} = {n}; // a long line that runs past the card's edge"))
            .collect::<Vec<_>>()
            .join("\n");
        let voice = File {
            name: "Audio clip.m4a".into(),
            mimetype: "audio/mp4".into(),
            voice: true,
            wave: (0..100).map(|n| (n * 7 % 100) as u8).collect(),
            duration_ms: Some(13_977),
            transcript: Some("Get to Work.".into()),
            ..File::default()
        };
        let quiet = File {
            transcript: None,
            ..voice.clone()
        };
        let sheet = File {
            name: "Budget.xlsx".into(),
            filetype: "xlsx".into(),
            converted_pdf: Some(
                "https://files.slack.com/files-tmb/T-F-x/budget_converted.pdf".into(),
            ),
            ..File::default()
        };
        let video = File {
            name: "walkthrough.mp4".into(),
            mimetype: "video/mp4".into(),
            duration_ms: Some(279_145),
            ..File::default()
        };
        for file in [
            snippet("fn main() {}", None),
            snippet(&long, Some(200)),
            voice,
            quiet,
            sheet,
            video,
        ] {
            let heights = drawn_heights(3, |ui| {
                let mut actions = Vec::new();
                if is_voice(&file) {
                    voice_card(ui, &palette, "T1", &file, &mut actions)
                } else if file.media().is_some() {
                    media_card(ui, &palette, "T1", &file, &mut actions)
                } else if let Some(preview) = &file.preview {
                    text_card(ui, &palette, &file, preview, &mut actions)
                } else {
                    file_card(ui, &palette, &file, &mut actions)
                }
            });
            assert!(
                heights.iter().all(|h| *h == card_height(&file)),
                "{}: {heights:?}, guessed {}",
                file.name,
                card_height(&file)
            );
        }
    }

    #[test]
    fn a_long_preview_is_capped_and_a_short_one_is_shorter() {
        let long = (1..=30)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let capped = card_height(&snippet(&long, None));
        let more = card_height(&snippet(&long, Some(500)));
        assert_eq!(capped, more, "the cap holds however long the file");
        assert_eq!(
            capped,
            PAD + ICON + PAD + BOX_PAD * 2.0 + PREVIEW_LINES as f32 * LINE + MORE + PAD
        );
        // One line, nothing more: no line saying so.
        assert_eq!(
            card_height(&snippet("one", None)),
            PAD + ICON + PAD + BOX_PAD * 2.0 + LINE + PAD
        );
    }

    #[test]
    fn a_files_still_plays_opens_or_downloads() {
        let sheet = File {
            name: "Budget.xlsx".into(),
            mimetype: "application/vnd.ms-excel".into(),
            url_private: Some("https://files.slack.com/files-pri/T-F/budget.xlsx".into()),
            converted_pdf: Some(
                "https://files.slack.com/files-tmb/T-F-x/budget_converted.pdf".into(),
            ),
            ..File::default()
        };
        assert!(matches!(
            still_action(&sheet).1,
            Some(Action::OpenFile { url, name })
                if url == "https://files.slack.com/files-tmb/T-F-x/budget_converted.pdf"
                    && name == "Budget.pdf"
        ));
        // Without Slack's PDF a spreadsheet is saved, never opened.
        let plain = File {
            converted_pdf: None,
            ..sheet
        };
        assert!(matches!(
            still_action(&plain).1,
            Some(Action::Download { .. })
        ));
        let pdf = File {
            name: "guide.pdf".into(),
            mimetype: "application/pdf".into(),
            ..plain
        };
        assert!(matches!(
            still_action(&pdf).1,
            Some(Action::OpenFile { .. })
        ));
    }

    #[test]
    fn a_picture_file_of_unknown_size_gets_a_fixed_box() {
        let file = File {
            mimetype: "image/png".into(),
            thumb: Some("https://files.slack.com/t.png".into()),
            ..File::default()
        };
        assert_eq!(thumb_size(&file, 800.0), Vec2::new(360.0, 240.0));
        // A narrow column shrinks the box, keeping its shape.
        assert_eq!(thumb_size(&file, 180.0), Vec2::new(180.0, 120.0));
    }
}
