//! A thread beside the conversation: the parent message, its replies and a
//! reply composer.

use egui::{Align, Margin, RichText, Stroke};

use super::composer::{self, Composer};
use super::message::{self, Lead, Row};
use crate::app::{App, Draft};
use crate::i18n::{t, tn};
use crate::model::Action;
use crate::theme::{self, Icon};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let Some((channel, ts)) = app.thread.clone() else {
        return;
    };
    let Some(team) = app.active_team() else {
        return;
    };
    let key = App::draft_key(&team, &channel, Some(&ts));
    let mut draft: Draft = app.drafts.remove(&key).unwrap_or_default();
    let to_bottom = app.scroll_to_bottom.remove(&key);
    let width = app.settings.thread_width;
    let overlay = app.overlay_open();
    let App {
        workspaces,
        settings,
        actions,
        editing,
        ..
    } = app;
    let Some(workspace) = workspaces
        .iter()
        .find(|w| Some(&w.info.team_id) == settings.active_workspace.as_ref())
        .or_else(|| workspaces.first())
    else {
        // Put the draft back: it was taken out to be edited.
        app.drafts.insert(key, draft);
        return;
    };
    let channel_name = workspace
        .conversation(&channel)
        .map(|c| workspace.title(c))
        .unwrap_or_default();
    let response = egui::Panel::right("thread")
        .resizable(true)
        .default_size(width)
        .size_range(300.0..=720.0)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().vline(
                rect.left() + 0.5,
                rect.y_range(),
                Stroke::new(1.0, palette.outline),
            );
            let inset = theme::titlebar_inset(ui.ctx());
            egui::Panel::top("thread-header")
                .exact_size(52.0 + inset)
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(Margin {
                    left: 16,
                    right: 8,
                    top: inset as i8,
                    bottom: 0,
                }))
                .show(ui, |ui| {
                    let rect = ui.max_rect();
                    ui.painter().hline(
                        rect.x_range(),
                        rect.bottom() - 0.5,
                        Stroke::new(1.0, palette.outline),
                    );
                    ui.horizontal_centered(|ui| {
                        ui.label(
                            RichText::new(t("Thread"))
                                .font(theme::bold(16.0))
                                .color(palette.text),
                        );
                        if !channel_name.is_empty() {
                            ui.label(
                                RichText::new(format!("#{channel_name}"))
                                    .font(theme::regular(13.0))
                                    .color(palette.secondary),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if theme::icon_button(ui, &palette, Icon::X, 17.0, &t("Close (Esc)"))
                                .clicked()
                            {
                                actions.push(Action::CloseThread);
                            }
                        });
                    });
                });
            egui::Panel::bottom("thread-composer")
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(Margin {
                    left: 16,
                    right: 16,
                    top: 4,
                    bottom: 16,
                }))
                .show(ui, |ui| {
                    let composer = Composer {
                        palette: &palette,
                        workspace,
                        key: key.clone(),
                        placeholder: t("Reply…").into_owned(),
                        thread: Some(ts.clone()),
                        enter_sends: settings.enter_sends,
                        focus: false,
                        channel_name: Some(channel_name.clone()),
                    };
                    composer::show(ui, &composer, &mut draft, actions);
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::new())
                .show(ui, |ui| {
                    let timeline = workspace.threads.get(&(channel.clone(), ts.clone()));
                    let parent = timeline
                        .and_then(|t| t.messages.iter().find(|m| m.ts == ts))
                        .or_else(|| {
                            workspace
                                .timelines
                                .get(&channel)
                                .and_then(|t| t.messages.iter().find(|m| m.ts == ts))
                        });
                    let row = Row {
                        palette: &palette,
                        workspace,
                        channel: &channel,
                        in_thread: true,
                        enter_sends: settings.enter_sends,
                        overlay,
                    };
                    egui::ScrollArea::vertical()
                        .id_salt(("thread", &channel, ts.as_str()))
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.add_space(4.0);
                            if let Some(parent) = parent {
                                message::show(ui, &row, parent, Lead::Full, editing, actions);
                            }
                            let replies: Vec<_> = timeline
                                .map(|t| t.messages.iter().filter(|m| m.ts != ts).collect())
                                .unwrap_or_default();
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.add_space(16.0);
                                ui.label(
                                    RichText::new(tn(
                                        "{count} reply",
                                        "{count} replies",
                                        replies.len() as u32,
                                    ))
                                    .font(theme::regular(12.5))
                                    .color(palette.dim),
                                );
                                let rect = ui.max_rect();
                                ui.painter().hline(
                                    egui::Rangef::new(ui.cursor().min.x + 8.0, rect.right() - 16.0),
                                    rect.center().y,
                                    Stroke::new(1.0, palette.outline),
                                );
                            });
                            ui.add_space(4.0);
                            if timeline.is_none_or(|t| t.loading && !t.loaded) {
                                ui.add_space(16.0);
                                ui.vertical_centered(|ui| {
                                    ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
                                });
                            }
                            let mut previous = None;
                            for reply in replies {
                                let lead = if message::continues(previous, reply) {
                                    Lead::Compact
                                } else {
                                    Lead::Full
                                };
                                message::show(ui, &row, reply, lead, editing, actions);
                                previous = Some(reply);
                            }
                            ui.add_space(12.0);
                            if to_bottom {
                                // Your own reply: show it even when reading
                                // further up.
                                ui.scroll_to_cursor(Some(Align::BOTTOM));
                            }
                        });
                });
        });
    let width = response.response.rect.width();
    app.drafts.insert(key, draft);
    if (width - app.settings.thread_width).abs() > 1.0 {
        app.settings.thread_width = width;
        app.settings_changed();
    }
}
