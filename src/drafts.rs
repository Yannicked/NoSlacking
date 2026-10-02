//! Unsent messages kept across restarts, one per conversation and thread.
//!
//! They live in the state folder (`drafts.json`), never in the settings:
//! a draft can hold anything you were about to say, so the file is
//! readable by you alone where the platform allows, nothing here logs
//! what a draft says, and a signed-out workspace's drafts go with it.
//! Writes wait until typing pauses and happen off the interface thread.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// One saved draft.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    pub text: String,
    /// The picked mentions, as the composer keeps them: the text inserted
    /// and the markup it stands for.
    #[serde(default)]
    pub mentions: Vec<(String, String)>,
    /// "Also send to the channel", for a thread's draft.
    #[serde(default)]
    pub broadcast: bool,
}

/// Prints how long the draft is, never what it says.
impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved")
            .field("chars", &self.text.chars().count())
            .field("mentions", &self.mentions.len())
            .field("broadcast", &self.broadcast)
            .finish()
    }
}

/// Every draft by its key (`team/channel` or `team/channel/thread`).
pub type Drafts = BTreeMap<String, Saved>;

/// The drafts in `path`; none when there is no file or it cannot be read.
/// What went wrong is logged without the file's contents.
pub fn load(path: &Path) -> Drafts {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Drafts::new(),
        Err(error) => {
            log::warn!("could not read the drafts: {error}");
            return Drafts::new();
        }
    };
    match serde_json::from_slice::<Drafts>(&bytes) {
        Ok(drafts) => drafts
            .into_iter()
            .filter(|(_, d)| !d.text.trim().is_empty())
            .collect(),
        Err(error) => {
            // The error's position says nothing of the text.
            log::warn!(
                "ignoring unreadable drafts (line {}, column {})",
                error.line(),
                error.column()
            );
            Drafts::new()
        }
    }
}

/// A draft as the app holds it: key, text, mentions and broadcast.
pub type View<'a> = (&'a str, &'a str, &'a [(String, String)], bool);

/// Which drafts are worth keeping: those with something typed.
fn kept<'a>(drafts: impl Iterator<Item = View<'a>>) -> Vec<View<'a>> {
    let mut kept: Vec<View<'a>> = drafts.filter(|d| !d.1.trim().is_empty()).collect();
    kept.sort_by(|a, b| a.0.cmp(b.0));
    kept
}

/// A number that changes when any kept draft does, to notice typing
/// without copying every draft on every frame.
pub fn fingerprint<'a>(drafts: impl Iterator<Item = View<'a>>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for draft in kept(drafts) {
        draft.hash(&mut hasher);
    }
    hasher.finish()
}

/// The kept drafts, for saving.
pub fn snapshot<'a>(drafts: impl Iterator<Item = View<'a>>) -> Drafts {
    kept(drafts)
        .into_iter()
        .map(|(key, text, mentions, broadcast)| {
            (
                key.to_owned(),
                Saved {
                    text: text.to_owned(),
                    mentions: mentions.to_vec(),
                    broadcast,
                },
            )
        })
        .collect()
}

fn write(path: &Path, drafts: &Drafts) {
    let bytes = match serde_json::to_vec(drafts) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::warn!("could not encode the drafts: {error}");
            return;
        }
    };
    if let Err(error) = crate::paths::write_private(path, &bytes) {
        log::warn!("could not save the drafts: {error}");
    }
}

enum Job {
    Write(PathBuf, Drafts),
    Flush(mpsc::Sender<()>),
}

/// Writes the drafts on a thread of its own; of writes queued faster than
/// the disk takes them only the newest is written.
pub struct Writer {
    jobs: Option<mpsc::Sender<Job>>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("threaded", &self.jobs.is_some())
            .finish()
    }
}

impl Writer {
    /// Starts the writer thread; without one, writes happen in place.
    pub fn new() -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("drafts-writer".into())
            .spawn(move || {
                while let Ok(first) = queue.recv() {
                    let mut latest = None;
                    let mut flushes = Vec::new();
                    for job in std::iter::once(first).chain(queue.try_iter()) {
                        match job {
                            Job::Write(path, drafts) => latest = Some((path, drafts)),
                            Job::Flush(done) => flushes.push(done),
                        }
                    }
                    if let Some((path, drafts)) = latest {
                        write(&path, &drafts);
                    }
                    for done in flushes {
                        let _ = done.send(());
                    }
                }
            });
        match spawned {
            Ok(_) => Self { jobs: Some(jobs) },
            Err(error) => {
                log::warn!("no drafts thread, saving in place: {error}");
                Self { jobs: None }
            }
        }
    }

    /// Writes `drafts` to `path` soon.
    pub fn save(&self, drafts: Drafts, path: &Path) {
        let Some(jobs) = &self.jobs else {
            write(path, &drafts);
            return;
        };
        if let Err(mpsc::SendError(Job::Write(path, drafts))) =
            jobs.send(Job::Write(path.to_owned(), drafts))
        {
            write(&path, &drafts);
        }
    }

    /// Writes `drafts` and waits until they are on disk, for quitting.
    pub fn save_now(&self, drafts: Drafts, path: &Path) {
        self.save(drafts, path);
        let Some(jobs) = &self.jobs else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if jobs.send(Job::Flush(done)).is_ok() && wait.recv_timeout(Duration::from_secs(5)).is_err()
        {
            log::warn!("the drafts took too long to write");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TestDir;

    fn views(list: &[(String, Saved)]) -> Vec<View<'_>> {
        list.iter()
            .map(|(k, d)| {
                (
                    k.as_str(),
                    d.text.as_str(),
                    d.mentions.as_slice(),
                    d.broadcast,
                )
            })
            .collect()
    }

    fn draft(text: &str) -> Saved {
        Saved {
            text: text.into(),
            mentions: vec![("@Ann".into(), "<@U1>".into())],
            broadcast: false,
        }
    }

    #[test]
    fn drafts_survive_a_restart_and_empty_ones_are_dropped() {
        let dir = TestDir::new("drafts");
        let path = dir.0.join("drafts.json");
        let list = vec![
            ("T1/C1".to_owned(), draft("hi @Ann")),
            ("T1/C2".to_owned(), draft("   ")),
            ("T1/C1/1.0".to_owned(), draft("in the thread")),
        ];
        let writer = Writer::new();
        writer.save_now(snapshot(views(&list).into_iter()), &path);
        let loaded = load(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("T1/C1"), Some(&draft("hi @Ann")));
        assert_eq!(
            loaded.get("T1/C1/1.0").map(|d| d.text.as_str()),
            Some("in the thread")
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_you_can_read_the_drafts() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("drafts-private");
        let path = dir.0.join("drafts.json");
        Writer::new().save_now(Drafts::new(), &path);
        let mode = std::fs::metadata(&path)
            .expect("written")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_missing_or_damaged_file_is_no_drafts() {
        let dir = TestDir::new("drafts-bad");
        assert!(load(&dir.0.join("nothing.json")).is_empty());
        let path = dir.0.join("drafts.json");
        std::fs::write(&path, "{\"T1/C1\": {\"text\": 5").expect("write");
        assert!(load(&path).is_empty());
    }

    #[test]
    fn the_fingerprint_follows_what_is_kept() {
        let one = vec![("T1/C1".to_owned(), draft("a"))];
        let two = vec![("T1/C1".to_owned(), draft("ab"))];
        let with_empty = vec![
            ("T1/C1".to_owned(), draft("a")),
            ("T1/C9".to_owned(), draft("")),
        ];
        let print = |list: &[(String, Saved)]| fingerprint(views(list).into_iter());
        assert_ne!(print(&one), print(&two));
        assert_eq!(print(&one), print(&with_empty), "empty drafts do not count");
    }

    #[test]
    fn debug_output_never_shows_the_text() {
        let printed = format!("{:?}", draft("my secret plan"));
        assert!(!printed.contains("secret"), "{printed}");
    }
}
