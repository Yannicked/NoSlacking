//! What you do to messages: send, retry, react, edit and delete them,
//! and the files that go with them.
//!
//! A second `impl App`, kept apart so the main loop in `app.rs` stays
//! readable. Each change shows at once and the worker is told after.

use std::path::PathBuf;

use super::workspace::local_message;
use super::{App, Upload, UploadTarget, to_wire};
use crate::backend::Command;
use crate::failure::Failure;
use crate::i18n::{t, tf};
use crate::model::Ts;
use crate::mrkdwn;

impl App {
    fn next_local(&mut self) -> Ts {
        self.local_counter += 1;
        Ts::new(format!("local-{}", self.local_counter))
    }

    pub(super) fn send(&mut self, text: String, thread: Option<Ts>, broadcast: bool) {
        let Some(team) = self.active_team() else {
            return;
        };
        let channel = match &thread {
            Some(_) => self.thread.as_ref().map(|(c, _)| c.clone()),
            None => self.active_workspace().and_then(|w| w.active.clone()),
        };
        let Some(channel) = channel else {
            return;
        };
        let key = Self::draft_key(&team, &channel, thread.as_ref());
        let draft = self.drafts.remove(&key).unwrap_or_default();
        if !draft.attachments.is_empty() {
            // Files go with the message: its text is the first file's
            // comment, as Slack shows a message with files.
            let wire = crate::emoji::tone_shortcodes(
                &to_wire(text.trim_end(), &draft.mentions),
                self.settings.skin_tone,
            );
            self.used_emoji(&crate::emoji::used_in(&wire));
            let target = (team, channel, thread);
            let mut comment = wire;
            for path in draft.attachments {
                self.start_upload(target.clone(), path, std::mem::take(&mut comment));
            }
            self.scroll_to_bottom.insert(key);
            return;
        }
        let mut text = text;
        if let Some((command, args)) = crate::slash::parse(&text) {
            match command.as_str() {
                // Plain messages in the end, sent as any other.
                "shrug" => text = crate::slash::shrug(args),
                // chat.meMessage cannot reply in a thread; italics read
                // the same there.
                "me" if thread.is_some() && !args.is_empty() => text = format!("_{args}_"),
                _ => {
                    let text = to_wire(args, &draft.mentions);
                    self.backend.send(Command::Slash {
                        team,
                        channel,
                        command,
                        text,
                    });
                    return;
                }
            }
        }
        let wire = to_wire(&text, &draft.mentions);
        if wire.trim().is_empty() {
            return;
        }
        if thread.is_none() {
            // What you send goes at the end, which a list of older history
            // does not show.
            self.show_newest(&team, &channel);
        }
        let wire = crate::emoji::tone_shortcodes(&wire, self.settings.skin_tone);
        self.used_emoji(&crate::emoji::used_in(&wire));
        let local = self.next_local();
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let message = local_message(&workspace.info.user_id, &local, &wire, &thread, broadcast);
        workspace.add_local(&channel, message);
        self.scroll_to_bottom.insert(key);
        self.backend.send(Command::Send {
            team,
            channel,
            text: wire,
            thread,
            broadcast,
            local,
        });
    }

    pub(super) fn retry(&mut self, channel: &str, local: &Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        let found = self
            .workspace_mut(&team)
            .and_then(|w| w.retry_local(channel, local));
        if let Some((text, thread, broadcast)) = found {
            self.backend.send(Command::Send {
                team,
                channel: channel.to_owned(),
                text,
                thread,
                broadcast,
                local: local.clone(),
            });
        }
    }

    pub(super) fn react(&mut self, channel: &str, ts: &Ts, name: &str) {
        let Some(team) = self.active_team() else {
            return;
        };
        let add = self
            .workspace_mut(&team)
            .and_then(|w| w.toggle_my_reaction(channel, ts, name));
        if add == Some(true) {
            self.used_emoji(&[name.to_owned()]);
        }
        if let Some(add) = add {
            self.backend.send(Command::React {
                team,
                channel: channel.to_owned(),
                ts: ts.clone(),
                name: name.to_owned(),
                add,
            });
        }
    }

    /// Puts emoji just sent or reacted with at the front of the picker's
    /// "Recently used".
    fn used_emoji(&mut self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let before = self.settings.recent_emoji.clone();
        crate::emoji::remember(&mut self.settings.recent_emoji, names);
        if self.settings.recent_emoji != before {
            self.save_settings();
        }
    }

    /// The reactions offered first on a message's toolbar: your five most
    /// recent emoji at your skin tone, or Slack's usual two before you
    /// have used any.
    pub fn quick_reactions(&self) -> Vec<String> {
        let recent = &self.settings.recent_emoji;
        if recent.is_empty() {
            return vec!["white_check_mark".to_owned(), "eyes".to_owned()];
        }
        recent
            .iter()
            .take(5)
            .map(|name| crate::emoji::toned(name, self.settings.skin_tone))
            .collect()
    }

    pub(super) fn edit(&mut self, channel: String, ts: Ts, text: String) {
        let Some(team) = self.active_team() else {
            return;
        };
        let mentions = self
            .editing
            .take()
            .filter(|e| e.ts == ts && e.channel == channel)
            .map(|e| e.mentions)
            .unwrap_or_default();
        let wire =
            crate::emoji::tone_shortcodes(&to_wire(&text, &mentions), self.settings.skin_tone);
        let before = self
            .workspace_mut(&team)
            .and_then(|w| w.edit_locally(&channel, &ts, &wire));
        self.backend.send(Command::Edit {
            team,
            channel,
            ts,
            text: wire,
            before: before.map(Box::new),
        });
    }

    pub(super) fn delete(&mut self, channel: String, ts: Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        let removed = self
            .active_workspace()
            .and_then(|w| w.find_message(&channel, &ts))
            .cloned();
        self.remove_message(&team, &channel, &ts);
        if !ts.is_local() {
            self.backend.send(Command::Delete {
                team,
                channel,
                ts,
                removed: removed.map(Box::new),
            });
        }
    }

    /// Where a file from the composer of `thread` (or the conversation)
    /// goes, as it is on screen now.
    fn upload_target(&self, thread: Option<Ts>) -> Option<UploadTarget> {
        let team = self.active_team()?;
        let channel = match &thread {
            Some(_) => self.thread.as_ref().map(|(c, _)| c.clone()),
            None => self.active_workspace().and_then(|w| w.active.clone()),
        }?;
        Some((team, channel, thread))
    }

    /// A file for the composer: one with a comment goes now; one without
    /// waits in the composer, to go with the message.
    pub(super) fn upload(&mut self, thread: Option<Ts>, path: PathBuf, comment: String) {
        if let Some(target) = self.upload_target(thread) {
            if comment.is_empty() {
                self.stage(target, path);
            } else {
                self.start_upload(target, path, comment);
            }
        }
    }

    /// Adds a file to the composer of `target`, once.
    pub(super) fn stage(&mut self, (team, channel, thread): UploadTarget, path: PathBuf) {
        let key = Self::draft_key(&team, &channel, thread.as_ref());
        let draft = self.drafts.entry(key).or_default();
        if !draft.attachments.contains(&path) {
            draft.attachments.push(path);
        }
        self.focus_composer = true;
    }

    /// A file taken out of a composer before sending: a pasted picture is
    /// a copy of ours, so it goes.
    pub(super) fn unstage(&self, path: &std::path::Path) {
        if path.starts_with(self.dirs.pasted())
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::debug!("could not remove a pasted image: {error}");
        }
    }

    /// Sends a file to the worker and lists it under its composer.
    fn start_upload(
        &mut self,
        (team, channel, thread): UploadTarget,
        path: PathBuf,
        comment: String,
    ) {
        self.next_upload += 1;
        let id = self.next_upload;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let pasted = path.starts_with(self.dirs.pasted()).then(|| path.clone());
        self.transfers.push(Upload {
            id,
            key: Self::draft_key(&team, &channel, thread.as_ref()),
            name,
            sent: 0,
            total: 0,
            pasted,
        });
        self.backend.send(Command::Upload {
            id,
            team,
            channel,
            thread,
            path,
            comment,
        });
    }

    /// A slash command finished: Slack's reply if it gave one, a word
    /// that it worked otherwise, or why not.
    pub(super) fn slash_done(&mut self, command: &str, result: Result<Option<String>, Failure>) {
        let name = format!("/{command}");
        match result {
            Ok(Some(reply)) => {
                let reply = mrkdwn::plain(&reply, |_| None);
                self.toast(reply, false);
            }
            Ok(None) => {
                let done = match command {
                    // The message itself shows that it worked.
                    "me" => return,
                    "away" => t("You are now shown as away"),
                    "active" => t("You are now shown as active"),
                    "status" => t("Your status is updated"),
                    "topic" => t("The topic is changed"),
                    "invite" => t("Invited"),
                    "leave" => t("You left the channel"),
                    _ => t("Done"),
                };
                self.toast(done.into_owned(), false);
            }
            Err(Failure::NeedsSession) => self.toast(
                tf(
                    "{command} only works when you sign in with your browser",
                    &[("command", &name)],
                ),
                true,
            ),
            Err(error) => self.toast(
                tf(
                    "{command} failed: {error}",
                    &[("command", &name), ("error", &error.message())],
                ),
                true,
            ),
        }
    }

    /// An upload ended one way or another: it leaves the composer, and a
    /// pasted image's temporary file goes.
    pub(super) fn upload_done(&mut self, id: u64) {
        let Some(index) = self.transfers.iter().position(|u| u.id == id) else {
            return;
        };
        let upload = self.transfers.remove(index);
        if let Some(path) = upload.pasted
            && let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::debug!("could not remove a pasted image: {error}");
        }
    }

    /// Uploads the clipboard's files or image, if it holds no text: Ctrl+V
    /// with text was already pasted into the field by egui. The clipboard
    /// is read on a thread of its own, as some desktops answer slowly.
    pub(super) fn paste_image(&mut self, thread: Option<Ts>) {
        let Some(target) = self.upload_target(thread) else {
            return;
        };
        let sender = self.uploads.0.clone();
        let waker = self.waker.clone();
        let dir = self.dirs.pasted();
        std::thread::spawn(move || match crate::paste::clipboard_files(&dir) {
            Ok(paths) => {
                for path in paths {
                    let _ = sender.send((target.clone(), path));
                }
                waker.wake();
            }
            Err(error) => log::warn!("could not paste: {error}"),
        });
    }

    pub(super) fn pick_upload(&mut self, thread: Option<Ts>) {
        // Decided now: the dialog may stay open while you switch to
        // another conversation, and the file belongs to this one.
        let Some(target) = self.upload_target(thread) else {
            return;
        };
        let sender = self.uploads.0.clone();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                let _ = sender.send((target, path));
                waker.wake();
            }
        });
    }
}
