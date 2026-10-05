//! Writing a message: a growing text field with @mention and :emoji:
//! suggestions, attachments and the send button.

use egui::text::{CCursor, CCursorRange};
use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Stroke, Vec2};

use super::format::{self, Format};
use crate::app::{Draft, Upload, WorkspaceState};
use crate::i18n::{t, tf};
use crate::model::{Action, Ts};
use crate::theme::{self, Icon, Palette};

mod spelling;

pub struct Composer<'a> {
    pub palette: &'a Palette,
    pub workspace: &'a WorkspaceState,
    pub key: String,
    pub placeholder: String,
    pub thread: Option<Ts>,
    pub enter_sends: bool,
    pub focus: bool,
    /// The channel's name, for "also send to #channel" in threads.
    pub channel_name: Option<String>,
    /// Every upload in flight; the composer shows those sent from it.
    pub uploads: &'a [Upload],
}

/// A suggestion for the word being typed.
#[derive(Clone, Debug, PartialEq)]
enum Suggestion {
    User {
        id: String,
        label: String,
        detail: String,
        avatar: Option<String>,
    },
    Emoji {
        name: String,
    },
    Special(&'static str),
    /// A user group, by the handle you type to mention it.
    Group {
        id: String,
        handle: String,
        name: String,
        members: Option<usize>,
    },
    Channel {
        id: String,
        name: String,
        private: bool,
    },
    Command(&'static crate::slash::Known),
}

impl Suggestion {
    fn insert(&self) -> String {
        match self {
            Self::User { label, .. } => format!("@{label} "),
            Self::Emoji { name } => format!(":{name}: "),
            Self::Special(name) => format!("@{name} "),
            Self::Group { handle, .. } => format!("@{handle} "),
            Self::Channel { name, .. } => format!("#{name} "),
            Self::Command(known) => format!("/{} ", known.name),
        }
    }
}

/// What a slash command does, for its suggestion.
/// Whether this frame's Enter came with Shift: a new line, never a send.
/// Read from the key's own event, as egui's key matching ignores Shift.
pub(crate) fn shift_enter(input: &egui::InputState) -> bool {
    input.events.iter().any(|event| {
        matches!(
            event,
            egui::Event::Key {
                key: Key::Enter,
                pressed: true,
                modifiers,
                ..
            } if modifiers.shift
        )
    })
}

fn command_description(name: &str) -> std::borrow::Cow<'static, str> {
    match name {
        "me" => t("Say what you are doing, in italics"),
        "shrug" => t("Add a shrug to your message"),
        "status" => t("Set or clear your status"),
        "away" => t("Show yourself as away"),
        "active" => t("Show yourself as active"),
        "topic" => t("Set the conversation's topic"),
        "invite" => t("Add someone to this channel"),
        "leave" => t("Leave this channel"),
        "remind" => t("Set a reminder"),
        _ => t("Run a command"),
    }
}

/// What `@here`, `@channel` and `@everyone` each do, for the suggestion.
fn broadcast_description(name: &str) -> std::borrow::Cow<'static, str> {
    match name {
        "here" => t("Notify everyone online in this conversation"),
        "channel" => t("Notify every member of this conversation"),
        _ => t("Notify everyone in the workspace"),
    }
}

/// A user group's name and, when known, how many are in it.
fn group_detail(name: &str, members: Option<usize>) -> String {
    let count = members.map(|n| {
        crate::i18n::tn(
            "{count} member",
            "{count} members",
            u32::try_from(n).unwrap_or(u32::MAX),
        )
    });
    match count {
        Some(count) if name.is_empty() => count,
        Some(count) => format!("{name} · {count}"),
        None => name.to_owned(),
    }
}

/// The word before the cursor, and where it starts (in chars).
fn current_word(text: &str, cursor: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let word: String = chars[start..cursor].iter().collect();
    (!word.is_empty()).then_some((start, word))
}

/// Someone who can be mentioned, with names lower-cased once rather than
/// for every person on every frame of typing.
#[derive(Clone, Debug)]
struct Person {
    id: String,
    label: String,
    detail: String,
    avatar: Option<String>,
    is_bot: bool,
    /// Lower-cased: the handle, the real name and the display name.
    names: [String; 3],
    label_lower: String,
}

/// Everyone who can be mentioned, built when people arrive.
#[derive(Clone, Debug, Default)]
struct People {
    /// `users.len()` and [`WorkspaceState::users_version`] when built.
    version: (usize, u64),
    people: Vec<Person>,
}

impl People {
    fn build(workspace: &WorkspaceState) -> Self {
        let people = workspace
            .users
            .values()
            .filter(|u| !u.deleted)
            .map(|u| Person {
                id: u.id.clone(),
                label: u.label().to_owned(),
                detail: if u.real_name.is_empty() || u.real_name == u.label() {
                    u.name.clone()
                } else {
                    u.real_name.clone()
                },
                avatar: u.avatar.clone(),
                is_bot: u.is_bot,
                names: [
                    u.name.to_lowercase(),
                    u.real_name.to_lowercase(),
                    u.display_name.to_lowercase(),
                ],
                label_lower: u.label().to_lowercase(),
            })
            .collect();
        Self {
            version: People::version_of(workspace),
            people,
        }
    }

    fn version_of(workspace: &WorkspaceState) -> (usize, u64) {
        (workspace.users.len(), workspace.users_version())
    }
}

/// How many custom emoji, conversations and user groups a workspace has:
/// when one changes, remembered suggestions may be out of date.
type Counts = (usize, usize, usize);

/// The suggestions for the last word typed, kept per composer: the field
/// asks again on every frame while the word stays the same.
#[derive(Clone, Debug, Default)]
struct Memo {
    people: People,
    /// The word, the counts, and what was found.
    last: Option<(String, Counts, Vec<Suggestion>)>,
}

impl Memo {
    fn suggestions(&mut self, workspace: &WorkspaceState, word: &str) -> Vec<Suggestion> {
        if self.people.version != People::version_of(workspace) {
            self.people = People::build(workspace);
            self.last = None;
        }
        let custom = (
            workspace.emoji.custom_names().count(),
            workspace.conversations.len(),
            workspace.groups.len(),
        );
        if let Some((last, count, found)) = &self.last
            && last == word
            && *count == custom
        {
            return found.clone();
        }
        let found = suggest(&self.people, workspace, word);
        self.last = Some((word.to_owned(), custom, found.clone()));
        found
    }
}

#[cfg(test)]
fn suggestions(workspace: &WorkspaceState, word: &str) -> Vec<Suggestion> {
    suggest(&People::build(workspace), workspace, word)
}

fn suggest(people: &People, workspace: &WorkspaceState, word: &str) -> Vec<Suggestion> {
    if let Some(typed) = word.strip_prefix('/') {
        return crate::slash::matching(typed)
            .map(Suggestion::Command)
            .collect();
    }
    if let Some(query) = word.strip_prefix('#') {
        let query = query.to_lowercase();
        let mut channels: Vec<(String, &crate::model::Conversation)> = workspace
            .conversations
            .iter()
            .filter(|c| {
                matches!(
                    c.kind,
                    crate::model::ConversationKind::Channel
                        | crate::model::ConversationKind::Private
                ) && !c.archived
            })
            .map(|c| (c.name.to_lowercase(), c))
            .filter(|(name, _)| name.contains(&query))
            .collect();
        channels.sort_by(|(a, _), (b, _)| {
            (!a.starts_with(&query), a).cmp(&(!b.starts_with(&query), b))
        });
        return channels
            .into_iter()
            .take(8)
            .map(|(_, c)| Suggestion::Channel {
                id: c.id.clone(),
                name: c.name.clone(),
                private: c.kind == crate::model::ConversationKind::Private,
            })
            .collect();
    }
    if let Some(query) = word.strip_prefix('@') {
        let query = query.to_lowercase();
        let mut out: Vec<Suggestion> = ["here", "channel", "everyone"]
            .into_iter()
            .filter(|name| !query.is_empty() && name.starts_with(&query))
            .map(Suggestion::Special)
            .collect();
        let mut users: Vec<&Person> = people
            .people
            .iter()
            .filter(|p| query.is_empty() || p.names.iter().any(|n| n.contains(&query)))
            .collect();
        users.sort_by(|a, b| {
            let key = |p: &Person| (p.is_bot, !p.label_lower.starts_with(&query));
            key(a)
                .cmp(&key(b))
                .then_with(|| a.label_lower.cmp(&b.label_lower))
        });
        users.truncate(8);
        // People whose name starts with what you typed come before groups,
        // as you most often mean a person; groups come before the rest.
        let first = users
            .iter()
            .take_while(|p| !p.is_bot && p.label_lower.starts_with(&query))
            .count();
        let person = |p: &Person| Suggestion::User {
            id: p.id.clone(),
            label: p.label.clone(),
            detail: p.detail.clone(),
            avatar: p.avatar.clone(),
        };
        out.extend(users[..first].iter().map(|p| person(p)));
        out.extend(groups(&workspace.groups, &query));
        out.extend(users[first..].iter().map(|p| person(p)));
        return out;
    }
    if let Some(query) = word.strip_prefix(':')
        && query.len() >= 2
        && !query.contains(':')
    {
        let query = query.to_lowercase();
        let mut names: Vec<String> = workspace
            .emoji
            .custom_names()
            .map(|(name, _)| name.to_owned())
            .filter(|name| name.contains(&query))
            .take(4)
            .collect();
        // Slack's own names, which are what other Slack clients render.
        let mut standard: Vec<&str> = crate::emoji::all_names()
            .filter(|code| code.contains(&query))
            .collect();
        standard.sort_by_key(|code| (!code.starts_with(&query), code.len()));
        names.extend(standard.into_iter().take(8).map(str::to_owned));
        return names
            .into_iter()
            .map(|name| Suggestion::Emoji { name })
            .collect();
    }
    Vec::new()
}

/// The user groups whose handle or name contains `query` (lower-cased),
/// those whose handle starts with it first. A workspace has few groups, so
/// they are matched afresh rather than kept lower-cased.
fn groups(groups: &[crate::model::UserGroup], query: &str) -> Vec<Suggestion> {
    let mut found: Vec<(String, &crate::model::UserGroup)> = groups
        .iter()
        .map(|g| (g.handle.to_lowercase(), g))
        .filter(|(handle, g)| handle.contains(query) || g.name.to_lowercase().contains(query))
        .collect();
    found.sort_by(|(a, _), (b, _)| (!a.starts_with(query), a).cmp(&(!b.starts_with(query), b)));
    found
        .into_iter()
        .take(4)
        .map(|(_, g)| Suggestion::Group {
            id: g.id.clone(),
            handle: g.handle.clone(),
            name: g.name.clone(),
            members: g.members,
        })
        .collect()
}

/// The text field of the composer for the draft `key`.
pub fn field_id(key: &str) -> egui::Id {
    egui::Id::new(("composer", key))
}

pub fn show(
    ui: &mut egui::Ui,
    composer: &Composer<'_>,
    draft: &mut Draft,
    actions: &mut Vec<Action>,
) {
    let palette = composer.palette;
    let id = field_id(&composer.key);
    let focused = ui.memory(|m| m.has_focus(id));
    let state = egui::TextEdit::load_state(ui.ctx(), id);
    let cursor = state
        .as_ref()
        .and_then(|s| s.cursor.char_range())
        .map_or(draft.text.chars().count(), |r| r.primary.index.into());
    let word = if focused {
        current_word(&draft.text, cursor)
    } else {
        None
    };
    if draft.dismissed != word {
        draft.dismissed = None;
    }
    let mut found = match &word {
        // Commands only suggest as the message's first word.
        Some((start, w)) if draft.dismissed.is_none() && (*start == 0 || !w.starts_with('/')) => {
            let memo_id = id.with("suggestions");
            let mut memo: Memo = ui.data_mut(|d| d.remove_temp(memo_id)).unwrap_or_default();
            let found = memo.suggestions(composer.workspace, w);
            ui.data_mut(|d| d.insert_temp(memo_id, memo));
            found
        }
        _ => Vec::new(),
    };
    if draft.selected >= found.len() {
        draft.selected = 0;
    }

    // Keys the text field must not see.
    let mut accept = None;
    let mut send = false;
    let mut format = None;
    if focused {
        ui.input_mut(|input| {
            format = format_shortcut(input);
            // egui only pastes text. When Ctrl+V lets go, the clipboard
            // is asked for an image, which is uploaded if there is no
            // text (that went into the field already).
            let pasted = input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key { key: Key::V, pressed: false, modifiers, .. }
                        if modifiers.command
                )
            });
            if pasted {
                actions.push(Action::PasteImage {
                    thread: composer.thread.clone(),
                });
            }
            // Files copied in a file manager paste as `file://` addresses:
            // upload them instead of typing the addresses into the message.
            input.events.retain(|event| {
                let egui::Event::Paste(text) = event else {
                    return true;
                };
                match crate::paste::pasted_files(text) {
                    Some(paths) => {
                        for path in paths {
                            actions.push(Action::Upload {
                                thread: composer.thread.clone(),
                                path,
                                comment: String::new(),
                            });
                        }
                        false
                    }
                    None => true,
                }
            });
            // Esc closes the suggestions, so "@chan" can be sent as typed.
            if !found.is_empty() && input.consume_key(Modifiers::NONE, Key::Escape) {
                draft.dismissed = word.clone();
                found.clear();
            }
            // Shift+Enter is a new line, whatever Enter does: egui's own
            // match ignores Shift, so it is left for the text field.
            let shift = shift_enter(input);
            if !found.is_empty() {
                if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    draft.selected = (draft.selected + 1) % found.len();
                }
                if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    draft.selected = (draft.selected + found.len() - 1) % found.len();
                }
                if input.consume_key(Modifiers::NONE, Key::Tab)
                    || (!shift && input.consume_key(Modifiers::NONE, Key::Enter))
                {
                    accept = found.get(draft.selected).cloned();
                }
            } else if composer.enter_sends {
                if !shift && input.consume_key(Modifiers::NONE, Key::Enter) {
                    send = true;
                }
            } else if input.consume_key(Modifiers::COMMAND, Key::Enter) {
                send = true;
            }
            if draft.text.is_empty()
                && composer.thread.is_none()
                && input.consume_key(Modifiers::NONE, Key::ArrowUp)
            {
                actions.push(Action::EditLast);
            }
        });
    }

    if let Some(format) = format {
        apply_format(ui.ctx(), id, draft, format);
    }

    draft.suggesting = !found.is_empty();
    if !found.is_empty() {
        // Floating above the composer, over the messages: drawn in line it
        // grew and shrank the bottom panel with every letter, so the
        // composer and the whole message list jumped while you typed.
        let bottom = ui.cursor().min - Vec2::new(0.0, 4.0);
        let width = ui.available_width();
        egui::Area::new(egui::Id::new(("composer-suggestions", &composer.key)))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(bottom)
            .show(ui.ctx(), |ui| {
                ui.set_width(width);
                suggestion_list(ui, palette, &found, draft.selected, &mut accept);
            });
    }

    if let Some(suggestion) = accept
        && let Some((start, word)) = &word
    {
        let insert = suggestion.insert();
        let chars: Vec<char> = draft.text.chars().collect();
        let end = (start + word.chars().count()).min(chars.len());
        let mut text: String = chars[..*start].iter().collect();
        text.push_str(&insert);
        text.extend(&chars[end..]);
        draft.text = text;
        match &suggestion {
            Suggestion::User { id, label, .. } => {
                draft
                    .mentions
                    .push((format!("@{label}"), format!("<@{id}>")));
            }
            // The label form draws as the handle even where the group is
            // unknown, and is what Slack itself sends.
            Suggestion::Group { id, handle, .. } => {
                draft
                    .mentions
                    .push((format!("@{handle}"), format!("<!subteam^{id}|@{handle}>")));
            }
            Suggestion::Channel { id, name, .. } => {
                draft
                    .mentions
                    .push((format!("#{name}"), format!("<#{id}|{name}>")));
            }
            _ => {}
        }
        let at = start + insert.chars().count();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(at))));
            state.store(ui.ctx(), id);
        }
    }

    uploads(ui, composer, actions);
    staged(ui, palette, draft, actions);

    let frame = egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(
            1.0,
            // The accent says where typing goes, as a field you just
            // opened a conversation into should.
            if focused {
                palette.accent
            } else {
                palette.outline
            },
        ))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin {
            left: 12,
            right: 8,
            top: 8,
            bottom: 6,
        });
    // Whether the formatting bar is open: one choice for every composer,
    // kept with the window's other remembered state.
    let bar_id = egui::Id::new("composer-formatting-bar");
    let mut bar_open = ui.data_mut(|d| *d.get_persisted_mut_or_default::<bool>(bar_id));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if bar_open && let Some(format) = formatting_bar(ui, palette) {
            apply_format(ui.ctx(), id, draft, format);
        }
        ui.scope(|ui| {
            // No edge fade: a frame with input briefly counts the field as
            // overflowing, and the fade painted over the text for that one
            // frame, so on a HiDPI screen typing made the text blink. It
            // only ever shows on drafts tall enough to scroll.
            ui.spacing_mut().scroll.fade.strength = 0.0;
            egui::ScrollArea::vertical()
                .id_salt(("composer-scroll", &composer.key))
                .max_height(220.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    let output = (egui::TextEdit::multiline(&mut draft.text)
                        .id(id)
                        .frame(egui::Frame::NONE)
                        .hint_text(
                            RichText::new(&composer.placeholder)
                                .font(theme::regular(14.5))
                                .color(palette.dim),
                        )
                        .desired_rows(1)
                        .desired_width(f32::INFINITY)
                        .font(theme::regular(14.5))
                        .lock_focus(true))
                    .show(ui);
                    spelling::show(ui, &output, id, draft, palette);
                    let response = output.response.response;
                    if composer.focus {
                        response.request_focus();
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if theme::icon_button(ui, palette, Icon::Paperclip, 17.0, &t("Upload a file")).clicked()
            {
                actions.push(Action::PickUpload {
                    thread: composer.thread.clone(),
                });
            }
            if theme::icon_button(ui, palette, Icon::Smile, 17.0, &t("Emoji")).clicked() {
                actions.push(Action::PickEmoji {
                    draft: composer.key.clone(),
                });
            }
            if theme::icon_button(ui, palette, Icon::AtSign, 17.0, &t("Mention someone")).clicked()
            {
                if !draft.text.is_empty() && !draft.text.ends_with(' ') {
                    draft.text.push(' ');
                }
                draft.text.push('@');
                ui.memory_mut(|m| m.request_focus(id));
            }
            let tip = if bar_open {
                t("Hide formatting")
            } else {
                t("Show formatting")
            };
            let toggle = theme::icon_button(ui, palette, Icon::Type, 17.0, &tip);
            if bar_open {
                // Marks the bar as open, as a pressed toggle would be.
                ui.painter().rect_stroke(
                    toggle.rect,
                    CornerRadius::same(theme::RADIUS_SMALL),
                    Stroke::new(1.0, palette.outline),
                    egui::StrokeKind::Inside,
                );
            }
            if toggle.clicked() {
                bar_open = !bar_open;
                ui.data_mut(|d| d.insert_persisted(bar_id, bar_open));
            }
            if let (Some(_), Some(channel)) = (&composer.thread, &composer.channel_name) {
                ui.add_space(8.0);
                ui.checkbox(
                    &mut draft.broadcast,
                    RichText::new(tf("Also send to #{channel}", &[("channel", channel)]))
                        .font(theme::regular(12.5))
                        .color(palette.secondary),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = !draft.text.trim().is_empty() || !draft.attachments.is_empty();
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(36.0, 28.0), Sense::click());
                let fill = if ready {
                    palette.accent
                } else {
                    palette.surface_hover
                };
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(theme::RADIUS_SMALL), fill);
                let tint = if ready {
                    palette.on_accent
                } else {
                    palette.dim
                };
                Icon::Send.image(tint, 16.0).paint_at(
                    ui,
                    egui::Rect::from_center_size(rect.center(), Vec2::splat(16.0)),
                );
                let tip = if composer.enter_sends {
                    t("Send (Enter)").into_owned()
                } else {
                    tf(
                        "Send ({shortcut})",
                        &[("shortcut", &super::keys::command("Enter"))],
                    )
                };
                theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
                theme::describe(&response, egui::WidgetType::Button, &tip);
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(tip)
                    .clicked()
                    && ready
                {
                    send = true;
                }
                // Sending later, from a menu beside the button.
                let later = theme::icon_button(ui, palette, Icon::Clock, 15.0, &t("Send later"));
                egui::Popup::menu(&later).show(|ui| {
                    super::views::send_later_menu(
                        ui,
                        palette,
                        ready,
                        composer.thread.as_ref(),
                        actions,
                    );
                });
            });
        });
    });
    if send && (!draft.text.trim().is_empty() || !draft.attachments.is_empty()) {
        actions.push(Action::Send {
            text: draft.text.clone(),
            thread: composer.thread.clone(),
            broadcast: draft.broadcast,
        });
    }
}

/// The files waiting to go with the message, each with a button to take
/// it out again.
fn staged(ui: &mut egui::Ui, palette: &Palette, draft: &mut Draft, actions: &mut Vec<Action>) {
    let mut removed = None;
    ui.horizontal_wrapped(|ui| {
        for (index, path) in draft.attachments.iter().enumerate() {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(theme::RADIUS_SMALL))
                .inner_margin(Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                        Icon::Paperclip
                            .image(palette.secondary, 14.0)
                            .paint_at(ui, icon);
                        ui.add(
                            egui::Label::new(
                                RichText::new(&name)
                                    .font(theme::regular(12.5))
                                    .color(palette.text),
                            )
                            .truncate(),
                        );
                        let remove = theme::icon_button(
                            ui,
                            palette,
                            Icon::X,
                            12.0,
                            &tf("Remove {name}", &[("name", &name)]),
                        );
                        if remove.clicked() {
                            removed = Some(index);
                        }
                    });
                });
        }
    });
    if let Some(index) = removed {
        actions.push(Action::Unstage(draft.attachments.remove(index)));
    }
}

/// The uploads sent from this composer, each with its progress and a
/// button to cancel it.
fn uploads(ui: &mut egui::Ui, composer: &Composer<'_>, actions: &mut Vec<Action>) {
    let palette = composer.palette;
    for upload in composer.uploads.iter().filter(|u| u.key == composer.key) {
        ui.horizontal(|ui| {
            let (icon, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            Icon::Paperclip
                .image(palette.secondary, 14.0)
                .paint_at(ui, icon);
            ui.add(
                egui::Label::new(
                    RichText::new(&upload.name)
                        .font(theme::regular(12.5))
                        .color(palette.text),
                )
                .truncate(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // In the last step Slack is already sharing the file, so
                // the button goes rather than offer what it can't do; the
                // label says why it went.
                if upload.can_cancel()
                    && theme::icon_button(ui, palette, Icon::X, 14.0, &t("Cancel upload")).clicked()
                {
                    actions.push(Action::CancelUpload(upload.id));
                }
                let fraction = upload.fraction();
                let said = if upload.can_cancel() {
                    tf(
                        "{percent}% uploaded",
                        &[("percent", &format!("{:.0}", fraction * 100.0))],
                    )
                } else {
                    t("Finishing upload…").into_owned()
                };
                let label = ui.label(
                    RichText::new(&said)
                        .font(theme::regular(12.0))
                        .color(palette.dim),
                );
                if !upload.can_cancel() {
                    label.on_hover_text(t("Too late to cancel: Slack is posting the file"));
                }
                let bar = egui::ProgressBar::new(fraction)
                    .desired_width(ui.available_width().clamp(60.0, 220.0))
                    .desired_height(6.0)
                    .fill(palette.accent);
                ui.add(bar);
            });
        });
    }
}

/// Uploads files dropped on this panel (the one under the pointer), and
/// shows where they will go while they are dragged over it. With no
/// pointer position, as some platforms give none during a drag, the
/// `fallback` panel (the conversation) takes them.
pub fn drop_target(
    ui: &mut egui::Ui,
    palette: &Palette,
    thread: Option<Ts>,
    fallback: bool,
    actions: &mut Vec<Action>,
) {
    let rect = ui.max_rect();
    let (hovering, dropped, pointer) = ui.input(|i| {
        (
            !i.raw.hovered_files.is_empty(),
            !i.raw.dropped_files.is_empty(),
            i.pointer.hover_pos(),
        )
    });
    let here = pointer.map_or(fallback, |p| rect.contains(p));
    if !here {
        return;
    }
    if hovering {
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new(("drop-target", fallback)),
        ));
        let area = rect.shrink(8.0);
        painter.rect(
            area,
            CornerRadius::same(theme::RADIUS),
            palette.window.gamma_multiply(0.85),
            Stroke::new(2.0, palette.accent),
            egui::StrokeKind::Inside,
        );
        painter.text(
            area.center(),
            egui::Align2::CENTER_CENTER,
            t("Drop files to upload"),
            theme::semibold(16.0),
            palette.text,
        );
    }
    if dropped {
        let files = ui
            .ctx()
            .input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        for path in files.iter().map(|file| file.path().to_path_buf()) {
            actions.push(Action::Upload {
                thread: thread.clone(),
                path,
                comment: String::new(),
            });
        }
    }
}

/// The formatting shortcut pressed this frame, if any, taken so the text
/// field does not also act on it.
fn format_shortcut(input: &mut egui::InputState) -> Option<Format> {
    if input.modifiers.command && input.modifiers.shift {
        // egui-winit turns Ctrl+Shift+X and Ctrl+Shift+C into Cut and Copy
        // rather than key presses. Left in, the cut would also delete the
        // selection that is about to be struck through.
        let mut found = None;
        input.events.retain(|event| match event {
            egui::Event::Cut => {
                found = Some(Format::Strike);
                false
            }
            egui::Event::Copy => {
                found = Some(Format::Code);
                false
            }
            _ => true,
        });
        if found.is_some() {
            return found;
        }
    }
    // Other backends may send them as keys after all.
    if input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::X) {
        Some(Format::Strike)
    } else if input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::C) {
        Some(Format::Code)
    } else if input.consume_key(Modifiers::COMMAND, Key::B) {
        Some(Format::Bold)
    } else if input.consume_key(Modifiers::COMMAND, Key::I) {
        Some(Format::Italic)
    } else {
        None
    }
}

/// Formats the selection of the field `id` (or inserts markers at its
/// cursor), keeps the formatted text selected and the field focused.
fn apply_format(ctx: &egui::Context, id: egui::Id, draft: &mut Draft, format: Format) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let end = draft.text.chars().count();
    let selection = state.cursor.char_range().map_or(end..end, |range| {
        let (a, b): (usize, usize) = (range.primary.index.into(), range.secondary.index.into());
        a.min(b)..a.max(b)
    });
    let (text, selection) = format::apply(&draft.text, selection, format);
    draft.text = text;
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(selection.start),
        CCursor::new(selection.end),
    )));
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

/// The formatting bar over the text field; returns the style clicked.
fn formatting_bar(ui: &mut egui::Ui, palette: &Palette) -> Option<Format> {
    let mut clicked = None;
    let shortcut = super::keys::command;
    let buttons = [
        (
            Format::Bold,
            Icon::Bold,
            tf("Bold ({shortcut})", &[("shortcut", &shortcut("B"))]),
        ),
        (
            Format::Italic,
            Icon::Italic,
            tf("Italic ({shortcut})", &[("shortcut", &shortcut("I"))]),
        ),
        (
            Format::Strike,
            Icon::Strike,
            tf(
                "Strikethrough ({shortcut})",
                &[("shortcut", &shortcut("Shift+X"))],
            ),
        ),
        (
            Format::Code,
            Icon::Code,
            tf("Code ({shortcut})", &[("shortcut", &shortcut("Shift+C"))]),
        ),
        (
            Format::CodeBlock,
            Icon::CodeBlock,
            t("Code block").into_owned(),
        ),
        (Format::Quote, Icon::TextQuote, t("Quote").into_owned()),
    ];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (format, icon, tip) in buttons {
            if theme::icon_button(ui, palette, icon, 15.0, &tip).clicked() {
                clicked = Some(format);
            }
        }
    });
    clicked
}

fn suggestion_list(
    ui: &mut egui::Ui,
    palette: &Palette,
    found: &[Suggestion],
    selected: usize,
    accept: &mut Option<Suggestion>,
) {
    egui::Frame::new()
        .fill(palette.overlay)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (index, suggestion) in found.iter().enumerate() {
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
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
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect.shrink2(Vec2::new(8.0, 3.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                match suggestion {
                    Suggestion::User {
                        id,
                        label,
                        detail,
                        avatar,
                    } => {
                        super::avatar(&mut child, avatar.as_deref(), label, id, 20.0);
                        child.label(
                            RichText::new(label)
                                .font(theme::semibold(13.5))
                                .color(palette.text),
                        );
                        child.label(
                            RichText::new(detail)
                                .font(theme::regular(12.5))
                                .color(palette.dim),
                        );
                    }
                    Suggestion::Emoji { name } => {
                        match crate::emoji::unicode(name, None) {
                            Some(unicode) => {
                                child.label(RichText::new(unicode).font(theme::regular(16.0)));
                            }
                            None => {
                                child.label(RichText::new("·").font(theme::regular(16.0)));
                            }
                        }
                        child.label(
                            RichText::new(format!(":{name}:"))
                                .font(theme::regular(13.5))
                                .color(palette.text),
                        );
                    }
                    Suggestion::Special(name) => {
                        child.label(
                            RichText::new(format!("@{name}"))
                                .font(theme::semibold(13.5))
                                .color(palette.text),
                        );
                        child.label(
                            RichText::new(broadcast_description(name))
                                .font(theme::regular(12.5))
                                .color(palette.dim),
                        );
                    }
                    Suggestion::Group {
                        handle,
                        name,
                        members,
                        ..
                    } => {
                        let (rect, _) =
                            child.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                        Icon::Users
                            .image(palette.secondary, 15.0)
                            .paint_at(&child, rect.shrink(2.5));
                        child.label(
                            RichText::new(format!("@{handle}"))
                                .font(theme::semibold(13.5))
                                .color(palette.text),
                        );
                        child.label(
                            RichText::new(group_detail(name, *members))
                                .font(theme::regular(12.5))
                                .color(palette.dim),
                        );
                    }
                    Suggestion::Channel { name, private, .. } => {
                        let icon = if *private { Icon::Lock } else { Icon::Hash };
                        let (rect, _) =
                            child.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        icon.image(palette.secondary, 14.0).paint_at(&child, rect);
                        child.label(
                            RichText::new(name)
                                .font(theme::semibold(13.5))
                                .color(palette.text),
                        );
                    }
                    Suggestion::Command(known) => {
                        child.label(
                            RichText::new(format!("/{}", known.name))
                                .font(theme::semibold(13.5))
                                .color(palette.text),
                        );
                        if !known.usage.is_empty() {
                            child.label(
                                RichText::new(known.usage)
                                    .font(theme::mono(12.0))
                                    .color(palette.secondary),
                            );
                        }
                        child.label(
                            RichText::new(command_description(known.name))
                                .font(theme::regular(12.5))
                                .color(palette.dim),
                        );
                    }
                }
                theme::describe_selected(
                    &response,
                    egui::WidgetType::SelectableLabel,
                    index == selected,
                    &suggestion.insert(),
                );
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    *accept = Some(suggestion.clone());
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Conversation;

    #[test]
    fn the_word_at_the_cursor() {
        assert_eq!(current_word("hi @an", 6), Some((3, "@an".into())));
        assert_eq!(current_word("hi @an", 2), Some((0, "hi".into())));
        assert_eq!(current_word("hi ", 3), None);
        assert_eq!(current_word("ünï :ta", 7), Some((4, ":ta".into())));
        assert_eq!(current_word("çà @Zoë", 7), Some((3, "@Zoë".into())));
    }

    fn workspace() -> WorkspaceState {
        let mut w = WorkspaceState::new(crate::model::Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
        });
        let user =
            |id: &str, name: &str, real: &str, display: &str, bot: bool| crate::model::User {
                id: id.into(),
                name: name.into(),
                real_name: real.into(),
                display_name: display.into(),
                is_bot: bot,
                ..Default::default()
            };
        for u in [
            user("U1", "joanna", "Joanna Ek", "", false),
            user("U2", "ann", "Ann Lee", "Ann", false),
            user("U3", "anbot", "", "", true),
            user("U4", "old", "Anders", "", false),
        ] {
            w.users.insert(u.id.clone(), u);
        }
        w.users.get_mut("U4").expect("U4").deleted = true;
        w.emoji = crate::emoji::EmojiSet::new(
            [("tacocat".to_owned(), "https://x.y/t.png".to_owned())].into(),
        );
        w
    }

    fn labels(found: &[Suggestion]) -> Vec<String> {
        found
            .iter()
            .map(|s| match s {
                Suggestion::User { label, .. } => label.clone(),
                Suggestion::Emoji { name } => format!(":{name}:"),
                Suggestion::Special(name) => format!("@{name}"),
                Suggestion::Group { handle, .. } => format!("@{handle}"),
                Suggestion::Channel { name, .. } => format!("#{name}"),
                Suggestion::Command(known) => format!("/{}", known.name),
            })
            .collect()
    }

    #[test]
    fn people_rank_by_prefix_and_bots_last() {
        let w = workspace();
        // "Ann" starts with the query, "Joanna Ek" only contains it, the bot
        // comes last and the deleted account not at all.
        assert_eq!(
            labels(&suggestions(&w, "@an")),
            ["Ann", "Joanna Ek", "anbot"]
        );
        assert_eq!(labels(&suggestions(&w, "@ch")), ["@channel"]);
        // A bare @ lists people, not broadcasts.
        assert_eq!(suggestions(&w, "@").len(), 3);
        assert!(suggestions(&w, "plain").is_empty());
    }

    #[test]
    fn groups_suggest_by_handle_and_name_after_people() {
        let mut w = workspace();
        let group = |id: &str, handle: &str, name: &str| crate::model::UserGroup {
            id: id.into(),
            handle: handle.into(),
            name: name.into(),
            members: Some(3),
        };
        w.groups = vec![
            group("S1", "design", "Design team"),
            group("S2", "ops", "Operations"),
            group("S3", "android", "Mobile"),
        ];
        // People starting with the query, then groups (handle prefix
        // first), then the people and bots that only contain it.
        assert_eq!(
            labels(&suggestions(&w, "@an")),
            ["Ann", "@android", "Joanna Ek", "anbot"]
        );
        // The name counts too, but what goes in is the handle.
        let found = suggestions(&w, "@operat");
        assert_eq!(labels(&found), ["@ops"]);
        assert_eq!(
            found.first().map(Suggestion::insert).as_deref(),
            Some("@ops ")
        );
        assert_eq!(
            group_detail("Operations", Some(3)),
            "Operations · 3 members"
        );
        assert_eq!(group_detail("Operations", None), "Operations");
        // A new list of groups is seen by remembered suggestions.
        let mut memo = Memo::default();
        assert_eq!(labels(&memo.suggestions(&w, "@des")), ["@design"]);
        w.groups.push(group("S4", "desk", "Help desk"));
        assert_eq!(labels(&memo.suggestions(&w, "@des")), ["@design", "@desk"]);
    }

    #[test]
    fn remembered_suggestions_follow_new_people() {
        let mut w = workspace();
        let mut memo = Memo::default();
        assert_eq!(
            labels(&memo.suggestions(&w, "@an")),
            ["Ann", "Joanna Ek", "anbot"]
        );
        assert_eq!(memo.suggestions(&w, "@an"), suggestions(&w, "@an"));
        let andy = crate::model::User {
            id: "U5".into(),
            name: "andy".into(),
            ..Default::default()
        };
        w.users.insert(andy.id.clone(), andy);
        assert_eq!(
            labels(&memo.suggestions(&w, "@an")),
            ["andy", "Ann", "Joanna Ek", "anbot"]
        );
        assert_eq!(memo.suggestions(&w, ":ta"), suggestions(&w, ":ta"));
    }

    fn channel(id: &str, name: &str, kind: crate::model::ConversationKind) -> Conversation {
        Conversation {
            id: id.into(),
            name: name.into(),
            kind,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: None,
            latest: None,
            unread: 0,
            mentions: 0,
            external: false,
        }
    }

    #[test]
    fn channels_suggest_by_name_and_become_channel_links() {
        use crate::model::ConversationKind::{Channel, Direct, Private};
        let mut w = workspace();
        w.conversations = vec![
            channel("C1", "random", Channel),
            channel("C2", "design-review", Private),
            channel("C3", "design", Channel),
            channel("D1", "U2", Direct),
            Conversation {
                archived: true,
                ..channel("C4", "design-old", Channel)
            },
        ];
        // Prefix matches first, archived channels and DMs never.
        assert_eq!(
            labels(&suggestions(&w, "#des")),
            ["#design", "#design-review"]
        );
        assert_eq!(labels(&suggestions(&w, "#view")), ["#design-review"]);
        assert_eq!(suggestions(&w, "#").len(), 3);
        // What the suggestion inserts goes out as Slack's channel link.
        let mentions = vec![("#design".to_owned(), "<#C3|design>".to_owned())];
        assert_eq!(
            crate::app::to_wire("see #design, not #designer", &mentions),
            "see <#C3|design>, not #designer"
        );
    }

    #[test]
    fn slash_commands_suggest_with_their_usage() {
        let w = workspace();
        assert_eq!(labels(&suggestions(&w, "/st")), ["/status"]);
        assert_eq!(suggestions(&w, "/").len(), crate::slash::KNOWN.len());
        assert!(suggestions(&w, "/zz").is_empty());
    }

    #[test]
    fn emoji_need_two_letters_and_custom_ones_come_first() {
        let w = workspace();
        assert!(suggestions(&w, ":t").is_empty());
        assert!(suggestions(&w, ":ta:").is_empty());
        let found = labels(&suggestions(&w, ":ta"));
        assert_eq!(found.first().map(String::as_str), Some(":tacocat:"));
        // Standard emoji starting with the query come before the rest.
        let standard = &found[1..];
        let starts = standard.iter().take_while(|n| n.starts_with(":ta")).count();
        assert!(starts > 0);
        assert!(standard[starts..].iter().all(|n| !n.starts_with(":ta")));
    }

    #[test]
    fn only_an_enter_with_shift_is_a_new_line() {
        let enter = |modifiers: Modifiers| {
            let mut input = egui::InputState::default();
            input.events.push(egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            });
            shift_enter(&input)
        };
        assert!(enter(Modifiers::SHIFT));
        assert!(!enter(Modifiers::NONE));
        assert!(!enter(Modifiers::COMMAND));
        assert!(!shift_enter(&egui::InputState::default()));
    }
}
