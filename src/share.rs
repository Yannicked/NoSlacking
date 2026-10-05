//! Sharing a message to another conversation.
//!
//! Slack's own apps share through a method outside its public API. What
//! works for everyone is to post the message's permalink, after an
//! optional comment, to the other conversation: Slack unfurls a link to
//! one of its messages as a quote of it. So that is what this does,
//! through the usual send, which shows it at once and reconciles it.

use crate::app::WorkspaceState;
use crate::model::{Conversation, ConversationKind, Ts};

/// The "Share message" dialog: the message being shared, where to, and
/// what to say with it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Share {
    /// The conversation the message is in.
    pub channel: String,
    pub ts: Ts,
    /// The thread's parent, when the message is a reply.
    pub thread: Option<Ts>,
    /// What is typed in the picker's search field.
    pub query: String,
    /// The highlighted match, by its place in the list shown.
    pub selected: usize,
    /// The comment that goes before the link, as typed.
    pub comment: String,
}

impl Share {
    /// The dialog for message `ts` of `channel`; `thread` is its parent
    /// for a reply.
    pub fn new(channel: String, ts: Ts, thread: Option<Ts>) -> Self {
        Self {
            channel,
            ts,
            thread,
            ..Self::default()
        }
    }

    /// The link to the shared message in the workspace at `domain`, which
    /// for a reply opens its thread.
    pub fn link(&self, domain: &str) -> Option<String> {
        crate::links::permalink(domain, &self.channel, &self.ts, self.thread.as_ref())
    }
}

/// The text to post: the `comment` (already in Slack's markup), then the
/// `permalink` on its own line, or the permalink alone without a comment.
///
/// The link goes in angle brackets, escaped as Slack writes links: a
/// reply's link holds an `&`, which bare text must not.
pub fn text(comment: &str, permalink: &str) -> String {
    let link = format!("<{}>", crate::mrkdwn::escape(permalink));
    let comment = comment.trim();
    if comment.is_empty() {
        link
    } else {
        format!("{comment}\n{link}")
    }
}

/// How a conversation is named in "Shared to …": `#name` for a channel,
/// the person or people for a direct message.
pub fn place(workspace: &WorkspaceState, conversation: &Conversation) -> String {
    match conversation.kind {
        ConversationKind::Channel | ConversationKind::Private => {
            format!("#{}", conversation.name)
        }
        ConversationKind::Direct | ConversationKind::Group => workspace.title(conversation),
    }
}

/// The first `lines` lines of a message's text for the preview, an
/// ellipsis marking any cut.
pub fn preview(text: &str, lines: usize) -> String {
    let mut kept: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let cut = kept.len() > lines;
    kept.truncate(lines);
    let mut out = kept.join("\n");
    if cut {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_text_is_the_comment_then_the_link() {
        let link = "https://acme.slack.com/archives/C1/p1700000000000100";
        assert_eq!(
            text("Look at this", link),
            "Look at this\n<https://acme.slack.com/archives/C1/p1700000000000100>"
        );
    }

    #[test]
    fn without_a_comment_only_the_link_goes() {
        let link = "https://acme.slack.com/archives/C1/p1700000000000100";
        assert_eq!(text("", link), format!("<{link}>"));
        assert_eq!(text("  \n ", link), format!("<{link}>"));
    }

    #[test]
    fn a_reply_shares_its_own_link_with_its_thread() {
        let share = Share::new(
            "C1".into(),
            Ts::new("1700000500.000200"),
            Some(Ts::new("1700000000.000100")),
        );
        let link = share.link("acme").expect("a link");
        assert_eq!(
            link,
            "https://acme.slack.com/archives/C1/p1700000500000200?thread_ts=1700000000.000100&cid=C1"
        );
        // Slack reads `&amp;` in a link back as `&`.
        assert_eq!(
            text("", &link),
            "<https://acme.slack.com/archives/C1/p1700000500000200?thread_ts=1700000000.000100&amp;cid=C1>"
        );
        let target = crate::links::parse_web(&link).map(|l| l.target);
        assert_eq!(
            target,
            Some(crate::links::Target::Message {
                channel: "C1".into(),
                ts: Ts::new("1700000500.000200"),
                thread: Some(Ts::new("1700000000.000100")),
            })
        );
    }

    #[test]
    fn a_message_not_sent_yet_has_nothing_to_share() {
        let share = Share::new("C1".into(), Ts::new("local-1"), None);
        assert_eq!(share.link("acme"), None);
    }

    #[test]
    fn the_preview_keeps_the_first_lines() {
        assert_eq!(preview("one\n\ntwo\nthree\nfour", 3), "one\ntwo\nthree…");
        assert_eq!(preview("one\ntwo", 3), "one\ntwo");
    }
}
