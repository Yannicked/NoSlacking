//! Why something failed, as the interface gets told.
//!
//! The worker turns Slack's refusals and the network's trouble into a
//! [`Failure`]; the interface words it, in the reader's language, only when
//! it shows it. So the backend never writes a sentence, and the interface
//! can tell a sign-out from a rate limit from a missing permission. Nothing
//! here holds a Slack type.

use std::borrow::Cow;

use crate::i18n::fill;

/// Why a request, a sign-in or a file operation did not work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The token no longer works: the workspace needs signing in again.
    SignedOut,
    /// No token is saved for the workspace.
    NoSavedSignIn,
    /// The keyring could not be read, with its reason if it gave one.
    KeyringUnread(Option<Keyring>),
    /// The workspace is not signed in here, so nothing was asked.
    NotSignedIn,
    /// The sign-in lacks a permission (scope) the call needs.
    MissingPermission,
    /// The conversation was deleted, or was never visible to you.
    ConversationGone,
    NotInChannel,
    Archived,
    /// The message is longer than Slack takes.
    TooLong,
    /// The message is not yours, or its edit window has closed.
    CantEdit,
    CantDelete,
    /// The sign-in code was used already or has expired.
    LinkExpired,
    /// The OAuth redirect URL does not match the Slack app's.
    BadRedirect,
    /// The Slack app's client ID or secret is wrong.
    BadClient,
    /// A channel by that name exists already.
    NameTaken,
    /// Slack does not take the channel name (characters, punctuation).
    InvalidName,
    NameTooLong,
    CantLeaveGeneral,
    /// The workspace's admins do not allow it.
    Restricted,
    /// It cannot be done in this kind of conversation.
    WrongKind,
    /// Slack is rate limiting, and waiting it out did not help.
    RateLimited,
    /// The connection failed; the detail never carries a URL, which could
    /// hold a secret.
    Network(String),
    /// Slack answered with this HTTP status.
    Http(u16),
    /// Slack's answer could not be read; the detail is technical.
    Unexpected(String),
    /// Slack answered a post without the message.
    NoMessage,
    /// A file is over Slack's 1 GB limit.
    TooLarge,
    /// What was picked to upload is not a file.
    NotAFile,
    /// The system has no downloads folder (nor a home folder).
    NoDownloadsFolder,
    /// A file or folder is not there.
    FileNotFound,
    /// The system does not allow reading or writing a file or folder.
    FileDenied,
    /// The disk is full.
    DiskFull,
    /// Some other trouble with a file; the detail is the system's own and
    /// technical.
    Io(String),
    /// A file that could run code if opened, so only downloading is offered.
    NotOpenable,
    /// The sign-in by cookie needs the workspace's address.
    NoWorkspaceAddress,
    /// The browser could not be opened for the sign-in.
    NoBrowser,
    /// What was pasted is not a `slack://` sign-in link.
    NotASignInLink,
    /// The OAuth sign-in needs the Slack app's client ID first.
    NoClientId,
    /// What was pasted is not a Slack token.
    NotAToken,
    /// What was pasted is not a `d` session cookie (`xoxd-…`).
    NotACookie,
    /// The cookie signed in, but the workspace's page carried no session
    /// token.
    NoSessionToken,
    /// The cookie did not sign in to the workspace.
    CookieRefused,
    /// A bot token was pasted where a user token is needed.
    BotToken,
    /// Slack signed in but set no session cookie.
    NoSessionCookie,
    /// A sign-in link signed in to no workspace.
    NoWorkspace,
    /// The loopback listener for the OAuth redirect could not start, with
    /// the system's reason.
    NoListener(String),
    /// A sign-in redirect from another attempt.
    ForeignLink,
    /// The sign-in was cancelled in the browser.
    Cancelled,
    /// Slack refused the sign-in with this OAuth error code.
    Refused(String),
    /// Slack's redirect carried no authorization code.
    NoCode,
    /// A slash command only Slack's own runner takes, which needs a
    /// browser session's sign-in.
    NeedsSession,
    /// `/invite` with nobody named.
    NoInvitee,
    /// The manual proxy's URL cannot be used.
    BadProxy,
    /// Slack's error code, for codes not worded here: shown with its
    /// underscores as spaces, which mostly reads.
    Slack(String),
    /// Text already worded by someone else, the system (a file error) or
    /// Slack (a sign-in refusal), shown as it is.
    Other(String),
}

impl Failure {
    /// What a file operation's error means, by its kind; only an uncommon
    /// error keeps the system's own (English) words, as a detail.
    pub fn io(error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::FileNotFound,
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
                Self::FileDenied
            }
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => Self::DiskFull,
            _ => Self::Io(error.to_string()),
        }
    }

    /// The failure in words, in the interface's language: a clause, such as
    /// "the channel is archived", to go after "Could not …:", except for
    /// the sign-in's own failures, which are whole sentences.
    pub fn message(&self) -> String {
        self.worded(&crate::i18n::t)
    }

    /// [`Self::message`] as a sentence of its own, for where it stands
    /// alone: a capital first and a full stop at the end.
    pub fn sentence(&self) -> String {
        sentence(&self.message())
    }

    /// The words, through the translator `t`. Taking it as an argument lets
    /// the tests ask for every language without touching the process-wide
    /// one; it is called `t` so the catalog check finds these messages.
    fn worded(&self, t: &dyn Fn(&'static str) -> Cow<'static, str>) -> String {
        let text = match self {
            Self::SignedOut => t("the sign-in is no longer valid; sign in again"),
            Self::NoSavedSignIn => t("no sign-in is saved for this workspace"),
            Self::KeyringUnread(None) => t("the keyring could not be read"),
            Self::KeyringUnread(Some(error)) => {
                return fill(
                    &t("the keyring could not be read: {error}"),
                    &[("error", &error.worded(t))],
                );
            }
            Self::NotSignedIn => t("that workspace is not signed in"),
            Self::MissingPermission => {
                t("the Slack app lacks a permission; reinstall it from the manifest")
            }
            Self::ConversationGone => t("the conversation no longer exists"),
            Self::NotInChannel => t("you are not in that channel"),
            Self::Archived => t("the channel is archived"),
            Self::TooLong => t("the message is too long"),
            Self::CantEdit => t("that message can no longer be edited"),
            Self::CantDelete => t("you cannot delete that message"),
            Self::LinkExpired => t("the sign-in link expired; try again"),
            Self::BadRedirect => {
                t("the redirect URL does not match the Slack app; check its OAuth settings")
            }
            Self::BadClient => t("the client ID or secret is wrong"),
            Self::NameTaken => t("a channel by that name exists already"),
            Self::InvalidName => t("Slack does not take that name"),
            Self::NameTooLong => t("the name is too long"),
            Self::CantLeaveGeneral => t("nobody can leave the general channel"),
            Self::Restricted => t("the workspace does not allow you to do that"),
            Self::WrongKind => t("that cannot be done in this kind of conversation"),
            Self::RateLimited => t("Slack is rate limiting requests; try again shortly"),
            Self::Network(detail) => {
                return fill(&t("the network failed: {detail}"), &[("detail", detail)]);
            }
            Self::Http(status) => {
                return fill(&t("HTTP {status}"), &[("status", &status.to_string())]);
            }
            Self::Unexpected(detail) => {
                return fill(
                    &t("Slack answered unexpectedly: {detail}"),
                    &[("detail", detail)],
                );
            }
            Self::NoMessage => t("Slack did not return the message"),
            Self::TooLarge => t("it is larger than Slack's 1 GB limit"),
            Self::NotAFile => t("it is not a file"),
            Self::NoDownloadsFolder => t("there is no downloads folder"),
            Self::FileNotFound => t("the file is not there"),
            Self::FileDenied => t("the system does not allow access to the file"),
            Self::DiskFull => t("the disk is full"),
            Self::Io(detail) => {
                return fill(
                    &t("the file system failed: {detail}"),
                    &[("detail", detail)],
                );
            }
            Self::NotOpenable => t("it cannot be opened here; download it instead"),
            Self::NoWorkspaceAddress => {
                t("Enter your workspace's Slack address, such as acme.slack.com.")
            }
            Self::NoBrowser => {
                t("Could not open the browser. Open app.slack.com/ssb/signin yourself.")
            }
            Self::NotASignInLink => t(
                "That is not a Slack sign-in link. Copy the slack:// link the browser offers to open.",
            ),
            Self::NoClientId => t("Enter the Slack app's client ID first."),
            Self::NotAToken => {
                t("That does not look like a Slack token (it should start with xoxp-).")
            }
            Self::NotACookie => t(
                "That does not look like a session cookie (it should start with xoxd-). Copy the value of the cookie named d.",
            ),
            Self::NoSessionToken => t(
                "Slack signed in, but did not put a session token on the page for this workspace.",
            ),
            Self::CookieRefused => t(
                "The d cookie did not sign in. Copy a fresh one from a browser where this workspace is open.",
            ),
            Self::BotToken => {
                t("That is a bot token. NoSlacking needs the User OAuth Token (xoxp-).")
            }
            Self::NoSessionCookie => t("Slack signed in but set no session cookie."),
            Self::NoWorkspace => t("Slack signed in to no workspace with that link."),
            Self::NoListener(error) => {
                return fill(
                    &t("Could not listen for Slack's redirect: {error}. Is the port in use?"),
                    &[("error", error)],
                );
            }
            Self::ForeignLink => t("This sign-in link does not belong to the current attempt."),
            Self::Cancelled => t("Sign-in was cancelled."),
            Self::Refused(code) => {
                return fill(&t("Slack refused the sign-in: {code}"), &[("code", code)]);
            }
            Self::NoCode => t("Slack sent no authorization code."),
            Self::NeedsSession => t("it only works when you sign in with your browser"),
            Self::NoInvitee => t("name someone to invite with @"),
            Self::BadProxy => t("the proxy URL cannot be used; check it in Settings"),
            Self::Slack(code) => return code.replace('_', " "),
            Self::Other(text) => return text.clone(),
        };
        text.into_owned()
    }
}

/// Why the system keyring did not do what was asked. The worker hands this
/// over instead of the keyring's own words, so the interface can say it in
/// the reader's language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyring {
    /// The keyring is there but locked, and was not unlocked.
    Locked,
    /// The system has no keyring (no Secret Service running, say).
    Unavailable,
    /// A stored secret could not be decoded.
    Damaged,
}

impl Keyring {
    /// The trouble as a clause, such as "the keyring is locked", in the
    /// interface's language.
    pub fn message(self) -> String {
        self.worded(&crate::i18n::t)
    }

    /// The words, through the translator `t` (see [`Failure::worded`]).
    fn worded(self, t: &dyn Fn(&'static str) -> Cow<'static, str>) -> String {
        match self {
            Self::Locked => t("the keyring is locked"),
            Self::Unavailable => t("no keyring is available"),
            Self::Damaged => t("a stored secret is damaged"),
        }
        .into_owned()
    }
}

/// `text` with a capital first and a full stop, unless it ends in one.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    let mut out: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// What the worker was doing when a [`Failure`] stopped it, for the
/// failures that have no event of their own and show as a toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Doing {
    /// Checking a workspace's sign-in, by its name.
    Reach {
        workspace: String,
    },
    ListConversations,
    ChangeSidebar,
    LoadThread,
    /// Muting or unmuting conversations in Slack's preferences.
    ChangeMute,
    /// Telling Slack about a snooze, which holds here regardless.
    Snooze,
    Upload {
        name: String,
    },
    Download {
        name: String,
    },
    /// Writing a downloaded file to disk.
    Save {
        name: String,
    },
    /// Opening a file in the system's app for it.
    Open {
        name: String,
    },
    /// A Web API call made for its effect, by its method name.
    Call {
        method: String,
    },
    UseProxy,
    /// Registering `noslacking://` links for the OAuth redirect.
    RegisterLinks,
}

/// A [`Failure`] and what it stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub doing: Doing,
    pub failure: Failure,
}

impl Problem {
    /// The problem `failure` caused while `doing`.
    pub fn new(doing: Doing, failure: Failure) -> Self {
        Self { doing, failure }
    }

    /// Whether it shows as an error; a snooze Slack did not take still
    /// holds here, so that is only worth a notice.
    pub fn is_error(&self) -> bool {
        self.doing != Doing::Snooze
    }

    /// The problem in a sentence, in the interface's language.
    pub fn message(&self) -> String {
        self.worded(&crate::i18n::t)
    }

    /// The sentence, through the translator `t` (see [`Failure::worded`]).
    fn worded(&self, t: &dyn Fn(&'static str) -> Cow<'static, str>) -> String {
        let error = self.failure.worded(t);
        let error = error.as_str();
        match &self.doing {
            Doing::Reach { workspace } => fill(
                &t("Could not reach {workspace}: {error}"),
                &[("workspace", workspace), ("error", error)],
            ),
            Doing::ListConversations => fill(
                &t("Could not list conversations: {error}"),
                &[("error", error)],
            ),
            Doing::ChangeSidebar => fill(
                &t("Could not change the sidebar: {error}"),
                &[("error", error)],
            ),
            Doing::LoadThread => fill(
                &t("Could not load the thread: {error}"),
                &[("error", error)],
            ),
            Doing::ChangeMute => fill(
                &t("Could not change the mute in Slack: {error}"),
                &[("error", error)],
            ),
            Doing::Snooze => fill(
                &t("Slack did not take the snooze ({error}); it holds on this computer only."),
                &[("error", error)],
            ),
            Doing::Upload { name } => fill(
                &t("Could not upload {name}: {error}"),
                &[("name", name), ("error", error)],
            ),
            Doing::Download { name } => fill(
                &t("Could not download {name}: {error}"),
                &[("name", name), ("error", error)],
            ),
            Doing::Save { name } => fill(
                &t("Could not save {name}: {error}"),
                &[("name", name), ("error", error)],
            ),
            Doing::Open { name } => fill(
                &t("Could not open {name}: {error}"),
                &[("name", name), ("error", error)],
            ),
            Doing::Call { method } => fill(
                &t("{method} failed: {error}"),
                &[("method", method), ("error", error)],
            ),
            Doing::UseProxy => fill(&t("Could not use the proxy: {error}"), &[("error", error)]),
            Doing::RegisterLinks => fill(
                &t(
                    "Could not register noslacking:// links ({error}). Try the loopback redirect in Settings.",
                ),
                &[("error", error)],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    /// One of every failure, with sample details.
    fn every_failure() -> Vec<Failure> {
        vec![
            Failure::SignedOut,
            Failure::NoSavedSignIn,
            Failure::KeyringUnread(None),
            Failure::KeyringUnread(Some(Keyring::Locked)),
            Failure::KeyringUnread(Some(Keyring::Unavailable)),
            Failure::KeyringUnread(Some(Keyring::Damaged)),
            Failure::NotSignedIn,
            Failure::MissingPermission,
            Failure::ConversationGone,
            Failure::NotInChannel,
            Failure::Archived,
            Failure::TooLong,
            Failure::CantEdit,
            Failure::CantDelete,
            Failure::LinkExpired,
            Failure::BadRedirect,
            Failure::BadClient,
            Failure::NameTaken,
            Failure::InvalidName,
            Failure::NameTooLong,
            Failure::CantLeaveGeneral,
            Failure::Restricted,
            Failure::WrongKind,
            Failure::RateLimited,
            Failure::Network("timed out".into()),
            Failure::Http(502),
            Failure::Unexpected("missing field".into()),
            Failure::NoMessage,
            Failure::TooLarge,
            Failure::NotAFile,
            Failure::NoDownloadsFolder,
            Failure::FileNotFound,
            Failure::FileDenied,
            Failure::DiskFull,
            Failure::Io("broken pipe".into()),
            Failure::NotOpenable,
            Failure::NoWorkspaceAddress,
            Failure::NoBrowser,
            Failure::NotASignInLink,
            Failure::NoClientId,
            Failure::NotAToken,
            Failure::NotACookie,
            Failure::NoSessionToken,
            Failure::CookieRefused,
            Failure::BotToken,
            Failure::NoSessionCookie,
            Failure::NoWorkspace,
            Failure::NoListener("address in use".into()),
            Failure::ForeignLink,
            Failure::Cancelled,
            Failure::Refused("invalid_scope".into()),
            Failure::NoCode,
            Failure::NeedsSession,
            Failure::NoInvitee,
            Failure::BadProxy,
            Failure::Slack("some_new_code".into()),
            Failure::Other("disk full".into()),
        ]
    }

    /// One of everything a failure can stop.
    fn every_doing() -> Vec<Doing> {
        vec![
            Doing::Reach {
                workspace: "Acme".into(),
            },
            Doing::ListConversations,
            Doing::ChangeSidebar,
            Doing::LoadThread,
            Doing::ChangeMute,
            Doing::Snooze,
            Doing::Upload {
                name: "a.png".into(),
            },
            Doing::Download {
                name: "a.png".into(),
            },
            Doing::Save {
                name: "a.png".into(),
            },
            Doing::Open {
                name: "a.png".into(),
            },
            Doing::Call {
                method: "conversations.mark".into(),
            },
            Doing::UseProxy,
            Doing::RegisterLinks,
        ]
    }

    fn english(text: &'static str) -> Cow<'static, str> {
        fastframe_i18n::gettext(Locale::English, text)
    }

    fn dutch(text: &'static str) -> Cow<'static, str> {
        fastframe_i18n::gettext(Locale::Dutch, text)
    }

    #[test]
    fn every_failure_has_words_in_every_language() {
        for failure in every_failure() {
            let en = failure.worded(&english);
            let nl = failure.worded(&dutch);
            assert!(!en.is_empty(), "{failure:?}");
            assert!(!nl.is_empty(), "{failure:?}");
            assert!(
                !en.contains('{') && !nl.contains('{'),
                "{failure:?}: a hole"
            );
            // Only Slack's own codes, other people's text and the bare
            // status read the same in Dutch.
            if !matches!(
                failure,
                Failure::Slack(_) | Failure::Other(_) | Failure::Http(_)
            ) {
                assert_ne!(en, nl, "{failure:?} is not translated");
            }
        }
        for doing in every_doing() {
            // Text that is not translated, so only the sentence around it
            // can tell the languages apart.
            let problem = Problem::new(doing, Failure::Other("disk full".into()));
            let en = problem.worded(&english);
            let nl = problem.worded(&dutch);
            assert!(en.contains("disk full") && nl.contains("disk full"), "{nl}");
            assert_ne!(en, nl, "{problem:?} is not translated");
            assert!(
                !en.contains('{') && !nl.contains('{'),
                "{problem:?}: a hole"
            );
        }
    }

    #[test]
    fn details_and_codes_show_through() {
        assert_eq!(
            Failure::Slack("some_new_code".into()).worded(&english),
            "some new code"
        );
        assert_eq!(Failure::Http(502).worded(&english), "HTTP 502");
        assert_eq!(
            Problem::new(
                Doing::Upload {
                    name: "{name}.png".into()
                },
                Failure::TooLarge
            )
            .worded(&english),
            "Could not upload {name}.png: it is larger than Slack's 1 GB limit"
        );
    }

    #[test]
    fn file_errors_are_told_by_their_kind() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            Failure::io(&Error::from(ErrorKind::NotFound)),
            Failure::FileNotFound
        );
        assert_eq!(
            Failure::io(&Error::from(ErrorKind::PermissionDenied)),
            Failure::FileDenied
        );
        assert_eq!(
            Failure::io(&Error::from(ErrorKind::StorageFull)),
            Failure::DiskFull
        );
        assert!(matches!(
            Failure::io(&Error::other("odd")),
            Failure::Io(detail) if detail == "odd"
        ));
    }

    #[test]
    fn a_standalone_failure_reads_as_a_sentence() {
        assert_eq!(
            sentence("the channel is archived"),
            "The channel is archived."
        );
        assert_eq!(sentence("Sign-in was cancelled."), "Sign-in was cancelled.");
        assert_eq!(sentence(""), "");
    }

    #[test]
    fn only_a_snooze_is_mild() {
        assert!(!Problem::new(Doing::Snooze, Failure::RateLimited).is_error());
        assert!(Problem::new(Doing::LoadThread, Failure::RateLimited).is_error());
    }
}
