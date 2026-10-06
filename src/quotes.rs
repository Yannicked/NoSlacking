//! Links to Slack messages shown as quotes of those messages, the way
//! Slack shows a permalink under the message that posts it.
//!
//! Each link is quoted once. Slack usually unfurls such a link itself, as
//! an attachment with `is_msg_unfurl` that arrives as a
//! [`crate::model::Attachment`] with its `quote` set; that one is drawn as
//! the quote. A link Slack did not unfurl (one posted through the API, or
//! with unfurls off) into a signed-in workspace is quoted from the message
//! itself: from what is loaded if it is there, else fetched once by the
//! worker and kept in that workspace's [`Cache`]. Anything else stays the
//! plain link it is.

use std::collections::{HashMap, VecDeque};

use crate::app::WorkspaceState;
use crate::failure::Failure;
use crate::links::{Link, Target};
use crate::model::{ConversationKind, Message, Quote, Ts};

/// How many links of one message are quoted, so a message that is a list
/// of links does not become a wall of cards.
pub const MAX_QUOTES: usize = 3;
/// How many lines of the quoted text a card shows.
const EXCERPT_LINES: usize = 4;
/// About how many characters of the quoted text a card shows.
const EXCERPT_CHARS: usize = 320;
/// How many fetched messages a workspace keeps. Only the ones on screen
/// matter, so a few hundred is plenty.
const CACHE_LIMIT: usize = 256;

/// The message a link names, in a signed-in workspace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub team: String,
    pub channel: String,
    pub ts: Ts,
    /// The thread's parent, for a reply.
    pub thread: Option<Ts>,
}

/// The links to Slack messages in a message's text (in Slack's markup), in
/// the order they appear, each message once.
pub fn message_links(text: &str) -> Vec<Link> {
    // Most messages have none; skip the word-by-word read for them.
    if !text.contains("slack.com/archives/") {
        return Vec::new();
    }
    let mut links: Vec<Link> = Vec::new();
    let words = text
        .split(|c: char| c.is_whitespace() || c == '<' || c == '>')
        .map(|word| word.split('|').next().unwrap_or_default())
        .filter(|word| word.starts_with("https://"));
    for word in words {
        let Some(link) = crate::links::parse_web(&word.replace("&amp;", "&")) else {
            continue;
        };
        if target(&link).is_some() && !links.iter().any(|known| same_message(known, &link)) {
            links.push(link);
        }
    }
    links
}

/// The conversation and message a link names, if it names a message.
fn target(link: &Link) -> Option<(&str, &Ts)> {
    match &link.target {
        Target::Message { channel, ts, .. } => Some((channel, ts)),
        _ => None,
    }
}

/// Whether two links name the same message: the same conversation and
/// time, however each is written (with a thread or not, another case).
fn same_message(a: &Link, b: &Link) -> bool {
    target(a).is_some_and(|a| target(b) == Some(a))
}

/// The message a quote Slack unfurled is of: by its link, else by what
/// the unfurl says of it.
fn quoted(quote: &Quote) -> Option<(String, Ts)> {
    if let Some(link) = crate::links::parse_web(&quote.url.replace("&amp;", "&"))
        && let Target::Message { channel, ts, .. } = link.target
    {
        return Some((channel, ts));
    }
    Some((quote.channel.clone()?, quote.ts.clone()?))
}

/// The message links of `message` it needs a quote of its own for: those
/// Slack did not unfurl, at most [`MAX_QUOTES`] with the unfurls. A link
/// is never quoted twice.
pub fn own_quotes(message: &Message) -> Vec<Link> {
    let unfurled: Vec<(String, Ts)> = message
        .attachments
        .iter()
        .filter_map(|a| a.quote.as_ref())
        .filter_map(quoted)
        .collect();
    let room = MAX_QUOTES.saturating_sub(unfurled.len());
    message_links(&message.text)
        .into_iter()
        .filter(|link| {
            target(link)
                .is_some_and(|(channel, ts)| !unfurled.iter().any(|(c, t)| c == channel && t == ts))
        })
        .take(room)
        .collect()
}

/// Which signed-in workspace's message a link names, given each
/// workspace's id and web address. A link into a workspace not signed in
/// here, or to something other than a message, names none.
pub fn resolve<'a>(
    link: &Link,
    workspaces: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<Key> {
    let Target::Message {
        channel,
        ts,
        thread,
    } = &link.target
    else {
        return None;
    };
    let (team, _) = workspaces
        .into_iter()
        .find(|(team, domain)| link.is_for(team, domain))?;
    Some(Key {
        team: team.to_owned(),
        channel: channel.clone(),
        ts: ts.clone(),
        thread: thread.clone().filter(|parent| parent != ts),
    })
}

/// What became of fetching one message to quote.
#[derive(Clone, Debug, PartialEq)]
pub enum Fetched {
    /// Asked for; the answer has not come.
    Asked,
    Found(Box<Message>),
    /// Slack says it is not there, or not for you: deleted, or in a
    /// conversation you cannot read.
    Gone,
    /// The fetch did not work (the network, say). It is not asked again,
    /// so a link that cannot be fetched is not fetched on every frame;
    /// the plain link stays.
    Failed,
}

/// The messages fetched to quote in one workspace, by conversation and
/// time, with what was asked for so each is asked once.
#[derive(Debug, Default)]
pub struct Cache {
    entries: HashMap<(String, Ts), Fetched>,
    /// The keys oldest first, to forget the oldest past [`CACHE_LIMIT`].
    order: VecDeque<(String, Ts)>,
}

impl Cache {
    /// What is known of message `ts` of `channel`, if it was asked for.
    pub fn get(&self, channel: &str, ts: &Ts) -> Option<&Fetched> {
        self.entries.get(&(channel.to_owned(), ts.clone()))
    }

    /// Notes that message `ts` of `channel` is being fetched. Returns
    /// whether to fetch it: only the first time.
    pub fn ask(&mut self, channel: &str, ts: &Ts) -> bool {
        let key = (channel.to_owned(), ts.clone());
        if self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key.clone(), Fetched::Asked);
        self.order.push_back(key);
        while self.order.len() > CACHE_LIMIT {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
        true
    }

    /// The worker's answer for message `ts` of `channel`: the message,
    /// none (it is not there), or why it could not be read. An answer
    /// nobody asked for (or one forgotten since) is dropped.
    pub fn arrived(&mut self, channel: &str, ts: &Ts, result: Result<Option<Message>, Failure>) {
        let Some(entry) = self.entries.get_mut(&(channel.to_owned(), ts.clone())) else {
            return;
        };
        *entry = match result {
            Ok(Some(message)) => Fetched::Found(Box::new(message)),
            Ok(None) => Fetched::Gone,
            Err(failure) if is_unavailable(&failure) => Fetched::Gone,
            Err(_) => Fetched::Failed,
        };
    }
}

/// Whether a failure to read a message means it cannot be read at all
/// (gone, or not yours to see), rather than that reading it went wrong.
pub fn is_unavailable(failure: &Failure) -> bool {
    match failure {
        Failure::ConversationGone | Failure::NotInChannel => true,
        Failure::Slack(code) => matches!(
            code.as_str(),
            "thread_not_found" | "message_not_found" | "access_denied"
        ),
        _ => false,
    }
}

/// What a link to a message shows, from what its workspace has.
#[derive(Debug, PartialEq)]
pub enum Lookup<'a> {
    /// The message, loaded in a list or fetched to quote.
    Found(&'a Message),
    /// Deleted, or not yours to read.
    Gone,
    /// Being fetched, or fetching it failed: the plain link is all.
    Waiting,
    /// Neither loaded nor asked for: it is to be fetched.
    Unasked,
}

/// What `workspace` has of message `ts` of `channel`: a loaded copy first
/// (which stays up to date), else what was fetched to quote.
pub fn lookup<'a>(workspace: &'a WorkspaceState, channel: &str, ts: &Ts) -> Lookup<'a> {
    if let Some(message) = workspace.find_message(channel, ts) {
        return Lookup::Found(message);
    }
    match workspace.quotes.get(channel, ts) {
        Some(Fetched::Found(message)) => Lookup::Found(message),
        Some(Fetched::Gone) => Lookup::Gone,
        Some(Fetched::Asked | Fetched::Failed) => Lookup::Waiting,
        None => Lookup::Unasked,
    }
}

/// The quote of `message`, posted in `channel` of `workspace`, for the
/// link `url`: its author and conversation by the names this workspace
/// knows them by.
pub fn from_message(
    workspace: &WorkspaceState,
    url: &str,
    channel: &str,
    message: &Message,
) -> Quote {
    let mut text = message.text.clone();
    if text.trim().is_empty() {
        // A message of only files says which.
        text = message
            .files
            .iter()
            .map(|f| {
                crate::mrkdwn::escape(if f.title.is_empty() {
                    &f.name
                } else {
                    &f.title
                })
            })
            .collect::<Vec<_>>()
            .join(", ");
    }
    Quote {
        url: url.to_owned(),
        channel: Some(channel.to_owned()),
        channel_name: workspace.conversation(channel).map(|c| workspace.title(c)),
        ts: Some(message.ts.clone()),
        user: message.user.clone(),
        author: Some(workspace.author(message)),
        author_icon: workspace.author_icon(message).map(str::to_owned),
        text,
        unavailable: false,
    }
}

/// The card for a link to a message that is deleted or not yours to read.
pub fn unavailable(url: &str, channel: &str) -> Quote {
    Quote {
        url: url.to_owned(),
        channel: Some(channel.to_owned()),
        unavailable: true,
        ..Quote::default()
    }
}

/// Where a quoted message was posted, as its card says it: "#general", or
/// the name of a direct message.
pub fn place(workspace: &WorkspaceState, quote: &Quote) -> Option<String> {
    let known = quote
        .channel
        .as_deref()
        .and_then(|id| workspace.conversation(id));
    if let Some(conversation) = known {
        let title = workspace.title(conversation);
        return Some(match conversation.kind {
            ConversationKind::Channel | ConversationKind::Private => format!("#{title}"),
            ConversationKind::Direct | ConversationKind::Group => title,
        });
    }
    let name = quote.channel_name.as_deref()?.trim_start_matches('#');
    let direct = quote
        .channel
        .as_deref()
        .is_some_and(|id| id.starts_with('D'));
    Some(if direct {
        name.to_owned()
    } else {
        format!("#{name}")
    })
}

/// The start of a quoted message's text: its first few lines, cut short
/// with an ellipsis. A cut never splits a `<…>` link or mention, which
/// would show as markup.
pub fn excerpt(text: &str) -> String {
    let mut lines = text.trim().lines();
    let mut out: Vec<&str> = lines.by_ref().take(EXCERPT_LINES).collect();
    let mut cut = lines.next().is_some();
    let mut length = 0;
    for (index, line) in out.iter_mut().enumerate() {
        if length + line.len() > EXCERPT_CHARS {
            let mut end = (EXCERPT_CHARS - length.min(EXCERPT_CHARS)).min(line.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            let mut kept = &line[..end];
            if let Some(open) = kept.rfind('<')
                && !kept[open..].contains('>')
            {
                kept = &kept[..open];
            }
            *line = kept.trim_end();
            cut = true;
            let keep = index + 1;
            out.truncate(keep);
            break;
        }
        length += line.len();
    }
    // A code block cut open would swallow the ellipsis; close it.
    let mut text = out.join("\n");
    if cut {
        if text.matches("```").count() % 2 == 1 {
            text.push_str("\n```");
        }
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attachment, Conversation, Delivery, User, Workspace};

    fn message(text: &str) -> Message {
        Message {
            ts: Ts::new("1790000000.000100"),
            user: Some("U1".into()),
            username: None,
            bot_icon: None,
            bot_id: None,
            text: text.into(),
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
        }
    }

    fn workspace() -> WorkspaceState {
        let mut workspace = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
            sign_in: Default::default(),
        });
        workspace.users.insert(
            "U1".into(),
            User {
                id: "U1".into(),
                display_name: "Ana".into(),
                avatar: Some("https://avatars.example/ana.png".into()),
                ..User::default()
            },
        );
        workspace.conversations.push(Conversation {
            id: "C1".into(),
            name: "general".into(),
            kind: ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: None,
            latest: None,
            unread: 0,
            mentions: 0,
            external: false,
        });
        workspace
    }

    const LINK: &str = "https://acme.slack.com/archives/C1/p1700000000000100";

    fn unfurl(url: &str) -> Attachment {
        Attachment {
            quote: Some(Quote {
                url: url.into(),
                text: "quoted".into(),
                ..Quote::default()
            }),
            ..Attachment::default()
        }
    }

    #[test]
    fn links_to_messages_are_found_once_each() {
        let text = format!(
            "see <{LINK}> and <{LINK}?thread_ts=1699999999.000100&amp;cid=C1|this>, \
             <https://acme.slack.com/archives/C1> and <https://example.com/x>"
        );
        let links = message_links(&text);
        assert_eq!(links.len(), 1, "{links:?}");
        assert!(message_links("no links here").is_empty());
    }

    #[test]
    fn a_link_slack_unfurled_is_not_quoted_again() {
        let mut posted = message(&format!("Look: <{LINK}>"));
        // No unfurl: it needs a quote of its own.
        assert_eq!(own_quotes(&posted).len(), 1);
        // Slack's unfurl, however its link is written, covers it.
        posted.attachments.push(unfurl(&format!(
            "{LINK}?thread_ts=1700000000.000100&amp;cid=C1"
        )));
        assert!(own_quotes(&posted).is_empty());
        // An unfurl without a link is matched by what it says it quotes.
        posted.attachments = vec![Attachment {
            quote: Some(Quote {
                channel: Some("C1".into()),
                ts: Some(Ts::new("1700000000.000100")),
                ..Quote::default()
            }),
            ..Attachment::default()
        }];
        assert!(own_quotes(&posted).is_empty());
        // An unfurl of another message leaves this one to quote.
        posted.attachments = vec![unfurl(
            "https://acme.slack.com/archives/C1/p1700000000000200",
        )];
        assert_eq!(own_quotes(&posted).len(), 1);
        // Never more than a few cards, unfurls counted.
        let many: Vec<String> = (1..=5)
            .map(|i| format!("<https://acme.slack.com/archives/C1/p170000000000010{i}>"))
            .collect();
        let mut list = message(&many.join(" "));
        assert_eq!(own_quotes(&list).len(), MAX_QUOTES);
        list.attachments.push(unfurl(
            "https://acme.slack.com/archives/C9/p1700000000000900",
        ));
        assert_eq!(own_quotes(&list).len(), MAX_QUOTES - 1);
    }

    #[test]
    fn links_resolve_only_into_signed_in_workspaces() {
        let signed_in = [("T1", "acme"), ("T2", "other")];
        let link = crate::links::parse_web(&format!("{LINK}?thread_ts=1699999999.000100&cid=C1"))
            .expect("a link");
        assert_eq!(
            resolve(&link, signed_in),
            Some(Key {
                team: "T1".into(),
                channel: "C1".into(),
                ts: Ts::new("1700000000.000100"),
                thread: Some(Ts::new("1699999999.000100")),
            })
        );
        // A thread's parent is not a reply to itself.
        let parent = crate::links::parse_web(&format!("{LINK}?thread_ts=1700000000.000100&cid=C1"))
            .expect("a link");
        assert_eq!(resolve(&parent, signed_in).and_then(|k| k.thread), None);
        let elsewhere =
            crate::links::parse_web("https://unknown.slack.com/archives/C1/p1700000000000100")
                .expect("a link");
        assert_eq!(resolve(&elsewhere, signed_in), None);
        let channel =
            crate::links::parse_web("https://acme.slack.com/archives/C1").expect("a link");
        assert_eq!(resolve(&channel, signed_in), None);
        // By id, as a deep link names it.
        let deep = crate::links::parse_deep("slack://channel?team=T2&id=C5&message=1.000001")
            .expect("a deep link");
        assert_eq!(resolve(&deep, signed_in).map(|k| k.team), Some("T2".into()));
    }

    #[test]
    fn a_message_is_asked_for_once_and_a_failure_is_not_retried() {
        let mut cache = Cache::default();
        let ts = Ts::new("1700000000.000100");
        assert_eq!(cache.get("C1", &ts), None);
        assert!(cache.ask("C1", &ts));
        assert!(!cache.ask("C1", &ts), "asked twice");
        assert_eq!(cache.get("C1", &ts), Some(&Fetched::Asked));
        cache.arrived("C1", &ts, Err(Failure::Network("offline".into())));
        assert_eq!(cache.get("C1", &ts), Some(&Fetched::Failed));
        assert!(!cache.ask("C1", &ts), "a failure is not asked again");
        // Not there, or not yours: the card says so.
        let gone = Ts::new("1700000000.000200");
        assert!(cache.ask("C1", &gone));
        cache.arrived("C1", &gone, Err(Failure::NotInChannel));
        assert_eq!(cache.get("C1", &gone), Some(&Fetched::Gone));
        let deleted = Ts::new("1700000000.000300");
        assert!(cache.ask("C1", &deleted));
        cache.arrived("C1", &deleted, Ok(None));
        assert_eq!(cache.get("C1", &deleted), Some(&Fetched::Gone));
        // An answer nobody asked for is dropped.
        let stray = Ts::new("1700000000.000400");
        cache.arrived("C1", &stray, Ok(Some(message("x"))));
        assert_eq!(cache.get("C1", &stray), None);
        // It stays small, forgetting the oldest first.
        for i in 0..CACHE_LIMIT {
            cache.ask("C2", &Ts::new(format!("{i}.000001")));
        }
        assert_eq!(cache.get("C1", &ts), None);
        assert_eq!(cache.entries.len(), CACHE_LIMIT);
    }

    #[test]
    fn a_loaded_message_is_quoted_without_a_fetch() {
        let mut workspace = workspace();
        let ts = Ts::new("1790000000.000100");
        assert_eq!(lookup(&workspace, "C1", &ts), Lookup::Unasked);
        assert!(workspace.quotes.ask("C1", &ts));
        assert_eq!(lookup(&workspace, "C1", &ts), Lookup::Waiting);
        workspace
            .quotes
            .arrived("C1", &ts, Ok(Some(message("fetched"))));
        assert!(matches!(
            lookup(&workspace, "C1", &ts),
            Lookup::Found(m) if m.text == "fetched"
        ));
        let mut timeline = crate::model::Timeline::default();
        timeline.messages.push(message("loaded"));
        workspace.timelines.insert("C1".into(), timeline);
        assert!(matches!(
            lookup(&workspace, "C1", &ts),
            Lookup::Found(m) if m.text == "loaded"
        ));
    }

    #[test]
    fn a_quote_names_its_author_and_conversation() {
        let workspace = workspace();
        let quote = from_message(&workspace, LINK, "C1", &message("Hi <@U1> :wave:"));
        assert_eq!(quote.author.as_deref(), Some("Ana"));
        assert_eq!(
            quote.author_icon.as_deref(),
            Some("https://avatars.example/ana.png")
        );
        assert_eq!(quote.channel_name.as_deref(), Some("general"));
        assert_eq!(place(&workspace, &quote).as_deref(), Some("#general"));
        assert_eq!(quote.ts, Some(Ts::new("1790000000.000100")));
        // The text stays markup, for the rich renderer.
        assert_eq!(quote.text, "Hi <@U1> :wave:");
        assert!(!quote.unavailable);
        // A message of only a file names it.
        let mut file = message("");
        file.files.push(crate::model::File {
            name: "plan.pdf".into(),
            ..crate::model::File::default()
        });
        assert_eq!(from_message(&workspace, LINK, "C1", &file).text, "plan.pdf");
        // Somewhere not listed here goes by the name Slack gave.
        let other = Quote {
            channel: Some("C9".into()),
            channel_name: Some("random".into()),
            ..Quote::default()
        };
        assert_eq!(place(&workspace, &other).as_deref(), Some("#random"));
        assert!(unavailable(LINK, "C1").unavailable);
    }

    #[test]
    fn excerpts_keep_the_first_lines_whole() {
        assert_eq!(excerpt("short"), "short");
        assert_eq!(excerpt("1\n2\n3\n4\n5\n6"), "1\n2\n3\n4…");
        let long = format!("{} <@U1> end", "a".repeat(EXCERPT_CHARS - 3));
        let cut = excerpt(&long);
        assert!(cut.ends_with('…'), "{cut}");
        assert!(!cut.contains('<'), "a mention cut in half: {cut}");
        assert_eq!(excerpt("```\n1\n2\n3\n4\n5\n```"), "```\n1\n2\n3\n```…");
        // Never splits a character.
        let wide = "é".repeat(EXCERPT_CHARS);
        assert!(excerpt(&wide).ends_with('…'));
    }

    #[test]
    fn some_failures_mean_the_message_cannot_be_read() {
        assert!(is_unavailable(&Failure::ConversationGone));
        assert!(is_unavailable(&Failure::Slack("thread_not_found".into())));
        assert!(!is_unavailable(&Failure::RateLimited));
        assert!(!is_unavailable(&Failure::Network("x".into())));
    }
}
