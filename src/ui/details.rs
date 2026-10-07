//! A conversation's details beside it: its topic and description (which
//! members can change), when and by whom it was made, its members, its
//! pinned messages, its bookmarks and the files shared in it.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use crate::app::{App, WorkspaceState};
use crate::convos::{Action as Convos, ChannelData, Details, Field, Loaded, Tab};
use crate::i18n::{t, tf};
use crate::model::{Ability, Action, Conversation, ConversationKind, Workspace};
use crate::scopes::Feature;
use crate::theme::{self, Icon, Palette};

/// The height of a person or a file in the lists.
const ROW: f32 = 40.0;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let App {
        workspaces,
        settings,
        convos,
        actions,
        ..
    } = app;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        return;
    };
    let Some(details) = convos.details.as_mut() else {
        return;
    };
    // The panel belongs to the conversation on screen.
    let Some(conversation) = workspace
        .active
        .as_deref()
        .filter(|active| *active == details.channel)
        .and_then(|active| workspace.conversation(active))
    else {
        convos.details = None;
        return;
    };
    // A tab the workspace's service has no use for opens as About.
    if !shown(&workspace.info, details.tab) {
        details.tab = Tab::About;
    }
    let team = workspace.info.team_id.clone();
    let empty = ChannelData::default();
    let data = convos
        .data
        .get(&(team, conversation.id.clone()))
        .unwrap_or(&empty);
    egui::Panel::right("details")
        .resizable(true)
        .default_size(340.0)
        .size_range(280.0..=560.0)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().vline(
                rect.left() + 0.5,
                rect.y_range(),
                Stroke::new(1.0, palette.outline),
            );
            header(ui, &palette, workspace, conversation, actions);
            tabs(
                ui,
                &palette,
                &workspace.info,
                details,
                conversation,
                actions,
            );
            egui::Frame::new()
                .inner_margin(Margin::symmetric(16, 10))
                .show(ui, |ui| match details.tab {
                    Tab::About => about(
                        ui,
                        &palette,
                        workspace,
                        conversation,
                        details,
                        data,
                        actions,
                    ),
                    Tab::Members => members(ui, &palette, workspace, conversation, data, actions),
                    Tab::Files => files(ui, &palette, workspace, conversation, data, actions),
                    Tab::Pins => pins(ui, &palette, workspace, conversation, data, actions),
                    Tab::Bookmarks => {
                        let editable = workspace.info.can(Feature::EditBookmarks);
                        bookmarks(ui, &palette, conversation, data, editable, actions);
                    }
                });
        });
}

/// The conversation's name and a close button, level with the
/// conversation's own header.
fn header(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    actions: &mut Vec<Action>,
) {
    let inset = theme::titlebar_inset(ui.ctx());
    egui::Panel::top("details-header")
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
                let title = match conversation.kind {
                    ConversationKind::Channel => format!("# {}", conversation.name),
                    _ => workspace.title(conversation),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, palette, Icon::X, 16.0, &t("Close details")).clicked()
                    {
                        actions.push(Action::Convos(Convos::CloseDetails));
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(title)
                                    .font(theme::bold(16.0))
                                    .color(palette.text),
                            )
                            .truncate(),
                        );
                    });
                });
            });
        });
}

/// Whether the details panel shows `tab` for `workspace`: pins and
/// bookmarks only where its service has them.
fn shown(workspace: &Workspace, tab: Tab) -> bool {
    match tab {
        Tab::Pins => workspace.offers(Ability::Pins),
        Tab::Bookmarks => workspace.offers(Ability::Bookmarks),
        Tab::About | Tab::Members | Tab::Files => true,
    }
}

fn tabs(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &Workspace,
    details: &Details,
    conversation: &Conversation,
    actions: &mut Vec<Action>,
) {
    egui::Frame::new()
        .inner_margin(Margin {
            left: 12,
            right: 12,
            top: 8,
            bottom: 0,
        })
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let members = match conversation.members {
                    Some(count) => tf("Members ({count})", &[("count", &count.to_string())]),
                    None => t("Members").into_owned(),
                };
                for (tab, label) in [
                    (Tab::About, t("About").into_owned()),
                    (Tab::Members, members),
                    (Tab::Pins, t("Pinned").into_owned()),
                    (Tab::Bookmarks, t("Bookmarks").into_owned()),
                    (Tab::Files, t("Files").into_owned()),
                ] {
                    if !shown(workspace, tab) {
                        continue;
                    }
                    let text = RichText::new(label).font(theme::medium(13.5)).color(
                        if details.tab == tab {
                            palette.text
                        } else {
                            palette.secondary
                        },
                    );
                    if ui.selectable_label(details.tab == tab, text).clicked() {
                        actions.push(Action::Convos(Convos::Details {
                            channel: conversation.id.clone(),
                            tab,
                        }));
                    }
                }
            });
        });
    let rect = ui.cursor();
    ui.painter().hline(
        rect.x_range(),
        rect.top() + 4.0,
        Stroke::new(1.0, palette.outline),
    );
    ui.add_space(4.0);
}

/// The topic, the description, who made the conversation and when, and
/// leaving it.
fn about(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    details: &mut Details,
    data: &ChannelData,
    actions: &mut Vec<Action>,
) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // One person's DM has no topic to set.
            if conversation.kind != ConversationKind::Direct {
                for (field, label, text) in [
                    (Field::Topic, t("Topic"), &conversation.topic),
                    (Field::Purpose, t("Description"), &conversation.purpose),
                ] {
                    let described = Described {
                        field,
                        label: &label,
                        text,
                        editable: workspace.info.offers(Ability::Describe),
                    };
                    describable(ui, palette, conversation, details, described, actions);
                    ui.add_space(12.0);
                }
            }
            super::section_label(ui, palette, &t("Created"));
            match &data.about {
                Loaded::Ready(about) => {
                    let date = about.created.and_then(date_of);
                    let line = match (date, &about.creator) {
                        (Some(date), Some(creator)) => tf(
                            "By {name} on {date}",
                            &[("name", &workspace.user_label(creator)), ("date", &date)],
                        ),
                        (Some(date), None) => date,
                        (None, Some(creator)) => {
                            tf("By {name}", &[("name", &workspace.user_label(creator))])
                        }
                        (None, None) => t("Unknown").into_owned(),
                    };
                    ui.label(RichText::new(line).color(palette.text));
                }
                Loaded::Failed(error) => {
                    ui.label(RichText::new(error.sentence()).color(palette.dim));
                }
                Loaded::Idle | Loaded::Loading => {
                    ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                }
            }
            if !conversation.kind.is_dm() {
                ui.add_space(18.0);
                let leave = egui::Button::new(
                    RichText::new(t("Leave channel"))
                        .font(theme::medium(14.0))
                        .color(palette.danger),
                )
                .fill(palette.surface)
                .min_size(Vec2::new(0.0, 32.0));
                if ui.add(leave).clicked() {
                    actions.push(Action::Convos(Convos::AskLeave {
                        channel: conversation.id.clone(),
                    }));
                }
            }
        });
}

/// A topic or a description as the details panel shows it.
struct Described<'a> {
    /// Which of the two it is.
    field: Field,
    /// Its heading, in words.
    label: &'a str,
    /// What it says now.
    text: &'a str,
    /// Whether the workspace's service lets it be changed here.
    editable: bool,
}

/// A topic or description, with "Edit" to change it in place.
fn describable(
    ui: &mut egui::Ui,
    palette: &Palette,
    conversation: &Conversation,
    details: &mut Details,
    described: Described<'_>,
    actions: &mut Vec<Action>,
) {
    let Described {
        field,
        label,
        text,
        editable,
    } = described;
    ui.horizontal(|ui| {
        super::section_label(ui, palette, label);
        let editing = details.editing.as_ref().is_some_and(|e| e.field == field);
        if editable
            && !editing
            && ui
                .link(RichText::new(t("Edit")).font(theme::regular(12.5)))
                .clicked()
        {
            details.editing = Some(crate::convos::Describing {
                field,
                draft: text.to_owned(),
            });
        }
    });
    match &mut details.editing {
        Some(crate::convos::Describing {
            field: editing,
            draft,
        }) if *editing == field => {
            let id = egui::Id::new(("describe", conversation.id.as_str(), label));
            let field_response = ui.add(
                egui::TextEdit::multiline(draft)
                    .id(id)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 6)),
            );
            if !field_response.has_focus() && !ui.ctx().memory(|m| m.focused().is_some()) {
                field_response.request_focus();
            }
            let mut save = field_response.has_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            let mut cancel = false;
            ui.horizontal(|ui| {
                cancel = theme::secondary_button(ui, palette, &t("Cancel")).clicked();
                save |= theme::primary_button(ui, palette, &t("Save")).clicked();
            });
            if save {
                actions.push(Action::Convos(Convos::Describe {
                    channel: conversation.id.clone(),
                    field,
                    text: draft.trim().to_owned(),
                }));
            } else if cancel {
                details.editing = None;
            }
        }
        _ if text.is_empty() => {
            ui.label(RichText::new(t("None yet")).color(palette.dim));
        }
        _ => {
            let shown = crate::mrkdwn::plain(text, |_| None);
            ui.label(RichText::new(shown).color(palette.text));
        }
    }
}

/// What a tab shows while its list loads or after it failed. Returns the
/// list once it is there.
fn loaded<'a, T>(
    ui: &mut egui::Ui,
    palette: &Palette,
    conversation: &Conversation,
    tab: Tab,
    loaded: &'a Loaded<T>,
    actions: &mut Vec<Action>,
) -> Option<&'a T> {
    match loaded {
        Loaded::Ready(list) => Some(list),
        Loaded::Failed(error) => {
            ui.label(RichText::new(error.sentence()).color(palette.dim));
            if ui.link(t("Try again")).clicked() {
                actions.push(Action::Convos(Convos::Details {
                    channel: conversation.id.clone(),
                    tab,
                }));
            }
            None
        }
        Loaded::Idle | Loaded::Loading => {
            ui.add(egui::Spinner::new().size(16.0).color(palette.dim));
            None
        }
    }
}

/// Everyone in the conversation; a click shows their profile.
fn members(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    data: &ChannelData,
    actions: &mut Vec<Action>,
) {
    let Some(members) = loaded(
        ui,
        palette,
        conversation,
        Tab::Members,
        &data.members,
        actions,
    ) else {
        return;
    };
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW, members.len(), |ui, range| {
            for id in &members[range] {
                let user = workspace.user(id);
                let name = workspace.user_label(id);
                let (rect, row) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW), Sense::click());
                if row.hovered() {
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(theme::RADIUS_SMALL),
                        palette.surface_hover,
                    );
                }
                let picture = egui::Rect::from_center_size(
                    egui::pos2(rect.left() + 18.0, rect.center().y),
                    Vec2::splat(28.0),
                );
                super::paint_avatar(
                    ui,
                    picture,
                    user.and_then(|u| u.avatar.as_deref()),
                    &name,
                    id,
                );
                let behind = if row.hovered() {
                    palette.surface_hover
                } else {
                    palette.window
                };
                super::people::dot(
                    ui.painter(),
                    palette,
                    picture,
                    workspace.people.presence(id),
                    behind,
                );
                let label = ui.painter().text(
                    egui::pos2(rect.left() + 42.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &name,
                    theme::semibold(14.0),
                    palette.text,
                );
                if let Some(user) = user {
                    let detail = if !user.title.is_empty() {
                        user.title.clone()
                    } else if user.real_name != name {
                        user.real_name.clone()
                    } else {
                        String::new()
                    };
                    let detail = ui.painter().text(
                        egui::pos2(label.right() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        detail,
                        theme::regular(12.5),
                        palette.dim,
                    );
                    if crate::people::is_external(workspace, id) {
                        let at = egui::Rect::from_center_size(
                            egui::pos2(detail.right() + 14.0, rect.center().y),
                            Vec2::splat(12.0),
                        );
                        theme::Icon::Globe.image(palette.dim, 12.0).paint_at(ui, at);
                    }
                }
                theme::describe(&row, egui::WidgetType::Button, &name);
                if row
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    actions.push(Action::OpenProfile(id.clone()));
                }
            }
        });
}

/// The newest files shared here; a click shows an image or saves a file.
fn files(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    data: &ChannelData,
    actions: &mut Vec<Action>,
) {
    let Some(files) = loaded(ui, palette, conversation, Tab::Files, &data.files, actions) else {
        return;
    };
    // A file you just deleted goes at once.
    let files: Vec<&crate::convos::SharedFile> = files
        .iter()
        .filter(|shared| workspace.shows_file(&shared.file.id))
        .collect();
    if files.is_empty() {
        ui.label(RichText::new(t("No files shared here yet.")).color(palette.dim));
        return;
    }
    let team = &workspace.info.team_id;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW + 4.0, files.len(), |ui, range| {
            for shared in &files[range] {
                let file = &shared.file;
                let deletable = file.deletable_by(&workspace.info.user_id)
                    && workspace.info.offers(Ability::Files);
                let (rect, row) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), ROW + 4.0),
                    Sense::click(),
                );
                if row.hovered() {
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(theme::RADIUS_SMALL),
                        palette.surface_hover,
                    );
                }
                let icon = if file.is_image() {
                    Icon::Image
                } else {
                    Icon::FileText
                };
                icon.image(palette.accent, 20.0).paint_at(
                    ui,
                    egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 18.0, rect.center().y),
                        Vec2::splat(20.0),
                    ),
                );
                let name = if file.title.is_empty() {
                    &file.name
                } else {
                    &file.title
                };
                let mut job = egui::text::LayoutJob::simple_singleline(
                    name.clone(),
                    theme::semibold(14.0),
                    palette.text,
                );
                job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 48.0);
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(
                    egui::pos2(rect.left() + 40.0, rect.top() + 4.0),
                    galley,
                    palette.text,
                );
                let mut detail = vec![super::file_size(file.size)];
                if let Some(user) = &shared.user {
                    detail.push(workspace.user_label(user));
                }
                if let Some(date) = shared.created.and_then(date_of) {
                    detail.push(date);
                }
                ui.painter().text(
                    egui::pos2(rect.left() + 40.0, rect.bottom() - 6.0),
                    egui::Align2::LEFT_BOTTOM,
                    detail.join(" · "),
                    theme::regular(12.0),
                    palette.dim,
                );
                theme::describe(&row, egui::WidgetType::Button, name);
                if deletable {
                    row.context_menu(|ui| {
                        crate::ui::context::delete_file_item(ui, &file.id, &file.name, actions);
                    });
                }
                if row
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    if file.is_image()
                        && let Some(url) = file.url_private.as_ref().or(file.thumb.as_ref())
                    {
                        actions.push(Action::Preview {
                            uri: super::image_uri(team, url),
                            name: file.name.clone(),
                        });
                    } else if let Some(url) =
                        file.download_url.as_ref().or(file.url_private.as_ref())
                    {
                        actions.push(Action::Download {
                            url: url.clone(),
                            name: file.name.clone(),
                        });
                    }
                }
            }
        });
}

/// The pinned messages, newest pin first; a click opens the message as a
/// thread, and each can be unpinned.
fn pins(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    data: &ChannelData,
    actions: &mut Vec<Action>,
) {
    let Some(pins) = loaded(ui, palette, conversation, Tab::Pins, &data.pins, actions) else {
        return;
    };
    if pins.is_empty() {
        ui.label(
            RichText::new(t(
                "Nothing is pinned here yet. Pin a message from its toolbar.",
            ))
            .color(palette.dim),
        );
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for pin in pins {
                let message = &pin.message;
                let author = workspace.author(message);
                let card = egui::Frame::new()
                    .fill(palette.surface)
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            super::avatar(
                                ui,
                                workspace.author_icon(message),
                                &author,
                                message.user.as_deref().unwrap_or(&author),
                                20.0,
                            );
                            ui.label(
                                RichText::new(&author)
                                    .font(theme::semibold(13.5))
                                    .color(palette.text),
                            );
                            if let Some(day) = message.ts.seconds().and_then(date_of) {
                                ui.label(
                                    RichText::new(day)
                                        .font(theme::regular(12.0))
                                        .color(palette.dim),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if theme::icon_button(
                                        ui,
                                        palette,
                                        Icon::PinOff,
                                        14.0,
                                        &t("Unpin"),
                                    )
                                    .clicked()
                                    {
                                        actions.push(Action::Convos(Convos::Pin {
                                            channel: conversation.id.clone(),
                                            ts: message.ts.clone(),
                                            pin: false,
                                        }));
                                    }
                                },
                            );
                        });
                        let text = super::message::plain_text(workspace, message);
                        ui.add(egui::Label::new(RichText::new(text).color(palette.text)).wrap());
                        if let Some(by) = &pin.by {
                            ui.label(
                                RichText::new(tf(
                                    "Pinned by {name}",
                                    &[("name", &workspace.user_label(by))],
                                ))
                                .font(theme::regular(12.0))
                                .color(palette.dim),
                            );
                        }
                    });
                let open = card
                    .response
                    .interact(Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t("Open in a thread"));
                if open.clicked() {
                    actions.push(Action::OpenThread {
                        channel: conversation.id.clone(),
                        ts: message
                            .thread_ts
                            .clone()
                            .unwrap_or_else(|| message.ts.clone()),
                    });
                }
                ui.add_space(6.0);
            }
        });
}

/// The links saved at the top of the conversation; a click opens one, and
/// each has a menu to edit or remove it. "Add a bookmark" adds one. A
/// sign-in without `bookmarks:write` (an app made from an older manifest)
/// is not `editable`: it only lists them.
fn bookmarks(
    ui: &mut egui::Ui,
    palette: &Palette,
    conversation: &Conversation,
    data: &ChannelData,
    editable: bool,
    actions: &mut Vec<Action>,
) {
    let Some(bookmarks) = loaded(
        ui,
        palette,
        conversation,
        Tab::Bookmarks,
        &data.bookmarks,
        actions,
    ) else {
        return;
    };
    if editable && !conversation.archived {
        let add = egui::Button::image_and_text(
            Icon::Plus.image(palette.text, 14.0),
            RichText::new(t("Add a bookmark"))
                .font(theme::medium(14.0))
                .color(palette.text),
        )
        .fill(palette.surface)
        .min_size(Vec2::new(0.0, 32.0));
        if ui.add(add).clicked() {
            actions.push(Action::Convos(Convos::AskBookmark {
                channel: conversation.id.clone(),
                bookmark: None,
            }));
        }
        ui.add_space(6.0);
    }
    if bookmarks.is_empty() {
        ui.label(RichText::new(t("No bookmarks here yet.")).color(palette.dim));
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW, bookmarks.len(), |ui, range| {
            for bookmark in &bookmarks[range] {
                let (rect, row) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW), Sense::click());
                // Not saved yet, it has no id Slack would know to change.
                let menu = editable && !bookmark.is_local() && !conversation.archived;
                if row.hovered() {
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(theme::RADIUS_SMALL),
                        palette.surface_hover,
                    );
                }
                let left = egui::pos2(rect.left() + 18.0, rect.center().y);
                match bookmark
                    .emoji
                    .as_deref()
                    .and_then(|e| crate::emoji::unicode(e, None))
                {
                    Some(emoji) => {
                        ui.painter().text(
                            left,
                            egui::Align2::CENTER_CENTER,
                            emoji,
                            theme::regular(16.0),
                            palette.text,
                        );
                    }
                    None => Icon::Bookmark
                        .image(palette.accent, 16.0)
                        .paint_at(ui, egui::Rect::from_center_size(left, Vec2::splat(16.0))),
                }
                let mut job = egui::text::LayoutJob::simple_singleline(
                    bookmark.title.clone(),
                    theme::semibold(14.0),
                    palette.text,
                );
                let room = if menu { 80.0 } else { 48.0 };
                job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - room);
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(
                    egui::pos2(rect.left() + 40.0, rect.center().y - galley.size().y / 2.0),
                    galley,
                    palette.text,
                );
                theme::describe(&row, egui::WidgetType::Link, &bookmark.title);
                let row = row
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(&bookmark.link);
                if row.clicked() {
                    actions.push(Action::OpenUrl(bookmark.link.clone()));
                }
                if !menu {
                    continue;
                }
                row.context_menu(|ui| bookmark_menu(ui, conversation, bookmark, actions));
                // Drawn after the row, so it takes the clicks over it.
                let at = egui::Rect::from_center_size(
                    egui::pos2(rect.right() - 18.0, rect.center().y),
                    Vec2::splat(28.0),
                );
                let more = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(at), |ui| {
                        theme::icon_button(
                            ui,
                            palette,
                            Icon::Ellipsis,
                            16.0,
                            &tf("More for {name}", &[("name", &bookmark.title)]),
                        )
                    })
                    .inner;
                egui::Popup::menu(&more)
                    .id(egui::Id::new((
                        "bookmark-menu",
                        &conversation.id,
                        &bookmark.id,
                    )))
                    .show(|ui| bookmark_menu(ui, conversation, bookmark, actions));
            }
        });
}

/// What a bookmark's menu offers: editing it and removing it.
fn bookmark_menu(
    ui: &mut egui::Ui,
    conversation: &Conversation,
    bookmark: &crate::convos::Bookmark,
    actions: &mut Vec<Action>,
) {
    if ui.button(t("Edit bookmark")).clicked() {
        actions.push(Action::Convos(Convos::AskBookmark {
            channel: conversation.id.clone(),
            bookmark: Some(bookmark.clone()),
        }));
        ui.close();
    }
    if ui.button(t("Remove bookmark")).clicked() {
        actions.push(Action::Convos(Convos::AskRemoveBookmark {
            channel: conversation.id.clone(),
            bookmark: bookmark.clone(),
        }));
        ui.close();
    }
}

/// A day, written out: "Monday, March 3, 2025".
fn date_of(seconds: i64) -> Option<String> {
    let zoned = jiff::Timestamp::from_second(seconds)
        .ok()?
        .to_zoned(jiff::tz::TimeZone::system());
    Some(super::long_date(&crate::i18n::t, zoned.date(), true))
}
