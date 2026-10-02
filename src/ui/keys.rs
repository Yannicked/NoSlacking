//! Keyboard shortcuts that work anywhere in the window.
//!
//! - Ctrl+K (⌘K): jump to a conversation
//! - Alt+↑ / Alt+↓: previous / next conversation in the sidebar
//! - Alt+Shift+↑ / ↓: previous / next unread conversation
//!
//!   Both leave a text field with text in it alone.
//! - Ctrl+, : settings
//! - Ctrl+= / Ctrl+- / Ctrl+0: zoom
//! - Esc: close the thread or the open overlay

use egui::{Key, Modifiers};

use crate::app::{App, Page, WorkspaceState};
use crate::model::{Action, Conversation};
use crate::sidebar::Sort;

pub fn global(app: &mut App, ctx: &egui::Context) {
    let overlay = app.overlay_open();
    // Esc in the edit field cancels the edit, and nothing else.
    let editing = app
        .editing
        .as_ref()
        .is_some_and(|e| ctx.memory(|m| m.has_focus(super::message::edit_id(e))));
    // Esc closes open suggestions first.
    let suggesting = app
        .visible_drafts()
        .iter()
        .any(|key| app.drafts.get(key).is_some_and(|d| d.suggesting));
    // Alt+↑/↓ also move through text (by paragraph on macOS): a field with
    // text in it keeps them. An empty composer has nothing to move through.
    let arrows = !ctx.text_edit_focused() || in_empty_composer(app, ctx);
    let (switch, settings, unread_up, unread_down, up, down, escape, zoom_in, zoom_out, zoom_reset) =
        ctx.input_mut(|input| {
            (
                input.consume_key(Modifiers::COMMAND, Key::K),
                input.consume_key(Modifiers::COMMAND, Key::Comma),
                // With Shift first, so plain Alt does not swallow them.
                arrows && input.consume_key(Modifiers::ALT | Modifiers::SHIFT, Key::ArrowUp),
                arrows && input.consume_key(Modifiers::ALT | Modifiers::SHIFT, Key::ArrowDown),
                arrows && input.consume_key(Modifiers::ALT, Key::ArrowUp),
                arrows && input.consume_key(Modifiers::ALT, Key::ArrowDown),
                !overlay && !editing && !suggesting && input.key_pressed(Key::Escape),
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
        let next = app
            .active_workspace()
            .and_then(|w| step(w, app.settings.sidebar_sort, forward, only_unread));
        if let Some(next) = next {
            app.actions.push(Action::OpenConversation(next));
        }
    }
}

/// How a hint spells the command key with `key`: "⌘K" on macOS, where
/// `Modifiers::COMMAND` is Cmd, and "Ctrl+K" elsewhere.
pub fn command(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

/// Whether the focused field is a composer on screen with nothing typed.
fn in_empty_composer(app: &App, ctx: &egui::Context) -> bool {
    let Some(focused) = ctx.memory(|m| m.focused()) else {
        return false;
    };
    app.visible_drafts().iter().any(|key| {
        super::composer::field_id(key) == focused
            && app.drafts.get(key).is_none_or(|d| d.text.is_empty())
    })
}

/// The conversation before or after the open one, in sidebar order,
/// wrapping around at either end.
fn step(
    workspace: &WorkspaceState,
    sort: Sort,
    forward: bool,
    only_unread: bool,
) -> Option<String> {
    let shown = crate::sidebar::layout(
        workspace.sections.as_deref(),
        &workspace.conversations,
        &workspace.users,
        |c| workspace.title(c),
        sort,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConversationKind, Ts, Workspace};

    fn channel(id: &str, name: &str, unread: bool) -> Conversation {
        Conversation {
            id: id.into(),
            name: name.into(),
            kind: ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: Some(Ts::new("1.0")),
            latest: Some(Ts::new(if unread { "2.0" } else { "1.0" })),
            unread: 0,
            mentions: 0,
        }
    }

    fn workspace(active: Option<&str>) -> WorkspaceState {
        let mut w = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U1".into(),
        });
        w.conversations = vec![
            channel("C3", "gamma", false),
            channel("C1", "alpha", false),
            channel("C2", "beta", true),
        ];
        w.active = active.map(str::to_owned);
        w
    }

    fn next(w: &WorkspaceState, forward: bool, only_unread: bool) -> Option<String> {
        step(w, Sort::Name, forward, only_unread)
    }

    #[test]
    fn stepping_follows_the_sidebar_and_wraps() {
        let w = workspace(Some("C1"));
        assert_eq!(next(&w, true, false).as_deref(), Some("C2"));
        assert_eq!(next(&w, false, false).as_deref(), Some("C3"));
        let w = workspace(Some("C3"));
        assert_eq!(next(&w, true, false).as_deref(), Some("C1"));
        // Nothing open: forward starts at the top, back at the bottom.
        let w = workspace(None);
        assert_eq!(next(&w, true, false).as_deref(), Some("C1"));
        assert_eq!(next(&w, false, false).as_deref(), Some("C3"));
    }

    #[test]
    fn unread_stepping_skips_read_conversations() {
        let w = workspace(Some("C3"));
        assert_eq!(next(&w, true, true).as_deref(), Some("C2"));
        assert_eq!(next(&w, false, true).as_deref(), Some("C2"));
        // The only unread one is open: it is found again after a full turn.
        let w = workspace(Some("C2"));
        assert_eq!(next(&w, true, true).as_deref(), Some("C2"));
        let mut w = workspace(Some("C1"));
        w.conversations.retain(|c| c.id != "C2");
        assert_eq!(next(&w, true, true), None);
    }
}
