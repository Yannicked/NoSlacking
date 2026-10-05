//! Keyboard shortcuts that work anywhere in the window.
//!
//! - Ctrl+K (⌘K): jump to a conversation
//! - Ctrl+F (⌘F): search messages and files
//! - Ctrl+J (⌘J): jump to the "New" line of the open conversation
//! - Ctrl+Shift+J (⌘⇧J), or End outside a text field: jump to its newest
//!   messages
//! - Alt+↑ / Alt+↓: previous / next conversation in the sidebar
//! - Alt+Shift+↑ / ↓: previous / next unread conversation
//!
//!   Both leave a text field with text in it alone.
//! - Ctrl+, : settings
//! - Ctrl+/ (⌘/): the keyboard shortcut sheet
//! - Ctrl+= / Ctrl+- / Ctrl+0: zoom
//! - Esc: close the thread or the open overlay

use egui::{Key, Modifiers};

use crate::app::{App, Page, WorkspaceState};
use crate::model::{Action, Conversation};
use crate::sidebar::Arrange;

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
    let sheet = ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::Slash));
    if sheet {
        if app.shortcuts {
            app.shortcuts = false;
        } else if !overlay {
            app.actions.push(Action::ShowShortcuts);
        }
    }
    let search = ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::F));
    if search && !app.workspaces.is_empty() {
        if app.search.open {
            app.search.open = false;
        } else if !overlay {
            app.actions.push(Action::OpenSearch);
        }
    }
    let typing = ctx.text_edit_focused();
    let (newest, unread) = ctx.input_mut(|input| {
        (
            // With Shift first, so plain Ctrl+J does not swallow it.
            input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::J)
                || (!typing && input.consume_key(Modifiers::NONE, Key::End)),
            input.consume_key(Modifiers::COMMAND, Key::J),
        )
    });
    if app.page == Page::Main && !overlay {
        if unread {
            app.actions.push(Action::JumpToUnread);
        }
        if newest {
            app.actions.push(Action::JumpToNewest);
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
        let next = app.active_workspace().and_then(|w| {
            // The order the sidebar shows, the open conversation held in
            // its place.
            let held = super::sidebar::held(ctx, &w.info.team_id);
            let arrange = Arrange {
                sort: app.settings.sidebar_sort,
                unread_first: app.settings.unread_first,
                hold: held.as_ref(),
            };
            let order = visible(
                w,
                &arrange,
                app.settings.closed.get(&w.info.team_id),
                &app.sidebar_filter,
                |key| super::sidebar::folding(ctx, &w.info.team_id, key),
            );
            step(&order, w.active.as_deref(), forward, only_unread)
        });
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

/// The conversations the sidebar shows, in its order: closed ones,
/// deactivated people's DMs, those past "Show more" and the read rows of
/// folded sections left out, as on screen.
fn visible<'a>(
    workspace: &'a WorkspaceState,
    arrange: &Arrange<'_>,
    closed: Option<&std::collections::BTreeMap<String, String>>,
    filter: &str,
    folding: impl Fn(&str) -> (bool, bool),
) -> Vec<&'a Conversation> {
    let shown = crate::sidebar::layout(
        workspace.sections.as_deref(),
        &workspace.conversations,
        &workspace.users,
        |c| workspace.title(c),
        |c| workspace.rank(c),
        arrange,
    );
    super::sidebar::drawn(workspace, &shown, closed, filter, folding)
        .into_iter()
        .flat_map(|section| section.rows)
        .collect()
}

/// The conversation before or after the `active` one in `order`, wrapping
/// around at either end.
fn step(
    order: &[&Conversation],
    active: Option<&str>,
    forward: bool,
    only_unread: bool,
) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let current = active.and_then(|id| order.iter().position(|c| c.id == id));
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
            external: false,
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
        let arrange = Arrange::plain(crate::sidebar::Sort::Name);
        let order = visible(w, &arrange, None, "", |_| (true, false));
        step(&order, w.active.as_deref(), forward, only_unread)
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

    #[test]
    fn stepping_skips_rows_the_sidebar_hides() {
        let arrange = Arrange::plain(crate::sidebar::Sort::Name);
        let w = workspace(Some("C1"));
        // C3 (gamma) is closed and nothing new has come since.
        let closed = std::collections::BTreeMap::from([("C3".to_owned(), "9.0".to_owned())]);
        let order = visible(&w, &arrange, Some(&closed), "", |_| (true, false));
        let ids: Vec<&str> = order.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["C1", "C2"]);
        // A folded section shows only what is unread or open.
        let order = visible(&w, &arrange, None, "", |_| (false, false));
        let ids: Vec<&str> = order.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["C1", "C2"]);
        let w = workspace(None);
        let order = visible(&w, &arrange, None, "", |_| (false, false));
        assert_eq!(step(&order, None, true, false).as_deref(), Some("C2"));
        // A filter leaves only the matches, as on screen.
        let order = visible(&w, &arrange, None, "gam", |_| (true, false));
        let ids: Vec<&str> = order.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["C3"]);
    }
}
