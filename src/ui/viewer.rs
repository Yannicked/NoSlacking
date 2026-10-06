//! The file viewer: a spreadsheet's sheets, a CSV file, a zip archive's
//! listing or a whole text file, in a panel over the window. It only
//! draws what the worker read (see `crate::viewer`); Esc, the close
//! button or a click beside the panel closes it.
//!
//! It is an overlay rather than a side panel because a table wants all
//! the width the window has, and a file is read on its own: the
//! conversation waits behind it, as it does behind the image viewer.

use egui::{Align, CornerRadius, Key, Layout, Modifiers, RichText, Sense, Stroke, Vec2};
use egui_extras::{Column, TableBuilder};

use crate::app::App;
use crate::i18n::{t, tf, tn};
use crate::model::Action;
use crate::theme::{self, Icon, Palette};
use crate::viewer::{Archive, Body, Document, Kind, Note, Sheet, State, Text, Viewer};

/// Room kept around the panel.
const MARGIN: f32 = 32.0;
/// The bar across the panel's top.
const BAR: f32 = 52.0;
/// One row of a table, and one line of text.
const ROW: f32 = 24.0;
const LINE: f32 = 18.0;
/// A column's width before it is dragged.
const COLUMN: f32 = 120.0;

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(mut viewer) = app.viewer.take() else {
        return;
    };
    let palette = app.palette;
    let mut close = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    let screen = ctx.content_rect();
    let panel = screen.shrink(MARGIN);
    egui::Area::new(egui::Id::new("file-viewer"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.set_min_size(screen.size());
            ui.painter()
                .rect_filled(screen, CornerRadius::ZERO, palette.shadow);
            // A click beside the panel closes it; the panel's own widgets,
            // added after, take their clicks first.
            let backdrop = ui.interact(
                screen,
                egui::Id::new("file-viewer-backdrop"),
                Sense::click(),
            );
            if backdrop.clicked()
                && backdrop
                    .interact_pointer_pos()
                    .is_some_and(|p| !panel.contains(p))
            {
                close = true;
            }
            ui.painter().rect(
                panel,
                CornerRadius::same(theme::RADIUS + 4),
                palette.window,
                Stroke::new(1.0, palette.outline),
                egui::StrokeKind::Inside,
            );
            let inner = panel.shrink2(Vec2::new(16.0, 8.0));
            let mut ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(inner)
                    .layout(Layout::top_down(Align::Min)),
            );
            ui.set_clip_rect(inner);
            if bar(&mut ui, &palette, &viewer, &mut app.actions) {
                close = true;
            }
            body(&mut ui, &palette, &mut viewer, &mut app.actions);
        });
    if !close {
        app.viewer = Some(viewer);
    }
}

/// The name, what kind of file it is, and the buttons. True when Close
/// was clicked.
fn bar(ui: &mut egui::Ui, palette: &Palette, viewer: &Viewer, actions: &mut Vec<Action>) -> bool {
    let mut close = false;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), BAR), Sense::hover());
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 4.0;
    if theme::icon_button(&mut row, palette, Icon::X, 18.0, &t("Close (Esc)")).clicked() {
        close = true;
    }
    if let Some(url) = &viewer.download
        && theme::icon_button(&mut row, palette, Icon::Download, 18.0, &t("Download")).clicked()
    {
        actions.push(Action::Download {
            url: url.clone(),
            name: viewer.name.clone(),
        });
    }
    row.add_space(8.0);
    row.with_layout(Layout::left_to_right(Align::Center), |row| {
        let icon = match viewer.kind {
            Kind::Zip => Icon::Archive,
            Kind::Text => Icon::Code,
            Kind::Sheet | Kind::Csv | Kind::Tsv => Icon::FileText,
        };
        let (glyph, _) = row.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
        icon.image(palette.accent, 20.0).paint_at(row, glyph);
        row.add_space(6.0);
        row.add(
            egui::Label::new(
                RichText::new(&viewer.name)
                    .font(theme::semibold(15.0))
                    .color(palette.text),
            )
            .truncate(),
        );
        if let Some(detail) = detail(viewer) {
            row.add(
                egui::Label::new(
                    RichText::new(detail)
                        .font(theme::regular(13.0))
                        .color(palette.secondary),
                )
                .truncate(),
            );
        }
    });
    close
}

/// What the file is and how big, beside its name.
fn detail(viewer: &Viewer) -> Option<String> {
    let State::Ready(document) = &viewer.state else {
        return None;
    };
    Some(match &document.body {
        Body::Sheets(sheets) if viewer.kind == Kind::Sheet => {
            tn("{count} sheet", "{count} sheets", count(sheets.len()))
        }
        Body::Sheets(sheets) => tn(
            "{count} row",
            "{count} rows",
            count(sheets.first().map_or(0, |s| s.rows.len())),
        ),
        Body::Archive(archive) => format!(
            "{} · {}",
            tn("{count} item", "{count} items", count(archive.count)),
            tf(
                "{size} unpacked",
                &[("size", &crate::ui::file_size(archive.unpacked))]
            )
        ),
        Body::Text(text) => tn("{count} line", "{count} lines", count(text.lines.len())),
    })
}

/// An icon in the flow of `ui`, `size` square.
fn glyph(ui: &mut egui::Ui, icon: Icon, color: egui::Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    icon.image(color, size).paint_at(ui, rect);
}

/// A count for [`tn`], which takes a `u32`.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The panel under the bar: the file, or its loading or failure.
fn body(ui: &mut egui::Ui, palette: &Palette, viewer: &mut Viewer, actions: &mut Vec<Action>) {
    match &viewer.state {
        State::Loading => {
            centred(ui, |ui| {
                ui.add(egui::Spinner::new().size(28.0).color(palette.secondary));
                ui.add_space(8.0);
                ui.label(
                    RichText::new(tf("Loading {name}…", &[("name", &viewer.name)]))
                        .font(theme::regular(14.0))
                        .color(palette.secondary),
                );
            });
            return;
        }
        State::Failed(failure) => {
            let failure = failure.clone();
            centred(ui, |ui| {
                glyph(ui, Icon::CircleAlert, palette.danger, 28.0);
                ui.add_space(8.0);
                ui.label(
                    RichText::new(tf("Could not show {name}", &[("name", &viewer.name)]))
                        .font(theme::semibold(15.0))
                        .color(palette.text),
                );
                ui.label(
                    RichText::new(failure.sentence())
                        .font(theme::regular(13.0))
                        .color(palette.secondary),
                );
                if let Some(url) = &viewer.download {
                    ui.add_space(10.0);
                    if theme::primary_button(ui, palette, &t("Download")).clicked() {
                        actions.push(Action::Download {
                            url: url.clone(),
                            name: viewer.name.clone(),
                        });
                    }
                }
            });
            return;
        }
        State::Ready(_) => {}
    };
    // Taken out while it is drawn, so the viewer's own state can change
    // beside it without copying the file each frame.
    let state = std::mem::replace(&mut viewer.state, State::Loading);
    if let State::Ready(document) = &state {
        ready(ui, palette, viewer, document);
    }
    viewer.state = state;
}

/// A file that has been read.
fn ready(ui: &mut egui::Ui, palette: &Palette, viewer: &mut Viewer, document: &Document) {
    notes(ui, palette, &document.notes);
    match &document.body {
        Body::Sheets(sheets) => sheets_view(ui, palette, viewer, sheets),
        Body::Archive(archive) => archive_view(ui, palette, viewer.id, archive),
        Body::Text(text) => text_view(ui, palette, viewer, text),
    }
}

/// `add` in the middle of what is left of the panel.
fn centred(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let rect = ui.available_rect_before_wrap();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink(24.0))
            .layout(Layout::top_down(Align::Center)),
    );
    child.add_space((rect.height() / 2.0 - 60.0).max(0.0));
    add(&mut child);
}

/// What was left out, one line each, in the warning colour.
fn notes(ui: &mut egui::Ui, palette: &Palette, notes: &[Note]) {
    for note in notes {
        ui.horizontal(|ui| {
            glyph(ui, Icon::Info, palette.warning, 14.0);
            ui.label(
                RichText::new(note_text(note))
                    .font(theme::regular(12.5))
                    .color(palette.secondary),
            );
        });
    }
}

/// A note in words.
fn note_text(note: &Note) -> String {
    let number = |n: usize| n.to_string();
    match note {
        Note::Download { shown } => tf(
            "Only the first {size} of the file is shown.",
            &[("size", &crate::ui::file_size(*shown))],
        ),
        Note::Rows { shown } => tf(
            "Only the first {count} rows are shown.",
            &[("count", &number(*shown))],
        ),
        Note::Columns { shown } => tf(
            "Only the first {count} columns are shown.",
            &[("count", &number(*shown))],
        ),
        Note::Sheets { shown, total } => tf(
            "Only {shown} of {total} sheets are shown.",
            &[("shown", &number(*shown)), ("total", &number(*total))],
        ),
        Note::LongCells { count: cut } => tn(
            "{count} long cell is cut short.",
            "{count} long cells are cut short.",
            count(*cut),
        ),
        Note::Lines { shown } => tf(
            "Only the first {count} lines are shown.",
            &[("count", &number(*shown))],
        ),
        Note::LongLines { count: cut } => tn(
            "{count} long line is cut short.",
            "{count} long lines are cut short.",
            count(*cut),
        ),
        Note::Entries { shown, total } => tf(
            "Only {shown} of {total} entries are listed.",
            &[("shown", &number(*shown)), ("total", &number(*total))],
        ),
        Note::NotUtf8 => t("Some characters could not be read and show as �.").into_owned(),
        Note::Packed { ratio } => tf(
            "This archive unpacks to {ratio} times its size; take care unpacking it.",
            &[("ratio", &ratio.to_string())],
        ),
    }
}

/// A workbook's tabs, the sheet's own notes, and its grid.
fn sheets_view(ui: &mut egui::Ui, palette: &Palette, viewer: &mut Viewer, sheets: &[Sheet]) {
    viewer.sheet = viewer.sheet.min(sheets.len().saturating_sub(1));
    ui.horizontal(|ui| {
        if sheets.len() > 1 || sheets.first().is_some_and(|s| !s.name.is_empty()) {
            for (index, sheet) in sheets.iter().enumerate() {
                let selected = index == viewer.sheet;
                let text =
                    RichText::new(&sheet.name)
                        .font(theme::medium(13.0))
                        .color(if selected {
                            palette.text
                        } else {
                            palette.secondary
                        });
                let tab = ui.add(egui::Button::selectable(selected, text));
                if tab.clicked() {
                    viewer.sheet = index;
                }
            }
            ui.add_space(12.0);
        }
        ui.checkbox(&mut viewer.freeze, t("Freeze first row"));
    });
    ui.add_space(4.0);
    let Some(sheet) = sheets.get(viewer.sheet) else {
        return;
    };
    notes(ui, palette, &sheet.notes);
    if sheet.rows.is_empty() {
        centred(ui, |ui| {
            ui.label(
                RichText::new(t("This sheet is empty."))
                    .font(theme::regular(14.0))
                    .color(palette.secondary),
            );
        });
        return;
    }
    grid(ui, palette, viewer.id, viewer.sheet, viewer.freeze, sheet);
}

/// A sheet's cells, with column letters along the top and row numbers
/// down the side. Only the rows in view are drawn, and only the cells of
/// columns in view are laid out.
fn grid(ui: &mut egui::Ui, palette: &Palette, id: u64, index: usize, freeze: bool, sheet: &Sheet) {
    let frozen = usize::from(freeze && !sheet.rows.is_empty());
    let numbers = number_width(ui, sheet.rows.len());
    let header = if frozen == 1 { ROW * 2.0 } else { ROW };
    egui::ScrollArea::horizontal()
        .id_salt(("viewer-grid", id, index))
        .auto_shrink(false)
        .show(ui, |ui| {
            TableBuilder::new(ui)
                .id_salt(("viewer-table", id, index))
                .striped(true)
                .resizable(true)
                .auto_shrink(false)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(numbers))
                .columns(
                    Column::initial(COLUMN).at_least(36.0).clip(true),
                    sheet.columns.max(1),
                )
                .header(header, |mut row| {
                    row.col(|_| {});
                    for column in 0..sheet.columns.max(1) {
                        row.col(|ui| {
                            ui.vertical(|ui| {
                                ui.set_height(header);
                                let letter = crate::viewer::column_name(column);
                                ui.add_sized(
                                    [ui.available_width(), ROW],
                                    egui::Label::new(
                                        RichText::new(letter)
                                            .font(theme::medium(12.0))
                                            .color(palette.dim),
                                    ),
                                );
                                if frozen == 1 {
                                    let text = cell(sheet, 0, column);
                                    ui.add_sized(
                                        [ui.available_width(), ROW],
                                        egui::Label::new(
                                            RichText::new(text)
                                                .font(theme::semibold(13.0))
                                                .color(palette.text),
                                        )
                                        .truncate(),
                                    );
                                }
                            });
                        });
                    }
                })
                .body(|body| {
                    body.rows(ROW, sheet.rows.len() - frozen, |mut row| {
                        let at = row.index() + frozen;
                        row.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new((at + 1).to_string())
                                        .font(theme::regular(12.0))
                                        .color(palette.dim),
                                );
                            });
                        });
                        for column in 0..sheet.columns.max(1) {
                            row.col(|ui| {
                                // Cells scrolled out of sight sideways are
                                // skipped: a sheet can be 500 columns wide.
                                if !ui.is_rect_visible(ui.max_rect()) {
                                    return;
                                }
                                let text = cell(sheet, at, column);
                                if text.is_empty() {
                                    return;
                                }
                                let label = egui::Label::new(
                                    RichText::new(text)
                                        .font(theme::regular(13.0))
                                        .color(palette.text),
                                )
                                .truncate();
                                // Numbers line up on the right, as in a
                                // spreadsheet.
                                if text.parse::<f64>().is_ok() {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.add(label);
                                    });
                                } else {
                                    ui.add(label).on_hover_text(text);
                                }
                            });
                        }
                    });
                });
        });
}

/// The text of a cell, or nothing.
fn cell(sheet: &Sheet, row: usize, column: usize) -> &str {
    sheet
        .rows
        .get(row)
        .and_then(|cells| cells.get(column))
        .map_or("", String::as_str)
}

/// How wide the row numbers' column must be for `rows` rows.
fn number_width(ui: &egui::Ui, rows: usize) -> f32 {
    let widest = ui
        .painter()
        .layout_no_wrap(
            rows.max(1).to_string(),
            theme::regular(12.0),
            egui::Color32::WHITE,
        )
        .size()
        .x;
    widest + 16.0
}

/// An archive's entries: path, size, packed size and date.
fn archive_view(ui: &mut egui::Ui, palette: &Palette, id: u64, archive: &Archive) {
    let text = |ui: &mut egui::Ui, text: &str, color| {
        ui.add(
            egui::Label::new(RichText::new(text).font(theme::regular(13.0)).color(color))
                .truncate(),
        );
    };
    let right = |ui: &mut egui::Ui, value: String| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(value)
                    .font(theme::regular(13.0))
                    .color(palette.secondary),
            );
        });
    };
    TableBuilder::new(ui)
        .id_salt(("viewer-archive", id))
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(200.0).clip(true))
        .column(Column::initial(100.0))
        .column(Column::initial(100.0))
        .column(Column::initial(150.0))
        .header(ROW, |mut row| {
            for (title, at_right) in [
                (t("Name"), false),
                (t("Size"), true),
                (t("Packed"), true),
                (t("Modified"), false),
            ] {
                row.col(|ui| {
                    let label = RichText::new(title)
                        .font(theme::semibold(12.5))
                        .color(palette.secondary);
                    if at_right {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(label);
                        });
                    } else {
                        ui.label(label);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(ROW, archive.entries.len(), |mut row| {
                let Some(entry) = archive.entries.get(row.index()) else {
                    return;
                };
                row.col(|ui| {
                    let icon = if entry.encrypted {
                        Some((Icon::Lock, t("Locked with a password")))
                    } else {
                        None
                    };
                    if let Some((icon, tip)) = icon {
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                        icon.image(palette.secondary, 14.0).paint_at(ui, rect);
                        response.on_hover_text(tip);
                    }
                    let color = if entry.folder {
                        palette.secondary
                    } else {
                        palette.text
                    };
                    text(ui, &entry.path, color);
                });
                row.col(|ui| {
                    if !entry.folder {
                        right(ui, crate::ui::file_size(entry.size));
                    }
                });
                row.col(|ui| {
                    if !entry.folder {
                        right(ui, crate::ui::file_size(entry.packed));
                    }
                });
                row.col(|ui| {
                    text(
                        ui,
                        entry.modified.as_deref().unwrap_or(""),
                        palette.secondary,
                    )
                });
            });
        });
}

/// A text file: a find field, then its lines numbered, in monospace and
/// coloured by its language. Only the lines in view are laid out.
fn text_view(ui: &mut egui::Ui, palette: &Palette, viewer: &mut Viewer, text: &Text) {
    find_bar(ui, palette, viewer, &text.lines);
    ui.add_space(4.0);
    let numbers = number_width(ui, text.lines.len());
    let language = language(viewer);
    let current = viewer.matches.get(viewer.found).copied();
    let mut scroll = egui::ScrollArea::both()
        .id_salt(("viewer-text", viewer.id))
        .auto_shrink(false);
    if std::mem::take(&mut viewer.jump)
        && let Some(line) = current
    {
        let offset = (line as f32 * LINE - ui.available_height() / 3.0).max(0.0);
        scroll = scroll.vertical_scroll_offset(offset);
    }
    let matches = &viewer.matches;
    scroll.show_rows(ui, LINE, text.lines.len(), |ui, range| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        for index in range {
            let line = text.lines.get(index).map_or("", String::as_str);
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width().max(numbers), LINE),
                Sense::hover(),
            );
            let lit = if current == Some(index) {
                Some(palette.accent.gamma_multiply(0.35))
            } else if matches.binary_search(&index).is_ok() {
                Some(palette.accent.gamma_multiply(0.15))
            } else {
                None
            };
            if let Some(fill) = lit {
                ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);
            }
            ui.painter().text(
                egui::pos2(rect.left() + numbers - 12.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                (index + 1).to_string(),
                theme::mono(12.0),
                palette.dim,
            );
            let galley = ui.painter().layout_job(line_job(palette, language, line));
            let width = galley.size().x;
            ui.painter().galley(
                egui::pos2(
                    rect.left() + numbers,
                    rect.top() + (LINE - galley.size().y) / 2.0,
                ),
                galley,
                palette.text,
            );
            // Widen the content so long lines scroll into view sideways.
            if numbers + width > rect.width() {
                ui.allocate_exact_size(Vec2::new(numbers + width, 0.0), Sense::hover());
            }
        }
    });
}

/// The find field, how many lines match, and the buttons between them.
fn find_bar(ui: &mut egui::Ui, palette: &Palette, viewer: &mut Viewer, lines: &[String]) {
    ui.horizontal(|ui| {
        glyph(ui, Icon::Search, palette.secondary, 16.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut viewer.find)
                .id(egui::Id::new(("viewer-find", viewer.id)))
                .hint_text(t("Find in file"))
                .desired_width(240.0),
        );
        viewer.search(lines);
        if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let back = ui.input(|i| i.modifiers.shift);
            viewer.step(if back { -1 } else { 1 });
            field.request_focus();
        }
        if viewer.find.trim().is_empty() {
            return;
        }
        let status = if viewer.matches.is_empty() {
            t("No matches").into_owned()
        } else {
            tf(
                "{current} of {count}",
                &[
                    ("current", &(viewer.found + 1).to_string()),
                    ("count", &viewer.matches.len().to_string()),
                ],
            )
        };
        ui.label(
            RichText::new(status)
                .font(theme::regular(12.5))
                .color(palette.secondary),
        );
        if theme::icon_button(ui, palette, Icon::ArrowUp, 14.0, &t("Previous match")).clicked() {
            viewer.step(-1);
        }
        if theme::icon_button(ui, palette, Icon::ArrowDown, 14.0, &t("Next match")).clicked() {
            viewer.step(1);
        }
    });
}

/// The language a text file is coloured as: from Slack's kind for it,
/// else its name's extension.
#[cfg(feature = "highlight")]
fn language(viewer: &Viewer) -> Option<&'static crate::highlight::Language> {
    crate::highlight::language(&viewer.filetype).or_else(|| {
        let (_, ext) = viewer.name.rsplit_once('.')?;
        crate::highlight::language(ext)
    })
}

#[cfg(not(feature = "highlight"))]
fn language(_viewer: &Viewer) -> Option<()> {
    None
}

/// One line laid out in monospace, coloured like a code block when its
/// language is known. A line is coloured on its own, so a comment over
/// several lines shows plain after its first.
#[cfg(feature = "highlight")]
fn line_job(
    palette: &Palette,
    language: Option<&'static crate::highlight::Language>,
    line: &str,
) -> egui::text::LayoutJob {
    let format = |color| egui::TextFormat::simple(theme::mono(12.5), color);
    let Some(language) = language else {
        return egui::text::LayoutJob::single_section(line.to_owned(), format(palette.text));
    };
    let mut job = egui::text::LayoutJob::default();
    for (range, kind) in crate::highlight::highlight(language, line) {
        let mut run = format(crate::ui::rich::code_color(palette, palette.text, kind));
        run.italics = kind == crate::highlight::Kind::Comment;
        job.append(line.get(range).unwrap_or_default(), 0.0, run);
    }
    job
}

#[cfg(not(feature = "highlight"))]
fn line_job(palette: &Palette, _language: Option<()>, line: &str) -> egui::text::LayoutJob {
    egui::text::LayoutJob::single_section(
        line.to_owned(),
        egui::TextFormat::simple(theme::mono(12.5), palette.text),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_note_has_words() {
        for note in [
            Note::Download { shown: 20 },
            Note::Rows { shown: 1 },
            Note::Columns { shown: 1 },
            Note::Sheets { shown: 1, total: 2 },
            Note::LongCells { count: 2 },
            Note::Lines { shown: 1 },
            Note::LongLines { count: 1 },
            Note::Entries { shown: 1, total: 2 },
            Note::NotUtf8,
            Note::Packed { ratio: 500 },
        ] {
            let text = note_text(&note);
            assert!(!text.is_empty() && !text.contains('{'), "{note:?}: {text}");
        }
    }
}
