//! A thread beside the conversation: the parent message, its replies and a
//! reply composer.

use egui::{Align, Margin, RichText, Stroke};

use super::composer::{self, Composer};
use super::jump::Steer;
use super::message::{self, Lead, Row};
use super::rows;
use crate::app::App;
use crate::i18n::{t, tn};
use crate::model::{Ability, Action};
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
    let mut taken = app.drafts.take(&key);
    let to_bottom = app.scroll_to_bottom.remove(&key);
    let width = app.settings.thread_width;
    let overlay = app.overlay_open();
    let focus = std::mem::take(&mut app.focus_thread_composer) && !overlay;
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
    let steer = Steer::of(jumps, &key);
    let steering = steer.steering();
    let to_bottom = to_bottom && !steering;
    let mut target: Option<(f32, f32)> = None;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        // Put the draft back: it was taken out to be edited.
        app.drafts.put_back(key, taken);
        return;
    };
    let channel_name = workspace
        .conversation(&channel)
        .map(|c| workspace.title(c))
        .unwrap_or_default();
    // Whether you follow the thread, for a sign-in that can change it. A
    // parent that does not say is taken as not followed. The parent shows
    // in the conversation too, where history may not say: any copy that
    // does counts.
    let following = (workspace.info.offers(Ability::Threads)
        && crate::views::can_follow(workspace.info.sign_in))
    .then(|| {
        workspace
            .timelines_for(&channel)
            .find_map(|t| t.messages.iter().find(|m| m.ts == ts)?.subscribed)
            .unwrap_or(false)
    });
    let files = workspace.info.offers(Ability::Files);
    let response = egui::Panel::right("thread")
        .resizable(true)
        .default_size(width)
        .size_range(300.0..=720.0)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            if files {
                composer::drop_target(ui, &palette, Some(ts.clone()), false, actions);
            }
            let rect = ui.max_rect();
            ui.painter().vline(
                rect.left() + 0.5,
                rect.y_range(),
                Stroke::new(1.0, palette.outline),
            );
            theme::pane_header(
                ui,
                &palette,
                "thread-header",
                egui::Color32::TRANSPARENT,
                [16, 8],
                |ui| {
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
                        if let Some(following) = following
                            && follow_button(ui, &palette, following).clicked()
                        {
                            actions.push(Action::Views(crate::views::Action::Follow {
                                channel: channel.clone(),
                                thread: ts.clone(),
                                follow: !following,
                            }));
                        }
                    });
                },
            );
            egui::Panel::bottom("thread-composer")
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(Margin {
                    left: 16,
                    right: 16,
                    top: 4,
                    // The typing line fills the rest of the bottom space.
                    bottom: 2,
                }))
                .show(ui, |ui| {
                    let composer = Composer {
                        palette: &palette,
                        workspace,
                        key: key.clone(),
                        placeholder: t("Reply…").into_owned(),
                        thread: Some(ts.clone()),
                        enter_sends: settings.enter_sends,
                        focus,
                        // Teams has no "also send to the channel".
                        channel_name: (!workspace.info.is_teams()).then(|| channel_name.clone()),
                        uploads: transfers,
                    };
                    composer::with_typing(ui, &composer, &channel, &mut taken.draft, actions);
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
                        workspaces,
                        channel: &channel,
                        in_thread: true,
                        enter_sends: settings.enter_sends,
                        look,
                        overlay,
                        selected: selected
                            .as_ref()
                            .filter(|s| s.in_thread && s.channel == channel),
                    };
                    let heights_id = egui::Id::new(("thread-heights", &channel, ts.as_str()));
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
                            let drawn = rows::virtual_list(
                                ui,
                                heights_id,
                                look.key(),
                                viewport,
                                &entries,
                                |ui, index| {
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
                                                ui.add(theme::spinner(&palette, 18.0));
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
                                        let light = steer.light(&reply.ts);
                                        if light > 0.0 {
                                            super::conversation::paint_light(
                                                ui, background, top, &palette, light,
                                            );
                                        }
                                    }
                                },
                            );
                            if let Some(ts) = steer.ts() {
                                target = replies
                                    .iter()
                                    .position(|reply| reply.ts == *ts)
                                    .map(|i| (drawn.tops[i + 1], drawn.tops[i + 2]));
                            }
                            ui.add_space(12.0);
                            if to_bottom {
                                // Your own reply: show it even when reading
                                // further up.
                                ui.scroll_to_cursor(Some(Align::BOTTOM));
                            }
                            drawn.moved
                        });
                    // Rows above the one being read came out taller or
                    // shorter than placed: move with them so the reading
                    // stays put. At the end, egui keeps the view stuck there.
                    let moved = output.inner;
                    let offset = output.state.offset.y;
                    let bottom = (output.content_size.y - output.inner_rect.height()).max(0.0);
                    let loading = timeline.is_none_or(|t| t.loading || !t.loaded);
                    if let Some(wanted) =
                        steer.drive(jumps, &key, target, loading, &output, ui.ctx())
                    {
                        let mut state = output.state;
                        state.offset.y = wanted;
                        state.store(ui.ctx(), output.id);
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
    app.drafts.put_back(key, taken);
    if (width - app.settings.thread_width).abs() > 1.0 {
        app.settings.thread_width = width;
        app.settings_changed();
    }
}

/// The header's Follow / Following toggle: lit while you follow the
/// thread, as in Slack.
fn follow_button(ui: &mut egui::Ui, palette: &theme::Palette, following: bool) -> egui::Response {
    let (label, tip, color) = if following {
        (
            t("Following"),
            t("Stop following: replies no longer notify you or show under Threads"),
            palette.accent,
        )
    } else {
        (
            t("Follow"),
            t("Follow: replies notify you and show under Threads"),
            palette.secondary,
        )
    };
    ui.add(
        egui::Button::new(RichText::new(label).font(theme::medium(13.0)).color(color))
            .fill(palette.surface)
            .stroke(Stroke::new(
                1.0,
                if following {
                    palette.accent.gamma_multiply(0.6)
                } else {
                    palette.outline
                },
            ))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL + 2))
            .min_size(egui::Vec2::new(0.0, 26.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .on_hover_text(tip)
}
