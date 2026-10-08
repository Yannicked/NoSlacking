//! Files: uploading, downloading, opening, viewing and deleting them, and
//! adding custom emoji.

use super::{Otherwise, Worker};
use crate::backend::Event;
use crate::backend::api::{Call, failure};
use crate::backend::files::{
    Attachment, Destination, UploadGate, download, fetch_bytes, file_name, open_file, upload, view,
};
use crate::failure::{Doing, Problem};
use crate::model::Ts;
use crate::notice::Notice;

impl Worker {
    /// `files.delete`, answered with [`Event::FileDeleteSettled`] either
    /// way, so a file hidden on screen never stays hidden after a refusal.
    pub(super) fn delete_file(&self, team: String, file: String, name: String) {
        // Gone already is what was asked.
        let call = delete_file_request(&file);
        self.answer_slack(
            team,
            |client| async move { call.run(&client).await.map_err(|e| failure(&e)) },
            |team, result| Event::FileDeleteSettled {
                team,
                file,
                name,
                result,
            },
        );
    }

    /// `emoji.add` as the web client sends it; only a browser session
    /// may, so any other sign-in is told so without asking Slack.
    pub(super) fn add_emoji(
        &self,
        team: String,
        name: String,
        image: Vec<u8>,
        file_name: String,
        mime: String,
    ) {
        let named = name.clone();
        self.answer_slack(
            team,
            |client| async move {
                client
                    .add_emoji(&named, image, &file_name, &mime)
                    .await
                    .map_err(|e| failure(&e))
            },
            |team, result| Event::EmojiAdded { team, name, result },
        );
    }

    pub(super) fn upload(
        &mut self,
        id: u64,
        team: String,
        channel: String,
        thread: Option<Ts>,
        path: std::path::PathBuf,
        comment: String,
    ) {
        let Some((client, sink)) = self.slack(&team) else {
            let missing = self.missing(&team);
            self.refuse(
                missing,
                Doing::Upload {
                    name: file_name(&path),
                },
            );
            self.sink.send(Event::UploadDone { id, shared: false });
            return;
        };
        let poll_after = !self.is_live(&team);
        self.uploads
            .retain(|_, running| !running.task.is_finished());
        let gate = UploadGate::default();
        let task = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let to = Destination {
                    team,
                    channel,
                    thread,
                };
                let file = Attachment { path, comment };
                let shared = upload(id, client, to, file, poll_after, gate, &sink).await;
                sink.send(Event::UploadDone { id, shared });
            })
        };
        self.uploads.insert(
            id,
            Running {
                task: task.abort_handle(),
                gate,
            },
        );
    }

    /// Stops an upload only while its gate still allows it. Once Slack is
    /// being told to share the file the cancel is ignored, and the upload
    /// ends with its own [`Event::UploadDone`], so the interface never
    /// says "cancelled" about a file that was posted.
    pub(super) fn cancel_upload(&mut self, id: u64) {
        let Some(Running { task, gate }) = self.uploads.remove(&id) else {
            // Already over: its own UploadDone was sent.
            return;
        };
        if task.is_finished() {
            return;
        }
        if gate.cancel() {
            task.abort();
            self.sink.send(Event::UploadCancelled { id });
        } else {
            log::debug!("upload {id} is already being shared; not cancelled");
            self.uploads.insert(id, Running { task, gate });
        }
    }

    pub(super) fn download(&self, team: String, url: String, name: String) {
        self.spawn_slack(
            team,
            Otherwise::Refuse(Doing::Download { name: name.clone() }),
            |client, _, sink| async move {
                match download(&client, &url, &name).await {
                    Ok(path) => sink.send(Event::Notice(Notice::Saved {
                        path: path.display().to_string(),
                    })),
                    Err(error) => sink.send(Event::Error(error)),
                }
            },
        );
    }

    pub(super) fn open_file(&self, team: String, url: String, name: String) {
        let dir = self.images.open_dir(&team, &url);
        self.spawn_slack(
            team,
            Otherwise::Refuse(Doing::Open { name: name.clone() }),
            |client, _, sink| async move {
                if let Err(error) = open_file(&client, dir, &url, &name).await {
                    sink.send(Event::Error(error));
                }
            },
        );
    }

    /// Fetches a sound whole, for playing in the app. Its answer always
    /// comes, so the card never waits for ever.
    pub(super) fn fetch_audio(&self, team: String, id: u64, url: String, name: String) {
        let doing = Doing::Download { name: name.clone() };
        self.answer_slack(
            team,
            |client| async move {
                Ok(fetch_bytes(&client, &url, &name, crate::audio::MAX_BYTES)
                    .await
                    .map(crate::audio::Bytes::from))
            },
            move |_, result| Event::AudioFetched {
                id,
                result: result.unwrap_or_else(|missing| Err(Problem::new(doing, missing))),
            },
        );
    }

    /// Fetches a file for the viewer and reads it off the runtime's
    /// threads.
    pub(super) fn view_file(
        &self,
        id: u64,
        team: String,
        url: String,
        kind: crate::viewer::Kind,
        size: u64,
    ) {
        self.answer_slack(
            team,
            |client| async move { view(&client, &url, kind, size).await },
            move |_, result| Event::FileView { id, result },
        );
    }
}

/// An upload still running.
pub(super) struct Running {
    /// Its task, to stop it.
    pub(super) task: tokio::task::AbortHandle,
    /// Whether it may still be stopped.
    pub(super) gate: UploadGate,
}

/// The call that deletes file `file`, with the refusals that mean it is
/// gone already.
fn delete_file_request(file: &str) -> Call {
    Call::new(
        "files.delete",
        vec![("file", file.to_owned())],
        &["file_not_found", "file_deleted"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_a_file_names_only_the_file_and_takes_gone_as_done() {
        let call = delete_file_request("F1");
        assert_eq!(call.method, "files.delete");
        assert_eq!(call.params, vec![("file", "F1".to_owned())]);
        assert!(call.done.contains(&"file_not_found"));
        assert!(call.done.contains(&"file_deleted"));
        assert!(
            !call.done.contains(&"cant_delete_file"),
            "a refusal is undone"
        );
    }
}
