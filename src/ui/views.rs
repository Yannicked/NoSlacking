//! The views at the top of the sidebar (All unreads, Threads, Activity,
//! Later, Scheduled) and the pane each shows in place of the conversation,
//! and the composer's "Send later" menu and dialog.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use super::rich::{self, Rich};
use super::rows;
use crate::app::{App, WorkspaceState};
use crate::failure::Failure;
use crate::i18n::{t, tf, tn};
use crate::model::{Ability, Action, ConversationKind, Message, Ts, Workspace};
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
        View::Unreads => Icon::Inbox,
        View::Threads => Icon::Messages,
        View::Later => Icon::Bookmark,
        View::Scheduled => Icon::Clock,
    }
}

/// Where [`super::show`] leaves the messages saved for later in the open
/// workspace, for the message toolbars.
pub fn saved_id() -> egui::Id {
    egui::Id::new("saved-for-later")
}

/// Whether a message is saved for later, as far as is known.
pub fn is_saved(ui: &egui::Ui, channel: &str, ts: &Ts) -> bool {
    ui.data(|d| d.get_temp::<std::sync::Arc<std::collections::HashSet<(String, Ts)>>>(saved_id()))
        .is_some_and(|saved| saved.contains(&(channel.to_owned(), ts.clone())))
}

/// What a view's sidebar row counts: unread activity, unread
/// conversations.
fn count(view: View, workspace: &WorkspaceState, views: Option<&TeamViews>) -> usize {
    match view {
        View::Activity => views.map_or(0, TeamViews::unread_activity),
        View::Unreads => workspace
            .conversations
            .iter()
            .filter(|c| !c.archived && workspace.is_unread(c))
            .count(),
        View::Threads => views.map_or(0, TeamViews::unread_threads),
        // A reminder that is due shows in Slackbot's messages already.
        View::Later => 0,
        // What waits to be sent is no news.
        View::Scheduled => 0,
    }
}

/// The views `workspace` has, in the sidebar's order: none where its
/// service has no views, and Threads, Later and Scheduled only where it
/// has threads, saving for later and scheduling.
pub fn offered(workspace: &Workspace) -> Vec<View> {
    if !workspace.offers(Ability::Views) {
        return Vec::new();
    }
    View::ALL
        .into_iter()
        .filter(|view| match view {
            View::Threads => workspace.offers(Ability::Threads),
            View::Later => workspace.offers(Ability::Later),
            View::Scheduled => workspace.offers(Ability::Scheduled),
            View::Activity | View::Unreads => true,
        })
        .collect()
}

/// The views' rows at the top of the conversation list.
pub fn entries(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    state: &State,
    actions: &mut Vec<Action>,
) {
    let offered = offered(&workspace.info);
    // No rows, and no rule under them either.
    if offered.is_empty() {
        return;
    }
    ui.add_space(6.0);
    let views = state.team(&workspace.info.team_id);
    for view in offered {
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
            super::badge(&mut badge, palette, crate::i18n::count(count));
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

/// Shortcuts that open the views, as Slack's: Ctrl+Shift+M for Activity,
/// Ctrl+Shift+A for All unreads, Ctrl+Shift+T for Threads, Ctrl+Shift+S
/// for Later.
pub fn keys(app: &mut App, ctx: &egui::Context) {
    if app.overlay_open() || app.workspaces.is_empty() {
        return;
    }
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    let offered = offered(&workspace.info);
    let shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
    let pressed = ctx.input_mut(|input| {
        offered
            .into_iter()
            .find(|view| shortcut(*view).is_some_and(|key| input.consume_key(shift, key)))
    });
    if let Some(view) = pressed {
        app.actions.push(Action::Views(Views::Open(view)));
    }
}

/// The letter that opens a view with Ctrl+Shift, if one does.
pub(super) fn shortcut(view: View) -> Option<egui::Key> {
    match view {
        View::Activity => Some(egui::Key::M),
        View::Unreads => Some(egui::Key::A),
        View::Threads => Some(egui::Key::T),
        View::Later => Some(egui::Key::S),
        View::Scheduled => None,
    }
}

/// The open view, in place of the conversation.
pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let Some(view) = app.views.open else {
        return;
    };
    // A view left open from a workspace that has it closes in one that
    // does not.
    if !app
        .active_workspace()
        .is_some_and(|w| offered(&w.info).contains(&view))
    {
        app.actions.push(Action::Views(Views::Close));
        return;
    }
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
                View::Unreads => unreads(ui, &palette, workspace, data, actions),
                View::Threads => threads(ui, &palette, workspace, data, actions),
                View::Later => later(ui, &palette, workspace, data, actions),
                View::Scheduled => scheduled(ui, &palette, workspace, data, actions),
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
        View::Unreads => data.unread.values().any(|f| f.loading),
        View::Threads => data.threads.loading,
        View::Later => data.saved.loading || data.reminders.loading,
        View::Scheduled => data.scheduled.loading,
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
fn status(ui: &mut egui::Ui, palette: &Palette, waiting: bool, error: Option<&Failure>) {
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
                    RichText::new(tf(
                        "Could not load this list: {error}",
                        &[("error", &error.message())],
                    ))
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

/// What a [`card`] shows, and what a click on it asks for.
struct Card<'a> {
    /// The message itself.
    message: &'a Message,
    /// Where and why, said above the author, if that is said.
    above: Option<&'a str>,
    /// Whether it wears an unread dot.
    unread: bool,
    /// What a click on the card asks for (showing the message in
    /// context, say).
    open: Action,
}

/// One message as a card: where and why above it, if that is said, then
/// who wrote it and what it says, with `buttons` on the right. A click on
/// the card asks for its `open`; the buttons, links and mentions inside
/// it take their own clicks.
fn card(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    card: Card<'_>,
    actions: &mut Vec<Action>,
    buttons: impl FnOnce(&mut egui::Ui, &mut Vec<Action>),
) {
    let background = ui.painter().add(egui::Shape::Noop);
    let author = workspace.author(card.message);
    // Sensed before its contents are laid out, so they sit on top of it.
    let scope = ui.scope_builder(
        egui::UiBuilder::new().sense(Sense::click()).id_salt("card"),
        |ui| {
            egui::Frame::new()
                .inner_margin(Margin::symmetric(20, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    card_contents(ui, palette, workspace, &card, &author, actions, buttons);
                });
        },
    );
    let rect = scope.response.rect;
    let response = scope
        .response
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if ui.rect_contains_pointer(rect) {
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
    let spoken = match card.above {
        Some(above) => format!("{above}: {author}"),
        None => author.clone(),
    };
    theme::describe(&response, egui::WidgetType::Button, &spoken);
    if response.clicked() {
        actions.push(card.open);
    }
}

/// What a [`card`] holds, `author` being who wrote its message.
fn card_contents(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    card: &Card<'_>,
    author: &str,
    actions: &mut Vec<Action>,
    buttons: impl FnOnce(&mut egui::Ui, &mut Vec<Action>),
) {
    let Card {
        message,
        above,
        unread,
        ..
    } = *card;
    let time = |ui: &mut egui::Ui| {
        ui.label(
            RichText::new(when(&message.ts))
                .font(theme::regular(12.0))
                .color(palette.dim),
        )
        .on_hover_text(super::full_time(&message.ts).unwrap_or_default());
    };
    let mut buttons = Some(buttons);
    if let Some(above) = above {
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
            time(ui);
            if let Some(buttons) = buttons.take() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    buttons(ui, actions);
                });
            }
        });
        ui.add_space(4.0);
    }
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        super::avatar(
            ui,
            workspace.author_icon(message),
            author,
            message.user.as_deref().unwrap_or(author),
            32.0,
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if unread && above.is_none() {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, palette.accent);
                }
                ui.label(
                    RichText::new(author)
                        .font(theme::bold(14.0))
                        .color(palette.text),
                );
                if above.is_none() {
                    time(ui);
                }
                if let Some(buttons) = buttons.take() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        buttons(ui, actions);
                    });
                }
            });
            let rich = Rich::new(palette, workspace).size(14.0);
            rich::message(ui, &rich, message, message.edited, actions);
        });
    });
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
        data.activity.error.as_ref(),
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
            Card {
                message: &item.message,
                above: Some(&above),
                unread: item.unread,
                open: jump(&item.channel, &item.message),
            },
            actions,
            |_, _| {},
        );
    });
}

/// A row of the unreads list.
#[derive(Clone, Copy)]
enum UnreadRow {
    /// A conversation's name and its "Mark as read".
    Head(usize),
    /// One of its unread messages.
    Message(usize, usize),
    /// Its messages on their way, or why they are not.
    Status(usize),
    /// A link to the conversation for the messages not loaded.
    More(usize),
}

/// Every conversation with something new, and the new messages in each.
fn unreads(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    data: &TeamViews,
    actions: &mut Vec<Action>,
) {
    let conversations = crate::views::unread_conversations(workspace);
    if conversations.is_empty() {
        note(ui, palette, &t("You are all caught up."));
        return;
    }
    egui::Frame::new()
        .inner_margin(Margin::symmetric(20, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tn(
                        "{count} conversation with unread messages",
                        "{count} conversations with unread messages",
                        crate::i18n::count(conversations.len()),
                    ))
                    .font(theme::regular(13.0))
                    .color(palette.secondary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::secondary_button(ui, palette, &t("Mark all as read")).clicked() {
                        actions.push(Action::Views(Views::MarkAllRead));
                    }
                });
            });
        });
    let mut rows = Vec::new();
    for (index, conversation) in conversations.iter().enumerate() {
        rows.push(UnreadRow::Head(index));
        match data
            .unread
            .get(&conversation.id)
            .and_then(|f| f.value.as_ref())
        {
            Some((messages, more)) => {
                rows.extend((0..messages.len()).map(|m| UnreadRow::Message(index, m)));
                // Nothing new outside threads, or more than was read.
                if *more || messages.is_empty() {
                    rows.push(UnreadRow::More(index));
                }
            }
            None => rows.push(UnreadRow::Status(index)),
        }
    }
    let messages = |index: usize| {
        data.unread
            .get(&conversations[index].id)
            .and_then(|f| f.value.as_ref())
            .map(|(messages, _)| messages.as_slice())
            .unwrap_or_default()
    };
    let keys: Vec<u64> = rows
        .iter()
        .map(|row| match *row {
            UnreadRow::Head(c) => key(("head", &conversations[c].id)),
            UnreadRow::Message(c, m) => key((&conversations[c].id, messages(c)[m].ts.as_str())),
            UnreadRow::Status(c) => key(("status", &conversations[c].id)),
            UnreadRow::More(c) => key(("more", &conversations[c].id)),
        })
        .collect();
    let guesses: Vec<f32> = rows
        .iter()
        .map(|row| match *row {
            UnreadRow::Head(_) => 52.0,
            UnreadRow::Message(c, m) => card_guess(&messages(c)[m]) - 20.0,
            UnreadRow::Status(_) | UnreadRow::More(_) => 34.0,
        })
        .collect();
    list(ui, "unreads", &keys, &guesses, |ui, index| {
        match rows[index] {
            UnreadRow::Head(c) => unread_head(ui, palette, workspace, conversations[c], actions),
            UnreadRow::Message(c, m) => card(
                ui,
                palette,
                workspace,
                Card {
                    message: &messages(c)[m],
                    above: None,
                    unread: false,
                    open: jump(&conversations[c].id, &messages(c)[m]),
                },
                actions,
                |_, _| {},
            ),
            UnreadRow::Status(c) => {
                let channel = &conversations[c].id;
                let fetch = data.unread.get(channel);
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(20, 8))
                    .show(ui, |ui| match fetch {
                        Some(fetch) if fetch.loading => {
                            ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                        }
                        Some(fetch) => {
                            let error = fetch.error.as_ref().map(Failure::message);
                            ui.label(
                                RichText::new(tf(
                                    "Could not load the messages: {error}",
                                    &[("error", &error.unwrap_or_default())],
                                ))
                                .font(theme::regular(13.0))
                                .color(palette.danger),
                            );
                        }
                        // Scrolled to before its messages were asked for.
                        None => {
                            actions.push(Action::Views(Views::LoadUnread {
                                channel: channel.clone(),
                            }));
                            ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                        }
                    });
            }
            UnreadRow::More(c) => {
                let label = if messages(c).is_empty() {
                    t("Open the conversation")
                } else {
                    t("Open the conversation for the rest")
                };
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(20, 8))
                    .show(ui, |ui| {
                        if ui.link(label).clicked() {
                            actions.push(Action::OpenConversation(conversations[c].id.clone()));
                        }
                    });
            }
        }
    });
}

/// The heading of a conversation in the unreads list.
fn unread_head(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &crate::model::Conversation,
    actions: &mut Vec<Action>,
) {
    ui.add_space(10.0);
    egui::Frame::new()
        .fill(palette.surface)
        .inner_margin(Margin::symmetric(20, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let name = place(workspace, &conversation.id);
                let title = ui
                    .add(
                        egui::Label::new(
                            RichText::new(&name)
                                .font(theme::bold(15.0))
                                .color(palette.text),
                        )
                        .sense(Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t("Open the conversation"));
                if title.clicked() {
                    actions.push(Action::OpenConversation(conversation.id.clone()));
                }
                if conversation.mentions > 0 {
                    super::badge(ui, palette, conversation.mentions);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, palette, Icon::CheckCheck, 15.0, &t("Mark as read"))
                        .clicked()
                    {
                        actions.push(Action::Views(Views::MarkRead {
                            channel: conversation.id.clone(),
                        }));
                    }
                });
            });
        });
}

/// A row of the threads list.
#[derive(Clone, Copy)]
enum ThreadRow {
    /// Where the thread is, and how much is new in it.
    Head(usize),
    /// The message that started it.
    Parent(usize),
    /// One of its newest replies.
    Reply(usize, usize),
    /// How many replies there are, and a way into the thread.
    Foot(usize),
}

/// The threads you follow, the newest reply first, each with its newest
/// replies. A click opens the thread beside the list.
fn threads(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    data: &TeamViews,
    actions: &mut Vec<Action>,
) {
    status(
        ui,
        palette,
        data.threads.waiting(),
        data.threads.error.as_ref(),
    );
    if data.threads_searched {
        egui::Frame::new()
            .inner_margin(Margin::symmetric(20, 6))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(t("Threads you replied in, found by searching."))
                        .font(theme::regular(12.5))
                        .color(palette.dim),
                );
            });
    }
    let threads = data.threads.value.as_deref().unwrap_or_default();
    if threads.is_empty() {
        if !data.threads.waiting() {
            note(
                ui,
                palette,
                &t("No threads yet. Reply in one to follow it."),
            );
        }
        return;
    }
    let mut rows = Vec::new();
    for (index, thread) in threads.iter().enumerate() {
        rows.push(ThreadRow::Head(index));
        rows.push(ThreadRow::Parent(index));
        rows.extend((0..thread.replies.len()).map(|r| ThreadRow::Reply(index, r)));
        rows.push(ThreadRow::Foot(index));
    }
    let thread_key = |t: &crate::views::Followed| (t.channel.clone(), t.parent.ts.0.clone());
    let keys: Vec<u64> = rows
        .iter()
        .map(|row| match *row {
            ThreadRow::Head(i) => key(("head", thread_key(&threads[i]))),
            ThreadRow::Parent(i) => key(("parent", thread_key(&threads[i]))),
            ThreadRow::Reply(i, r) => key(("reply", threads[i].replies[r].ts.as_str())),
            ThreadRow::Foot(i) => key(("foot", thread_key(&threads[i]))),
        })
        .collect();
    let guesses: Vec<f32> = rows
        .iter()
        .map(|row| match *row {
            ThreadRow::Head(_) => 52.0,
            ThreadRow::Parent(i) => card_guess(&threads[i].parent) - 20.0,
            ThreadRow::Reply(i, r) => card_guess(&threads[i].replies[r]) - 20.0,
            ThreadRow::Foot(_) => 36.0,
        })
        .collect();
    let open = |thread: &crate::views::Followed| {
        Action::Views(Views::OpenThread {
            channel: thread.channel.clone(),
            ts: thread.parent.ts.clone(),
        })
    };
    list(ui, "threads", &keys, &guesses, |ui, index| {
        match rows[index] {
            ThreadRow::Head(i) => {
                let thread = &threads[i];
                ui.add_space(10.0);
                egui::Frame::new()
                    .fill(palette.surface)
                    .inner_margin(Margin::symmetric(20, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.label(
                                RichText::new(place(workspace, &thread.channel))
                                    .font(theme::bold(15.0))
                                    .color(palette.text),
                            );
                            if thread.unread > 0 {
                                ui.label(
                                    RichText::new(tn(
                                        "{count} new reply",
                                        "{count} new replies",
                                        thread.unread,
                                    ))
                                    .font(theme::semibold(12.5))
                                    .color(palette.accent),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if theme::icon_button(
                                        ui,
                                        palette,
                                        Icon::Reply,
                                        15.0,
                                        &t("Reply"),
                                    )
                                    .clicked()
                                    {
                                        actions.push(open(thread));
                                    }
                                },
                            );
                        });
                    });
            }
            ThreadRow::Parent(i) => {
                let thread = &threads[i];
                card(
                    ui,
                    palette,
                    workspace,
                    Card {
                        message: &thread.parent,
                        above: None,
                        unread: false,
                        open: open(thread),
                    },
                    actions,
                    |_, _| {},
                );
            }
            ThreadRow::Reply(i, r) => {
                let thread = &threads[i];
                // The last `unread` replies are the new ones.
                let new = r + usize::try_from(thread.unread).unwrap_or(usize::MAX)
                    >= thread.replies.len();
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    ui.vertical(|ui| {
                        card(
                            ui,
                            palette,
                            workspace,
                            Card {
                                message: &thread.replies[r],
                                above: None,
                                unread: new,
                                open: open(thread),
                            },
                            actions,
                            |_, _| {},
                        );
                    });
                });
            }
            ThreadRow::Foot(i) => {
                let thread = &threads[i];
                let shown = crate::i18n::count(thread.replies.len());
                let total = thread.parent.reply_count.max(shown);
                egui::Frame::new()
                    .inner_margin(Margin {
                        left: 48,
                        right: 20,
                        top: 6,
                        bottom: 8,
                    })
                    .show(ui, |ui| {
                        let label = if total > shown {
                            tn("See all {count} reply", "See all {count} replies", total)
                        } else {
                            t("Open the thread").into_owned()
                        };
                        if ui.link(label).clicked() {
                            actions.push(open(thread));
                        }
                    });
            }
        }
    });
}

/// A row of the Later list.
#[derive(Clone, Copy)]
enum LaterRow {
    /// "Saved messages" or "Reminders".
    Heading(bool),
    Saved(usize),
    Reminder(usize),
    /// A list with nothing in it, or still on its way.
    Empty(bool),
}

/// Messages saved for later, then your reminders.
fn later(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    data: &TeamViews,
    actions: &mut Vec<Action>,
) {
    let errors = [
        data.saved.error.as_ref().map(|error| {
            tf(
                "Could not load the saved messages: {error}",
                &[("error", &error.message())],
            )
        }),
        data.reminders.error.as_ref().map(|error| {
            tf(
                "Could not load the reminders: {error}",
                &[("error", &error.message())],
            )
        }),
    ];
    for error in errors.into_iter().flatten() {
        egui::Frame::new()
            .fill(palette.danger.gamma_multiply(0.12))
            .inner_margin(Margin::symmetric(20, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(&error)
                        .font(theme::regular(13.0))
                        .color(palette.text),
                );
            });
    }
    let saved = data.saved.value.as_deref().unwrap_or_default();
    let reminders = data.reminders.value.as_deref().unwrap_or_default();
    let mut rows = vec![LaterRow::Heading(true)];
    if saved.is_empty() {
        rows.push(LaterRow::Empty(true));
    }
    rows.extend((0..saved.len()).map(LaterRow::Saved));
    rows.push(LaterRow::Heading(false));
    if reminders.is_empty() {
        rows.push(LaterRow::Empty(false));
    }
    rows.extend((0..reminders.len()).map(LaterRow::Reminder));
    let keys: Vec<u64> = rows
        .iter()
        .map(|row| match *row {
            LaterRow::Heading(first) => key(("heading", first)),
            LaterRow::Saved(i) => key((&saved[i].channel, saved[i].message.ts.as_str())),
            LaterRow::Reminder(i) => key(("reminder", &reminders[i].id)),
            LaterRow::Empty(first) => key(("empty", first)),
        })
        .collect();
    let guesses: Vec<f32> = rows
        .iter()
        .map(|row| match *row {
            LaterRow::Saved(i) => card_guess(&saved[i].message),
            LaterRow::Reminder(_) => 56.0,
            LaterRow::Heading(_) | LaterRow::Empty(_) => 40.0,
        })
        .collect();
    list(ui, "later", &keys, &guesses, |ui, index| {
        match rows[index] {
            LaterRow::Heading(first) => {
                ui.add_space(12.0);
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(20, 4))
                    .show(ui, |ui| {
                        let text = match (first, data.starred) {
                            (true, false) => t("Saved for later"),
                            (true, true) => t("Starred messages"),
                            (false, _) => t("Reminders"),
                        };
                        super::section_label(ui, palette, &text);
                    });
            }
            LaterRow::Empty(first) => {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(20, 6))
                    .show(ui, |ui| {
                        let fetch_waiting = if first {
                            data.saved.waiting()
                        } else {
                            data.reminders.waiting()
                        };
                        if fetch_waiting {
                            ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                        } else {
                            let text = if first {
                                t("Nothing saved. Save a message from its ⋯ menu.")
                            } else {
                                t("No reminders. Ask Slack with /remind.")
                            };
                            ui.label(
                                RichText::new(text)
                                    .font(theme::regular(13.5))
                                    .color(palette.dim),
                            );
                        }
                    });
            }
            LaterRow::Saved(i) => {
                let item = &saved[i];
                let above = place(workspace, &item.channel);
                card(
                    ui,
                    palette,
                    workspace,
                    Card {
                        message: &item.message,
                        above: Some(&above),
                        unread: false,
                        open: jump(&item.channel, &item.message),
                    },
                    actions,
                    |ui, actions| {
                        if theme::icon_button(ui, palette, Icon::X, 14.0, &t("Remove from Later"))
                            .clicked()
                        {
                            actions.push(Action::Views(Views::Save {
                                channel: item.channel.clone(),
                                ts: item.message.ts.clone(),
                                save: false,
                            }));
                        }
                    },
                );
            }
            LaterRow::Reminder(i) => reminder_row(ui, palette, workspace, &reminders[i], actions),
        }
    });
}

/// One reminder: what, when, and a button to mark it complete.
fn reminder_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    reminder: &crate::views::Reminder,
    actions: &mut Vec<Action>,
) {
    let inner = egui::Frame::new()
        .inner_margin(Margin::symmetric(20, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (spot, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                Icon::Bell.image(palette.secondary, 16.0).paint_at(ui, spot);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let rich = Rich::new(palette, workspace).size(14.0);
                    rich::show(ui, &rich, &reminder.text, false, actions);
                    let due = reminder
                        .time
                        .map(|seconds| when(&Ts::new(format!("{seconds}.000000"))));
                    let line = match (due, reminder.recurring) {
                        (Some(due), true) => tf("Next on {when}, repeating", &[("when", &due)]),
                        (Some(due), false) => tf("Due {when}", &[("when", &due)]),
                        (None, true) => t("Repeating").into_owned(),
                        (None, false) => String::new(),
                    };
                    if !line.is_empty() {
                        ui.label(
                            RichText::new(line)
                                .font(theme::regular(12.5))
                                .color(palette.dim),
                        );
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if theme::icon_button(ui, palette, Icon::Check, 15.0, &t("Mark as complete"))
                        .clicked()
                    {
                        actions.push(Action::Views(Views::CompleteReminder {
                            id: reminder.id.clone(),
                        }));
                    }
                });
            });
        });
    let rect = inner.response.rect;
    ui.painter().hline(
        (rect.left() + 20.0)..=(rect.right() - 20.0),
        rect.bottom(),
        Stroke::new(1.0, palette.outline.gamma_multiply(0.6)),
    );
}

/// The composer's "Send later" menu; `ready` says there is something to
/// send.
pub fn send_later_menu(
    ui: &mut egui::Ui,
    palette: &Palette,
    ready: bool,
    thread: Option<&Ts>,
    actions: &mut Vec<Action>,
) {
    use crate::views::schedule::When;
    super::section_label(ui, palette, &t("Send later"));
    for (when, label) in [
        (When::HalfHour, t("In 30 minutes")),
        (When::TomorrowMorning, t("Tomorrow at 9:00")),
    ] {
        if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
            actions.push(Action::Views(Views::SendLater {
                thread: thread.cloned(),
                when,
            }));
            ui.close();
        }
    }
    if ui
        .add_enabled(ready, egui::Button::new(t("Custom time…")))
        .clicked()
    {
        actions.push(Action::Views(Views::AskSendLater {
            thread: thread.cloned(),
        }));
        ui.close();
    }
    ui.separator();
    if ui.button(t("See scheduled messages")).clicked() {
        actions.push(Action::Views(Views::Open(View::Scheduled)));
        ui.close();
    }
}

/// The messages waiting to be sent, soonest first, each with a way to
/// change or cancel it.
fn scheduled(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    data: &TeamViews,
    actions: &mut Vec<Action>,
) {
    status(
        ui,
        palette,
        data.scheduled.waiting(),
        data.scheduled.error.as_ref(),
    );
    let items = data.scheduled.value.as_deref().unwrap_or_default();
    if items.is_empty() {
        if !data.scheduled.waiting() {
            note(
                ui,
                palette,
                &t("Nothing scheduled. Pick a time from the clock beside Send."),
            );
        }
        return;
    }
    let keys: Vec<u64> = items.iter().map(|s| key(("scheduled", &s.id))).collect();
    let guesses: Vec<f32> = items
        .iter()
        .map(|s| 70.0 + (s.text.len() / 90) as f32 * 20.0)
        .collect();
    list(ui, "scheduled", &keys, &guesses, |ui, index| {
        let item = &items[index];
        let inner = egui::Frame::new()
            .inner_margin(Margin::symmetric(20, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (spot, _) = ui.allocate_exact_size(Vec2::splat(15.0), Sense::hover());
                    Icon::Clock
                        .image(palette.secondary, 14.0)
                        .paint_at(ui, spot);
                    let line = tf(
                        "To {place}, {when}",
                        &[
                            ("place", &place(workspace, &item.channel)),
                            ("when", &super::moment_label(item.post_at)),
                        ],
                    );
                    ui.label(
                        RichText::new(line)
                            .font(theme::semibold(12.5))
                            .color(palette.secondary),
                    );
                    if item.thread.is_some() {
                        ui.label(
                            RichText::new(t("in a thread"))
                                .font(theme::regular(12.0))
                                .color(palette.dim),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, palette, Icon::Trash, 14.0, &t("Cancel"))
                            .on_hover_text(t("Do not send it"))
                            .clicked()
                        {
                            actions.push(Action::Views(Views::CancelScheduled {
                                channel: item.channel.clone(),
                                id: item.id.clone(),
                            }));
                        }
                        if theme::icon_button(ui, palette, Icon::Pencil, 14.0, &t("Edit")).clicked()
                        {
                            actions.push(Action::Views(Views::EditScheduled {
                                id: item.id.clone(),
                            }));
                        }
                    });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(21.0);
                    ui.vertical(|ui| {
                        let rich = Rich::new(palette, workspace).size(14.0);
                        rich::show(ui, &rich, &item.text, false, actions);
                    });
                });
            });
        let rect = inner.response.rect;
        ui.painter().hline(
            (rect.left() + 20.0)..=(rect.right() - 20.0),
            rect.bottom(),
            Stroke::new(1.0, palette.outline.gamma_multiply(0.6)),
        );
    });
}

/// The "Send at" dialog: a date and a time for a draft, and also the text
/// for a scheduled message being changed.
pub fn dialog(app: &mut App, ctx: &egui::Context) {
    use crate::views::schedule::Target;
    let palette = app.palette;
    let focus = std::mem::take(&mut app.focus_overlay);
    let frame = super::overlays::modal_frame(app);
    let Some(dialog) = app.views.dialog.as_mut() else {
        return;
    };
    let editing = matches!(dialog.target, Target::Edit(_));
    let reminding = matches!(dialog.target, Target::Remind { .. });
    let mut confirm = false;
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("send-at"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(380.0);
            let title = if editing {
                t("Change the scheduled message")
            } else if reminding {
                t("Remind me at a time of your choosing")
            } else {
                t("Send at a time of your choosing")
            };
            ui.label(
                RichText::new(title)
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.add_space(10.0);
            if editing {
                let field = ui.add(
                    egui::TextEdit::multiline(&mut dialog.text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(8, 6)),
                );
                if focus {
                    field.request_focus();
                }
                ui.add_space(8.0);
            }
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    super::section_label(ui, &palette, &t("Date"));
                    let date = ui.add(
                        egui::TextEdit::singleline(&mut dialog.date)
                            .hint_text("2026-03-31")
                            .desired_width(140.0)
                            .margin(Margin::symmetric(8, 5)),
                    );
                    if focus && !editing {
                        date.request_focus();
                    }
                });
                ui.vertical(|ui| {
                    super::section_label(ui, &palette, &t("Time"));
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.time)
                            .hint_text("14:30")
                            .desired_width(90.0)
                            .margin(Margin::symmetric(8, 5)),
                    );
                });
            });
            if let Some(problem) = dialog.problem {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(problem.to_string())
                        .font(theme::regular(13.0))
                        .color(palette.danger),
                );
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    close = true;
                }
                let label = if editing {
                    t("Save")
                } else if reminding {
                    t("Remind me")
                } else {
                    t("Schedule")
                };
                let button = ui.add_enabled_ui(!dialog.busy, |ui| {
                    theme::primary_button(ui, &palette, &label)
                });
                if button.inner.clicked() {
                    confirm = true;
                }
                if dialog.busy {
                    ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                }
            });
            // Enter schedules, except in the text, where it breaks a line.
            if !editing && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                confirm = true;
            }
        });
    if response.should_close() {
        close = true;
    }
    if close {
        app.actions.push(Action::Views(Views::CloseSchedule));
    } else if confirm {
        app.actions.push(Action::Views(Views::ConfirmSchedule));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(service: crate::model::Service) -> Workspace {
        Workspace {
            service,
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
        }
    }

    #[test]
    fn slack_has_every_view_and_teams_none() {
        assert_eq!(
            offered(&workspace(crate::model::Service::Slack)),
            View::ALL.to_vec()
        );
        assert!(offered(&workspace(crate::model::Service::Teams)).is_empty());
    }
}
