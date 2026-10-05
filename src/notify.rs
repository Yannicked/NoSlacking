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
pub fn calls_everyone(text: &str) -> bool {
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
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
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
}

/// A notification that was clicked: the workspace and conversation to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clicked {
    pub team: String,
    pub channel: String,
}

/// Shows notifications on a thread of its own and reports clicks.
pub struct Notifier {
    notes: mpsc::Sender<Note>,
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
        let (notes, queue) = mpsc::channel::<Note>();
        let (clicked, clicks) = mpsc::channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let spawned = std::thread::Builder::new()
            .name("notifications".into())
            .spawn(move || {
                platform::prepare();
                let waiting = Arc::new(AtomicUsize::new(0));
                for note in queue {
                    platform::show(&note, &clicked, &wake, &waiting);
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
        if self.notes.send(note).is_err() {
            log::warn!("the notification thread has stopped");
        }
    }

    /// The notifications clicked since the last call, oldest first.
    pub fn clicks(&self) -> Vec<Clicked> {
        self.clicks.try_iter().collect()
    }
}

/// `text` safe as freedesktop notification markup, where `&`, `<` and
/// `>` would otherwise start an entity or a tag.
#[cfg(any(test, not(any(target_os = "macos", windows))))]
fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
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

    pub fn show(
        note: &Note,
        clicked: &mpsc::Sender<Clicked>,
        wake: &Arc<dyn Fn() + Send + Sync>,
        waiting: &Arc<AtomicUsize>,
    ) {
        // Freedesktop servers that announce `body-markup` read the body as
        // markup, so plain text has to be escaped: `a < b && c` would break,
        // and a message could pass off a disguised link. The summary is
        // always plain text.
        #[cfg(not(any(target_os = "macos", windows)))]
        let body = if reads_markup() {
            super::escape_markup(&note.body)
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
                .action("default", &crate::i18n::t("Open"));
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
                return;
            }
        };
        // Without a free place the handle is dropped: the notification
        // still shows (macOS sends it on drop), but its click is not heard.
        let Some(place) = Waiting::take(waiting) else {
            return;
        };
        let target = Clicked {
            team: note.team.clone(),
            channel: note.channel.clone(),
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
    }
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
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    fn why(kind: ConversationKind, m: &Message, level: Level) -> Option<Reason> {
        reason(kind, m, &m.text, "U1", level, &["deploy".to_owned()])
    }

    #[test]
    fn notification_text_is_never_read_as_markup() {
        assert_eq!(
            escape_markup("a < b && c > d"),
            "a &lt; b &amp;&amp; c &gt; d"
        );
        assert_eq!(
            escape_markup("<a href=\"https://evil\">bank</a>"),
            "&lt;a href=\"https://evil\"&gt;bank&lt;/a&gt;"
        );
        assert_eq!(
            escape_markup("&amp; stays as typed"),
            "&amp;amp; stays as typed"
        );
        assert_eq!(escape_markup("plain words 👍"), "plain words 👍");
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
