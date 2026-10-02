//! The open conversation: its header, the message list with day
//! separators and a "new" line, and the composer.

use egui::{Align, CornerRadius, Margin, RichText, Stroke, Vec2};

use super::composer::{self, Composer};
use super::message::{self, Lead, Row};
use crate::app::{App, Draft};
use crate::backend::Socket;
use crate::i18n::t;
use crate::model::{Action, ConversationKind, Ts};
use crate::theme::{self, Icon};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            let Some(team) = app.active_team() else {
                empty(ui, app, &t("No workspace"));
                return;
            };
            let channel = app.active_workspace().and_then(|w| w.active.clone());
            let Some(channel) = channel else {
                let text = if app.active_workspace().is_some_and(|w| w.loaded) {
                    t("Pick a conversation")
                } else {
                    t("Loading conversations…")
                };
                empty(ui, app, &text);
                return;
            };
            header(app, ui, &channel);
            footer(app, ui, &team, &channel);
            messages(app, ui, &team, &channel);
        });
}

fn empty(ui: &mut egui::Ui, app: &App, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(
            RichText::new(text)
                .font(theme::regular(15.0))
                .color(app.palette.dim),
        );
    });
}

fn header(app: &mut App, ui: &mut egui::Ui, channel: &str) {
    let palette = app.palette;
    let App {
        workspaces,
        settings,
        actions,
        socket,
        ..
    } = app;
    let Some(workspace) = workspaces
        .iter()
        .find(|w| Some(&w.info.team_id) == settings.active_workspace.as_ref())
        .or_else(|| workspaces.first())
    else {
        return;
    };
    let Some(conversation) = workspace.conversation(channel) else {
        return;
    };
    let inset = theme::titlebar_inset(ui.ctx());
    egui::Panel::top("conversation-header")
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
                ui.spacing_mut().item_spacing.x = 6.0;
                let title = workspace.title(conversation);
                match conversation.kind {
                    ConversationKind::Channel => {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(17.0), egui::Sense::hover());
                        Icon::Hash.image(palette.secondary, 17.0).paint_at(ui, icon);
                    }
                    ConversationKind::Private => {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
                        Icon::Lock.image(palette.secondary, 16.0).paint_at(ui, icon);
                    }
                    ConversationKind::Direct => {
                        let user = conversation.user.as_deref().and_then(|id| workspace.user(id));
                        super::avatar(
                            ui,
                            user.and_then(|u| u.avatar.as_deref()),
                            &title,
                            conversation.user.as_deref().unwrap_or(&title),
                            22.0,
                        );
                    }
                    ConversationKind::Group => {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
                        Icon::Users.image(palette.secondary, 16.0).paint_at(ui, icon);
                    }
                }
                let name = ui
                    .add(
                        egui::Label::new(
                            RichText::new(&title)
                                .font(theme::bold(17.0))
                                .color(palette.text),
                        )
                        .sense(egui::Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if name.clicked()
                    && let Some(user) = &conversation.user
                {
                    actions.push(Action::OpenProfile(user.clone()));
                }
                if !conversation.topic.is_empty() {
                    ui.add_space(8.0);
                    let topic = crate::mrkdwn::plain(&conversation.topic, |_| None);
                    ui.add(
                        egui::Label::new(
                            RichText::new(topic)
                                .font(theme::regular(13.0))
                                .color(palette.secondary),
                        )
                        .truncate(),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if theme::icon_button(ui, &palette, Icon::Search, 17.0, &t("Jump to… (Ctrl+K)")).clicked() {
                        actions.push(Action::OpenSwitcher);
                    }
                    match socket {
                        Socket::Connected => {}
                        Socket::Off => {
                            ui.label(
                                RichText::new(t("Live updates off"))
                                    .font(theme::regular(12.0))
                                    .color(palette.dim),
                            )
                            .on_hover_text(t("No live connection: new messages in this conversation are fetched every few seconds."));
                        }
                        Socket::Connecting => {
                            ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                            ui.label(RichText::new(t("Connecting…")).font(theme::regular(12.0)).color(palette.dim));
                        }
                        Socket::Disconnected(reason) | Socket::Rejected(reason) => {
                            if theme::icon_button(ui, &palette, Icon::Refresh, 15.0, &t("Reconnect")).clicked() {
                                actions.push(Action::Reconnect);
                            }
                            ui.label(
                                RichText::new(t("Offline"))
                                    .font(theme::semibold(12.0))
                                    .color(palette.warning),
                            )
                            .on_hover_text(reason.as_str());
                        }
                    }
                    if let Some(members) = conversation.members
                        && !conversation.kind.is_dm()
                    {
                        ui.label(
                            RichText::new(crate::i18n::tn("{count} member", "{count} members", members))
                                .font(theme::regular(12.5))
                                .color(palette.dim),
                        );
                    }
                });
            });
        });
}

fn footer(app: &mut App, ui: &mut egui::Ui, team: &str, channel: &str) {
    let palette = app.palette;
    let key = App::draft_key(team, channel, None);
    let mut draft: Draft = app.drafts.remove(&key).unwrap_or_default();
    let focus =
        std::mem::take(&mut app.focus_composer) && app.picker.is_none() && app.switcher.is_none();
    let App {
        workspaces,
        settings,
        actions,
        ..
    } = app;
    let Some(workspace) = workspaces
        .iter()
        .find(|w| Some(&w.info.team_id) == settings.active_workspace.as_ref())
        .or_else(|| workspaces.first())
    else {
        return;
    };
    let Some(conversation) = workspace.conversation(channel) else {
        app.drafts.insert(key, draft);
        return;
    };
    let title = workspace.title(conversation);
    let placeholder = match conversation.kind {
        ConversationKind::Channel | ConversationKind::Private => {
            format!("{} #{title}", t("Message"))
        }
        _ => format!("{} {title}", t("Message")),
    };
    egui::Panel::bottom("composer")
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(palette.window)
                .inner_margin(Margin {
                    left: 20,
                    right: 20,
                    top: 4,
                    bottom: 16,
                }),
        )
        .show(ui, |ui| {
            if conversation.archived {
                ui.label(RichText::new(t("This channel is archived.")).color(palette.dim));
                return;
            }
            let composer = Composer {
                palette: &palette,
                workspace,
                key: key.clone(),
                placeholder,
                thread: None,
                enter_sends: settings.enter_sends,
                focus,
                channel_name: None,
            };
            composer::show(ui, &composer, &mut draft, actions);
        });
    app.drafts.insert(key, draft);
}

fn messages(app: &mut App, ui: &mut egui::Ui, team: &str, channel: &str) {
    let palette = app.palette;
    let scroll_key = format!("{team}/{channel}");
    let to_bottom = app.scroll_to_bottom.remove(&scroll_key);
    // An anchor for another list is stale by now: drop it either way.
    let prepended = app
        .prepended
        .take()
        .is_some_and(|(key, _)| key == scroll_key);
    let App {
        workspaces,
        settings,
        actions,
        editing,
        read_line,
        ..
    } = app;
    let Some(workspace) = workspaces
        .iter()
        .find(|w| Some(&w.info.team_id) == settings.active_workspace.as_ref())
        .or_else(|| workspaces.first())
    else {
        return;
    };
    let Some(conversation) = workspace.conversation(channel) else {
        return;
    };
    let timeline = workspace.timelines.get(channel);
    let height_id = egui::Id::new(("content-height", &scroll_key));
    let previous_height: Option<f32> = ui.data(|d| d.get_temp(height_id));
    let mut area = egui::ScrollArea::vertical()
        .id_salt(("messages", &scroll_key))
        .auto_shrink([false, false])
        .stick_to_bottom(true);
    let offset_id = egui::Id::new(("scroll-offset", &scroll_key));
    if let Some(offset) = ui.data_mut(|d| d.remove_temp::<f32>(offset_id)) {
        area = area.vertical_scroll_offset(offset);
    }
    // The newest message you had read when the conversation opened.
    let read_line: Option<Ts> = read_line
        .as_ref()
        .filter(|(key, _)| *key == scroll_key)
        .and_then(|(_, ts)| ts.clone());
    let output = area.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let Some(timeline) = timeline.filter(|t| t.loaded) else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new().size(22.0).color(palette.dim));
            });
            return;
        };
        if timeline.has_more {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                if timeline.loading {
                    ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
                } else if ui
                    .add(
                        egui::Button::new(
                            RichText::new(t("Load older messages"))
                                .font(theme::medium(13.0))
                                .color(palette.secondary),
                        )
                        .fill(palette.surface),
                    )
                    .clicked()
                {
                    actions.push(Action::LoadOlder);
                }
            });
            ui.add_space(12.0);
        } else {
            beginning(ui, workspace, conversation, &palette);
        }
        let row = Row {
            palette: &palette,
            workspace,
            channel,
            in_thread: false,
        };
        let mut previous = None;
        let mut previous_day: Option<jiff::civil::Date> = None;
        let mut drew_new_line = false;
        for message in timeline.messages.iter().filter(|m| m.in_channel()) {
            let day = message.ts.zoned().map(|z| z.date());
            let new_day = day.is_some() && day != previous_day;
            if new_day {
                day_separator(ui, &palette, &super::day_label(&message.ts));
                previous_day = day;
            }
            let unread = !drew_new_line
                && !message.ts.is_local()
                && read_line.as_ref().is_some_and(|read| message.ts > *read)
                && message.user.as_deref() != Some(workspace.info.user_id.as_str());
            if unread {
                drew_new_line = true;
                new_line(ui, &palette);
            }
            let lead = if !new_day && !unread && message::continues(previous, message) {
                Lead::Compact
            } else {
                Lead::Full
            };
            message::show(ui, &row, message, lead, editing, actions);
            previous = Some(message);
        }
        ui.add_space(12.0);
        if to_bottom {
            // Jump, don't glide, so this very frame already shows the end;
            // the pin below keeps it there as the content settles.
            ui.scroll_to_cursor_animation(
                Some(Align::BOTTOM),
                egui::style::ScrollAnimation::none(),
            );
        }
    });
    let content = output.content_size.y;
    let offset = output.state.offset.y;
    let bottom = (content - output.inner_rect.height()).max(0.0);
    // Whether the reader is at the newest message. egui's own stick-to-end
    // only holds while the offset equals the end exactly, which a jump that
    // overshoots by the item spacing never does, so pictures loading after
    // the first layout used to strand the view. Here: opening a conversation
    // pins it; a move while the content holds still is the reader's and
    // decides; growth (pictures, new messages, older history) keeps the
    // intent, and a pinned view follows the new end.
    let pin_id = egui::Id::new(("pinned", &scroll_key));
    let (mut pinned, last_content) = ui
        .data(|d| d.get_temp::<(bool, f32)>(pin_id))
        .unwrap_or((true, content));
    if to_bottom {
        pinned = true;
    } else if (content - last_content).abs() < 0.5 {
        pinned = offset >= bottom - 2.0;
    }
    if pinned {
        if offset < bottom - 1.0 {
            ui.data_mut(|d| d.insert_temp(offset_id, bottom));
            ui.ctx().request_repaint();
        }
    } else if prepended && let Some(before) = previous_height {
        // Older history went in above: keep the messages being read in place.
        let anchored = offset + (content - before).max(0.0);
        ui.data_mut(|d| d.insert_temp(offset_id, anchored));
        ui.ctx().request_repaint();
    }
    ui.data_mut(|d| {
        d.insert_temp(pin_id, (pinned, content));
        d.insert_temp(height_id, content);
    });
    // Near the top: fetch the page before. Not on the frame that jumps to the
    // bottom, whose offset still reads from before the jump.
    if !to_bottom
        && !pinned
        && offset < 120.0
        && let Some(timeline) = timeline
        && timeline.loaded
        && timeline.has_more
        && !timeline.loading
        && content > output.inner_rect.height()
    {
        actions.push(Action::LoadOlder);
    }
}

fn beginning(
    ui: &mut egui::Ui,
    workspace: &crate::app::WorkspaceState,
    conversation: &crate::model::Conversation,
    palette: &crate::theme::Palette,
) {
    egui::Frame::new()
        .inner_margin(Margin {
            left: 20,
            right: 20,
            top: 28,
            bottom: 12,
        })
        .show(ui, |ui| {
            let title = workspace.title(conversation);
            let (heading, line) = match conversation.kind {
                ConversationKind::Direct => (
                    title.clone(),
                    format!(
                        "{} {title}.",
                        t("This is the very beginning of your direct message history with")
                    ),
                ),
                ConversationKind::Group => (
                    title.clone(),
                    t("This is the very beginning of this group conversation.").into_owned(),
                ),
                _ => (
                    format!("#{title}"),
                    format!("{} #{title}.", t("This is the very beginning of")),
                ),
            };
            ui.label(
                RichText::new(heading)
                    .font(theme::bold(24.0))
                    .color(palette.text),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(line)
                    .font(theme::regular(14.0))
                    .color(palette.secondary),
            );
            if !conversation.purpose.is_empty() {
                ui.label(
                    RichText::new(crate::mrkdwn::plain(&conversation.purpose, |_| None))
                        .font(theme::regular(14.0))
                        .color(palette.secondary),
                );
            }
        });
}

fn day_separator(ui: &mut egui::Ui, palette: &crate::theme::Palette, label: &str) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), egui::Sense::hover());
    let y = rect.center().y;
    ui.painter().hline(
        rect.x_range().shrink(16.0),
        y,
        Stroke::new(1.0, palette.outline),
    );
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(12.5), palette.text);
    let pill = egui::Rect::from_center_size(rect.center(), galley.size() + Vec2::new(24.0, 8.0));
    ui.painter()
        .rect_filled(pill, CornerRadius::same(12), palette.window);
    ui.painter().rect_stroke(
        pill,
        CornerRadius::same(12),
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(pill.center() - galley.size() / 2.0, galley, palette.text);
}

fn new_line(ui: &mut egui::Ui, palette: &crate::theme::Palette) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), egui::Sense::hover());
    let y = rect.center().y;
    let range = egui::Rangef::new(rect.left() + 16.0, rect.right() - 16.0);
    ui.painter()
        .hline(range, y, Stroke::new(1.0, palette.badge));
    let galley = ui.painter().layout_no_wrap(
        t("New").into_owned(),
        theme::bold(11.0),
        egui::Color32::WHITE,
    );
    let pill = egui::Rect::from_min_size(
        egui::pos2(
            range.max - galley.size().x - 12.0,
            y - galley.size().y / 2.0 - 2.0,
        ),
        galley.size() + Vec2::new(12.0, 4.0),
    );
    ui.painter()
        .rect_filled(pill, CornerRadius::same(4), palette.badge);
    ui.painter()
        .galley(pill.min + Vec2::new(6.0, 2.0), galley, egui::Color32::WHITE);
}
