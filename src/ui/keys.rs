//! Keyboard shortcuts that work anywhere in the window.
//!
//! - Ctrl+K (⌘K): jump to a conversation
//! - Alt+↑ / Alt+↓: previous / next conversation in the sidebar
//! - Alt+Shift+↑ / ↓: previous / next unread conversation
//! - Ctrl+, : settings
//! - Ctrl+= / Ctrl+- / Ctrl+0: zoom
//! - Esc: close the thread or the open overlay

use egui::{Key, Modifiers};

use crate::app::{App, Page};
use crate::model::{Action, Conversation};

pub fn global(app: &mut App, ctx: &egui::Context) {
    let overlay = app.switcher.is_some()
        || app.picker.is_some()
        || app.profile.is_some()
        || app.preview.is_some()
        || app.confirm_delete.is_some()
        || app.section_dialog.is_some();
    let (switch, settings, unread_up, unread_down, up, down, escape, zoom_in, zoom_out, zoom_reset) =
        ctx.input_mut(|input| {
            (
                input.consume_key(Modifiers::COMMAND, Key::K),
                input.consume_key(Modifiers::COMMAND, Key::Comma),
                // With Shift first, so plain Alt does not swallow them.
                input.consume_key(Modifiers::ALT | Modifiers::SHIFT, Key::ArrowUp),
                input.consume_key(Modifiers::ALT | Modifiers::SHIFT, Key::ArrowDown),
                input.consume_key(Modifiers::ALT, Key::ArrowUp),
                input.consume_key(Modifiers::ALT, Key::ArrowDown),
                !overlay && app.editing.is_none() && input.key_pressed(Key::Escape),
                input.consume_key(Modifiers::COMMAND, Key::Equals)
                    || input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Equals)
                    || input.consume_key(Modifiers::COMMAND, Key::Plus),
                input.consume_key(Modifiers::COMMAND, Key::Minus),
                input.consume_key(Modifiers::COMMAND, Key::Num0),
            )
        });
    if switch && !app.workspaces.is_empty() {
        if app.switcher.is_some() {
            app.switcher = None;
        } else {
            app.actions.push(Action::OpenSwitcher);
        }
    }
    if settings {
        app.actions.push(if app.page == Page::Settings {
            Action::HideSettings
        } else {
            Action::ShowSettings
        });
    }
    if escape {
        if app.page == Page::Settings {
            app.actions.push(Action::HideSettings);
        } else if app.thread.is_some() {
            app.actions.push(Action::CloseThread);
        }
    }
    let zoom = app.settings.zoom;
    let zoom = if zoom_in {
        (zoom + 0.1).min(1.75)
    } else if zoom_out {
        (zoom - 0.1).max(0.75)
    } else if zoom_reset {
        1.0
    } else {
        zoom
    };
    if (zoom - app.settings.zoom).abs() > f32::EPSILON {
        app.settings.zoom = (zoom * 20.0).round() / 20.0;
        app.settings_changed();
    }
    if up || down || unread_up || unread_down {
        let only_unread = unread_up || unread_down;
        let forward = down || unread_down;
        if let Some(next) = step(app, forward, only_unread) {
            app.actions.push(Action::OpenConversation(next));
        }
    }
}

/// The conversation before or after the open one, in sidebar order.
fn step(app: &App, forward: bool, only_unread: bool) -> Option<String> {
    let workspace = app.active_workspace()?;
    let shown = crate::sidebar::layout(
        workspace.sections.as_deref(),
        &workspace.conversations,
        &workspace.users,
        |c| workspace.title(c),
        app.settings.sidebar_sort,
    );
    let order: Vec<&Conversation> = shown
        .iter()
        .flat_map(|section| section.conversations.iter().copied())
        .collect();
    if order.is_empty() {
        return None;
    }
    let current = workspace
        .active
        .as_deref()
        .and_then(|id| order.iter().position(|c| c.id == id));
    let len = order.len();
    let start = current.unwrap_or(if forward { len - 1 } else { 0 });
    for offset in 1..=len {
        let index = if forward {
            (start + offset) % len
        } else {
            (start + len - offset % len) % len
        };
        let candidate = order[index];
        if !only_unread || candidate.has_unread() {
            return Some(candidate.id.clone());
        }
    }
    None
}
