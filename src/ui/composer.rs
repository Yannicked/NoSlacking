//! Writing a message: a growing text field with @mention and :emoji:
//! suggestions, attachments and the send button.

use egui::text::{CCursor, CCursorRange};
use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Stroke, Vec2};

use crate::app::{Draft, WorkspaceState};
use crate::i18n::t;
use crate::model::{Action, Ts};
use crate::theme::{self, Icon, Palette};

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
}

impl Suggestion {
    fn insert(&self) -> String {
        match self {
            Self::User { label, .. } => format!("@{label} "),
            Self::Emoji { name } => format!(":{name}: "),
            Self::Special(name) => format!("@{name} "),
        }
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

fn suggestions(workspace: &WorkspaceState, word: &str) -> Vec<Suggestion> {
    if let Some(query) = word.strip_prefix('@') {
        let query = query.to_lowercase();
        let mut out: Vec<Suggestion> = ["here", "channel", "everyone"]
            .into_iter()
            .filter(|name| !query.is_empty() && name.starts_with(&query))
            .map(Suggestion::Special)
            .collect();
        let mut users: Vec<_> = workspace
            .users
            .values()
            .filter(|u| !u.deleted)
            .filter(|u| {
                query.is_empty()
                    || u.name.to_lowercase().contains(&query)
                    || u.real_name.to_lowercase().contains(&query)
                    || u.display_name.to_lowercase().contains(&query)
            })
            .collect();
        users.sort_by_key(|u| {
            (
                u.is_bot,
                !u.label().to_lowercase().starts_with(&query),
                u.label().to_lowercase(),
            )
        });
        out.extend(users.into_iter().take(8).map(|u| Suggestion::User {
            id: u.id.clone(),
            label: u.label().to_owned(),
            detail: if u.real_name.is_empty() || u.real_name == u.label() {
                u.name.clone()
            } else {
                u.real_name.clone()
            },
            avatar: u.avatar.clone(),
        }));
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
        let mut standard: Vec<&str> = emojis::iter()
            .flat_map(|e| e.shortcodes())
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

pub fn show(
    ui: &mut egui::Ui,
    composer: &Composer<'_>,
    draft: &mut Draft,
    actions: &mut Vec<Action>,
) {
    let palette = composer.palette;
    let id = egui::Id::new(("composer", &composer.key));
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
    let found = word
        .as_ref()
        .map(|(_, w)| suggestions(composer.workspace, w))
        .unwrap_or_default();
    if draft.selected >= found.len() {
        draft.selected = 0;
    }

    // Keys the text field must not see.
    let mut accept = None;
    let mut send = false;
    if focused {
        ui.input_mut(|input| {
            if !found.is_empty() {
                if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    draft.selected = (draft.selected + 1) % found.len();
                }
                if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    draft.selected = (draft.selected + found.len() - 1) % found.len();
                }
                if input.consume_key(Modifiers::NONE, Key::Tab)
                    || input.consume_key(Modifiers::NONE, Key::Enter)
                {
                    accept = found.get(draft.selected).cloned();
                }
            } else if composer.enter_sends {
                if input.consume_key(Modifiers::NONE, Key::Enter) {
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

    if !found.is_empty() {
        suggestion_list(ui, palette, &found, draft.selected, &mut accept);
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
        if let Suggestion::User { id, label, .. } = &suggestion {
            draft
                .mentions
                .push((format!("@{label}"), format!("<@{id}>")));
        }
        let at = start + insert.chars().count();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(at))));
            state.store(ui.ctx(), id);
        }
    }

    let frame = egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(
            1.0,
            if focused {
                palette.secondary
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
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical()
            .id_salt(("composer-scroll", &composer.key))
            .max_height(220.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let response = ui.add(
                    egui::TextEdit::multiline(&mut draft.text)
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
                        .lock_focus(true),
                );
                if composer.focus {
                    response.request_focus();
                }
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
            if let (Some(_), Some(channel)) = (&composer.thread, &composer.channel_name) {
                ui.add_space(8.0);
                ui.checkbox(
                    &mut draft.broadcast,
                    RichText::new(format!("{} #{channel}", t("Also send to")))
                        .font(theme::regular(12.5))
                        .color(palette.secondary),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = !draft.text.trim().is_empty();
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
                    t("Send (Enter)")
                } else {
                    t("Send (Ctrl+Enter)")
                };
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(tip)
                    .clicked()
                    && ready
                {
                    send = true;
                }
            });
        });
    });
    if send && !draft.text.trim().is_empty() {
        actions.push(Action::Send {
            text: draft.text.clone(),
            thread: composer.thread.clone(),
            broadcast: draft.broadcast,
        });
    }
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
                            RichText::new(t("Notify everyone here"))
                                .font(theme::regular(12.5))
                                .color(palette.dim),
                        );
                    }
                }
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

    #[test]
    fn the_word_at_the_cursor() {
        assert_eq!(current_word("hi @an", 6), Some((3, "@an".into())));
        assert_eq!(current_word("hi @an", 2), Some((0, "hi".into())));
        assert_eq!(current_word("hi ", 3), None);
        assert_eq!(current_word("ünï :ta", 7), Some((4, ":ta".into())));
    }
}
