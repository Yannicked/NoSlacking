//! The views at the top of the sidebar (Activity) and the pane each shows in
//! place of the conversation.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use super::rich::{self, Rich};
use super::rows;
use crate::app::{App, WorkspaceState};
use crate::i18n::{t, tf};
use crate::model::{Action, ConversationKind, Message, Ts};
use crate::theme::{self, Icon, Palette};
use crate::views::{Action as Views, Activity, State, TeamViews, View};

/// Where [`super::show`] says whether a view covers the conversation, so
/// the sidebar does not light up the conversation underneath.
pub fn open_id() -> egui::Id {
    egui::Id::new("view-open")
}

/// The icon of a view, in the sidebar and its header.
fn icon(view: View) -> Icon {
    match view {
        View::Activity => Icon::AtSign,
    }
}

/// What a view's sidebar row counts: unread activity.
fn count(view: View, _workspace: &WorkspaceState, views: Option<&TeamViews>) -> usize {
    match view {
        View::Activity => views.map_or(0, TeamViews::unread_activity),
    }
}

/// The views' rows at the top of the conversation list.
pub fn entries(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    state: &State,
    actions: &mut Vec<Action>,
) {
    ui.add_space(6.0);
    let views = state.team(&workspace.info.team_id);
    for view in View::ALL {
        let selected = state.open == Some(view);
        let count = count(view, workspace, views);
        let (outer, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
        let rect = outer.shrink2(Vec2::new(8.0, 0.0));
        if selected {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(theme::RADIUS_SMALL + 2),
                palette.accent,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(theme::RADIUS_SMALL + 2),
                palette.surface_hover,
            );
        }
        let color = if selected {
            palette.on_accent
        } else if count > 0 {
            palette.text
        } else {
            palette.secondary
        };
        icon(view).image(color, 15.0).paint_at(
            ui,
            egui::Rect::from_center_size(
                egui::pos2(rect.left() + 18.0, rect.center().y),
                Vec2::splat(16.0),
            ),
        );
        let label = view.label();
        let font = if count > 0 {
            theme::bold(14.5)
        } else {
            theme::regular(14.5)
        };
        ui.painter().text(
            egui::pos2(rect.left() + 34.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            &label,
            font,
            color,
        );
        if count > 0 && !selected {
            let mut badge = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(egui::Rect::from_min_max(
                        egui::pos2(rect.right() - 40.0, rect.top()),
                        rect.right_bottom() - Vec2::new(4.0, 0.0),
                    ))
                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
            );
            super::badge(
                &mut badge,
                palette,
                u32::try_from(count).unwrap_or(u32::MAX),
            );
        }
        theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL + 2);
        let spoken = if count > 0 {
            tf(
                "{name}, {count} new",
                &[("name", &label), ("count", &count.to_string())],
            )
        } else {
            label.clone()
        };
        theme::describe_selected(
            &response,
            egui::WidgetType::SelectableLabel,
            selected,
            &spoken,
        );
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            actions.push(Action::Views(if selected {
                Views::Close
            } else {
                Views::Open(view)
            }));
        }
    }
    ui.add_space(4.0);
    let rect = ui.available_rect_before_wrap();
    ui.painter().hline(
        (rect.left() + 16.0)..=(rect.right() - 16.0),
        rect.top(),
        Stroke::new(1.0, palette.outline),
    );
    ui.add_space(2.0);
}

/// Shortcuts that open the views, as Slack's: Ctrl+Shift+M for Activity.
pub fn keys(app: &mut App, ctx: &egui::Context) {
    if app.overlay_open() || app.workspaces.is_empty() {
        return;
    }
    let shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
    let pressed = ctx.input_mut(|input| {
        View::ALL
            .into_iter()
            .find(|view| input.consume_key(shift, shortcut(*view)))
    });
    if let Some(view) = pressed {
        app.actions.push(Action::Views(Views::Open(view)));
    }
}

/// The letter that opens a view with Ctrl+Shift.
fn shortcut(view: View) -> egui::Key {
    match view {
        View::Activity => egui::Key::M,
    }
}

/// The open view, in place of the conversation.
pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let Some(view) = app.views.open else {
        return;
    };
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            let App {
                workspaces,
                settings,
                actions,
                views,
                ..
            } = app;
            let Some(workspace) = crate::app::active_in(workspaces, settings) else {
                return;
            };
            let team = workspace.info.team_id.clone();
            let data = views.team_mut(&team);
            header(ui, &palette, view, data, actions);
            match view {
                View::Activity => activity(ui, &palette, workspace, data, actions),
            }
        });
}

/// The view's name, a refresh button and a close button.
fn header(
    ui: &mut egui::Ui,
    palette: &Palette,
    view: View,
    data: &TeamViews,
    actions: &mut Vec<Action>,
) {
    let inset = theme::titlebar_inset(ui.ctx());
    let loading = match view {
        View::Activity => data.activity.loading,
    };
    egui::Panel::top("view-header")
        .exact_size(52.0 + inset)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(palette.window)
                .inner_margin(Margin {
                    left: 20,
                    right: 12,
                    top: inset as i8,
                    bottom: 0,
                }),
        )
        .show(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().hline(
                rect.x_range(),
                rect.bottom() - 0.5,
                Stroke::new(1.0, palette.outline),
            );
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (spot, _) = ui.allocate_exact_size(Vec2::splat(17.0), Sense::hover());
                icon(view).image(palette.secondary, 17.0).paint_at(ui, spot);
                ui.label(
                    RichText::new(view.label())
                        .font(theme::bold(17.0))
                        .color(palette.text),
                );
                if loading {
                    ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, palette, Icon::X, 16.0, &t("Close")).clicked() {
                        actions.push(Action::Views(Views::Close));
                    }
                    if theme::icon_button(ui, palette, Icon::Refresh, 15.0, &t("Refresh")).clicked()
                    {
                        actions.push(Action::Views(Views::Refresh));
                    }
                });
            });
        });
}

/// A line in the middle of the pane: nothing to list, or why not.
fn note(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(text)
                .font(theme::regular(15.0))
                .color(palette.dim),
        );
    });
}

/// A spinner while the first answer is on its way, the error if the last
/// one failed, or nothing.
fn status(ui: &mut egui::Ui, palette: &Palette, waiting: bool, error: Option<&str>) {
    if waiting {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.add(egui::Spinner::new().size(20.0).color(palette.dim));
        });
    } else if let Some(error) = error {
        egui::Frame::new()
            .fill(palette.danger.gamma_multiply(0.12))
            .inner_margin(Margin::symmetric(20, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(tf("Could not load this list: {error}", &[("error", error)]))
                        .font(theme::regular(13.0))
                        .color(palette.text),
                );
            });
    }
}

/// What a conversation is called in a list: "#name", or the person.
pub fn place(workspace: &WorkspaceState, channel: &str) -> String {
    match workspace.conversation(channel) {
        Some(c) if c.kind == ConversationKind::Direct => workspace.title(c),
        Some(c) if c.kind == ConversationKind::Group => c.name.clone(),
        Some(c) => format!("#{}", c.name),
        None => t("a conversation").into_owned(),
    }
}

/// The time of a message in a list that spans days: "Today at 14:03".
pub fn when(ts: &Ts) -> String {
    tf(
        "{date} at {time}",
        &[
            ("date", &super::day_label(ts)),
            ("time", &super::short_time(ts)),
        ],
    )
}

/// Where a click on a message takes you: the message in its
/// conversation, and for a reply its thread beside it.
pub fn jump(channel: &str, message: &Message) -> Action {
    Action::JumpTo {
        channel: channel.to_owned(),
        ts: message.ts.clone(),
        thread: message.thread_ts.clone().filter(|_| message.is_reply()),
    }
}

/// A list of rows of different heights, of which only those in and near
/// the view are laid out. `keys` tells the rows apart and `guesses` says
/// about how tall each is before it is drawn.
fn list(
    ui: &mut egui::Ui,
    salt: &str,
    keys: &[u64],
    guesses: &[f32],
    mut draw: impl FnMut(&mut egui::Ui, usize),
) {
    let heights_id = egui::Id::new(("view-heights", salt));
    let mut heights: rows::Heights = ui
        .data_mut(|d| d.remove_temp(heights_id))
        .unwrap_or_default();
    egui::ScrollArea::vertical()
        .id_salt(("view", salt))
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let entries: Vec<rows::Entry> = keys
                .iter()
                .zip(guesses)
                .map(|(key, guess)| rows::Entry {
                    key: *key,
                    guess: *guess,
                })
                .collect();
            let plan = rows::plan(
                entries.iter().map(|entry| heights.planned(entry)),
                viewport.min.y,
                viewport.max.y,
                400.0,
            );
            heights.sweep();
            rows::show(ui, &mut heights, &entries, &plan, &mut draw);
        });
    ui.data_mut(|d| d.insert_temp(heights_id, heights));
}

/// A row key from anything hashable.
fn key(value: impl std::hash::Hash + std::fmt::Debug) -> u64 {
    egui::Id::new(value).value()
}

/// About how tall a message card is before it is drawn.
fn card_guess(message: &Message) -> f32 {
    76.0 + (message.text.len() / 90) as f32 * 20.0
}

/// One message as a card: where and why above it, then who wrote it and
/// what it says. A click on the card shows the message in context.
#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    channel: &str,
    message: &Message,
    above: &str,
    unread: bool,
    actions: &mut Vec<Action>,
    buttons: impl FnOnce(&mut egui::Ui, &mut Vec<Action>),
) {
    let background = ui.painter().add(egui::Shape::Noop);
    let author = workspace.author(message);
    let mut inner_actions = Vec::new();
    let inner = egui::Frame::new()
        .inner_margin(Margin::symmetric(20, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if unread {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, palette.accent);
                }
                ui.label(
                    RichText::new(above)
                        .font(theme::semibold(12.5))
                        .color(palette.secondary),
                );
                ui.label(
                    RichText::new(when(&message.ts))
                        .font(theme::regular(12.0))
                        .color(palette.dim),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    buttons(ui, &mut inner_actions);
                });
            });
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                super::avatar(
                    ui,
                    workspace.author_icon(message),
                    &author,
                    message.user.as_deref().unwrap_or(&author),
                    32.0,
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new(&author)
                            .font(theme::bold(14.0))
                            .color(palette.text),
                    );
                    let rich = Rich::new(palette, workspace).size(14.0);
                    let mut ignored = Vec::new();
                    rich::show(ui, &rich, &message.text, message.edited, &mut ignored);
                });
            });
        });
    let rect = inner.response.rect;
    let response = ui
        .interact(rect, ui.id().with("card"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.hovered() {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(
                rect,
                CornerRadius::ZERO,
                palette.surface.gamma_multiply(0.6),
            ),
        );
    }
    ui.painter().hline(
        (rect.left() + 20.0)..=(rect.right() - 20.0),
        rect.bottom(),
        Stroke::new(1.0, palette.outline.gamma_multiply(0.6)),
    );
    theme::describe(
        &response,
        egui::WidgetType::Button,
        &format!("{above}: {author}"),
    );
    // A button's own click comes first; the card's means "show it".
    if !inner_actions.is_empty() {
        actions.extend(inner_actions);
    } else if response.clicked() {
        actions.push(jump(channel, message));
    }
}

/// Mentions of you and everyone, and replies to your threads.
fn activity(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    data: &TeamViews,
    actions: &mut Vec<Action>,
) {
    status(
        ui,
        palette,
        data.activity.waiting(),
        data.activity.error.as_deref(),
    );
    if data.searched {
        egui::Frame::new()
            .inner_margin(Margin::symmetric(20, 6))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(t(
                        "Found by searching for your name; replies to your threads show as they arrive.",
                    ))
                    .font(theme::regular(12.5))
                    .color(palette.dim),
                );
            });
    }
    let items: Vec<&Activity> = data.activity();
    if items.is_empty() {
        if !data.activity.waiting() {
            note(ui, palette, &t("Nothing new mentions you."));
        }
        return;
    }
    let keys: Vec<u64> = items
        .iter()
        .map(|a| key((&a.channel, a.message.ts.as_str())))
        .collect();
    let guesses: Vec<f32> = items.iter().map(|a| card_guess(&a.message)).collect();
    list(ui, "activity", &keys, &guesses, |ui, index| {
        let item = items[index];
        let above = item.reason.label(&place(workspace, &item.channel));
        card(
            ui,
            palette,
            workspace,
            &item.channel,
            &item.message,
            &above,
            item.unread,
            actions,
            |_, _| {},
        );
    });
}
