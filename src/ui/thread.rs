//! A thread beside the conversation: the parent message, its replies and a
//! reply composer.

use egui::{Align, Margin, RichText, Stroke};

use super::composer::{self, Composer};
use super::message::{self, Lead, Row};
use super::rows;
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
        selected,
        jumps,
        transfers,
        ..
    } = app;
    // A reply being brought into view here.
    let now = std::time::Instant::now();
    let jump = jumps.iter().find(|j| j.list == key).cloned();
    let steering = jump.as_ref().is_some_and(crate::jump::Jump::steering);
    let light = jump.as_ref().map_or(0.0, |j| j.light(now));
    let to_bottom = to_bottom && !steering;
    let mut target: Option<(f32, f32)> = None;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
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
            composer::drop_target(ui, &palette, Some(ts.clone()), false, actions);
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
                        uploads: transfers,
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
                    let look = message::Look::of(settings);
                    let row = Row {
                        palette: &palette,
                        workspace,
                        channel: &channel,
                        in_thread: true,
                        enter_sends: settings.enter_sends,
                        look,
                        overlay,
                        selected: selected
                            .as_ref()
                            .filter(|s| s.in_thread && s.channel == channel),
                    };
                    // Only the rows in and near the view are laid out; the
                    // rest are placed by the heights they were last drawn at.
                    let heights_id = egui::Id::new(("thread-heights", &channel, ts.as_str()));
                    let mut heights: rows::Heights = ui
                        .data_mut(|d| d.remove_temp(heights_id))
                        .unwrap_or_default();
                    heights.for_layout(look.key());
                    let output = egui::ScrollArea::vertical()
                        .id_salt(("thread", &channel, ts.as_str()))
                        .auto_shrink([false, false])
                        .stick_to_bottom(!steering)
                        .show_viewport(ui, |ui, viewport| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let replies: Vec<_> = timeline
                                .map(|t| t.messages.iter().filter(|m| m.ts != ts).collect())
                                .unwrap_or_default();
                            // The first row is the parent with the reply
                            // count under it; then a row per reply.
                            let mut leads = Vec::with_capacity(replies.len());
                            let mut entries = vec![rows::Entry {
                                key: egui::Id::new("parent").value(),
                                guess: parent
                                    .map_or(0.0, |p| message::guess_height(p, Lead::Full, look))
                                    + 40.0,
                            }];
                            let mut previous = None;
                            for reply in &replies {
                                let lead = if message::continues(previous, reply) {
                                    Lead::Compact
                                } else {
                                    Lead::Full
                                };
                                leads.push(lead);
                                entries.push(rows::Entry {
                                    key: egui::Id::new(reply.ts.as_str()).value(),
                                    guess: message::guess_height(reply, lead, look),
                                });
                                previous = Some(*reply);
                            }
                            let plan = rows::plan(
                                entries.iter().map(|entry| heights.planned(entry)),
                                viewport.min.y,
                                viewport.max.y,
                                400.0,
                            );
                            heights.sweep();
                            if let Some(jump) = &jump {
                                target = replies
                                    .iter()
                                    .position(|reply| reply.ts == jump.ts)
                                    .map(|index| (plan.tops[index + 1], plan.tops[index + 2]));
                            }
                            let moved =
                                rows::show(ui, &mut heights, &entries, &plan, |ui, index| {
                                    if index == 0 {
                                        ui.add_space(4.0);
                                        if let Some(parent) = parent {
                                            message::show(
                                                ui,
                                                &row,
                                                parent,
                                                Lead::Full,
                                                editing,
                                                actions,
                                            );
                                        }
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
                                                egui::Rangef::new(
                                                    ui.cursor().min.x + 8.0,
                                                    rect.right() - 16.0,
                                                ),
                                                rect.center().y,
                                                Stroke::new(1.0, palette.outline),
                                            );
                                        });
                                        ui.add_space(4.0);
                                        if timeline.is_none_or(|t| t.loading && !t.loaded) {
                                            ui.add_space(16.0);
                                            ui.vertical_centered(|ui| {
                                                ui.add(
                                                    egui::Spinner::new()
                                                        .size(18.0)
                                                        .color(palette.dim),
                                                );
                                            });
                                        }
                                    } else {
                                        let reply = replies[index - 1];
                                        let background = ui.painter().add(egui::Shape::Noop);
                                        let top = ui.cursor().top();
                                        message::show(
                                            ui,
                                            &row,
                                            reply,
                                            leads[index - 1],
                                            editing,
                                            actions,
                                        );
                                        if light > 0.0
                                            && jump.as_ref().is_some_and(|j| j.ts == reply.ts)
                                        {
                                            super::conversation::paint_light(
                                                ui, background, top, &palette, light,
                                            );
                                        }
                                    }
                                });
                            ui.add_space(12.0);
                            if to_bottom {
                                // Your own reply: show it even when reading
                                // further up.
                                ui.scroll_to_cursor(Some(Align::BOTTOM));
                            }
                            moved
                        });
                    ui.data_mut(|d| d.insert_temp(heights_id, heights));
                    // Rows above the one being read came out taller or
                    // shorter than placed: move with them so the reading
                    // stays put. At the end, egui keeps the view stuck there.
                    let moved = output.inner;
                    let offset = output.state.offset.y;
                    let bottom = (output.content_size.y - output.inner_rect.height()).max(0.0);
                    let view = output.inner_rect.height();
                    if let Some(index) = jumps.iter().position(|j| j.list == key) {
                        let loading = timeline.is_none_or(|t| t.loading || !t.loaded);
                        let jump = &mut jumps[index];
                        let wanted = jump.steer(target, offset, view, bottom, loading, now);
                        if let Some(wanted) = wanted {
                            let mut state = output.state;
                            state.offset.y = wanted;
                            state.store(ui.ctx(), output.id);
                        }
                        if jump.done(now) {
                            jumps.remove(index);
                        } else {
                            ui.ctx().request_repaint();
                        }
                    }
                    if steering {
                        // The jump moved the view; nothing else may this frame.
                    } else if !to_bottom && moved.abs() > 0.5 && offset < bottom - 1.0 {
                        let mut state = output.state;
                        state.offset.y = (offset + moved).clamp(0.0, bottom);
                        state.store(ui.ctx(), output.id);
                        ui.ctx().request_repaint();
                    }
                });
        });
    let width = response.response.rect.width();
    app.drafts.insert(key, draft);
    if (width - app.settings.thread_width).abs() > 1.0 {
        app.settings.thread_width = width;
        app.settings_changed();
    }
}
