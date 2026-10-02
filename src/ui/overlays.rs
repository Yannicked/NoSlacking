//! What floats over the window: the quick switcher, the emoji picker, a
//! person's card, an image preview, the delete confirmation and toasts.

use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Stroke, Vec2};

use crate::app::{App, PickerTarget};
use crate::i18n::t;
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
            ui.label(
                RichText::new(heading)
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.add_space(6.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut dialog.name)
                    .id(egui::Id::new("section-name"))
                    .hint_text(t("Section name"))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 6)),
            );
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
    let workspace_name = workspace.info.name.clone();
    let custom: Vec<(String, String)> = {
        let mut list: Vec<(String, String)> = workspace
            .emoji
            .custom_names()
            .filter(|(name, _)| needle.is_empty() || name.contains(&needle))
            .map(|(name, url)| (name.to_owned(), url.to_owned()))
            .collect();
        list.sort();
        list
    };
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
                let first = standard_emoji()
                    .iter()
                    .flat_map(|(_, list)| list.iter())
                    .filter_map(|e| e.shortcode())
                    .find(|code| code.contains(&needle));
                chosen = custom
                    .first()
                    .map(|(name, _)| name.clone())
                    .or_else(|| first.map(str::to_owned));
            }
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(340.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let cell = 36.0;
                    let columns = ((ui.available_width() / cell).floor() as usize).max(1);
                    if !custom.is_empty() {
                        super::section_label(ui, &palette, &workspace_name);
                        for chunk in custom.chunks(columns) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                for (name, url) in chunk {
                                    let (rect, response) =
                                        ui.allocate_exact_size(Vec2::splat(cell), Sense::click());
                                    if response.hovered() {
                                        ui.painter().rect_filled(
                                            rect,
                                            CornerRadius::same(6),
                                            palette.surface_hover,
                                        );
                                    }
                                    egui::Image::new(url.clone())
                                        .fit_to_exact_size(Vec2::splat(24.0))
                                        .paint_at(
                                            ui,
                                            egui::Rect::from_center_size(
                                                rect.center(),
                                                Vec2::splat(24.0),
                                            ),
                                        );
                                    if response.on_hover_text(format!(":{name}:")).clicked() {
                                        chosen = Some(name.clone());
                                    }
                                }
                            });
                        }
                    }
                    for (group, list) in standard_emoji() {
                        let list: Vec<&&emojis::Emoji> = list
                            .iter()
                            .filter(|e| {
                                needle.is_empty()
                                    || e.shortcodes().any(|code| code.contains(&needle))
                                    || e.name().contains(&needle)
                            })
                            .collect();
                        if list.is_empty() {
                            continue;
                        }
                        ui.add_space(4.0);
                        super::section_label(
                            ui,
                            &palette,
                            &format!("{group:?}").replace("And", " & "),
                        );
                        for chunk in list.chunks(columns) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                for emoji in chunk {
                                    let (rect, response) =
                                        ui.allocate_exact_size(Vec2::splat(cell), Sense::click());
                                    if response.hovered() {
                                        ui.painter().rect_filled(
                                            rect,
                                            CornerRadius::same(6),
                                            palette.surface_hover,
                                        );
                                    }
                                    ui.painter().text(
                                        rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        emoji.as_str(),
                                        theme::regular(22.0),
                                        palette.text,
                                    );
                                    let code = emoji.shortcode().unwrap_or_default();
                                    if response.on_hover_text(format!(":{code}:")).clicked() {
                                        chosen = Some(code.to_owned());
                                    }
                                }
                            });
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
                        RichText::new(format!("{} {}", local.strftime("%H:%M"), t("local time")))
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
