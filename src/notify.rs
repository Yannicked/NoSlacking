//! Desktop notifications: which new messages deserve one, what it says,
//! and handing it to the desktop's own notification service.
//!
//! The decision is pure ([`reason`]) so it can be tested without a desktop.
//! Showing one ([`Notifier`]) happens on a thread of its own: the D-Bus,
//! Notification Center and toast calls can each take a moment, and a click
//! on a notification comes back later, from yet another thread.

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, mpsc};

use crate::i18n::{t, tf};
use crate::model::{ConversationKind, Message};

/// How much of a conversation makes a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Every new message (thread replies only when they mention you).
    All,
    /// Mentions of you, `@here` and `@channel`, and your keywords. Direct
    /// messages always count as mentions.
    Mentions,
    Nothing,
}

impl Level {
    /// Every level, in the order menus list them.
    pub const ALL: [Level; 3] = [Level::All, Level::Mentions, Level::Nothing];

    /// What a menu calls the level.
    pub fn label(self) -> String {
        match self {
            Level::All => t("All new messages"),
            Level::Mentions => t("Mentions and keywords"),
            Level::Nothing => t("Nothing"),
        }
        .into_owned()
    }

    /// The level a conversation has when neither you nor Slack chose one:
    /// everything in direct messages, mentions elsewhere, as in Slack.
    pub fn default_for(kind: ConversationKind) -> Level {
        if kind.is_dm() {
            Level::All
        } else {
            Level::Mentions
        }
    }
}

/// Why a message notifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A direct or group message.
    Direct,
    /// It names you, or `@here`, `@channel` or `@everyone`.
    Mention,
    /// It contains one of your keywords.
    Keyword,
    /// The conversation notifies for every message.
    Everything,
}

/// Whether `text` (Slack's markup) names `me`: `<@U123>` or `<@U123|jane>`.
pub fn names_me(text: &str, me: &str) -> bool {
    if me.is_empty() {
        return false;
    }
    let tag = format!("<@{me}");
    text.match_indices(&tag)
        .any(|(at, _)| matches!(text[at + tag.len()..].chars().next(), Some('>' | '|')))
}

/// Whether `text` calls on everyone present: `@here`, `@channel` or
/// `@everyone`. Slack only sends these to members, so the message reaching
/// you means you are one.
fn calls_everyone(text: &str) -> bool {
    ["<!here", "<!channel", "<!everyone"].iter().any(|tag| {
        text.match_indices(tag)
            .any(|(at, _)| matches!(text[at + tag.len()..].chars().next(), Some('>' | '|')))
    })
}

/// Whether `plain` (text without markup) has one of `keywords` as a whole
/// word, ignoring case: "deploy" matches "Deploy done" but not "deployed".
pub fn has_keyword(plain: &str, keywords: &[String]) -> bool {
    let haystack = plain.to_lowercase();
    keywords.iter().any(|keyword| {
        let needle = keyword.trim().to_lowercase();
        !needle.is_empty()
            && haystack.match_indices(&needle).any(|(at, _)| {
                let before = haystack[..at].chars().next_back();
                let after = haystack[at + needle.len()..].chars().next();
                !before.is_some_and(char::is_alphanumeric)
                    && !after.is_some_and(char::is_alphanumeric)
            })
    })
}

/// Whether, and why, a new message in a conversation of `kind` deserves a
/// notification at `level`, for you (`me`). `plain` is its text without
/// markup, for the keywords.
///
/// This only weighs the message. Whether you are looking at the
/// conversation, have notifications paused or the conversation muted is
/// for the caller.
pub fn reason(
    kind: ConversationKind,
    message: &Message,
    plain: &str,
    me: &str,
    level: Level,
    keywords: &[String],
) -> Option<Reason> {
    if level == Level::Nothing
        || message.user.as_deref() == Some(me)
        || message.is_system()
        || message.ts.is_local()
    {
        return None;
    }
    if kind.is_dm() {
        return Some(Reason::Direct);
    }
    if names_me(&message.text, me) || calls_everyone(&message.text) {
        return Some(Reason::Mention);
    }
    if has_keyword(plain, keywords) {
        return Some(Reason::Keyword);
    }
    // A reply notifies only those it names: in Slack, the people following
    // the thread, which this client does not track.
    if level == Level::All && message.in_channel() {
        return Some(Reason::Everything);
    }
    None
}

/// The longest body a notification gets, in characters. Desktops cut
/// longer ones anyway, some badly.
const BODY_LIMIT: usize = 240;

/// A notification's title and body: who wrote, where, and what.
///
/// `place` is the conversation's name as the sidebar shows it, with its
/// `#` for a channel.
pub fn compose(kind: ConversationKind, place: &str, author: &str, plain: &str) -> (String, String) {
    let text = shorten(plain.trim(), BODY_LIMIT);
    match kind {
        ConversationKind::Direct => (author.to_owned(), text),
        ConversationKind::Group => (
            place.to_owned(),
            tf("{author}: {text}", &[("author", author), ("text", &text)]),
        ),
        ConversationKind::Channel | ConversationKind::Private => (
            tf(
                "{author} in {place}",
                &[("author", author), ("place", place)],
            ),
            text,
        ),
    }
}

/// `text` cut to `limit` characters with an ellipsis.
fn shorten(text: &str, limit: usize) -> String {
    crate::text::ellipsize(text, limit).0.into_owned()
}

/// A notification to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    /// Where a click leads: the workspace and the conversation.
    pub team: String,
    pub channel: String,
    pub title: String,
    pub body: String,
    pub sound: bool,
    /// Where a click leads instead, outside NoSlacking: a huddle to join.
    pub link: Option<String>,
}

/// A notification that was clicked: the workspace and conversation to
/// open, or the link it carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clicked {
    pub team: String,
    pub channel: String,
    pub link: Option<String>,
}

/// What the notification thread is asked to do.
enum Job {
    Show(Note),
    /// Take away the huddle invitations shown for a conversation.
    Withdraw {
        team: String,
        channel: String,
    },
}

/// The most invitations remembered for taking away later; older ones
/// have long stopped ringing.
const WITHDRAWABLE: usize = 16;

/// The huddle invitations shown that the desktop could take away again,
/// by workspace, conversation and the desktop's id for them.
#[derive(Debug, Default)]
struct Shown(Vec<(String, String, u32)>);

impl Shown {
    /// Remembers an invitation shown as `id`.
    fn add(&mut self, team: &str, channel: &str, id: u32) {
        if self.0.len() >= WITHDRAWABLE {
            self.0.remove(0);
        }
        self.0.push((team.to_owned(), channel.to_owned(), id));
    }

    /// Forgets the invitations for `channel` and returns their ids.
    fn take(&mut self, team: &str, channel: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        self.0.retain(|(t, c, id)| {
            let this = t == team && c == channel;
            if this {
                ids.push(*id);
            }
            !this
        });
        ids
    }
}

/// Shows notifications on a thread of its own and reports clicks.
pub struct Notifier {
    notes: mpsc::Sender<Job>,
    clicks: mpsc::Receiver<Clicked>,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier").finish_non_exhaustive()
    }
}

impl Notifier {
    /// Starts the notification thread. `wake` is called after a click is
    /// queued, from whichever thread saw it. `None` when the thread could
    /// not start or the platform has no notifications.
    pub fn spawn(wake: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        if !platform::AVAILABLE {
            return None;
        }
        let (notes, queue) = mpsc::channel::<Job>();
        let (clicked, clicks) = mpsc::channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let spawned = std::thread::Builder::new()
            .name("notifications".into())
            .spawn(move || {
                platform::prepare();
                let waiting = Arc::new(AtomicUsize::new(0));
                let mut shown = Shown::default();
                for job in queue {
                    match job {
                        Job::Show(note) => {
                            let id = platform::show(&note, &clicked, &wake, &waiting);
                            if let (Some(id), Some(_)) = (id, &note.link) {
                                shown.add(&note.team, &note.channel, id);
                            }
                        }
                        Job::Withdraw { team, channel } => {
                            for id in shown.take(&team, &channel) {
                                platform::close(id);
                            }
                        }
                    }
                }
            });
        match spawned {
            Ok(_) => Some(Self { notes, clicks }),
            Err(error) => {
                log::warn!("no notifications: the thread did not start: {error}");
                None
            }
        }
    }

    /// Shows `note` soon.
    pub fn show(&self, note: Note) {
        if self.notes.send(Job::Show(note)).is_err() {
            log::warn!("the notification thread has stopped");
        }
    }

    /// Takes away the huddle invitations shown for `channel` of `team`,
    /// once the call stopped ringing: the caller hung up, or someone else
    /// answered. Only the freedesktop service (Linux) can be asked; on
    /// other desktops they stay until dismissed.
    pub fn withdraw(&self, team: &str, channel: &str) {
        let job = Job::Withdraw {
            team: team.to_owned(),
            channel: channel.to_owned(),
        };
        if self.notes.send(job).is_err() {
            log::warn!("the notification thread has stopped");
        }
    }

    /// The notifications clicked since the last call, oldest first.
    pub fn clicks(&self) -> Vec<Clicked> {
        self.clicks.try_iter().collect()
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "macos",
    windows
))]
mod platform {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    use notify_rust::{Notification, NotificationResponse};

    use super::{Clicked, Note};
    // Linux names the desktop entry and macOS the sender by it.
    #[cfg(not(windows))]
    use crate::paths::APP_ID;

    pub const AVAILABLE: bool = true;

    /// The most notifications waited on for a click at once. Each waits on a
    /// thread of its own until it is clicked or closed, and some desktops keep
    /// them for days; past this many, new ones still show, but a click only
    /// brings the desktop's usual behaviour.
    const MAX_WAITING: usize = 24;

    /// Counts a waiting notification for as long as it lives.
    struct Waiting(Arc<AtomicUsize>);

    impl Waiting {
        /// A place among the waiting notifications, if one is free.
        fn take(count: &Arc<AtomicUsize>) -> Option<Self> {
            count
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    (n < MAX_WAITING).then_some(n + 1)
                })
                .ok()
                .map(|_| Self(count.clone()))
        }
    }

    impl Drop for Waiting {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Tells macOS whose notifications these are, so they carry the app's
    /// name and icon and a click brings it forward. Outside the app bundle
    /// there is no such app, and macOS keeps its own default.
    pub fn prepare() {
        #[cfg(target_os = "macos")]
        if let Err(error) = notify_rust::set_application(APP_ID) {
            log::debug!("notifications keep the default sender: {error}");
        }
    }

    /// Whether the notification server reads the body as markup. Asked
    /// once, on the notification thread; a server that cannot be asked is
    /// taken to read markup, as most do, since escaping text it shows
    /// literally is far less harm than markup it would follow.
    #[cfg(not(any(target_os = "macos", windows)))]
    fn reads_markup() -> bool {
        static MARKUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *MARKUP.get_or_init(|| {
            notify_rust::get_capabilities()
                .map_or(true, |caps| caps.iter().any(|cap| cap == "body-markup"))
        })
    }

    /// Shows `note`, and returns the desktop's id for it where it can be
    /// taken away again ([`close`]).
    pub fn show(
        note: &Note,
        clicked: &mpsc::Sender<Clicked>,
        wake: &Arc<dyn Fn() + Send + Sync>,
        waiting: &Arc<AtomicUsize>,
    ) -> Option<u32> {
        // Freedesktop servers that announce `body-markup` read the body as
        // markup, so plain text has to be escaped: `a < b && c` would break,
        // and a message could pass off a disguised link. The summary is
        // always plain text.
        #[cfg(not(any(target_os = "macos", windows)))]
        let body = if reads_markup() {
            crate::mrkdwn::escape(&note.body)
        } else {
            note.body.clone()
        };
        #[cfg(any(target_os = "macos", windows))]
        let body = note.body.clone();
        let mut notification = Notification::new();
        notification
            .appname("NoSlacking")
            .summary(&note.title)
            .body(&body);
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            use notify_rust::Hint;
            notification
                .icon(APP_ID)
                .hint(Hint::DesktopEntry(APP_ID.to_owned()))
                .hint(Hint::Category("im.received".to_owned()))
                // "default" is the click on the notification itself; the
                // label shows only where a desktop draws it as a button.
                .action(
                    "default",
                    &if note.link.is_some() {
                        crate::i18n::t("Join")
                    } else {
                        crate::i18n::t("Open")
                    },
                );
            if note.sound {
                notification.hint(Hint::SoundName("message-new-instant".to_owned()));
            } else {
                notification.hint(Hint::SuppressSound(true));
            }
        }
        // Windows toasts and macOS banners are silent unless named a sound.
        #[cfg(any(target_os = "macos", windows))]
        if note.sound {
            notification.sound_name("Default");
        }
        let handle = match notification.show() {
            Ok(handle) => handle,
            Err(error) => {
                log::warn!("could not show a notification: {error}");
                return None;
            }
        };
        #[cfg(target_os = "linux")]
        let id = Some(handle.id());
        #[cfg(not(target_os = "linux"))]
        let id = None;
        // Without a free place the handle is dropped: the notification
        // still shows (macOS sends it on drop), but its click is not heard.
        let Some(place) = Waiting::take(waiting) else {
            return id;
        };
        let target = Clicked {
            team: note.team.clone(),
            channel: note.channel.clone(),
            link: note.link.clone(),
        };
        let clicked = clicked.clone();
        let wake = wake.clone();
        let spawned = std::thread::Builder::new()
            .name("notification-click".into())
            .spawn(move || {
                let _place = place;
                let heard = handle.wait_for_response(|response: &NotificationResponse| {
                    let opened = match response {
                        NotificationResponse::Default => true,
                        NotificationResponse::Action(action) => action == "default",
                        _ => false,
                    };
                    if opened && clicked.send(target).is_ok() {
                        wake();
                    }
                });
                if let Err(error) = heard {
                    log::debug!("lost track of a notification: {error}");
                }
            });
        if let Err(error) = spawned {
            log::debug!("a notification's click will not be heard: {error}");
        }
        id
    }

    /// Takes away the notification the desktop knows as `id`
    /// (freedesktop's CloseNotification). One already gone is no harm.
    #[cfg(target_os = "linux")]
    pub fn close(id: u32) {
        let closed = zbus::blocking::Connection::session().and_then(|bus| {
            bus.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "CloseNotification",
                &(id,),
            )
            .map(|_| ())
        });
        if let Err(error) = closed {
            log::debug!("could not take a notification away: {error}");
        }
    }

    /// Other desktops are not asked: notify-rust takes nothing away there
    /// by id, and no id is kept for them.
    #[cfg(not(target_os = "linux"))]
    pub fn close(_id: u32) {}
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "macos",
    windows
)))]
mod platform {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, mpsc};

    use super::{Clicked, Note};

    pub const AVAILABLE: bool = false;

    pub fn prepare() {}

    pub fn show(
        _note: &Note,
        _clicked: &mpsc::Sender<Clicked>,
        _wake: &Arc<dyn Fn() + Send + Sync>,
        _waiting: &Arc<AtomicUsize>,
    ) -> Option<u32> {
        None
    }

    pub fn close(_id: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitations_shown_are_taken_away_by_conversation() {
        let mut shown = Shown::default();
        shown.add("T1", "C1", 7);
        shown.add("T1", "C2", 8);
        shown.add("T2", "C1", 9);
        shown.add("T1", "C1", 10);
        assert_eq!(shown.take("T1", "C1"), [7, 10]);
        assert!(shown.take("T1", "C1").is_empty());
        for id in 0..40 {
            shown.add("T3", "C3", id);
        }
        assert_eq!(shown.0.len(), WITHDRAWABLE);
        assert_eq!(shown.take("T3", "C3").first(), Some(&24));
    }
    use crate::model::{Delivery, Ts};

    fn message(user: &str, text: &str) -> Message {
        Message {
            ts: Ts::new("1700000000.000100"),
            user: Some(user.to_owned()),
            username: None,
            bot_icon: None,
            bot_id: None,
            text: text.to_owned(),
            thread_ts: None,
            reply_count: 0,
            replies_known: false,
            reply_users: Vec::new(),
            latest_reply: None,
            reactions: Vec::new(),
            files: Vec::new(),
            attachments: Vec::new(),
            blocks: Vec::new(),
            edited: false,
            subtype: None,
            delivery: Delivery::Sent,
            broadcast: false,
            pinned: false,
            client_msg_id: None,
            subscribed: None,
        }
    }

    fn why(kind: ConversationKind, m: &Message, level: Level) -> Option<Reason> {
        reason(kind, m, &m.text, "U1", level, &["deploy".to_owned()])
    }

    #[test]
    fn notification_text_is_never_read_as_markup() {
        assert_eq!(
            crate::mrkdwn::escape("a < b && c > d"),
            "a &lt; b &amp;&amp; c &gt; d"
        );
        assert_eq!(
            crate::mrkdwn::escape("<a href=\"https://evil\">bank</a>"),
            "&lt;a href=\"https://evil\"&gt;bank&lt;/a&gt;"
        );
        assert_eq!(
            crate::mrkdwn::escape("&amp; stays as typed"),
            "&amp;amp; stays as typed"
        );
        assert_eq!(crate::mrkdwn::escape("plain words 👍"), "plain words 👍");
    }

    #[test]
    fn mentions_name_you_exactly() {
        assert!(names_me("hi <@U1>", "U1"));
        assert!(names_me("hi <@U1|jane>", "U1"));
        assert!(!names_me("hi <@U12>", "U1"));
        assert!(!names_me("hi U1", "U1"));
        assert!(calls_everyone("<!here> lunch"));
        assert!(calls_everyone("<!channel|@channel> lunch"));
        assert!(!calls_everyone("<!subteam^S1|@design> lunch"));
        assert!(!calls_everyone("<!hereford>"));
    }

    #[test]
    fn keywords_match_whole_words_in_any_case() {
        let words = ["deploy".to_owned(), "on call".to_owned()];
        assert!(has_keyword("Deploy done", &words));
        assert!(has_keyword("who is on call?", &words));
        assert!(!has_keyword("deployed it", &words));
        assert!(!has_keyword("redeploy", &words));
        assert!(!has_keyword("anything", &[" ".to_owned()]));
    }

    #[test]
    fn direct_messages_notify_unless_silenced() {
        let m = message("U2", "hey");
        assert_eq!(
            why(ConversationKind::Direct, &m, Level::All),
            Some(Reason::Direct)
        );
        assert_eq!(
            why(ConversationKind::Group, &m, Level::Mentions),
            Some(Reason::Direct)
        );
        assert_eq!(why(ConversationKind::Direct, &m, Level::Nothing), None);
    }

    #[test]
    fn channels_notify_by_level() {
        let plain = message("U2", "hello all");
        assert_eq!(
            why(ConversationKind::Channel, &plain, Level::Mentions),
            None
        );
        assert_eq!(
            why(ConversationKind::Channel, &plain, Level::All),
            Some(Reason::Everything)
        );
        let mention = message("U2", "<@U1> look");
        assert_eq!(
            why(ConversationKind::Private, &mention, Level::Mentions),
            Some(Reason::Mention)
        );
        let here = message("U2", "<!here> standup");
        assert_eq!(
            why(ConversationKind::Channel, &here, Level::Mentions),
            Some(Reason::Mention)
        );
        let keyword = message("U2", "Deploy at five");
        assert_eq!(
            why(ConversationKind::Channel, &keyword, Level::Mentions),
            Some(Reason::Keyword)
        );
        assert_eq!(
            why(ConversationKind::Channel, &mention, Level::Nothing),
            None
        );
    }

    #[test]
    fn your_own_and_quiet_messages_never_notify() {
        let mine = message("U1", "<!here> from me");
        assert_eq!(why(ConversationKind::Direct, &mine, Level::All), None);
        let mut joined = message("U2", "<@U2> has joined");
        joined.subtype = Some("channel_join".into());
        assert_eq!(why(ConversationKind::Channel, &joined, Level::All), None);
    }

    #[test]
    fn replies_notify_only_when_they_name_you() {
        let mut reply = message("U2", "agreed");
        reply.thread_ts = Some(Ts::new("1600000000.000100"));
        assert_eq!(why(ConversationKind::Channel, &reply, Level::All), None);
        reply.text = "<@U1> agreed".into();
        assert_eq!(
            why(ConversationKind::Channel, &reply, Level::All),
            Some(Reason::Mention)
        );
        reply.text = "agreed".into();
        reply.broadcast = true;
        assert_eq!(
            why(ConversationKind::Channel, &reply, Level::All),
            Some(Reason::Everything)
        );
    }

    #[test]
    fn notes_say_who_and_where() {
        let (title, body) = compose(ConversationKind::Direct, "jane", "Jane", "hi");
        assert_eq!((title.as_str(), body.as_str()), ("Jane", "hi"));
        let (title, body) = compose(ConversationKind::Channel, "#general", "Jane", " hi ");
        assert_eq!((title.as_str(), body.as_str()), ("Jane in #general", "hi"));
        let (title, body) = compose(ConversationKind::Group, "Jane, Bob", "Jane", "hi");
        assert_eq!((title.as_str(), body.as_str()), ("Jane, Bob", "Jane: hi"));
    }

    #[test]
    fn long_bodies_are_cut_with_an_ellipsis() {
        assert_eq!(shorten("short", 10), "short");
        assert_eq!(shorten("one two three", 8), "one two…");
        assert_eq!(shorten("ééééé", 3), "éé…");
    }
}
