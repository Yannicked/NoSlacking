//! Files going up to Slack and coming down: uploads with progress,
//! downloads under names that are safe on every system, and opening a
//! file in the app made for it.

use super::api::failure;
use super::fetch::history;
use super::{Event, Sink};
use crate::failure::{Doing, Failure, Problem};
use crate::model::Ts;
use crate::offline::Cache;
use crate::slack::{Client, SlackError};

const MAX_UPLOAD: u64 = 1024 * 1024 * 1024;

/// Decides, once, whether an upload is cancelled or shared, so a Cancel
/// and the start of the last step can never both win.
///
/// Slack posts the file once `files.completeUploadExternal` is sent, so
/// from then on cancelling would only hide an upload that still happens.
/// The worker and the upload's task each hold a clone.
#[derive(Clone, Debug, Default)]
pub struct UploadGate(std::sync::Arc<std::sync::atomic::AtomicU8>);

impl UploadGate {
    const SENDING: u8 = 0;
    const CANCELLED: u8 = 1;
    const FINISHING: u8 = 2;

    /// Cancels the upload unless its last step has begun. True when it is
    /// cancelled (now or before), false when it is too late.
    pub fn cancel(&self) -> bool {
        self.settle(Self::CANCELLED)
    }

    /// Starts the last step unless the upload was cancelled. True when
    /// the upload may go on to be shared.
    pub fn finish(&self) -> bool {
        self.settle(Self::FINISHING)
    }

    /// Moves from sending to `to`, or reports whether the upload is
    /// already there: whichever side asks first decides.
    fn settle(&self, to: u8) -> bool {
        use std::sync::atomic::Ordering;
        match self
            .0
            .compare_exchange(Self::SENDING, to, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(now) => now == to,
        }
    }
}

/// Uploads one file for [`Worker::upload`](super::worker::Worker::upload),
/// telling the interface how far it got along the way.
#[allow(clippy::too_many_arguments)]
pub(super) async fn upload(
    id: u64,
    client: Client,
    team: String,
    channel: String,
    thread: Option<Ts>,
    path: std::path::PathBuf,
    comment: String,
    poll_after: bool,
    gate: UploadGate,
    sink: &Sink,
) {
    let name = file_name(&path);
    // The size comes from the open file, so it is the size of what
    // gets streamed, not of whatever the path named a moment
    // earlier.
    let opened = match tokio::fs::File::open(&path).await {
        Ok(file) => file.metadata().await.map(|meta| (file, meta)),
        Err(error) => Err(error),
    };
    let failed = |why| {
        let doing = Doing::Upload { name: name.clone() };
        sink.send(Event::Error(Problem::new(doing, why)));
    };
    let (file, size) = match opened {
        Ok((_, meta)) if !meta.is_file() => return failed(Failure::NotAFile),
        Ok((file, meta)) => (file, meta.len()),
        Err(error) => return failed(Failure::Other(error.to_string())),
    };
    if size > MAX_UPLOAD {
        return failed(Failure::TooLarge);
    }
    sink.send(Event::UploadProgress {
        id,
        sent: 0,
        total: size,
    });
    let thread = thread.as_ref().map(Ts::as_str);
    let progress = {
        let sink = sink.clone();
        // Told only at each further hundredth, so a big file does not
        // wake the window for every chunk.
        let reported = std::sync::atomic::AtomicU64::new(0);
        let step = (size / 100).max(1);
        move |sent: u64| {
            let last = reported.load(std::sync::atomic::Ordering::Relaxed);
            if sent / step > last / step || sent == size {
                reported.store(sent, std::sync::atomic::Ordering::Relaxed);
                sink.send(Event::UploadProgress {
                    id,
                    sent,
                    total: size,
                });
            }
        }
    };
    let finish = {
        let sink = sink.clone();
        // The interface stops offering Cancel once this is said; the gate
        // makes sure a Cancel already on its way is refused, not obeyed.
        move || {
            let go_on = gate.finish();
            if go_on {
                sink.send(Event::UploadFinishing { id });
            }
            go_on
        }
    };
    match client
        .upload(
            &channel, thread, &name, file, size, &comment, progress, finish,
        )
        .await
    {
        // Cancelled in time: the worker has already told the interface.
        Ok(false) => {}
        Ok(true) => {
            sink.send(Event::Notice(format!("Uploaded {name}")));
            // Without a live socket the new file would only show
            // at the next poll.
            if poll_after {
                history(
                    client,
                    team,
                    channel,
                    None,
                    Cache::disabled(),
                    true,
                    sink.clone(),
                )
                .await;
            }
        }
        Err(error) => failed(failure(&error)),
    }
}

/// Saves the file at `url` in the downloads folder as `name`, or says
/// why not.
///
/// The body streams into a hidden temporary file next to its final place,
/// which is renamed once complete, so a large file never sits in memory
/// and a failed download never appears under the real name.
pub(super) async fn download(
    client: &Client,
    url: &str,
    name: &str,
) -> Result<std::path::PathBuf, Problem> {
    // Finding the folder can read a config file; keep it off the runtime.
    let dir = tokio::task::spawn_blocking(downloads_dir)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| Problem::new(saving(name), Failure::NoDownloadsFolder))?;
    save(client, url, name, &dir).await
}

/// Downloads a file into the private cache (see
/// [`ImageLoader::open_dir`](crate::images::ImageLoader::open_dir)) and
/// opens it in the system's app for it. A file fetched before is opened
/// again without fetching.
pub(super) async fn open_file(
    client: &Client,
    dir: std::path::PathBuf,
    url: &str,
    name: &str,
) -> Result<(), Problem> {
    // The system opens a file by its name, and a name is whatever the
    // sender chose: only Slack's own files with a player's extension are
    // opened, so "clip.mp4.exe" can never be run.
    if !crate::slack::client::is_slack_file_url(url) || !plays_safely(&safe_name(name)) {
        return Err(Problem::new(opening(name), Failure::NotOpenable));
    }
    let saved = dir.join(safe_name(name));
    let have = tokio::fs::metadata(&saved)
        .await
        .is_ok_and(|meta| meta.is_file() && meta.len() > 0);
    let path = if have {
        saved
    } else {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| Problem::new(saving(name), Failure::Other(error.to_string())))?;
        save(client, url, name, &dir).await?
    };
    tokio::task::spawn_blocking(move || open::that_detached(&path))
        .await
        .map_err(|error| error.to_string())
        .and_then(|opened| opened.map_err(|error| error.to_string()))
        .map_err(|error| Problem::new(opening(name), Failure::Other(error)))
}

/// Streams `url` into a new file named after `name` in `dir`, numbered if
/// the name is taken, and returns where it went.
async fn save(
    client: &Client,
    url: &str,
    name: &str,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, Problem> {
    use tokio::io::AsyncWriteExt as _;
    let downloading = |why| {
        Problem::new(
            Doing::Download {
                name: name.to_owned(),
            },
            why,
        )
    };
    let mut response = client
        .download(url)
        .await
        .map_err(|e| downloading(failure(&e)))?;
    let unsaved =
        |error: std::io::Error| Problem::new(saving(name), Failure::Other(error.to_string()));
    let dir = dir.to_path_buf();
    let safe = safe_name(name);
    let (part, mut file) = create_unique(&dir, |n| format!(".{}.part", numbered(&safe, n)))
        .await
        .map_err(unsaved)?;
    let written: Result<(), Problem> = async {
        let mut size = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| downloading(failure(&SlackError::from(e))))?
        {
            size += chunk.len() as u64;
            if size > MAX_UPLOAD {
                return Err(downloading(Failure::TooLarge));
            }
            file.write_all(&chunk).await.map_err(unsaved)?;
        }
        file.flush().await.map_err(unsaved)?;
        file.sync_all().await.map_err(unsaved)
    }
    .await;
    drop(file);
    if let Err(error) = written {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(error);
    }
    // Claim the final name with create_new, so no other file can take it
    // between the check and the rename, then move the download onto it.
    let claimed = create_unique(&dir, |n| numbered(&safe, n)).await;
    let renamed = match claimed {
        Ok((path, reserved)) => {
            drop(reserved);
            match tokio::fs::rename(&part, &path).await {
                Ok(()) => Ok(path),
                Err(error) => {
                    let _ = tokio::fs::remove_file(&path).await;
                    Err(error)
                }
            }
        }
        Err(error) => Err(error),
    };
    if renamed.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    renamed.map_err(unsaved)
}

/// The name a file to upload goes by: its own, or "file" for a path that
/// ends in none.
pub(super) fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned())
}

/// Writing `name` to disk, as a failure says it.
fn saving(name: &str) -> Doing {
    Doing::Save {
        name: name.to_owned(),
    }
}

/// Opening `name` in its app, as a failure says it.
fn opening(name: &str) -> Doing {
    Doing::Open {
        name: name.to_owned(),
    }
}

fn downloads_dir() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_path_buf))
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}

/// Creates the first of `name(0)`, `name(1)`, … that does not exist yet in
/// `dir`. `create_new` makes taking the name and creating the file one
/// step, so two downloads of the same name cannot both get it.
async fn create_unique(
    dir: &std::path::Path,
    name: impl Fn(u32) -> String,
) -> std::io::Result<(std::path::PathBuf, tokio::fs::File)> {
    for n in 0..10_000 {
        let path = dir.join(name(n));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("too many files with that name"))
}

/// The longest file name written, in bytes: room under the 255 most file
/// systems allow for " (n)" and the temporary ".part".
const MAX_NAME: usize = 200;

/// An extension worth keeping when a name is cut short: short and real.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 16 => {
            (stem, Some(ext))
        }
        _ => (name, None),
    }
}

/// `name`, or for `n > 0` the same with " (n)" before its extension.
fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match split_extension(name) {
        (stem, Some(ext)) => format!("{stem} ({n}).{ext}"),
        (stem, None) => format!("{stem} ({n})"),
    }
}

/// Cuts `name` to at most `max` bytes on a character boundary, keeping
/// its extension.
fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, ext) = split_extension(name);
    let room = max.saturating_sub(ext.map_or(0, |ext| ext.len() + 1));
    let mut end = room.min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(['.', ' ']);
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    }
}

/// Names Windows keeps for devices, whatever the extension: `nul.txt`
/// opens the null device, not a file.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.len() == 4
        && upper[3..].chars().all(|c| matches!(c, '1'..='9')))
}

/// A file name that cannot climb out of the downloads folder, hide, or
/// break on any of the systems the app runs on: no separators or
/// characters Windows refuses, no control characters, no leading dots,
/// no trailing dots or spaces (Windows drops them), no device names, and
/// not too long.
/// Whether a file named `name` opens in a player or viewer, never as a
/// program: its extension is one of a known list of videos, sounds and
/// PDFs.
fn plays_safely(name: &str) -> bool {
    const PLAYABLE: &[&str] = &[
        "mp4", "m4v", "mov", "webm", "mkv", "avi", "mpg", "mpeg", "3gp", "ogv", "mp3", "m4a",
        "aac", "wav", "flac", "ogg", "oga", "opus", "weba", "pdf",
    ];
    match split_extension(name) {
        (_, Some(ext)) => PLAYABLE.iter().any(|p| p.eq_ignore_ascii_case(ext)),
        (_, None) => false,
    }
}

fn safe_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    let mut safe = truncate_name(trimmed, MAX_NAME);
    if safe.is_empty() {
        return "download".to_owned();
    }
    if is_reserved(&safe) {
        safe.insert(0, '_');
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_before_the_last_step_stops_the_upload() {
        let gate = UploadGate::default();
        assert!(gate.cancel());
        assert!(gate.cancel(), "a second cancel still says cancelled");
        assert!(!gate.finish());
    }

    #[test]
    fn a_cancel_during_the_last_step_is_refused() {
        let gate = UploadGate::default();
        assert!(gate.finish());
        assert!(!gate.cancel());
        assert!(gate.finish(), "the upload stays on its way");
    }

    #[test]
    fn clones_share_one_decision() {
        let worker = UploadGate::default();
        let task = worker.clone();
        assert!(task.finish());
        assert!(!worker.cancel());
    }

    #[test]
    fn only_players_open_files() {
        assert!(plays_safely("clip.MP4"));
        assert!(plays_safely("memo.m4a"));
        assert!(plays_safely("plan.pdf"));
        assert!(!plays_safely("clip.mp4.exe"));
        assert!(!plays_safely("run.sh"));
        assert!(!plays_safely("app.desktop"));
        assert!(!plays_safely("mp4"));
        // As saved: trailing dots go, and what is left must still play.
        assert!(!plays_safely(&safe_name("evil.exe.")));
    }

    #[test]
    fn download_names_stay_in_the_folder() {
        assert_eq!(safe_name("../../.bashrc"), "_.._.bashrc");
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name(".."), "download");
        assert_eq!(safe_name("C:\\Windows\\x.exe"), "C__Windows_x.exe");
    }

    #[test]
    fn download_names_work_on_windows() {
        assert_eq!(safe_name("a<b>c:d\"e|f?g*h.txt"), "a_b_c_d_e_f_g_h.txt");
        assert_eq!(safe_name("tab\there\u{7}.txt"), "tab_here_.txt");
        assert_eq!(safe_name("notes. . ."), "notes");
        assert_eq!(safe_name("  spaced  "), "spaced");
        for reserved in [
            "CON",
            "nul.txt",
            "Com1.log",
            "LPT9",
            "aux.tar.gz",
            "conout$",
        ] {
            assert_eq!(safe_name(reserved), format!("_{reserved}"), "{reserved}");
        }
        for fine in ["console.txt", "COM10", "COM0", "nullish", "lpt.txt"] {
            assert_eq!(safe_name(fine), fine, "{fine}");
        }
    }

    #[test]
    fn long_download_names_keep_their_extension() {
        let long = format!("{}.pdf", "a".repeat(300));
        let safe = safe_name(&long);
        assert_eq!(safe.len(), MAX_NAME);
        assert!(safe.ends_with("a.pdf"));
        // Cut on a character boundary, never inside one.
        let wide = format!("{}.txt", "é".repeat(150));
        let safe = safe_name(&wide);
        assert!(safe.len() <= MAX_NAME && safe.ends_with(".txt"), "{safe}");
        // No real extension: cut the whole name.
        assert_eq!(safe_name(&"b".repeat(300)).len(), MAX_NAME);
    }

    #[tokio::test]
    async fn a_taken_name_is_never_reused() {
        let dir = std::env::temp_dir().join(format!("noslacking-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let first = create_unique(&dir, |n| numbered("a.txt", n))
            .await
            .expect("first");
        let second = create_unique(&dir, |n| numbered("a.txt", n))
            .await
            .expect("second");
        assert_eq!(first.0, dir.join("a.txt"));
        assert_eq!(second.0, dir.join("a (1).txt"));
        drop((first, second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn taken_names_get_a_number_before_the_extension() {
        assert_eq!(numbered("report.pdf", 0), "report.pdf");
        assert_eq!(numbered("report.pdf", 2), "report (2).pdf");
        assert_eq!(numbered("archive.tar.gz", 1), "archive.tar (1).gz");
        assert_eq!(numbered("README", 3), "README (3)");
    }
}
