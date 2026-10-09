//! Messages by keyboard: pick one, then act on it with a letter.
//!
//! - Shift+↑ in an empty composer selects that panel's last message; ↑
//!   with nothing focused selects the conversation's.
//! - ↑ / ↓: the message before or after; ↓ past the last, or Esc, goes back
//!   to the composer.
//! - R react, T reply in thread, E edit, Delete (or Backspace) delete,
//!   C copy the text, S share it to another conversation, U mark unread
//!   from here. Editing and deleting are
//!   for your own messages, and delete still asks first. Marking unread is
//!   for the conversation, not a thread (see the message menu).
//!
//! The keys work only while no text field has focus, so typing is never
//! taken; clicking into a field drops the selection.

use egui::{Event, Key, Modifiers};

use super::message::Verb;
use crate::app::{App, Selected, WorkspaceState};
use crate::model::{Action, Delivery, Message};

pub fn keys(app: &mut App, ctx: &egui::Context) {
    if let Some(selected) = &mut app.selected {
        selected.reveal = false;
    }
    let Some(workspace) = app.active_workspace() else {
        app.selected = None;
        return;
    };
    // A selection whose panel closed, or whose message went away, is gone.
    if let Some(selected) = &app.selected
        && !panel(app, workspace, selected.in_thread).is_some_and(|(channel, list)| {
            channel == selected.channel && list.iter().any(|m| m.ts == selected.ts)
        })
    {
        app.selected = None;
    }
    if ctx.text_edit_focused() {
        app.selected = None;
        if !starting(app, ctx) {
            return;
        }
    }
    if app.overlay_open() {
        return;
    }
    let focused = ctx.memory(|m| m.focused());
    let Some(selected) = app.selected.clone() else {
        start(app, ctx, focused);
        return;
    };
    let row = super::message::row_id(&selected.channel, &selected.ts, selected.in_thread);
    // Arrows belong to whatever else has focus, a toolbar button say.
    let arrows = focused.is_none_or(|id| id == row);
    let (up, down, escape, react, thread, edit, delete, copy, unread, share) =
        ctx.input_mut(|input| {
            (
                arrows && take(input, Key::ArrowUp, Modifiers::NONE),
                arrows && take(input, Key::ArrowDown, Modifiers::NONE),
                take(input, Key::Escape, Modifiers::NONE),
                take(input, Key::R, Modifiers::NONE),
                take(input, Key::T, Modifiers::NONE),
                take(input, Key::E, Modifiers::NONE),
                take(input, Key::Delete, Modifiers::NONE)
                    || take(input, Key::Backspace, Modifiers::NONE),
                take(input, Key::C, Modifiers::NONE),
                take(input, Key::U, Modifiers::NONE),
                take(input, Key::S, Modifiers::NONE),
            )
        });
    if up || down {
        // The arrow moved the selection: egui must not also move focus to
        // the widget above or below.
        ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    }
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    let Some((_, list)) = panel(app, workspace, selected.in_thread) else {
        return;
    };
    let Some(index) = list.iter().position(|m| m.ts == selected.ts) else {
        return;
    };
    let message = list[index];
    let channel = selected.channel.clone();
    let mut next = Some(selected.clone());
    if up || down {
        match step(&list, index, down) {
            Some(after) => {
                next = Some(Selected {
                    ts: after.ts.clone(),
                    reveal: true,
                    ..selected.clone()
                });
            }
            None if down => next = None,
            None => {}
        }
    }
    let subject = super::message::Subject {
        workspace,
        channel: &channel,
        message,
        in_thread: selected.in_thread,
    };
    let asked = [
        (react, Verb::React),
        (thread, Verb::Reply),
        (edit, Verb::Edit),
        (delete, Verb::Delete),
        (unread, Verb::MarkUnread),
        (copy, Verb::Copy),
        (share, Verb::Share),
    ];
    let actions: Vec<Action> = asked
        .into_iter()
        .filter(|(pressed, verb)| *pressed && verb.available(&subject))
        .map(|(_, verb)| verb.action(&subject))
        .collect();
    if escape {
        next = None;
    }
    let back_to = (next.is_none()).then(|| composer(app, ctx, selected.in_thread));
    app.actions.extend(actions);
    app.selected = next;
    if let Some(Some(field)) = back_to {
        ctx.memory_mut(|m| m.request_focus(field));
    }
}

/// Whether this frame's Shift+↑ in an empty composer is about to select a
/// message, which the focused field must not stop.
fn starting(app: &App, ctx: &egui::Context) -> bool {
    empty_composer(app, ctx).is_some()
        && ctx.input(|input| {
            input.events.iter().any(|e| {
                matches!(
                    e,
                    Event::Key { key: Key::ArrowUp, pressed: true, modifiers, .. }
                        if modifiers.shift_only()
                )
            })
        })
}

/// Selects a panel's last message: Shift+↑ from its empty composer, or ↑
/// with nothing focused for the conversation.
fn start(app: &mut App, ctx: &egui::Context, focused: Option<egui::Id>) {
    let from_composer = empty_composer(app, ctx);
    let in_thread = match from_composer {
        Some(in_thread) => {
            if !ctx.input_mut(|input| take(input, Key::ArrowUp, Modifiers::SHIFT)) {
                return;
            }
            in_thread
        }
        None => {
            if focused.is_some()
                || !ctx.input_mut(|input| take(input, Key::ArrowUp, Modifiers::NONE))
            {
                return;
            }
            false
        }
    };
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    let Some((channel, list)) = panel(app, workspace, in_thread) else {
        return;
    };
    let Some(last) = list.last() else {
        return;
    };
    let selected = Selected {
        channel: channel.to_owned(),
        ts: last.ts.clone(),
        in_thread,
        reveal: true,
    };
    if let Some(focused) = focused {
        ctx.memory_mut(|m| m.surrender_focus(focused));
    }
    app.selected = Some(selected);
}

/// The channel a panel shows and the messages in it you can select, oldest
/// first: what has been sent and is not a join or topic line.
fn panel<'a>(
    app: &'a App,
    workspace: &'a WorkspaceState,
    in_thread: bool,
) -> Option<(&'a str, Vec<&'a Message>)> {
    let selectable = |m: &&Message| m.delivery == Delivery::Sent && !m.is_system();
    if in_thread {
        let (channel, ts) = app.thread.as_ref()?;
        let replies = workspace.threads.get(&(channel.clone(), ts.clone()));
        let parent = replies
            .and_then(|t| t.messages.iter().find(|m| m.ts == *ts))
            .or_else(|| {
                workspace
                    .timelines
                    .get(channel)
                    .and_then(|t| t.messages.iter().find(|m| m.ts == *ts))
            });
        let list = parent
            .into_iter()
            .chain(
                replies
                    .into_iter()
                    .flat_map(|t| t.messages.iter().filter(|m| m.ts != *ts)),
            )
            .filter(selectable)
            .collect();
        Some((channel.as_str(), list))
    } else {
        let channel = workspace.active.as_deref()?;
        let list = workspace
            .timelines
            .get(channel)
            .into_iter()
            .flat_map(|t| t.messages.iter())
            .filter(|m| m.in_channel())
            .filter(selectable)
            .collect();
        Some((channel, list))
    }
}

/// The message after (`down`) or before the one at `index`, if any.
fn step<T>(list: &[T], index: usize, down: bool) -> Option<&T> {
    let next = if down {
        index.checked_add(1)?
    } else {
        index.checked_sub(1)?
    };
    list.get(next)
}

/// The composer field of the conversation or the thread.
fn composer(app: &App, ctx: &egui::Context, in_thread: bool) -> Option<egui::Id> {
    let team = app.active_team()?;
    let key = if in_thread {
        let (channel, ts) = app.thread.as_ref()?;
        App::draft_key(&team, channel, Some(ts))
    } else {
        let channel = app.active_workspace()?.active.clone()?;
        App::draft_key(&team, &channel, None)
    };
    Some(super::composer::field_id(ctx, &key))
}

/// Whether the focused field is an empty composer on screen, and if so
/// whether it is the thread's.
fn empty_composer(app: &App, ctx: &egui::Context) -> Option<bool> {
    let focused = ctx.memory(|m| m.focused())?;
    [false, true].into_iter().find(|&in_thread| {
        composer(app, ctx, in_thread) == Some(focused) && {
            let team = app.active_team().unwrap_or_default();
            let key = match (&app.thread, in_thread) {
                (Some((channel, ts)), true) => App::draft_key(&team, channel, Some(ts)),
                _ => app
                    .active_workspace()
                    .and_then(|w| w.active.as_deref())
                    .map(|channel| App::draft_key(&team, channel, None))
                    .unwrap_or_default(),
            };
            app.drafts.get(&key).is_none_or(|d| d.text.is_empty())
        }
    })
}

/// Takes a key press with exactly `modifiers`. egui's own `consume_key`
/// also matches with Shift or Alt held, which would steal Alt+↑ (next
/// conversation) and Shift+↑.
pub(super) fn take(input: &mut egui::InputState, key: Key, modifiers: Modifiers) -> bool {
    let mut found = false;
    input.events.retain(|event| {
        let hit = matches!(
            event,
            Event::Key { key: k, pressed: true, modifiers: m, .. }
                if *k == key && m.matches_exact(modifiers)
        );
        found |= hit;
        !hit
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_stop_at_either_end() {
        let list = ["a", "b"];
        assert_eq!(step(&list, 0, true), Some(&"b"));
        assert_eq!(step(&list, 1, false), Some(&"a"));
        assert_eq!(step(&list, 1, true), None);
        assert_eq!(step(&list, 0, false), None);
    }

    #[test]
    fn keys_need_the_exact_modifiers() {
        let mut input = egui::InputState::default();
        let press = |modifiers| Event::Key {
            key: Key::ArrowUp,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        input.events = vec![press(Modifiers::ALT)];
        assert!(!take(&mut input, Key::ArrowUp, Modifiers::NONE));
        assert_eq!(input.events.len(), 1);
        input.events = vec![press(Modifiers::SHIFT)];
        assert!(take(&mut input, Key::ArrowUp, Modifiers::SHIFT));
        assert!(input.events.is_empty());
    }
}
