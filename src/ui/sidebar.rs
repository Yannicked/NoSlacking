//! The workspace rail and the conversation list.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use crate::app::{App, Page, WorkspaceState};
use crate::i18n::{t, tf, tn};
use crate::model::{Action, Conversation, ConversationKind, SectionKind, SidebarSection};
use crate::sidebar::{self, SidebarEdit};
use crate::theme::{self, Icon, Palette};

/// Direct messages listed before "Show more".
const DM_LIMIT: usize = 25;

pub fn rail(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let rail_fill = if palette.dark {
        palette.panel.gamma_multiply(0.8)
    } else {
        palette.surface_active
    };
    let inset = theme::titlebar_inset(ui.ctx());
    egui::Panel::left("rail")
        .exact_size(theme::RAIL_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(rail_fill).inner_margin(Margin {
            left: 0,
            right: 0,
            top: 12 + inset as i8,
            bottom: 12,
        }))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                let active = app.active_team();
                for workspace in &app.workspaces {
                    let selected = active.as_deref() == Some(workspace.info.team_id.as_str());
                    let unread = workspace
                        .conversations
                        .iter()
                        .any(|c| workspace.is_unread(c));
                    let mentions: u32 = workspace.conversations.iter().map(|c| c.mentions).sum();
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
                    if selected {
                        ui.painter().rect_stroke(
                            rect.expand(3.0),
                            CornerRadius::same(12),
                            Stroke::new(2.0, palette.text),
                            egui::StrokeKind::Outside,
                        );
                    }
                    super::paint_avatar(
                        ui,
                        rect,
                        workspace.info.icon.as_deref(),
                        &workspace.info.name,
                        &workspace.info.team_id,
                    );
                    if workspace.signed_out.is_some() {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(9), palette.shadow);
                        Icon::CircleAlert.image(palette.warning, 18.0).paint_at(
                            ui,
                            egui::Rect::from_center_size(rect.center(), Vec2::splat(18.0)),
                        );
                    }
                    if mentions > 0 && !selected {
                        let dot = egui::Rect::from_center_size(
                            rect.right_bottom() - Vec2::splat(2.0),
                            // "9+" needs a pill rather than a dot.
                            Vec2::new(if mentions > 9 { 22.0 } else { 16.0 }, 16.0),
                        );
                        ui.painter()
                            .rect_filled(dot, CornerRadius::same(8), palette.badge);
                        ui.painter().text(
                            dot.center(),
                            egui::Align2::CENTER_CENTER,
                            rail_count(mentions),
                            theme::bold(10.0),
                            egui::Color32::WHITE,
                        );
                    } else if unread && !selected {
                        let left = egui::pos2(rect.left() - 11.0, rect.center().y);
                        ui.painter().rect_filled(
                            egui::Rect::from_center_size(left, Vec2::new(4.0, 8.0)),
                            CornerRadius::same(2),
                            palette.text,
                        );
                    }
                    let tip = match &workspace.signed_out {
                        Some(reason) => format!("{} ({reason})", workspace.info.name),
                        None => workspace.info.name.clone(),
                    };
                    theme::focus_ring(ui, &response, &palette, 12);
                    theme::describe_selected(
                        &response,
                        egui::WidgetType::SelectableLabel,
                        selected,
                        &spoken(&tip, unread, mentions),
                    );
                    let response = response
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(tip);
                    if response.clicked() {
                        app.actions
                            .push(Action::SelectWorkspace(workspace.info.team_id.clone()));
                    }
                }
                let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
                let fill = if response.hovered() {
                    palette.surface_hover
                } else {
                    palette.surface
                };
                ui.painter().rect_filled(rect, CornerRadius::same(9), fill);
                Icon::Plus.image(palette.secondary, 20.0).paint_at(
                    ui,
                    egui::Rect::from_center_size(rect.center(), Vec2::splat(20.0)),
                );
                theme::focus_ring(ui, &response, &palette, 9);
                theme::describe(&response, egui::WidgetType::Button, &t("Add a workspace"));
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t("Add a workspace"))
                    .clicked()
                {
                    app.actions.push(Action::AddWorkspace);
                }
            });
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                let settings_open = app.page == Page::Settings;
                let tip = tf(
                    "Settings ({shortcut})",
                    &[("shortcut", &super::keys::command(","))],
                );
                let response = theme::icon_button(ui, &palette, Icon::Settings, 20.0, &tip);
                if response.clicked() {
                    app.actions.push(if settings_open {
                        Action::HideSettings
                    } else {
                        Action::ShowSettings
                    });
                }
                if let Some(workspace) = crate::app::active_in(&app.workspaces, &app.settings) {
                    super::people::me_button(ui, &palette, workspace, &mut app.actions);
                }
            });
        });
}

/// What a screen reader says for a workspace or a conversation: its name,
/// and whether there is something new in it.
fn spoken(name: &str, unread: bool, mentions: u32) -> String {
    if mentions > 0 {
        tf(
            "{name}, {mentions}",
            &[
                ("name", name),
                (
                    "mentions",
                    &tn("{count} mention", "{count} mentions", mentions),
                ),
            ],
        )
    } else if unread {
        tf("{name}, unread", &[("name", name)])
    } else {
        name.to_owned()
    }
}

/// The mention count on a workspace icon. The dot fits one digit, and
/// "9" for twelve mentions would undercount.
fn rail_count(mentions: u32) -> String {
    if mentions > 9 {
        "9+".to_owned()
    } else {
        mentions.to_string()
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let width = app.settings.sidebar_width;
    let inset = theme::titlebar_inset(ui.ctx());
    let App {
        workspaces,
        settings,
        actions,
        sidebar_filter,
        socket,
        ..
    } = app;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        return;
    };
    let response = egui::Panel::left("sidebar")
        .resizable(true)
        .default_size(width)
        .size_range(200.0..=440.0)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(palette.panel))
        .show(ui, |ui| {
            egui::Panel::top("sidebar-header")
                .exact_size(52.0 + inset)
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(Margin {
                    left: 16,
                    right: 10,
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
                        ui.add(
                            egui::Label::new(
                                RichText::new(&workspace.info.name)
                                    .font(theme::bold(16.0))
                                    .color(palette.text),
                            )
                            .truncate(),
                        );
                        let (color, tip) = match socket {
                            crate::backend::Socket::Connected => (palette.accent, t("Live")),
                            crate::backend::Socket::Off => (palette.dim, t("Live updates off")),
                            crate::backend::Socket::Connecting => {
                                (palette.warning, t("Connecting…"))
                            }
                            _ => (palette.danger, t("Offline")),
                        };
                        let (dot, response) =
                            ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 4.0, color);
                        response.on_hover_text(tip);
                        super::desktop::dnd_button(ui, &palette, workspace, actions);
                        if workspace.sections.is_some() {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if theme::icon_button(
                                        ui,
                                        &palette,
                                        Icon::Plus,
                                        16.0,
                                        &t("New section"),
                                    )
                                    .clicked()
                                    {
                                        actions.push(Action::NameSection {
                                            rename: None,
                                            channel: None,
                                        });
                                    }
                                },
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            super::browse::header_buttons(ui, &palette, actions);
                            if workspace.sections.is_some()
                                && theme::icon_button(
                                    ui,
                                    &palette,
                                    Icon::Plus,
                                    16.0,
                                    &t("New section"),
                                )
                                .clicked()
                            {
                                actions.push(Action::NameSection {
                                    rename: None,
                                    channel: None,
                                });
                            }
                        });
                    });
                });
            if let Some(reason) = &workspace.signed_out {
                egui::Frame::new()
                    .fill(palette.warning.gamma_multiply(0.15))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(tf("Signed out: {reason}", &[("reason", reason)]))
                                .font(theme::regular(12.5))
                                .color(palette.text),
                        );
                        if ui.link(t("Sign in again")).clicked() {
                            actions.push(Action::AddWorkspace);
                        }
                    });
            }
            egui::Frame::new()
                .inner_margin(Margin {
                    left: 10,
                    right: 10,
                    top: 10,
                    bottom: 4,
                })
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(sidebar_filter)
                            .id(egui::Id::new("sidebar-filter"))
                            // No shortcut here: Ctrl+K opens the switcher.
                            .hint_text(t("Find a conversation"))
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(8, 5)),
                    );
                });
            egui::ScrollArea::vertical()
                .id_salt(("sidebar", &workspace.info.team_id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    list(
                        ui,
                        &palette,
                        workspace,
                        sidebar_filter,
                        settings.sidebar_sort,
                        actions,
                    );
                    ui.add_space(12.0);
                });
        });
    let width = response.response.rect.width();
    if (width - app.settings.sidebar_width).abs() > 1.0 {
        app.settings.sidebar_width = width;
        app.settings_changed();
    }
}

fn matches(workspace: &WorkspaceState, conversation: &Conversation, filter: &str) -> bool {
    filter.is_empty()
        || workspace
            .title(conversation)
            .to_lowercase()
            .contains(&filter.to_lowercase())
}

fn list(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    filter: &str,
    sort: sidebar::Sort,
    actions: &mut Vec<Action>,
) {
    let filter = filter.trim();
    let sections = workspace.sections.as_deref();
    // Remembered per workspace: sorting every conversation each frame is
    // the sidebar's main cost, and the order rarely changes.
    let memo_id = egui::Id::new(("sidebar-layout", workspace.info.team_id.as_str()));
    let shown = ui.data_mut(|d| {
        d.get_temp_mut_or_default::<sidebar::Memo>(memo_id).layout(
            sections,
            &workspace.conversations,
            &workspace.users,
            |c| workspace.title(c),
            sort,
        )
    });
    for section in &shown {
        let rows: Vec<&Conversation> = section
            .conversations
            .iter()
            .copied()
            .filter(|c| matches(workspace, c, filter))
            .filter(|c| {
                // Skip deactivated people's DMs unless they have something new.
                c.user
                    .as_deref()
                    .and_then(|u| workspace.user(u))
                    .is_none_or(|u| !u.deleted)
                    || c.has_unread()
            })
            .collect();
        if !filter.is_empty() && rows.is_empty() {
            continue;
        }
        let key = section
            .id
            .clone()
            .unwrap_or_else(|| format!("{:?}", section.kind));
        let limit = (section.kind == SectionKind::DirectMessages).then_some(DM_LIMIT);
        section_view(
            ui, palette, workspace, section, &key, &rows, limit, filter, actions,
        );
    }
    if workspace.conversations.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn section_view(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    section: &sidebar::Shown<'_>,
    key: &str,
    rows: &[&Conversation],
    limit: Option<usize>,
    filter: &str,
    actions: &mut Vec<Action>,
) {
    // Folded state survives restarts, per section.
    let open_id = egui::Id::new(("section-open", key));
    let open = ui
        .data_mut(|d| d.get_persisted::<bool>(open_id))
        .unwrap_or(true);
    let more_id = egui::Id::new(("section-more", key));
    let more = ui.data(|d| d.get_temp::<bool>(more_id)).unwrap_or(false);
    ui.add_space(8.0);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
    let icon = if open || !filter.is_empty() {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    };
    icon.image(palette.dim, 14.0).paint_at(
        ui,
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            Vec2::splat(14.0),
        ),
    );
    let mut job = egui::text::LayoutJob::simple_singleline(
        section.title.clone(),
        theme::semibold(13.0),
        palette.secondary,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 40.0);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0),
        galley,
        palette.secondary,
    );
    theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
    theme::describe_selected(
        &response,
        egui::WidgetType::CollapsingHeader,
        open || !filter.is_empty(),
        &section.title,
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        ui.data_mut(|d| d.insert_persisted(open_id, !open));
    }
    if let (Some(id), Some(sections)) = (&section.id, workspace.sections.as_deref()) {
        section_menu(&response, id, section.kind, sections, actions);
    }
    let rows_to_show: Vec<&&Conversation> = if open || !filter.is_empty() {
        let shown = match limit {
            Some(limit) if !more && filter.is_empty() => limit,
            _ => usize::MAX,
        };
        rows.iter().take(shown).collect()
    } else {
        // A folded section still shows what is unread or open.
        rows.iter()
            .filter(|c| {
                workspace.is_unread(c) || workspace.active.as_deref() == Some(c.id.as_str())
            })
            .collect()
    };
    for conversation in &rows_to_show {
        row(ui, palette, workspace, conversation, section, actions);
    }
    if (open || !filter.is_empty()) && rows.len() > rows_to_show.len() {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
        let label = tf(
            "Show more ({count})",
            &[("count", &(rows.len() - rows_to_show.len()).to_string())],
        );
        theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
        theme::describe(&response, egui::WidgetType::Button, &label);
        ui.painter().text(
            egui::pos2(rect.left() + 32.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            theme::medium(13.0),
            palette.dim,
        );
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            ui.data_mut(|d| d.insert_temp(more_id, true));
        }
    }
}

/// Right-click on a section header: new section, and for your own
/// sections rename, move and delete.
fn section_menu(
    response: &egui::Response,
    id: &str,
    kind: SectionKind,
    sections: &[SidebarSection],
    actions: &mut Vec<Action>,
) {
    response.context_menu(|ui| {
        if ui.button(t("New section…")).clicked() {
            actions.push(Action::NameSection {
                rename: None,
                channel: None,
            });
            ui.close();
        }
        if kind == SectionKind::Custom {
            if ui.button(t("Rename…")).clicked() {
                actions.push(Action::NameSection {
                    rename: Some(id.to_owned()),
                    channel: None,
                });
                ui.close();
            }
            ui.separator();
            let can_up = crate::sidebar::can_shift(sections, id, true);
            let can_down = crate::sidebar::can_shift(sections, id, false);
            if ui
                .add_enabled(can_up, egui::Button::new(t("Move up")))
                .clicked()
            {
                actions.push(Action::Sidebar(SidebarEdit::Shift {
                    section: id.to_owned(),
                    up: true,
                }));
                ui.close();
            }
            if ui
                .add_enabled(can_down, egui::Button::new(t("Move down")))
                .clicked()
            {
                actions.push(Action::Sidebar(SidebarEdit::Shift {
                    section: id.to_owned(),
                    up: false,
                }));
                ui.close();
            }
            ui.separator();
            if ui.button(t("Delete section")).clicked() {
                // Its conversations go back to Channels and Direct messages.
                actions.push(Action::Sidebar(SidebarEdit::Delete {
                    section: id.to_owned(),
                }));
                ui.close();
            }
        }
    });
}

/// Right-click on a conversation: star it, move it to another section, or
/// make a new section for it.
fn row_menu(
    response: &egui::Response,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    section: &sidebar::Shown<'_>,
    actions: &mut Vec<Action>,
) {
    let Some(sections) = workspace.sections.as_deref() else {
        // Without Slack's sections, leaving is all there is to offer.
        if !conversation.kind.is_dm() {
            response.context_menu(|ui| super::browse::leave_item(ui, conversation, false, actions));
        }
        return;
    };
    let channel = conversation.id.clone();
    let starred = sidebar::is_starred(Some(sections), &channel);
    response.context_menu(|ui| {
        let star = if starred { t("Unstar") } else { t("Star") };
        if ui.button(star).clicked() {
            actions.push(Action::Sidebar(SidebarEdit::Star {
                channel: channel.clone(),
                starred: !starred,
            }));
            ui.close();
        }
        ui.menu_button(t("Move to section"), |ui| {
            for (id, title) in sidebar::targets(sections, section.id.as_deref()) {
                if ui.button(title).clicked() {
                    actions.push(Action::Sidebar(SidebarEdit::Move {
                        channel: channel.clone(),
                        from: section.id.clone(),
                        to: id,
                    }));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button(t("New section…")).clicked() {
                actions.push(Action::NameSection {
                    rename: None,
                    channel: Some(channel.clone()),
                });
                ui.close();
            }
        });
        if section.kind == SectionKind::Custom {
            let home = if conversation.kind.is_dm() {
                SectionKind::DirectMessages
            } else {
                SectionKind::Channels
            };
            if let Some(home) = sections.iter().find(|s| s.kind == home)
                && ui.button(t("Remove from section")).clicked()
            {
                actions.push(Action::Sidebar(SidebarEdit::Move {
                    channel: channel.clone(),
                    from: section.id.clone(),
                    to: home.id.clone(),
                }));
                ui.close();
            }
        }
        ui.separator();
        super::desktop::conversation_menu(ui, workspace, conversation, actions);
        super::browse::leave_item(ui, conversation, true, actions);
    });
}

fn row(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    section: &sidebar::Shown<'_>,
    actions: &mut Vec<Action>,
) {
    let selected = workspace.active.as_deref() == Some(conversation.id.as_str());
    let unread = workspace.is_unread(conversation) && !selected;
    let muted = workspace.desktop.is_muted(&conversation.id);
    let (outer, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    // A row scrolled out of view keeps its place, but its title, avatar
    // and badge are not laid out: a big workspace has hundreds of rows.
    if !ui.is_rect_visible(outer) {
        return;
    }
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
    let text_color = if selected {
        palette.on_accent
    } else if unread {
        palette.text
    } else if muted {
        // Quieter than the rest, as Slack draws muted conversations.
        palette.dim
    } else {
        palette.secondary
    };
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 18.0, rect.center().y),
        Vec2::splat(16.0),
    );
    let title = workspace.title(conversation);
    match conversation.kind {
        ConversationKind::Channel => Icon::Hash.image(text_color, 15.0).paint_at(ui, icon_rect),
        ConversationKind::Private => Icon::Lock.image(text_color, 14.0).paint_at(ui, icon_rect),
        ConversationKind::Group => Icon::Users.image(text_color, 15.0).paint_at(ui, icon_rect),
        ConversationKind::Direct => {
            let user = conversation
                .user
                .as_deref()
                .and_then(|id| workspace.user(id));
            let avatar_rect = egui::Rect::from_center_size(icon_rect.center(), Vec2::splat(20.0));
            super::paint_avatar(
                ui,
                avatar_rect,
                user.and_then(|u| u.avatar.as_deref()),
                &title,
                conversation.user.as_deref().unwrap_or(&title),
            );
            let behind = if selected {
                palette.accent
            } else if response.hovered() {
                palette.surface_hover
            } else {
                palette.panel
            };
            let presence = conversation
                .user
                .as_deref()
                .and_then(|id| workspace.people.presence(id));
            super::people::dot(ui.painter(), palette, avatar_rect, presence, behind);
        }
    }
    let font = if unread {
        theme::bold(14.5)
    } else {
        theme::regular(14.5)
    };
    let badge_width = if conversation.mentions > 0 && !selected {
        30.0
    } else {
        0.0
    };
    // A pencil for what you started writing here, as Slack shows it.
    let drafted = !selected
        && ui
            .data(|d| {
                d.get_temp::<std::sync::Arc<std::collections::HashSet<String>>>(super::drafts_id())
            })
            .is_some_and(|drafts| drafts.contains(&conversation.id));
    let draft_width = if drafted { 20.0 } else { 0.0 };
    let max_text = rect.width() - 36.0 - 8.0 - badge_width - draft_width;
    // One line, cut with an ellipsis.
    theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL + 2);
    theme::describe_selected(
        &response,
        egui::WidgetType::SelectableLabel,
        selected,
        &if drafted {
            tf(
                "{name}, draft",
                &[("name", &spoken(&title, unread, conversation.mentions))],
            )
        } else {
            spoken(&title, unread, conversation.mentions)
        },
    );
    if drafted {
        let pencil = egui::Rect::from_center_size(
            egui::pos2(
                rect.right() - badge_width - 4.0 - draft_width / 2.0,
                rect.center().y,
            ),
            Vec2::splat(13.0),
        );
        Icon::Pencil.image(palette.dim, 13.0).paint_at(ui, pencil);
    }
    let mut job = egui::text::LayoutJob::simple_singleline(title, font, text_color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_text);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0),
        galley,
        text_color,
    );
    if conversation.mentions > 0 && !selected {
        let mut badge = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_max(
                    egui::pos2(rect.right() - badge_width - 4.0, rect.top()),
                    rect.right_bottom(),
                ))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        super::badge(&mut badge, palette, conversation.mentions);
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        actions.push(Action::OpenConversation(conversation.id.clone()));
    }
    row_menu(&response, workspace, conversation, section, actions);
    if workspace.sections.is_none() {
        // Without Slack's sections there is no section menu to add to.
        response.context_menu(|ui| {
            super::desktop::conversation_menu(ui, workspace, conversation, actions);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_counts_say_when_there_are_more() {
        assert_eq!(rail_count(3), "3");
        assert_eq!(rail_count(9), "9");
        assert_eq!(rail_count(12), "9+");
    }
}
