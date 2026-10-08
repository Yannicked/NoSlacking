//! Dialogs for starting and finding conversations: "New message", which
//! picks people for a direct message, the channel browser, "Create a
//! channel", the question before leaving one, and adding, editing and
//! removing a channel's bookmarks.
//!
//! Shortcuts: Ctrl+N (⌘N) starts a new message, Ctrl+Shift+L (⌘⇧L) browses
//! channels.

use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Vec2};

use crate::app::{App, Page};
use crate::convos::{Action as Convos, BookmarkProblem, MAX_NAME, MAX_PEOPLE, NameProblem};
use crate::i18n::{t, tf};
use crate::model::{Ability, Action, Conversation, ConversationKind};
use crate::theme::{self, Icon};

/// The shortcuts for these dialogs, while the main page shows and nothing
/// covers it.
pub fn keys(app: &mut App, ctx: &egui::Context) {
    if app.page != Page::Main || app.overlay_open() || app.workspaces.is_empty() {
        return;
    }
    // Where the service has no conversations to start or channels to
    // browse, those keys are left alone.
    let offers = |ability| {
        app.active_workspace()
            .is_some_and(|w| w.info.offers(ability))
    };
    let (may_compose, may_browse) = (offers(Ability::NewMessage), offers(Ability::Channels));
    let (compose, browse) = ctx.input_mut(|i| {
        (
            may_compose && i.consume_key(Modifiers::COMMAND, Key::N),
            may_browse && i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::L),
        )
    });
    if compose {
        app.actions.push(Action::Convos(Convos::NewMessage));
    }
    if browse {
        app.actions.push(Action::Convos(Convos::Browse));
    }
}

/// The sidebar header's buttons for these dialogs, laid out right to left.
pub fn header_buttons(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    actions: &mut Vec<Action>,
) {
    let tip = tf(
        "New message ({shortcut})",
        &[("shortcut", &super::keys::command("N"))],
    );
    if theme::icon_button(ui, palette, Icon::SquarePen, 16.0, &tip).clicked() {
        actions.push(Action::Convos(Convos::NewMessage));
    }
    let tip = tf(
        "Browse channels ({shortcut})",
        &[("shortcut", &super::keys::command("Shift+L"))],
    );
    if theme::icon_button(ui, palette, Icon::Hash, 16.0, &tip).clicked() {
        actions.push(Action::Convos(Convos::Browse));
    }
}

/// Someone's time of day in their time zone, and how far it is from
/// yours: "14:03 local time, 2 hours ahead of you".
pub fn local_time(tz: &str) -> Option<String> {
    let zone = jiff::tz::TimeZone::get(tz).ok()?;
    let now = jiff::Timestamp::now();
    let theirs = now.to_zoned(zone);
    let mine = now.to_zoned(jiff::tz::TimeZone::system());
    let time = theirs.strftime("%H:%M").to_string();
    let difference =
        crate::convos::zone_difference(theirs.offset().seconds(), mine.offset().seconds());
    Some(match difference {
        None => tf("{time} local time", &[("time", &time)]),
        Some(difference) => {
            let gap = if difference.minutes == 0 {
                if difference.ahead {
                    crate::i18n::tn(
                        "{count} hour ahead of you",
                        "{count} hours ahead of you",
                        difference.hours,
                    )
                } else {
                    crate::i18n::tn(
                        "{count} hour behind you",
                        "{count} hours behind you",
                        difference.hours,
                    )
                }
            } else {
                let span = format!("{}:{:02}", difference.hours, difference.minutes);
                if difference.ahead {
                    tf("{time} hours ahead of you", &[("time", &span)])
                } else {
                    tf("{time} hours behind you", &[("time", &span)])
                }
            };
            tf(
                "{time} local time, {difference}",
                &[("time", &time), ("difference", &gap)],
            )
        }
    })
}

/// The action that shows a conversation's details on `tab`.
pub fn details(channel: &str, tab: crate::convos::Tab) -> Action {
    Action::Convos(Convos::Details {
        channel: channel.to_owned(),
        tab,
    })
}

/// "Leave channel" in a conversation's context menu, for the channels one
/// can leave; `below` puts a line between it and the entries above.
pub fn leave_item(
    ui: &mut egui::Ui,
    conversation: &Conversation,
    below: bool,
    actions: &mut Vec<Action>,
) {
    if conversation.kind.is_dm() {
        return;
    }
    if below {
        ui.separator();
    }
    if ui.button(t("Leave channel")).clicked() {
        actions.push(Action::Convos(Convos::AskLeave {
            channel: conversation.id.clone(),
        }));
        ui.close();
    }
}

/// Draws whichever of the dialogs is open.
pub fn show(app: &mut App, ctx: &egui::Context) {
    new_message(app, ctx);
    browse(app, ctx);
    new_channel(app, ctx);
    confirm_leave(app, ctx);
    bookmark_dialog(app, ctx);
    confirm_remove_bookmark(app, ctx);
}

/// The frame every dialog here sits in, like the other overlays'.
fn frame(app: &App) -> egui::Frame {
    egui::Frame::new()
        .fill(app.palette.overlay)
        .stroke(egui::Stroke::new(1.0, app.palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 32,
            spread: 0,
            color: app.palette.shadow,
        })
        .inner_margin(Margin::same(16))
}

/// A dialog's heading.
fn heading(ui: &mut egui::Ui, app: &App, text: &str) -> egui::Response {
    ui.label(
        RichText::new(text)
            .font(theme::bold(17.0))
            .color(app.palette.text),
    )
}

/// Picks people for a direct message (one) or a group message (several),
/// with suggestions as you type.
fn new_message(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.convos.new_message.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let Some(workspace) = crate::app::active_in(&app.workspaces, &app.settings) else {
        return;
    };
    let found = dialog.suggestions(workspace);
    let searches = workspace.info.service.searches_people();
    let (down, up, enter, escape) = ctx.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::Enter),
            input.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    if !found.is_empty() {
        if down {
            dialog.selected = (dialog.selected + 1) % found.len();
        }
        if up {
            dialog.selected = (dialog.selected + found.len() - 1) % found.len();
        }
        dialog.selected = dialog.selected.min(found.len() - 1);
    }
    let full = dialog.picked.len() >= MAX_PEOPLE;
    let mut pick = None;
    let mut unpick = None;
    let mut go = false;
    let mut close = escape;
    let response = egui::Modal::new(egui::Id::new("new-message"))
        .frame(frame(app))
        .show(ctx, |ui| {
            ui.set_width(440.0);
            let title = heading(ui, app, &t("New message"));
            ui.add_space(6.0);
            if !dialog.picked.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    for id in &dialog.picked {
                        let name = workspace.user_label(id);
                        let chip = egui::Frame::new()
                            .fill(palette.surface)
                            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
                            .inner_margin(Margin::symmetric(6, 3))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    let avatar =
                                        workspace.user(id).and_then(|u| u.avatar.as_deref());
                                    super::avatar(ui, avatar, &name, id, 18.0);
                                    ui.label(RichText::new(&name).color(palette.text));
                                    let tip = tf("Remove {name}", &[("name", &name)]);
                                    if theme::icon_button(ui, &palette, Icon::X, 12.0, &tip)
                                        .clicked()
                                    {
                                        unpick = Some(id.clone());
                                    }
                                });
                            });
                        chip.response.on_hover_text(
                            workspace
                                .user(id)
                                .map(|u| u.name.clone())
                                .unwrap_or_default(),
                        );
                    }
                });
                ui.add_space(6.0);
            }
            let hint = if full {
                tf(
                    "A group message has at most {count} other people",
                    &[("count", &MAX_PEOPLE.to_string())],
                )
            } else if dialog.picked.is_empty() {
                t("Type a name").into_owned()
            } else {
                t("Add someone else").into_owned()
            };
            // Read before the field applies this frame's keys: else the
            // Backspace that deletes the last letter also takes a person.
            let was_empty = dialog.query.is_empty();
            let field = ui
                .add_enabled(
                    !full && !dialog.busy,
                    egui::TextEdit::singleline(&mut dialog.query)
                        .id(egui::Id::new("new-message-query"))
                        .hint_text(hint)
                        .font(theme::regular(15.0))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 7)),
                )
                .labelled_by(title.id);
            if focus || unpick.is_some() {
                field.request_focus();
            }
            // Backspace in an empty field takes back the last person, as
            // in Slack.
            if field.has_focus() && was_empty && ui.input(|i| i.key_pressed(Key::Backspace)) {
                unpick = dialog.picked.last().cloned();
            }
            ui.add_space(6.0);
            let typing = !dialog.query.trim().is_empty();
            if typing || dialog.picked.is_empty() {
                for (index, id) in found.iter().enumerate() {
                    let Some(user) = workspace.user(id) else {
                        continue;
                    };
                    let (rect, row) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
                    let highlighted = index == dialog.selected;
                    if highlighted || row.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(theme::RADIUS_SMALL),
                            if highlighted {
                                palette.accent.gamma_multiply(0.25)
                            } else {
                                palette.surface_hover
                            },
                        );
                    }
                    let picture = egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 20.0, rect.center().y),
                        Vec2::splat(24.0),
                    );
                    super::paint_avatar(ui, picture, user.avatar.as_deref(), user.label(), id);
                    let label = ui.painter().text(
                        egui::pos2(rect.left() + 42.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        user.label(),
                        theme::semibold(14.5),
                        palette.text,
                    );
                    let detail = if user.is_bot {
                        t("App").into_owned()
                    } else if !user.real_name.is_empty() && user.real_name != user.label() {
                        user.real_name.clone()
                    } else {
                        format!("@{}", user.name)
                    };
                    ui.painter().text(
                        egui::pos2(label.right() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        detail,
                        theme::regular(13.0),
                        palette.dim,
                    );
                    theme::describe_selected(
                        &row,
                        egui::WidgetType::SelectableLabel,
                        highlighted,
                        user.label(),
                    );
                    if row
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        pick = Some(id.clone());
                    }
                }
                if found.is_empty() && typing {
                    ui.label(RichText::new(t("Nobody by that name.")).color(palette.dim));
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    close = true;
                }
                let label = if dialog.picked.len() > 1 {
                    t("Start group message")
                } else {
                    t("Open")
                };
                let ready = !dialog.picked.is_empty() && !dialog.busy;
                if ui
                    .add_enabled_ui(ready, |ui| theme::primary_button(ui, &palette, &label))
                    .inner
                    .clicked()
                {
                    go = true;
                }
                if dialog.busy {
                    ui.add(egui::Spinner::new().size(16.0).color(palette.dim));
                }
            });
        });
    if response.should_close() {
        close = true;
    }
    // Enter picks the highlighted person while typing, and opens the
    // conversation once the field is empty again.
    if enter && !dialog.busy {
        if !dialog.query.trim().is_empty() {
            pick = pick.or_else(|| found.get(dialog.selected).cloned());
        } else if !dialog.picked.is_empty() {
            go = true;
        } else {
            pick = pick.or_else(|| found.get(dialog.selected).cloned());
        }
    }
    // Where the server has to be asked for people, it is asked as the
    // query changes; the answers land among the workspace's people and
    // so in the suggestions.
    let query = dialog.query.trim().to_owned();
    if searches && query.chars().count() >= 2 && query != dialog.asked {
        dialog.asked.clone_from(&query);
        app.actions
            .push(Action::Convos(Convos::FindPeople { query }));
    }
    if let Some(id) = pick {
        dialog.pick(id);
        app.focus_overlay = true;
    }
    if let Some(id) = unpick {
        dialog.picked.retain(|p| *p != id);
    }
    if close {
        return;
    }
    if go {
        let users = dialog.picked.clone();
        app.convos.new_message = Some(dialog);
        app.actions.push(Action::Convos(Convos::Open { users }));
        return;
    }
    app.convos.new_message = Some(dialog);
}

/// The height of one channel in the browser.
const LISTED_ROW: f32 = 54.0;

/// Public channels you are not in, with a search, their member counts and
/// topics, and a button to join each.
fn browse(app: &mut App, ctx: &egui::Context) {
    let Some(mut browse) = app.convos.browse.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let escape = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    let found = browse.matches();
    let mut join = None;
    let mut create = false;
    let mut close = escape;
    let height = (ctx.content_rect().height() - 260.0).clamp(160.0, 460.0);
    let response = egui::Modal::new(egui::Id::new("browse-channels"))
        .frame(frame(app))
        .show(ctx, |ui| {
            ui.set_width(520.0);
            ui.horizontal(|ui| {
                heading(ui, app, &t("Browse channels"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, &palette, Icon::X, 16.0, &t("Close")).clicked() {
                        close = true;
                    }
                    if theme::secondary_button(ui, &palette, &t("Create a channel")).clicked() {
                        create = true;
                    }
                });
            });
            ui.add_space(6.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut browse.query)
                    .id(egui::Id::new("browse-query"))
                    .hint_text(t("Search channels"))
                    .font(theme::regular(15.0))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 7)),
            );
            if focus {
                field.request_focus();
            }
            ui.add_space(4.0);
            let status = if !browse.done {
                tf(
                    "{count} channels so far…",
                    &[("count", &browse.channels.len().to_string())],
                )
            } else if let Some(error) = &browse.error {
                tf(
                    "Could not list the channels: {error}",
                    &[("error", &error.message())],
                )
            } else {
                crate::i18n::tn(
                    "{count} channel you can join",
                    "{count} channels you can join",
                    crate::i18n::count(found.len()),
                )
            };
            ui.horizontal(|ui| {
                if !browse.done {
                    ui.add(egui::Spinner::new().size(12.0).color(palette.dim));
                }
                ui.label(
                    RichText::new(status)
                        .font(theme::regular(12.5))
                        .color(palette.dim),
                );
            });
            ui.add_space(4.0);
            // As tall as the list, up to the window: the dialog keeps the
            // size it first had, and the list arrives after it opens.
            let rows = found.len() as f32 * (LISTED_ROW + ui.spacing().item_spacing.y);
            let list = rows.clamp(LISTED_ROW, height);
            egui::ScrollArea::vertical()
                .min_scrolled_height(list)
                .max_height(list)
                .auto_shrink([false, false])
                .show_rows(ui, LISTED_ROW, found.len(), |ui, range| {
                    for &index in &found[range] {
                        let Some(channel) = browse.channels.get(index) else {
                            continue;
                        };
                        let joining = browse.joining.contains(&channel.id);
                        if listed_row(ui, &palette, channel, joining) {
                            join = Some(channel.id.clone());
                        }
                    }
                });
            if browse.done && found.is_empty() && browse.error.is_none() {
                ui.label(RichText::new(t("Nothing matches.")).color(palette.dim));
            }
        });
    if response.should_close() {
        close = true;
    }
    if create {
        app.actions.push(Action::Convos(Convos::NewChannel));
        return;
    }
    if close {
        return;
    }
    if let Some(channel) = join {
        app.actions.push(Action::Convos(Convos::Join { channel }));
    }
    app.convos.browse = Some(browse);
}

/// One channel in the browser. Returns whether "Join" was clicked.
fn listed_row(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    channel: &crate::convos::Listed,
    joining: bool,
) -> bool {
    let (rect, row) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), LISTED_ROW), Sense::hover());
    if row.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS_SMALL),
            palette.surface_hover,
        );
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(10.0, 6.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let mut clicked = false;
    child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if joining {
            ui.add(egui::Spinner::new().size(16.0).color(palette.dim));
        } else {
            clicked = theme::primary_button(ui, palette, &t("Join")).clicked();
        }
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("# {}", channel.name))
                            .font(theme::semibold(14.5))
                            .color(palette.text),
                    )
                    .truncate(),
                );
                ui.label(
                    RichText::new(crate::i18n::tn(
                        "{count} member",
                        "{count} members",
                        channel.members,
                    ))
                    .font(theme::regular(12.5))
                    .color(palette.dim),
                );
            });
            let about = if channel.topic.is_empty() {
                &channel.purpose
            } else {
                &channel.topic
            };
            if !about.is_empty() {
                let about = crate::mrkdwn::plain(about, |_| None);
                ui.add(
                    egui::Label::new(
                        RichText::new(about)
                            .font(theme::regular(13.0))
                            .color(palette.secondary),
                    )
                    .truncate(),
                );
            }
        });
    });
    clicked
}

/// What is wrong with a channel name, in words.
fn name_problem(problem: NameProblem) -> String {
    match problem {
        NameProblem::Empty => t("Give the channel a name.").into_owned(),
        NameProblem::TooLong => tf(
            "Names can be at most {count} characters.",
            &[("count", &MAX_NAME.to_string())],
        ),
        NameProblem::Character(c) => tf(
            "Names can have only letters, numbers, hyphens and underscores, not “{character}”.",
            &[("character", &c.to_string())],
        ),
        NameProblem::Taken => t("You already have a channel by that name.").into_owned(),
    }
}

/// Names a new channel, public or private, and creates it.
fn new_channel(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.convos.new_channel.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let Some(workspace) = crate::app::active_in(&app.workspaces, &app.settings) else {
        return;
    };
    let checked = crate::convos::new_channel_name(workspace, &dialog.name);
    let escape = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    let mut close = escape;
    let mut create = false;
    let response = egui::Modal::new(egui::Id::new("new-channel"))
        .frame(frame(app))
        .show(ctx, |ui| {
            ui.set_width(420.0);
            let title = heading(ui, app, &t("Create a channel"));
            ui.add_space(6.0);
            let field = ui
                .add_enabled(
                    !dialog.busy,
                    egui::TextEdit::singleline(&mut dialog.name)
                        .id(egui::Id::new("new-channel-name"))
                        .hint_text(t("e.g. plan-budget"))
                        .char_limit(MAX_NAME + 1)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(8, 6)),
                )
                .labelled_by(title.id);
            if focus {
                field.request_focus();
            }
            match &checked {
                Ok(name) if *name != dialog.name.trim() => {
                    ui.label(
                        RichText::new(tf("It will be called #{name}.", &[("name", name)]))
                            .font(theme::regular(12.5))
                            .color(palette.dim),
                    );
                }
                Err(problem) if !dialog.name.is_empty() => {
                    ui.label(
                        RichText::new(name_problem(*problem))
                            .font(theme::regular(12.5))
                            .color(palette.danger),
                    );
                }
                _ => {}
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut dialog.private, false, t("Public"));
                ui.selectable_value(&mut dialog.private, true, t("Private"));
            });
            ui.label(
                RichText::new(if dialog.private {
                    t("Only people you invite can see and join it.")
                } else {
                    t("Anyone in the workspace can find and join it.")
                })
                .font(theme::regular(12.5))
                .color(palette.dim),
            );
            if field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                create = true;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    close = true;
                }
                let ready = checked.is_ok() && !dialog.busy;
                if ui
                    .add_enabled_ui(ready, |ui| {
                        theme::primary_button(ui, &palette, &t("Create"))
                    })
                    .inner
                    .clicked()
                {
                    create = true;
                }
                if dialog.busy {
                    ui.add(egui::Spinner::new().size(16.0).color(palette.dim));
                }
            });
        });
    if response.should_close() {
        close = true;
    }
    if close {
        return;
    }
    if create
        && !dialog.busy
        && let Ok(name) = checked
    {
        let private = dialog.private;
        app.actions
            .push(Action::Convos(Convos::Create { name, private }));
    }
    app.convos.new_channel = Some(dialog);
}

/// "Leave #channel?", before leaving it.
fn confirm_leave(app: &mut App, ctx: &egui::Context) {
    let Some(channel) = app.convos.leave.clone() else {
        return;
    };
    let palette = app.palette;
    let Some(conversation) = app
        .active_workspace()
        .and_then(|w| w.conversation(&channel))
        .cloned()
    else {
        app.convos.leave = None;
        return;
    };
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new("confirm-leave"))
        .frame(frame(app))
        .show(ctx, |ui| {
            ui.set_width(360.0);
            heading(
                ui,
                app,
                &tf("Leave #{name}?", &[("name", &conversation.name)]),
            );
            let note = if conversation.kind == ConversationKind::Private {
                t("It is private: you need an invitation to come back.")
            } else {
                t("You can join it again from the channel browser.")
            };
            ui.label(
                RichText::new(note)
                    .font(theme::regular(14.0))
                    .color(palette.secondary),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    answer = Some(false);
                }
                let leave = egui::Button::new(
                    RichText::new(t("Leave"))
                        .font(theme::medium(14.0))
                        .color(egui::Color32::WHITE),
                )
                .fill(palette.danger)
                .min_size(Vec2::new(0.0, 32.0));
                if ui.add(leave).clicked() {
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
        Some(true) => app.actions.push(Action::Convos(Convos::Leave { channel })),
        Some(false) => app.convos.leave = None,
        None => {}
    }
}

/// What is wrong with the bookmark dialog's fields, in words.
fn bookmark_problem(problem: BookmarkProblem) -> String {
    match problem {
        BookmarkProblem::NoLink => t("Enter a link.").into_owned(),
        BookmarkProblem::NotALink => {
            t("Only web links (http:// or https://) can be bookmarked.").into_owned()
        }
        BookmarkProblem::NotAnEmoji => {
            t("Enter an emoji name, such as rocket or :rocket:.").into_owned()
        }
    }
}

/// A field of the bookmark dialog with its label above it; returns the
/// field.
fn bookmark_field(
    ui: &mut egui::Ui,
    app: &App,
    label: &str,
    text: &mut String,
    id: &str,
    hint: &str,
) -> egui::Response {
    let label = ui.label(
        RichText::new(label)
            .font(theme::medium(13.0))
            .color(app.palette.secondary),
    );
    ui.add(
        egui::TextEdit::singleline(text)
            .id(egui::Id::new(id))
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(Margin::symmetric(8, 6)),
    )
    .labelled_by(label.id)
}

/// Adds a bookmark to a channel, or edits one: its link, its title and
/// an optional emoji.
fn bookmark_dialog(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.convos.bookmark.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let checked = crate::convos::bookmark_form(&dialog.title, &dialog.link, &dialog.emoji);
    let mut close = false;
    let mut save = false;
    let response = egui::Modal::new(egui::Id::new("bookmark-dialog"))
        .frame(frame(app))
        .show(ctx, |ui| {
            ui.set_width(420.0);
            heading(
                ui,
                app,
                &if dialog.editing.is_some() {
                    t("Edit bookmark")
                } else {
                    t("Add a bookmark")
                },
            );
            ui.add_space(6.0);
            let link = bookmark_field(
                ui,
                app,
                &t("Link"),
                &mut dialog.link,
                "bookmark-link",
                "https://",
            );
            if focus {
                link.request_focus();
            }
            match crate::convos::bookmark_link(&dialog.link) {
                Ok(full) if full != dialog.link.trim() => {
                    ui.label(
                        RichText::new(tf("It will link to {link}.", &[("link", &full)]))
                            .font(theme::regular(12.5))
                            .color(palette.dim),
                    );
                }
                Err(problem) if !dialog.link.trim().is_empty() => {
                    ui.label(
                        RichText::new(bookmark_problem(problem))
                            .font(theme::regular(12.5))
                            .color(palette.danger),
                    );
                }
                _ => {}
            }
            ui.add_space(4.0);
            let title = bookmark_field(
                ui,
                app,
                &t("Name"),
                &mut dialog.title,
                "bookmark-title",
                &t("The link, if left empty"),
            );
            ui.add_space(4.0);
            let emoji = bookmark_field(
                ui,
                app,
                &t("Emoji (optional)"),
                &mut dialog.emoji,
                "bookmark-emoji",
                &t("e.g. rocket"),
            );
            match crate::convos::bookmark_emoji(&dialog.emoji) {
                Ok(Some(name)) => {
                    if let Some(shown) = crate::emoji::unicode(&name, None) {
                        ui.label(RichText::new(shown).font(theme::regular(16.0)));
                    }
                }
                Ok(None) => {}
                Err(problem) => {
                    ui.label(
                        RichText::new(bookmark_problem(problem))
                            .font(theme::regular(12.5))
                            .color(palette.danger),
                    );
                }
            }
            let typing = link.has_focus() || title.has_focus() || emoji.has_focus();
            if typing && ui.input(|i| i.key_pressed(Key::Enter)) {
                save = true;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    close = true;
                }
                let label = if dialog.editing.is_some() {
                    t("Save")
                } else {
                    t("Add")
                };
                if ui
                    .add_enabled_ui(checked.is_ok(), |ui| {
                        theme::primary_button(ui, &palette, &label)
                    })
                    .inner
                    .clicked()
                {
                    save = true;
                }
            });
        });
    if response.should_close() || close {
        return;
    }
    if save && let Ok(form) = checked {
        app.actions.push(Action::Convos(Convos::SaveBookmark {
            channel: dialog.channel,
            id: dialog.editing.map(|b| b.id),
            form,
        }));
        return;
    }
    app.convos.bookmark = Some(dialog);
}

/// "Remove the bookmark?", before removing it.
fn confirm_remove_bookmark(app: &mut App, ctx: &egui::Context) {
    let Some((channel, bookmark)) = app.convos.remove_bookmark.clone() else {
        return;
    };
    let palette = app.palette;
    let mut answer = None;
    let response = egui::Modal::new(egui::Id::new("confirm-remove-bookmark"))
        .frame(frame(app))
        .show(ctx, |ui| {
            ui.set_width(360.0);
            heading(
                ui,
                app,
                &tf("Remove “{name}”?", &[("name", &bookmark.title)]),
            );
            ui.label(
                RichText::new(t("It is removed for everyone in the channel."))
                    .font(theme::regular(14.0))
                    .color(palette.secondary),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    answer = Some(false);
                }
                let remove = egui::Button::new(
                    RichText::new(t("Remove"))
                        .font(theme::medium(14.0))
                        .color(egui::Color32::WHITE),
                )
                .fill(palette.danger)
                .min_size(Vec2::new(0.0, 32.0));
                if ui.add(remove).clicked() {
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
        Some(true) => app.actions.push(Action::Convos(Convos::RemoveBookmark {
            channel,
            id: bookmark.id,
        })),
        Some(false) => app.convos.remove_bookmark = None,
        None => {}
    }
}
