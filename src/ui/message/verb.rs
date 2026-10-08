//! What you can do to a message, decided in one place for the three
//! places that offer it: the toolbar over the message, its right-click
//! menu, and the keys on a selected message.

use std::borrow::Cow;

use crate::app::WorkspaceState;
use crate::i18n::t;
use crate::model::{Ability, Action, Message};
use crate::theme::Icon;

/// A message and where it shows, which decide what can be done to it.
pub(crate) struct Subject<'a> {
    pub workspace: &'a WorkspaceState,
    pub channel: &'a str,
    pub message: &'a Message,
    /// Shown in the thread panel rather than the conversation.
    pub in_thread: bool,
}

impl Subject<'_> {
    /// Whether you wrote it.
    pub fn mine(&self) -> bool {
        self.message.user.as_deref() == Some(self.workspace.info.user_id.as_str())
    }
}

/// Something done to a message, offered by more than one of the toolbar,
/// the right-click menu and the selection's keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    React,
    Reply,
    Copy,
    Edit,
    Delete,
    MarkUnread,
    Share,
}

impl Verb {
    /// Whether `subject` offers it: what the service can do, and what you
    /// may do to this message.
    pub fn available(self, subject: &Subject<'_>) -> bool {
        let offers = |ability| subject.workspace.info.offers(ability);
        match self {
            Self::React => offers(Ability::Reactions),
            Self::Reply => !subject.in_thread && subject.workspace.threads_in(subject.channel),
            Self::Copy => true,
            Self::Edit => subject.mine() && offers(Ability::Edit),
            Self::Delete => subject.mine(),
            // Slack keeps a thread's read state apart from the
            // conversation's (`subscriptions.thread.mark`, for its own
            // apps only), so this is for the conversation's own list.
            Self::MarkUnread => !subject.in_thread && offers(Ability::MarkUnread),
            // A message still on its way has no link to share yet.
            Self::Share => !subject.message.ts.is_local() && offers(Ability::Share),
        }
    }

    /// What doing it to `subject` asks for.
    pub fn action(self, subject: &Subject<'_>) -> Action {
        let channel = subject.channel.to_owned();
        let ts = subject.message.ts.clone();
        match self {
            Self::React => Action::PickReaction { channel, ts },
            Self::Reply => Action::OpenThread {
                channel,
                ts: subject.message.thread_root().clone(),
            },
            Self::Copy => Action::Copy(super::plain_text(subject.workspace, subject.message)),
            Self::Edit => Action::StartEdit {
                channel,
                ts,
                in_thread: subject.in_thread,
            },
            Self::Delete => Action::AskDelete { channel, ts },
            Self::MarkUnread => Action::MarkUnread { channel, ts },
            Self::Share => Action::Share {
                channel,
                ts,
                thread: subject.message.thread_ts.clone(),
            },
        }
    }

    /// Its icon, for those on the toolbar.
    pub fn icon(self) -> Option<Icon> {
        match self {
            Self::React => Some(Icon::SmilePlus),
            Self::Reply => Some(Icon::MessageCircle),
            Self::Copy => Some(Icon::Copy),
            Self::Edit => Some(Icon::Pencil),
            Self::Delete => Some(Icon::Trash),
            Self::MarkUnread | Self::Share => None,
        }
    }

    /// Its line in a menu.
    pub fn label(self) -> Cow<'static, str> {
        match self {
            Self::React => t("Add reaction"),
            Self::Reply => t("Reply in thread"),
            Self::Copy => t("Copy text"),
            Self::Edit => t("Edit message"),
            Self::Delete => t("Delete message"),
            Self::MarkUnread => t("Mark unread"),
            Self::Share => t("Share message…"),
        }
    }

    /// Its tooltip on the toolbar, with the key that does it on a
    /// selected message.
    pub fn tooltip(self) -> Cow<'static, str> {
        match self {
            Self::React => t("Add reaction (R)"),
            Self::Reply => t("Reply in thread (T)"),
            Self::Copy => t("Copy text (C)"),
            Self::Edit => t("Edit message (E)"),
            Self::Delete => t("Delete message (Del)"),
            Self::MarkUnread | Self::Share => self.label(),
        }
    }
}
