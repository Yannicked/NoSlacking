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
//! - Ctrl+Shift+H: leave the huddle being listened to (huddle audio)
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
    // Behind an open overlay they would change the conversation unseen.
    let arrows = !overlay && (!ctx.text_edit_focused() || in_empty_composer(app, ctx));
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
        } else if !overlay {
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
    #[cfg(feature = "huddle-audio")]
    if app.huddles.listening.is_some()
        && ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::H))
    {
        app.actions
            .push(Action::Huddle(crate::huddles::Action::Leave));
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
            let drafts = app.channels_with_drafts(&w.info.team_id);
            let arrange = Arrange {
                sort: app.settings.sidebar_sort,
                unread_first: app.settings.unread_first,
                hold: held.as_ref(),
                tidy: Some(crate::sidebar::Tidy {
                    after: app.settings.hide_inactive,
                    now: app.now_seconds(),
                    drafts: Some(&drafts),
                }),
                revision: None,
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
/// deactivated people's DMs, those held back behind "N more" (quiet ones
/// too, unless their section is expanded) and the read rows of folded
/// sections left out, as on screen.
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
    let drafts = arrange.tidy.and_then(|tidy| tidy.drafts);
    super::sidebar::drawn(workspace, &shown, closed, drafts, filter, folding)
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
            is_open: None,
            empty: false,
        }
    }

    fn workspace(active: Option<&str>) -> WorkspaceState {
        let mut w = WorkspaceState::new(Workspace {
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
        });
        *w.conversations = vec![
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

    /// A "now" long after the sample's 1970 messages.
    const NOW: i64 = 1_000_000_800;

    /// Name order with quiet conversations hidden after a month.
    fn tidied<'a>(
        hold: Option<&'a crate::sidebar::Hold>,
        drafts: Option<&'a std::collections::HashSet<String>>,
    ) -> Arrange<'a> {
        Arrange {
            hold,
            tidy: Some(crate::sidebar::Tidy {
                after: crate::sidebar::HideInactive::Month,
                now: NOW,
                drafts,
            }),
            ..Arrange::plain(crate::sidebar::Sort::Name)
        }
    }

    fn ids(order: &[&Conversation]) -> Vec<String> {
        order.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn group_dms_slack_closed_or_never_used_stay_out_of_sight() {
        let mut w = workspace(None);
        let group = |id: &str, name: &str| Conversation {
            kind: ConversationKind::Group,
            ..channel(id, name, false)
        };
        // Closed in Slack, read; closed but unread; never used at all.
        *w.conversations = vec![
            Conversation {
                is_open: Some(false),
                ..group("G1", "closed")
            },
            Conversation {
                is_open: Some(false),
                ..group("G2", "closed but new")
            },
            Conversation {
                latest: None,
                last_read: None,
                empty: true,
                ..group("G3", "never used")
            },
            Conversation {
                latest: None,
                last_read: None,
                ..group("G4", "not known")
            },
        ];
        w.conversations[1].latest = Some(Ts::new("2.0"));
        let shown_ids = |w: &WorkspaceState, drafts| {
            let arrange = tidied(None, drafts);
            ids(&visible(w, &arrange, None, "", |_| (true, false)))
        };
        // The closed read one is gone; the never-used one waits behind
        // "N more"; the unknown one shows.
        assert_eq!(shown_ids(&w, None), ["G2", "G4"]);
        let drafts = std::collections::HashSet::from(["G1".to_owned(), "G3".to_owned()]);
        assert_eq!(shown_ids(&w, Some(&drafts)), ["G2", "G1", "G3", "G4"]);
        // Open (and so held), the closed one shows while it is open.
        w.active = Some("G1".into());
        let held = crate::sidebar::Hold {
            id: "G1".into(),
            rank: crate::sidebar::Rank::Read,
        };
        let open = ids(&visible(&w, &tidied(Some(&held), None), None, "", |_| {
            (true, false)
        }));
        assert_eq!(open, ["G2", "G1", "G4"]);
        // Expanded, the never-used one shows; the closed one stays out.
        w.active = None;
        let arrange = tidied(None, None);
        let all = ids(&visible(&w, &arrange, None, "", |_| (true, true)));
        assert!(all.contains(&"G3".to_owned()));
        assert!(!all.contains(&"G1".to_owned()));
    }

    #[test]
    fn quiet_conversations_wait_behind_n_more() {
        let w = workspace(None);
        let arrange = tidied(None, None);
        let shown = crate::sidebar::layout(
            None,
            &w.conversations,
            &w.users,
            |c| w.title(c),
            |c| w.rank(c),
            &arrange,
        );
        // Alpha and gamma are long quiet; beta is unread.
        let tidy = super::super::sidebar::drawn(&w, &shown, None, None, "", |_| (true, false));
        assert_eq!(ids(&tidy[0].rows), ["C2"]);
        assert_eq!((tidy[0].more, tidy[0].less), (2, false));
        assert_eq!((tidy[1].more, tidy[1].less), (0, false), "nothing to hide");
        // Expanded: everything, in order, and "Show less".
        let all = super::super::sidebar::drawn(&w, &shown, None, None, "", |_| (true, true));
        assert_eq!(ids(&all[0].rows), ["C1", "C2", "C3"]);
        assert_eq!((all[0].more, all[0].less), (0, true));
        assert!(!all[1].less, "an expanded section with nothing hidden");
        // Searching finds quiet ones too.
        let found = super::super::sidebar::drawn(&w, &shown, None, None, "gam", |_| (true, false));
        assert_eq!(ids(&found[0].rows), ["C3"]);
        assert_eq!(found[0].more, 0);
    }

    #[test]
    fn n_more_counts_quiet_ones_and_direct_messages_past_the_limit() {
        let mut w = workspace(None);
        let at = |seconds: i64| Ts::new(format!("{seconds}.000100"));
        // 28 people spoke lately, 5 long ago.
        *w.conversations = (0..33)
            .map(|i| {
                let latest = if i < 28 {
                    at(NOW - 60 - i)
                } else {
                    at(NOW - 100 * 86_400 - i)
                };
                Conversation {
                    kind: ConversationKind::Direct,
                    last_read: Some(latest.clone()),
                    latest: Some(latest),
                    ..channel(&format!("D{i}"), &format!("person {i}"), false)
                }
            })
            .collect();
        w.conversations.push(Conversation {
            kind: ConversationKind::Direct,
            last_read: Some(at(NOW - 5000)),
            latest: Some(at(NOW - 10)),
            ..channel("D99", "still unread", false)
        });
        let arrange = tidied(None, None);
        let order = visible(&w, &arrange, None, "", |_| (true, false));
        assert_eq!(order.len(), 25, "the first 25 that are not quiet");
        assert_eq!(order[0].id, "D99");
        let shown = crate::sidebar::layout(
            None,
            &w.conversations,
            &w.users,
            |c| w.title(c),
            |c| w.rank(c),
            &arrange,
        );
        let drawn = super::super::sidebar::drawn(&w, &shown, None, None, "", |_| (true, false));
        assert_eq!(drawn[1].more, 4 + 5, "past the limit, and the quiet ones");
        let all = visible(&w, &arrange, None, "", |_| (true, true));
        assert_eq!(all.len(), 34);
    }

    #[test]
    fn stepping_skips_quiet_conversations_until_expanded() {
        let w = workspace(None);
        let arrange = tidied(None, None);
        let order = visible(&w, &arrange, None, "", |_| (true, false));
        assert_eq!(ids(&order), ["C2"]);
        assert_eq!(step(&order, None, true, false).as_deref(), Some("C2"));
        let order = visible(&w, &arrange, None, "", |_| (true, true));
        assert_eq!(ids(&order), ["C1", "C2", "C3"]);
        // The open one shows while open, found from the switcher or not.
        let w = workspace(Some("C1"));
        let held = crate::sidebar::Hold {
            id: "C1".into(),
            rank: crate::sidebar::Rank::Read,
        };
        let order = visible(&w, &tidied(Some(&held), None), None, "", |_| (true, false));
        assert_eq!(ids(&order), ["C1", "C2"]);
        assert_eq!(
            step(&order, w.active.as_deref(), true, false).as_deref(),
            Some("C2")
        );
        // One with a draft stays too.
        let drafts = std::collections::HashSet::from(["C3".to_owned()]);
        let w = workspace(None);
        let order = visible(&w, &tidied(None, Some(&drafts)), None, "", |_| {
            (true, false)
        });
        assert_eq!(ids(&order), ["C2", "C3"]);
    }
}
