//! What floats over the window: the quick switcher, the emoji picker, a
//! person's card, an image preview, the delete confirmation, the shortcut
//! sheet and toasts.

use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Stroke, Vec2};

use crate::app::{App, PickerTarget};
use crate::i18n::t;
use crate::model::{Ability, Action, ConversationKind};
use crate::theme::{self, Icon};

pub fn show(app: &mut App, ctx: &egui::Context) {
    switcher(app, ctx);
    picker(app, ctx);
    profile(app, ctx);
    super::lightbox::show(app, ctx);
    super::viewer::show(app, ctx);
    confirm_delete(app, ctx);
    confirm_press(app, ctx);
    confirm_delete_file(app, ctx);
    super::add_emoji::dialog(app, ctx);
    section_dialog(app, ctx);
    super::share::dialog(app, ctx);
    super::people::status_dialog(app, ctx);
    super::shortcuts::show(app, ctx);
    super::people::invites(app, ctx);
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

/// The frame every dialog and picker floats in.
pub(super) fn modal_frame(app: &App) -> egui::Frame {
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

/// The quick switcher, which with `>` typed first is the command palette
/// (see [`crate::palette`]).
fn switcher(app: &mut App, ctx: &egui::Context) {
    let Some(crate::app::Switcher {
        mut query,
        mut selected,
    }) = app.switcher.take()
    else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    let commands = crate::palette::query(&query).map(|needle| {
        let me = &workspace.info.user_id;
        let state = crate::palette::State {
            away: workspace.people.presence(me) == Some(crate::people::Presence::Away),
            stay_active: app.settings.desktop.stay_active,
            dark: palette.dark,
            hide_inactive: app.settings.hide_inactive,
        };
        let found = crate::palette::matching(needle, &state);
        (state, found)
    });
    let matches = if commands.is_some() {
        Vec::new()
    } else {
        matching(ctx, "switcher", workspace, &query, |_| true, 12)
    };
    let count = commands
        .as_ref()
        .map_or(matches.len(), |(_, found)| found.len());
    let (down, up, enter, escape) = ctx.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::Enter),
            input.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    if count > 0 {
        if down {
            selected = (selected + 1) % count;
        }
        if up {
            selected = (selected + count - 1) % count;
        }
        selected = selected.min(count - 1);
    }
    let mac = cfg!(target_os = "macos");
    let mut open = None;
    let mut run = None;
    let mut close = escape;
    let response = egui::Modal::new(egui::Id::new("switcher"))
        .frame(modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(460.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut query)
                    .id(egui::Id::new("switcher-query"))
                    .hint_text(t("Jump to a channel or person, or type > for commands"))
                    .font(theme::regular(16.0))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 8)),
            );
            if focus {
                field.request_focus();
            }
            ui.add_space(8.0);
            if let Some((state, found)) = &commands {
                for (index, command) in found.iter().enumerate() {
                    let keys = command
                        .shortcut()
                        .and_then(super::shortcuts::keys_of)
                        .map(|k| super::shortcuts::spell(k, mac));
                    let label = command.label(state);
                    if command_row(ui, &palette, &label, keys.as_deref(), index == selected)
                        .clicked()
                    {
                        run = Some(command.action(state));
                    }
                }
            } else {
                for (index, found) in matches.iter().enumerate() {
                    if conversation_row(ui, &palette, found, index == selected).clicked() {
                        open = Some(found.id.clone());
                    }
                }
            }
            if count == 0 {
                ui.label(RichText::new(t("Nothing matches.")).color(palette.dim));
            }
        });
    if response.should_close() {
        close = true;
    }
    if enter {
        match &commands {
            Some((state, found)) => run = found.get(selected).map(|c| c.action(state)),
            None => open = matches.get(selected).map(|found| found.id.clone()),
        }
    }
    if let Some(action) = run {
        app.actions.push(action);
        return;
    }
    if let Some(id) = open {
        app.actions.push(Action::OpenConversation(id));
        return;
    }
    if !close {
        app.switcher = Some(crate::app::Switcher { query, selected });
    }
}

/// A conversation picker's last search, kept in egui's memory.
#[derive(Clone, Default)]
struct Matched {
    /// The query and [`crate::convos::candidates_fingerprint`] it was for.
    key: Option<(String, u64)>,
    /// The [`crate::convos::candidates_revision`] the fingerprint was last
    /// taken at: while it holds, the fingerprint cannot have moved.
    revision: Option<u64>,
    found: Vec<crate::convos::Candidate>,
}

impl Matched {
    /// The matches kept for `query`, if the conversations they came from
    /// are unchanged. Takes the fingerprint only when the revision moved.
    fn kept(
        &mut self,
        query: &str,
        workspace: &crate::app::WorkspaceState,
    ) -> Option<Vec<crate::convos::Candidate>> {
        let (kept_query, print) = self.key.as_ref()?;
        if kept_query != query {
            return None;
        }
        let revision = crate::convos::candidates_revision(workspace);
        if self.revision != Some(revision) {
            if *print != crate::convos::candidates_fingerprint(workspace) {
                return None;
            }
            self.revision = Some(revision);
        }
        Some(self.found.clone())
    }
}

/// The first `limit` conversations that `keep` lets through and whose
/// title holds `query`, best first, for the picker named `picker`. Kept
/// until the query or the conversations change: building, matching and
/// sorting every conversation on every frame is too slow in a big
/// workspace.
pub(super) fn matching(
    ctx: &egui::Context,
    picker: &str,
    workspace: &crate::app::WorkspaceState,
    query: &str,
    keep: fn(&crate::convos::Candidate) -> bool,
    limit: usize,
) -> Vec<crate::convos::Candidate> {
    let id = egui::Id::new(("conversation-matches", picker));
    if let Some(found) = ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Matched>(id)
            .kept(query, workspace)
    }) {
        return found;
    }
    let revision = crate::convos::candidates_revision(workspace);
    let key = (
        query.to_owned(),
        crate::convos::candidates_fingerprint(workspace),
    );
    let candidates = crate::convos::candidates(workspace)
        .into_iter()
        .filter(keep)
        .collect();
    let mut found = crate::convos::conversations_matching(candidates, query);
    found.truncate(limit);
    ctx.data_mut(|d| {
        d.insert_temp(
            id,
            Matched {
                key: Some(key),
                revision: Some(revision),
                found: found.clone(),
            },
        );
    });
    found
}

/// One conversation in a list to pick from: its kind's icon and its
/// title, bold when unread, lit when `selected`.
pub(super) fn conversation_row(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    found: &crate::convos::Candidate,
    selected: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    if selected || response.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS_SMALL),
            if selected {
                palette.accent.gamma_multiply(0.25)
            } else {
                palette.surface_hover
            },
        );
    }
    let icon = match found.kind {
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
        &found.title,
        if found.unread {
            theme::bold(14.5)
        } else {
            theme::regular(14.5)
        },
        palette.text,
    );
    theme::describe_selected(
        &response,
        egui::WidgetType::SelectableLabel,
        selected,
        &found.title,
    );
    response
}

/// One command of the palette: its name, and on the right the keys that
/// do the same, if any; lit when `selected`.
fn command_row(
    ui: &mut egui::Ui,
    palette: &theme::Palette,
    label: &str,
    keys: Option<&str>,
    selected: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    if selected || response.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS_SMALL),
            if selected {
                palette.accent.gamma_multiply(0.25)
            } else {
                palette.surface_hover
            },
        );
    }
    Icon::ChevronRight.image(palette.secondary, 15.0).paint_at(
        ui,
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 18.0, rect.center().y),
            Vec2::splat(15.0),
        ),
    );
    ui.painter().text(
        egui::pos2(rect.left() + 36.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        theme::regular(14.5),
        palette.text,
    );
    if let Some(keys) = keys {
        let galley =
            ui.painter()
                .layout_no_wrap(keys.to_owned(), theme::medium(12.5), palette.secondary);
        let size = galley.size() + Vec2::new(12.0, 6.0);
        let cap = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 8.0 - size.x, rect.center().y - size.y / 2.0),
            size,
        );
        ui.painter().rect(
            cap,
            CornerRadius::same(theme::RADIUS_SMALL),
            palette.surface,
            Stroke::new(1.0, palette.outline),
            egui::StrokeKind::Inside,
        );
        ui.painter()
            .galley(cap.min + Vec2::new(6.0, 3.0), galley, palette.secondary);
    }
    let spoken = match keys {
        Some(keys) => format!("{label}, {keys}"),
        None => label.to_owned(),
    };
    theme::describe_selected(&response, egui::WidgetType::Button, selected, &spoken);
    response
}

/// Emoji grouped as the picker shows them.
fn standard_emoji() -> &'static [(emojis::Group, Vec<&'static emojis::Emoji>)] {
    static GROUPS: std::sync::OnceLock<Vec<(emojis::Group, Vec<&'static emojis::Emoji>)>> =
        std::sync::OnceLock::new();
    GROUPS.get_or_init(|| {
        emojis::Group::iter()
            .map(|group| {
                let list = group
                    .emojis()
                    .filter(|e| crate::emoji::shortcode(e).is_some())
                    .collect();
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
    /// The recently used emoji it was made with.
    recent: Vec<String>,
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
    fn new(workspace: &crate::app::WorkspaceState, needle: &str, recent: &[String]) -> Self {
        let mut groups = Vec::new();
        // What you used lately comes first, while nothing is searched for.
        if needle.is_empty() {
            let cells: Vec<Cell> = recent
                .iter()
                .filter_map(|name| match workspace.emoji.resolve(name) {
                    crate::emoji::Resolved::Image(url) => Some(Cell::Custom {
                        name: name.clone(),
                        url,
                    }),
                    _ => crate::emoji::standard(name).map(Cell::Standard),
                })
                .collect();
            if !cells.is_empty() {
                groups.push(Group {
                    label: t("Recently used").into_owned(),
                    cells,
                });
            }
        }
        let mut custom: Vec<(&str, &str)> = workspace
            .emoji
            .custom_names()
            .filter(|(name, _)| needle.is_empty() || name.contains(needle))
            .collect();
        custom.sort();
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
                        || crate::emoji::names(e)
                            .iter()
                            .any(|code| code.contains(needle))
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
                    .filter_map(|e| crate::emoji::shortcode(e))
                    .find(|code| code.contains(needle))
                    .map(str::to_owned)
            });
        Self {
            needle: needle.to_owned(),
            custom_count: workspace.emoji.custom_names().count(),
            recent: recent.to_vec(),
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
    let mut add = false;
    let can_add = workspace.can_add_emoji;
    let reacts = workspace.info.offers(Ability::Reactions);
    let needle = query.trim().to_lowercase();
    // What matches the query, kept until the query or the custom emoji
    // change: filtering and sorting about 1,900 emoji on every frame was
    // most of the picker's cost.
    let found_id = egui::Id::new("emoji-picker-found");
    let custom_count = workspace.emoji.custom_names().count();
    let recent = &app.settings.recent_emoji;
    let mut tone = app.settings.skin_tone;
    let found = ctx
        .data(|d| d.get_temp::<std::sync::Arc<Found>>(found_id))
        .filter(|found| {
            found.needle == needle && found.custom_count == custom_count && found.recent == *recent
        })
        .unwrap_or_else(|| {
            let found = std::sync::Arc::new(Found::new(workspace, &needle, recent));
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
                    // Only browser sessions can; Slack offers no call for
                    // apps.
                    if can_add
                        && theme::icon_button(ui, &palette, Icon::Plus, 16.0, &t("Add emoji…"))
                            .clicked()
                    {
                        add = true;
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
            tone_picker(ui, &palette, &mut tone);
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
                                                crate::emoji::with_tone(emoji, tone),
                                                theme::regular(22.0),
                                                palette.text,
                                            );
                                            crate::emoji::shortcode(emoji).unwrap_or_default()
                                        }
                                    };
                                    let code = crate::emoji::toned(code, tone);
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
    if tone != app.settings.skin_tone {
        app.settings.skin_tone = tone;
        app.settings_changed();
    }
    if close || response.should_close() {
        app.picker = None;
    }
    if add {
        app.actions.push(Action::AddEmoji);
    }
    // What Enter picks gets your tone like a click would.
    let chosen = chosen.map(|name| crate::emoji::toned(&name, tone));
    if let Some(name) = chosen {
        match target {
            PickerTarget::Reaction { channel, ts } => {
                if reacts {
                    app.actions.push(Action::React { channel, ts, name });
                }
            }
            PickerTarget::Draft(key) => {
                let draft = app.drafts.edit(key);
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

/// The skin tones to choose from, as a waving hand in each: the default
/// yellow, then Slack's tones 2 (light) to 6 (dark).
fn tone_picker(ui: &mut egui::Ui, palette: &crate::theme::Palette, tone: &mut u8) {
    let Some(hand) = emojis::get("✋") else {
        return;
    };
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(t("Skin tone"))
                .font(theme::regular(12.5))
                .color(palette.secondary),
        );
        for choice in [0, 2, 3, 4, 5, 6] {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
            let current = crate::emoji::valid_tone(*tone).unwrap_or(0) == choice;
            if current || response.hovered() {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(6),
                    if current {
                        palette.accent.gamma_multiply(0.25)
                    } else {
                        palette.surface_hover
                    },
                );
            }
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                crate::emoji::with_tone(hand, choice),
                theme::regular(17.0),
                palette.text,
            );
            let name = match choice {
                0 => t("Default skin tone"),
                2 => t("Light skin tone"),
                3 => t("Medium-light skin tone"),
                4 => t("Medium skin tone"),
                5 => t("Medium-dark skin tone"),
                _ => t("Dark skin tone"),
            };
            theme::describe_selected(&response, egui::WidgetType::RadioButton, current, &name);
            if response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(name.as_ref())
                .clicked()
            {
                *tone = choice;
            }
        }
    });
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
    let presence = workspace.people.presence(&user_id);
    let external = crate::people::is_external(workspace, &user_id);
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
                let avatar = super::avatar(
                    ui,
                    user.as_ref().and_then(|u| u.avatar.as_deref()),
                    &name,
                    &user_id,
                    72.0,
                );
                super::people::dot(
                    ui.painter(),
                    &palette,
                    avatar.rect,
                    presence,
                    palette.overlay,
                );
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&name)
                            .font(theme::bold(18.0))
                            .color(palette.text),
                    );
                    if let Some(presence) = presence {
                        let color = match presence {
                            crate::people::Presence::Active => super::people::ACTIVE,
                            crate::people::Presence::Away => palette.dim,
                        };
                        ui.label(
                            RichText::new(super::people::word(presence))
                                .font(theme::regular(13.0))
                                .color(color),
                        );
                    }
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
                    if external {
                        super::people::external_tag(ui, &palette, true);
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
                if let Some(line) = user.tz.as_deref().and_then(super::browse::local_time) {
                    ui.label(
                        RichText::new(line)
                            .font(theme::regular(13.0))
                            .color(palette.dim),
                    );
                }
                if user.deleted {
                    ui.label(
                        RichText::new(t("This account is deactivated."))
                            .font(theme::regular(13.0))
                            .color(palette.warning),
                    );
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                // The DM you have, or a new one: Slack opens either.
                let can_message = dm.is_some() || user.as_ref().is_some_and(|u| !u.deleted);
                if can_message && theme::primary_button(ui, &palette, &t("Message")).clicked() {
                    app.actions
                        .push(Action::Convos(crate::convos::Action::Open {
                            users: vec![user_id.clone()],
                        }));
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

fn confirm_delete(app: &mut App, ctx: &egui::Context) {
    let Some(crate::app::MessageDeletion { channel, ts }) = app.confirm_delete.clone() else {
        return;
    };
    let answer = confirm(
        app,
        ctx,
        "confirm-delete",
        &t("Delete message?"),
        &t("This cannot be undone."),
    );
    match answer {
        Some(true) => {
            app.actions.push(Action::Delete { channel, ts });
            app.confirm_delete = None;
        }
        Some(false) => app.confirm_delete = None,
        None => {}
    }
}

/// "Delete sidebar-v2.png?" for your own file.
fn confirm_delete_file(app: &mut App, ctx: &egui::Context) {
    let Some(crate::app::FileDeletion { file, name }) = app.confirm_delete_file.clone() else {
        return;
    };
    let answer = confirm(
        app,
        ctx,
        "confirm-delete-file",
        &crate::i18n::tf("Delete {name}?", &[("name", &name)]),
        &t("This removes it for everyone."),
    );
    match answer {
        Some(true) => {
            app.actions.push(Action::DeleteFile { file, name });
            app.confirm_delete_file = None;
        }
        Some(false) => app.confirm_delete_file = None,
        None => {}
    }
}

/// A dialog asking whether to delete something: `title`, `body`, Cancel
/// and a red Delete. Answers once a choice is made; Enter deletes and
/// Escape or a click outside cancels.
fn confirm(app: &App, ctx: &egui::Context, id: &str, title: &str, body: &str) -> Option<bool> {
    let palette = app.palette;
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new(id))
        .frame(modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(
                RichText::new(title)
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(body)
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
    answer
}

/// The question an app asked to have put before its button is pressed,
/// in the app's words where it gave them.
fn confirm_press(app: &mut App, ctx: &egui::Context) {
    let Some(crate::app::PressConfirmation {
        press,
        confirm,
        link,
    }) = app.confirm_press.clone()
    else {
        return;
    };
    let palette = app.palette;
    let mut answer = None;
    let title = confirm
        .title
        .as_deref()
        .map(crate::mrkdwn::unescape)
        .unwrap_or_else(|| t("Are you sure?").into_owned());
    let go = confirm
        .confirm
        .as_deref()
        .map(crate::mrkdwn::unescape)
        .unwrap_or_else(|| t("Yes").into_owned());
    let back = confirm
        .deny
        .as_deref()
        .map(crate::mrkdwn::unescape)
        .unwrap_or_else(|| t("Cancel").into_owned());
    let mut actions = Vec::new();
    let response = egui::Modal::new(egui::Id::new("confirm-press"))
        .frame(modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(
                RichText::new(title)
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            if let Some(text) = &confirm.text
                && let Some(workspace) = app.active_workspace()
            {
                let rich = crate::ui::rich::Rich::new(&palette, workspace)
                    .size(14.0)
                    .color(palette.secondary);
                crate::ui::rich::show(ui, &rich, text, false, &mut actions);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &back).clicked() {
                    answer = Some(false);
                }
                let (fill, color) = if confirm.style.as_deref() == Some("danger") {
                    (palette.danger, egui::Color32::WHITE)
                } else {
                    (palette.accent, palette.on_accent)
                };
                let yes =
                    egui::Button::new(RichText::new(go).font(theme::medium(14.0)).color(color))
                        .fill(fill)
                        .min_size(Vec2::new(0.0, 32.0));
                if ui.add(yes).clicked() {
                    answer = Some(true);
                }
            });
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                answer = Some(true);
            }
        });
    // Links in the app's text still open.
    app.actions.extend(actions);
    if response.should_close() {
        answer = Some(false);
    }
    match answer {
        Some(true) => {
            app.confirm_press = None;
            app.actions.push(Action::PressButton {
                press: Box::new(press),
                confirm: Some(confirm),
                confirmed: true,
                link,
            });
        }
        Some(false) => app.confirm_press = None,
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
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
            sign_in: Default::default(),
            scopes: None,
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
        let all = Found::new(&w, "", &[]);
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
        assert_eq!(
            Found::new(&w, "taco", &[]).first.as_deref(),
            Some("tacocat")
        );
        let rocket = Found::new(&w, "rocket", &[]);
        assert_eq!(rocket.first.as_deref(), Some("rocket"));
        assert!(rocket.groups.iter().all(|g| g.label != "Acme"));
        assert!(
            Found::new(&w, "no-such-emoji-at-all", &[])
                .groups
                .is_empty()
        );
    }

    #[test]
    fn recently_used_emoji_lead_until_you_search() {
        let w = workspace();
        let recent = ["tacocat".to_owned(), "+1".to_owned(), "gone-now".to_owned()];
        let found = Found::new(&w, "", &recent);
        assert_eq!(found.groups[0].label, "Recently used");
        assert_eq!(found.groups[0].cells.len(), 2, "unknown names drop out");
        assert!(matches!(found.groups[0].cells[0], Cell::Custom { .. }));
        assert!(matches!(found.groups[0].cells[1], Cell::Standard(_)));
        let searched = Found::new(&w, "taco", &recent);
        assert!(searched.groups.iter().all(|g| g.label != "Recently used"));
    }
}
