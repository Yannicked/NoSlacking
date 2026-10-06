//! Conversations open in native windows of their own, through egui's
//! immediate viewports (eframe's glow backend gives each one a real
//! window).
//!
//! The conversation view draws "the active conversation", and the actions
//! it pushes (send, react, edit) act on it too. So a pop-out window makes
//! its conversation the active one while it draws and while its actions
//! are applied, then puts the main window's back. An action that moves to
//! another conversation (a link, a mention) is kept, and shows in the main
//! window.

use super::{App, Page};
use crate::model::Action;

/// One pop-out window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popout {
    pub team: String,
    pub channel: String,
}

impl Popout {
    fn viewport(&self) -> egui::ViewportId {
        egui::ViewportId::from_hash_of(("popout", &self.team, &self.channel))
    }
}

/// The main window's state a pop-out borrows while it draws.
struct Borrowed {
    active_workspace: Option<String>,
    active_channel: Option<String>,
    focus_composer: bool,
    prepended: Option<String>,
    actions: Vec<Action>,
    page: Page,
}

impl App {
    /// Opens the active workspace's `channel` in a window of its own, or
    /// leaves it be if one is open.
    pub(super) fn pop_out(&mut self, channel: String) {
        let Some(team) = self.active_team() else {
            return;
        };
        let popout = Popout { team, channel };
        if !self.popouts.contains(&popout) {
            log::info!("popping out {}", popout.channel);
            self.ensure_loaded(&popout.team, &popout.channel);
            self.popouts.push(popout);
        }
    }

    /// Draws every pop-out window; call once a frame, inside the main
    /// window's frame.
    pub fn show_popouts(&mut self, ctx: &egui::Context) {
        let popouts = self.popouts.clone();
        let mut closed = Vec::new();
        for popout in popouts {
            let title = self
                .workspace_mut(&popout.team)
                .and_then(|w| w.conversation(&popout.channel).map(|c| w.title(c)));
            let Some(title) = title else {
                // Signed out, or the conversation is gone.
                closed.push(popout);
                continue;
            };
            let builder = egui::ViewportBuilder::default()
                .with_title(format!("{title} – NoSlacking"))
                .with_app_id(crate::paths::APP_ID)
                .with_inner_size([560.0, 720.0])
                .with_min_inner_size([360.0, 320.0]);
            let open = ctx.show_viewport_immediate(popout.viewport(), builder, |ui, _class| {
                if ui.input(|i| i.viewport().close_requested()) {
                    return false;
                }
                let focused = ui.input(|i| i.viewport().focused.unwrap_or(false));
                let borrowed = self.borrow_for(&popout);
                if focused {
                    self.mark_seen(&popout.team, &popout.channel);
                }
                crate::ui::popout(self, ui);
                let ctx = ui.ctx().clone();
                for _ in 0..4 {
                    let actions = std::mem::take(&mut self.actions);
                    if actions.is_empty() {
                        break;
                    }
                    for action in actions {
                        self.apply(action, &ctx);
                    }
                }
                self.give_back(&popout, borrowed);
                true
            });
            if !open {
                closed.push(popout);
            }
        }
        self.popouts.retain(|p| !closed.contains(p));
    }

    fn borrow_for(&mut self, popout: &Popout) -> Borrowed {
        let prepended = match &self.prepended {
            Some(owner) if *owner == format!("{}/{}", popout.team, popout.channel) => None,
            _ => self.prepended.take(),
        };
        let mut borrowed = Borrowed {
            active_workspace: self.settings.active_workspace.clone(),
            active_channel: None,
            focus_composer: std::mem::take(&mut self.focus_composer),
            prepended,
            actions: std::mem::take(&mut self.actions),
            page: self.page,
        };
        self.page = Page::Main;
        self.settings.active_workspace = Some(popout.team.clone());
        if let Some(workspace) = self.workspace_mut(&popout.team) {
            borrowed.active_channel = workspace.active.replace(popout.channel.clone());
        }
        borrowed
    }

    fn give_back(&mut self, popout: &Popout, borrowed: Borrowed) {
        let still_here = self.settings.active_workspace.as_deref() == Some(popout.team.as_str())
            && self
                .workspace_mut(&popout.team)
                .and_then(|w| w.active.clone())
                .as_deref()
                == Some(popout.channel.as_str());
        if still_here {
            if let Some(workspace) = self.workspace_mut(&popout.team) {
                workspace.active = borrowed.active_channel;
            }
            self.settings.active_workspace = borrowed.active_workspace;
            self.page = borrowed.page;
            self.focus_composer |= borrowed.focus_composer;
        }
        // Otherwise the pop-out moved on (a link, a mention): the action
        // that opened the other conversation did so as in the main window,
        // which now shows it.
        if self.prepended.is_none() {
            self.prepended = borrowed.prepended;
        }
        let mut actions = borrowed.actions;
        actions.append(&mut self.actions);
        self.actions = actions;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_conversation_gets_its_own_window() {
        let a = Popout {
            team: "T1".into(),
            channel: "C1".into(),
        };
        let b = Popout {
            team: "T1".into(),
            channel: "C2".into(),
        };
        assert_ne!(a.viewport(), b.viewport());
        assert_eq!(a.viewport(), a.clone().viewport());
    }
}
