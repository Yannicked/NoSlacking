//! The "Add emoji" dialog's work: picking a picture off the interface's
//! thread, sending it, and showing the new emoji at once.

use std::io::Read as _;
use std::path::Path;

use super::App;
use crate::backend::Command;
use crate::custom_emoji::{Dialog, MAX_BYTES, Picked};
use crate::failure::Failure;
use crate::i18n::tf;

/// A picture picked for a new emoji: the workspace it is for, and its
/// file name and bytes, or why it could not be read; `None` when the
/// picker was closed without one.
pub(super) type PickedImage = (String, Option<Result<(String, Vec<u8>), Failure>>);

impl App {
    /// Opens the "Add emoji" dialog over the picker, for the workspace on
    /// screen, when its sign-in can add emoji.
    pub(super) fn open_add_emoji(&mut self) {
        let Some(team) = self.active_team() else {
            return;
        };
        if !self.active_workspace().is_some_and(|w| w.can_add_emoji) {
            return;
        }
        self.picker = None;
        self.focus_overlay = true;
        self.add_emoji = Some(Dialog {
            team,
            ..Dialog::default()
        });
    }

    /// Shows the system's file picker on a thread of its own, and reads
    /// what is picked there too: never more than a byte past Slack's
    /// limit, which is enough to say it is too large.
    pub(super) fn pick_emoji_image(&mut self) {
        let Some(dialog) = self.add_emoji.as_mut() else {
            return;
        };
        dialog.busy = true;
        let team = dialog.team.clone();
        let sender = self.emoji_images.0.clone();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "gif"])
                .pick_file();
            // Nothing picked still ends the wait.
            let _ = sender.send((team, picked.map(|path| read_limited(&path))));
            waker.wake();
        });
    }

    /// A picture the file picker's thread read, for the open dialog.
    pub(super) fn emoji_image_picked(&mut self, (team, result): PickedImage) {
        let Some(dialog) = self.add_emoji.as_mut().filter(|d| d.team == team) else {
            return;
        };
        dialog.busy = false;
        match result {
            Some(Ok((file_name, bytes))) => {
                self.emoji_picks += 1;
                // The file's own name, as the emoji's, when it fits.
                if dialog.name.is_empty() {
                    let stem = std::path::Path::new(&file_name)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_lowercase()
                        .replace(' ', "-");
                    dialog.name = stem;
                }
                dialog.picked = Some(Picked::new(file_name, bytes, self.emoji_picks));
                dialog.error = None;
            }
            None => {}
            Some(Err(error)) => dialog.error = Some(error.sentence()),
        }
    }

    /// Sends the dialog's emoji to Slack, if the name and picture pass.
    pub(super) fn send_emoji(&mut self) {
        let Some(dialog) = self.add_emoji.as_ref() else {
            return;
        };
        let Some(workspace) = self
            .workspaces
            .iter()
            .find(|w| w.info.team_id == dialog.team)
        else {
            return;
        };
        let Some((name, picked, info)) = dialog.ready(&workspace.emoji) else {
            return;
        };
        let command = Command::AddEmoji {
            team: dialog.team.clone(),
            name,
            image: picked.bytes.to_vec(),
            file_name: picked.file_name.clone(),
            mime: info.format.mime().to_owned(),
        };
        self.backend.send(command);
        if let Some(dialog) = self.add_emoji.as_mut() {
            dialog.busy = true;
            dialog.error = None;
        }
    }

    /// Slack answered: the new emoji shows at once (from the picture in
    /// hand) while the workspace's list is fetched again; a refusal is
    /// told in the dialog, which stays open to try again.
    pub(super) fn emoji_added(&mut self, team: &str, name: String, result: Result<(), Failure>) {
        let dialog = self.add_emoji.as_mut().filter(|d| d.team == team);
        match result {
            Ok(()) => {
                let uri = dialog
                    .as_ref()
                    .and_then(|d| d.picked.as_ref())
                    .map(|p| p.uri.clone());
                if let (Some(uri), Some(workspace)) = (uri, self.workspace_mut(team)) {
                    workspace.emoji_added(name.clone(), uri);
                }
                self.add_emoji = None;
                self.backend.send(Command::FetchEmoji {
                    team: team.to_owned(),
                });
                self.toast(tf("Added :{name}:", &[("name", &name)]), false);
            }
            Err(error) => {
                if let Some(dialog) = dialog {
                    dialog.busy = false;
                    dialog.error = Some(error.sentence());
                } else {
                    self.toast(
                        tf(
                            "Could not add :{name}:: {error}",
                            &[("name", &name), ("error", &error.message())],
                        ),
                        true,
                    );
                }
            }
        }
    }
}

/// The file's name and at most [`MAX_BYTES`] and one bytes of it.
fn read_limited(path: &Path) -> Result<(String, Vec<u8>), Failure> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("emoji")
        .to_owned();
    let file = std::fs::File::open(path).map_err(|e| Failure::io(&e))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::io(&e))?;
    Ok((name, bytes))
}
