//! What floats over the window: the quick switcher, the emoji picker, a
//! person's card, an image preview, the delete confirmation and toasts.

use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Stroke, Vec2};

use crate::app::{App, PickerTarget};
use crate::i18n::{t, tf};
use crate::model::{Action, ConversationKind};
use crate::theme::{self, Icon};

pub fn show(app: &mut App, ctx: &egui::Context) {
    switcher(app, ctx);
    picker(app, ctx);
    profile(app, ctx);
    preview(app, ctx);
    confirm_delete(app, ctx);
    section_dialog(app, ctx);
    toasts(app, ctx);
}

/// Names a new sidebar section, or renames one.
fn section_dialog(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.section_dialog.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let mut answer: Option<bool> = None;
    let frame = modal_frame(app);
    let response = egui::Modal::new(egui::Id::new("section-dialog"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(360.0);
            let heading = if dialog.rename.is_some() {
                t("Rename section")
            } else {
                t("New section")
            };
            let heading = ui.label(
                RichText::new(heading)
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.add_space(6.0);
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .id(egui::Id::new("section-name"))
                        .hint_text(t("Section name"))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(8, 6)),
                )
                .labelled_by(heading.id);
            if focus {
                field.request_focus();
            }
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                answer = Some(true);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    answer = Some(false);
                }
                let label = if dialog.rename.is_some() {
                    t("Rename")
                } else {
                    t("Create")
                };
                if theme::primary_button(ui, &palette, &label).clicked() {
                    answer = Some(true);
                }
            });
        });
    if response.should_close() {
        answer = Some(false);
    }
    match answer {
        Some(true) if !dialog.name.trim().is_empty() => {
            let name = dialog.name.trim().to_owned();
            let edit = match dialog.rename {
                Some(section) => crate::sidebar::SidebarEdit::Rename { section, name },
                None => crate::sidebar::SidebarEdit::Create {
                    name,
                    channel: dialog.channel,
                },
            };
            app.actions.push(Action::Sidebar(edit));
        }
        Some(_) => {}
        None => app.section_dialog = Some(dialog),
    }
}

fn modal_frame(app: &App) -> egui::Frame {
    egui::Frame::new()
        .fill(app.palette.overlay)
        .stroke(Stroke::new(1.0, app.palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 32,
            spread: 0,
            color: app.palette.shadow,
        })
        .inner_margin(Margin::same(16))
}

fn switcher(app: &mut App, ctx: &egui::Context) {
    let Some((mut query, mut selected)) = app.switcher.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    let needle = query.trim().to_lowercase();
    let mut matches: Vec<(String, String, ConversationKind, bool, i64)> = workspace
        .conversations
        .iter()
        .map(|c| {
            (
                c.id.clone(),
                workspace.title(c),
                c.kind,
                c.has_unread(),
                c.latest.as_ref().and_then(|l| l.seconds()).unwrap_or(0),
            )
        })
        .filter(|(_, title, ..)| needle.is_empty() || title.to_lowercase().contains(&needle))
        .collect();
    matches.sort_by_key(|(_, title, _, unread, latest)| {
        (
            !title.to_lowercase().starts_with(&needle),
            !unread,
            std::cmp::Reverse(*latest),
        )
    });
    matches.truncate(12);
    let (down, up, enter, escape) = ctx.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::Enter),
            input.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    if !matches.is_empty() {
        if down {
            selected = (selected + 1) % matches.len();
        }
        if up {
            selected = (selected + matches.len() - 1) % matches.len();
        }
        selected = selected.min(matches.len() - 1);
    }
    let mut open = None;
    let mut close = escape;
    let response = egui::Modal::new(egui::Id::new("switcher"))
        .frame(modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(460.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut query)
                    .id(egui::Id::new("switcher-query"))
                    .hint_text(t("Jump to a channel or person…"))
                    .font(theme::regular(16.0))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 8)),
            );
            if focus {
                field.request_focus();
            }
            ui.add_space(8.0);
            for (index, (id, title, kind, unread, _)) in matches.iter().enumerate() {
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
                if index == selected || response.hovered() {
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(theme::RADIUS_SMALL),
                        if index == selected {
                            palette.accent.gamma_multiply(0.25)
                        } else {
                            palette.surface_hover
                        },
                    );
                }
                let icon = match kind {
                    ConversationKind::Channel => Icon::Hash,
                    ConversationKind::Private => Icon::Lock,
                    ConversationKind::Direct => Icon::User,
                    ConversationKind::Group => Icon::Users,
                };
                icon.image(palette.secondary, 15.0).paint_at(
                    ui,
                    egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 18.0, rect.center().y),
                        Vec2::splat(15.0),
                    ),
                );
                ui.painter().text(
                    egui::pos2(rect.left() + 36.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    title,
                    if *unread {
                        theme::bold(14.5)
                    } else {
                        theme::regular(14.5)
                    },
                    palette.text,
                );
                theme::describe_selected(
                    &response,
                    egui::WidgetType::SelectableLabel,
                    index == selected,
                    title,
                );
                if response.clicked() {
                    open = Some(id.clone());
                }
            }
            if matches.is_empty() {
                ui.label(RichText::new(t("Nothing matches.")).color(palette.dim));
            }
        });
    if response.should_close() {
        close = true;
    }
    if enter && let Some((id, ..)) = matches.get(selected) {
        open = Some(id.clone());
    }
    if let Some(id) = open {
        app.actions.push(Action::OpenConversation(id));
        return;
    }
    if !close {
        app.switcher = Some((query, selected));
    }
}

/// Emoji grouped as the picker shows them.
fn standard_emoji() -> &'static [(emojis::Group, Vec<&'static emojis::Emoji>)] {
    static GROUPS: std::sync::OnceLock<Vec<(emojis::Group, Vec<&'static emojis::Emoji>)>> =
        std::sync::OnceLock::new();
    GROUPS.get_or_init(|| {
        emojis::Group::iter()
            .map(|group| {
                let list = group.emojis().filter(|e| e.shortcode().is_some()).collect();
                (group, list)
            })
            .collect()
    })
}

/// The side of an emoji in the picker.
const CELL: f32 = 36.0;
/// From one row of emoji to the next: a cell and the item spacing.
const PITCH: f32 = CELL + 6.0;
/// A group's heading row: a little space, the label and the item spacing.
const LABEL: f32 = 26.0;

/// One emoji in the picker.
#[derive(Clone, Debug, PartialEq)]
enum Cell {
    Custom { name: String, url: String },
    Standard(&'static emojis::Emoji),
}

/// A heading and the emoji under it that match the query.
#[derive(Clone, Debug)]
struct Group {
    label: String,
    cells: Vec<Cell>,
}

/// The picker's matches for one query, worked out once rather than on
/// every frame the picker is open.
#[derive(Clone, Debug)]
struct Found {
    needle: String,
    custom_count: usize,
    groups: Vec<Group>,
    /// What Enter picks: the first match of what was typed.
    first: Option<String>,
}

/// A row of the picker's list: a heading or up to a row's worth of emoji.
#[derive(Debug, PartialEq)]
enum PickerRow<'a> {
    Label(&'a str),
    Cells(&'a [Cell]),
}

impl PickerRow<'_> {
    fn height(&self) -> f32 {
        match self {
            Self::Label(_) => LABEL,
            Self::Cells(_) => PITCH,
        }
    }
}

impl Found {
    fn new(workspace: &crate::app::WorkspaceState, needle: &str) -> Self {
        let mut custom: Vec<(&str, &str)> = workspace
            .emoji
            .custom_names()
            .filter(|(name, _)| needle.is_empty() || name.contains(needle))
            .collect();
        custom.sort();
        let mut groups = Vec::new();
        if !custom.is_empty() {
            groups.push(Group {
                label: workspace.info.name.clone(),
                cells: custom
                    .iter()
                    .map(|(name, url)| Cell::Custom {
                        name: (*name).to_owned(),
                        url: (*url).to_owned(),
                    })
                    .collect(),
            });
        }
        for (group, list) in standard_emoji() {
            let cells: Vec<Cell> = list
                .iter()
                .filter(|e| {
                    needle.is_empty()
                        || e.shortcodes().any(|code| code.contains(needle))
                        || e.name().contains(needle)
                })
                .map(|e| Cell::Standard(e))
                .collect();
            if !cells.is_empty() {
                groups.push(Group {
                    label: crate::emoji::group_name(*group).into_owned(),
                    cells,
                });
            }
        }
        let first = custom
            .first()
            .map(|(name, _)| (*name).to_owned())
            .or_else(|| {
                standard_emoji()
                    .iter()
                    .flat_map(|(_, list)| list.iter())
                    .filter_map(|e| e.shortcode())
                    .find(|code| code.contains(needle))
                    .map(str::to_owned)
            });
        Self {
            needle: needle.to_owned(),
            custom_count: workspace.emoji.custom_names().count(),
            groups,
            first,
        }
    }

    /// The list as rows, `columns` emoji to a row.
    fn rows(&self, columns: usize) -> Vec<PickerRow<'_>> {
        let mut rows = Vec::new();
        for group in &self.groups {
            rows.push(PickerRow::Label(&group.label));
            rows.extend(group.cells.chunks(columns.max(1)).map(PickerRow::Cells));
        }
        rows
    }
}

fn picker(app: &mut App, ctx: &egui::Context) {
    let Some(target) = app.picker.clone() else {
        return;
    };
    let palette = app.palette;
    let mut query = std::mem::take(&mut app.picker_query);
    let focus = std::mem::take(&mut app.focus_overlay);
    let Some(workspace) = app.active_workspace() else {
        app.picker = None;
        return;
    };
    let mut chosen: Option<String> = None;
    let mut close = false;
    let needle = query.trim().to_lowercase();
    // What matches the query, kept until the query or the custom emoji
    // change: filtering and sorting about 1,900 emoji on every frame was
    // most of the picker's cost.
    let found_id = egui::Id::new("emoji-picker-found");
    let custom_count = workspace.emoji.custom_names().count();
    let found = ctx
        .data(|d| d.get_temp::<std::sync::Arc<Found>>(found_id))
        .filter(|found| found.needle == needle && found.custom_count == custom_count)
        .unwrap_or_else(|| {
            let found = std::sync::Arc::new(Found::new(workspace, &needle));
            ctx.data_mut(|d| d.insert_temp(found_id, found.clone()));
            found
        });
    let frame = modal_frame(app);
    let response = egui::Modal::new(egui::Id::new("emoji-picker"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(t("Emoji"))
                        .font(theme::bold(16.0))
                        .color(palette.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, &palette, Icon::X, 16.0, &t("Close")).clicked() {
                        close = true;
                    }
                });
            });
            let field = ui.add(
                egui::TextEdit::singleline(&mut query)
                    .id(egui::Id::new("emoji-query"))
                    .hint_text(t("Search emoji"))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 6)),
            );
            if focus {
                field.request_focus();
            }
            // Enter picks the first match of what you searched for; with
            // nothing typed there is no match, only the whole list.
            if !needle.is_empty() && ui.input(|i| i.key_pressed(Key::Enter)) {
                chosen = found.first.clone();
            }
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(340.0)
                .auto_shrink([false, true])
                .show_viewport(ui, |ui, viewport| {
                    let columns = ((ui.available_width() / CELL).floor() as usize).max(1);
                    let rows = found.rows(columns);
                    let tops = super::rows::tops(rows.iter().map(PickerRow::height));
                    let total = tops.last().copied().unwrap_or(0.0);
                    let (whole, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), total),
                        Sense::hover(),
                    );
                    // Only the rows in view are painted.
                    for index in super::rows::visible(&tops, viewport.min.y, viewport.max.y) {
                        let top = whole.top() + tops[index];
                        match &rows[index] {
                            PickerRow::Label(label) => {
                                let rect = egui::Rect::from_min_max(
                                    egui::pos2(whole.left(), top),
                                    egui::pos2(whole.right(), top + LABEL),
                                );
                                ui.scope_builder(
                                    egui::UiBuilder::new()
                                        .max_rect(rect)
                                        .layout(egui::Layout::bottom_up(egui::Align::Min)),
                                    |ui| super::section_label(ui, &palette, label),
                                );
                            }
                            PickerRow::Cells(cells) => {
                                for (column, cell) in cells.iter().enumerate() {
                                    let rect = egui::Rect::from_min_size(
                                        egui::pos2(whole.left() + column as f32 * CELL, top),
                                        Vec2::splat(CELL),
                                    );
                                    let response = ui.interact(
                                        rect,
                                        ui.id().with(("emoji", index, column)),
                                        Sense::click(),
                                    );
                                    if response.hovered() {
                                        ui.painter().rect_filled(
                                            rect,
                                            CornerRadius::same(6),
                                            palette.surface_hover,
                                        );
                                    }
                                    let code = match cell {
                                        Cell::Custom { name, url } => {
                                            egui::Image::new(url.as_str())
                                                .fit_to_exact_size(Vec2::splat(24.0))
                                                .paint_at(
                                                    ui,
                                                    egui::Rect::from_center_size(
                                                        rect.center(),
                                                        Vec2::splat(24.0),
                                                    ),
                                                );
                                            name.as_str()
                                        }
                                        Cell::Standard(emoji) => {
                                            ui.painter().text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                emoji.as_str(),
                                                theme::regular(22.0),
                                                palette.text,
                                            );
                                            emoji.shortcode().unwrap_or_default()
                                        }
                                    };
                                    // Screen readers get the name of the cells
                                    // drawn; sighted users only while hovered.
                                    theme::describe(
                                        &response,
                                        egui::WidgetType::Button,
                                        &format!(":{code}:"),
                                    );
                                    let response = response.on_hover_ui(|ui| {
                                        ui.label(format!(":{code}:"));
                                    });
                                    if response.clicked() {
                                        chosen = Some(code.to_owned());
                                    }
                                }
                            }
                        }
                    }
                });
        });
    if close || response.should_close() {
        app.picker = None;
    }
    if let Some(name) = chosen {
        match target {
            PickerTarget::Reaction { channel, ts } => {
                app.actions.push(Action::React { channel, ts, name });
            }
            PickerTarget::Draft(key) => {
                let draft = app.drafts.entry(key).or_default();
                if !draft.text.is_empty() && !draft.text.ends_with(' ') {
                    draft.text.push(' ');
                }
                draft.text.push_str(&format!(":{name}: "));
                app.focus_composer = true;
            }
        }
        app.picker = None;
    }
    if app.picker.is_some() {
        app.picker_query = query;
    }
}

fn profile(app: &mut App, ctx: &egui::Context) {
    let Some(user_id) = app.profile.clone() else {
        return;
    };
    let palette = app.palette;
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    let user = workspace.user(&user_id).cloned();
    let dm = app.direct_message(&user_id);
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("profile"))
        .frame(modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(320.0);
            let name = user
                .as_ref()
                .map_or_else(|| user_id.clone(), |u| u.label().to_owned());
            ui.horizontal(|ui| {
                super::avatar(
                    ui,
                    user.as_ref().and_then(|u| u.avatar.as_deref()),
                    &name,
                    &user_id,
                    72.0,
                );
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&name)
                            .font(theme::bold(18.0))
                            .color(palette.text),
                    );
                    if let Some(user) = &user {
                        if !user.real_name.is_empty() && user.real_name != name {
                            ui.label(RichText::new(&user.real_name).color(palette.secondary));
                        }
                        if !user.title.is_empty() {
                            ui.label(
                                RichText::new(&user.title)
                                    .font(theme::regular(13.0))
                                    .color(palette.secondary),
                            );
                        }
                        ui.label(
                            RichText::new(format!("@{}", user.name))
                                .font(theme::regular(13.0))
                                .color(palette.dim),
                        );
                    }
                });
            });
            if let Some(user) = &user {
                if !user.status_text.is_empty() || !user.status_emoji.is_empty() {
                    ui.add_space(6.0);
                    let emoji = user.status_emoji.trim_matches(':');
                    let shown = crate::emoji::unicode(emoji, None).unwrap_or_default();
                    ui.label(
                        RichText::new(format!("{shown} {}", user.status_text)).color(palette.text),
                    );
                }
                if let Some(tz) = &user.tz
                    && let Ok(zone) = jiff::tz::TimeZone::get(tz)
                {
                    let local = jiff::Timestamp::now().to_zoned(zone);
                    ui.label(
                        RichText::new(tf(
                            "{time} local time",
                            &[("time", &local.strftime("%H:%M").to_string())],
                        ))
                        .font(theme::regular(13.0))
                        .color(palette.dim),
                    );
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if let Some(dm) = &dm
                    && theme::primary_button(ui, &palette, &t("Message")).clicked()
                {
                    app.actions.push(Action::OpenConversation(dm.clone()));
                    close = true;
                }
                if theme::secondary_button(ui, &palette, &t("Close")).clicked() {
                    close = true;
                }
            });
        });
    if close || response.should_close() {
        app.profile = None;
    }
}

fn preview(app: &mut App, ctx: &egui::Context) {
    let Some((uri, name)) = app.preview.clone() else {
        return;
    };
    let palette = app.palette;
    let screen = ctx.content_rect();
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("preview"))
        .frame(modal_frame(app).inner_margin(Margin::same(10)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&name)
                        .font(theme::semibold(14.0))
                        .color(palette.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, &palette, Icon::X, 16.0, &t("Close")).clicked() {
                        close = true;
                    }
                });
            });
            let max = (screen.size() - Vec2::new(120.0, 160.0)).max(Vec2::splat(200.0));
            ui.add(
                egui::Image::new(uri)
                    .fit_to_original_size(1.0)
                    .max_size(max)
                    .show_loading_spinner(true)
                    .corner_radius(CornerRadius::same(theme::RADIUS_SMALL)),
            );
        });
    if close || response.should_close() {
        app.preview = None;
    }
}

fn confirm_delete(app: &mut App, ctx: &egui::Context) {
    let Some((channel, ts)) = app.confirm_delete.clone() else {
        return;
    };
    let palette = app.palette;
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new("confirm-delete"))
        .frame(modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(
                RichText::new(t("Delete message?"))
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(t("This cannot be undone."))
                    .font(theme::regular(14.0))
                    .color(palette.secondary),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    answer = Some(false);
                }
                let delete = egui::Button::new(
                    RichText::new(t("Delete"))
                        .font(theme::medium(14.0))
                        .color(egui::Color32::WHITE),
                )
                .fill(palette.danger)
                .min_size(Vec2::new(0.0, 32.0));
                if ui.add(delete).clicked() {
                    answer = Some(true);
                }
            });
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                answer = Some(true);
            }
        });
    if response.should_close() {
        answer = Some(false);
    }
    match answer {
        Some(true) => {
            app.actions.push(Action::Delete { channel, ts });
            app.confirm_delete = None;
        }
        Some(false) => app.confirm_delete = None,
        None => {}
    }
}

fn toasts(app: &mut App, ctx: &egui::Context) {
    if app.toasts.is_empty() {
        return;
    }
    let palette = app.palette;
    let mut dismiss = false;
    egui::Area::new(egui::Id::new("toasts"))
        .anchor(egui::Align2::CENTER_BOTTOM, Vec2::new(0.0, -96.0))
        .order(egui::Order::Tooltip)
        .interactable(true)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for toast in app.toasts.iter().rev().take(3) {
                let response = egui::Frame::new()
                    .fill(if toast.error {
                        palette.danger
                    } else {
                        palette.overlay
                    })
                    .stroke(Stroke::new(1.0, palette.outline))
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 4],
                        blur: 16,
                        spread: 0,
                        color: palette.shadow,
                    })
                    .inner_margin(Margin::symmetric(14, 9))
                    .show(ui, |ui| {
                        ui.set_max_width(520.0);
                        ui.label(RichText::new(&toast.text).font(theme::medium(13.5)).color(
                            if toast.error {
                                egui::Color32::WHITE
                            } else {
                                palette.text
                            },
                        ));
                    })
                    .response
                    .interact(Sense::click());
                if response.clicked() {
                    dismiss = true;
                }
            }
        });
    if dismiss {
        app.actions.push(Action::DismissError);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> crate::app::WorkspaceState {
        let mut w = crate::app::WorkspaceState::new(crate::model::Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
        });
        w.emoji = crate::emoji::EmojiSet::new(
            [
                ("tacocat".to_owned(), "https://x.y/t.png".to_owned()),
                ("party-parrot".to_owned(), "https://x.y/p.gif".to_owned()),
            ]
            .into(),
        );
        w
    }

    #[test]
    fn the_picker_lists_matches_in_rows_under_their_headings() {
        let w = workspace();
        let all = Found::new(&w, "");
        assert_eq!(all.groups[0].label, "Acme");
        assert_eq!(
            all.groups[0].cells[0],
            Cell::Custom {
                name: "party-parrot".into(),
                url: "https://x.y/p.gif".into()
            },
            "custom emoji come first, by name"
        );
        let shown: usize = all.groups.iter().map(|g| g.cells.len()).sum();
        assert!(shown > 1000, "everything with a shortcode shows");
        let rows = all.rows(10);
        assert_eq!(rows[0], PickerRow::Label("Acme"));
        assert!(rows.iter().all(|row| match row {
            PickerRow::Cells(cells) => (1..=10).contains(&cells.len()),
            PickerRow::Label(_) => true,
        }));
        let cells: usize = rows
            .iter()
            .map(|row| match row {
                PickerRow::Cells(cells) => cells.len(),
                PickerRow::Label(_) => 0,
            })
            .sum();
        assert_eq!(cells, shown);
    }

    #[test]
    fn enter_picks_the_first_match() {
        let w = workspace();
        assert_eq!(Found::new(&w, "taco").first.as_deref(), Some("tacocat"));
        let rocket = Found::new(&w, "rocket");
        assert_eq!(rocket.first.as_deref(), Some("rocket"));
        assert!(rocket.groups.iter().all(|g| g.label != "Acme"));
        assert!(Found::new(&w, "no-such-emoji-at-all").groups.is_empty());
    }
}
