//! Links into Slack: message permalinks (`https://acme.slack.com/archives/
//! C123/p1700000000123456`) to copy and to follow inside the app, and the
//! `slack://` deep links the desktop hands to whichever app handles them.

use crate::model::Ts;

/// Where a link into Slack points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The workspace itself.
    Workspace,
    Conversation(String),
    /// One message; `thread` is its thread's parent when it is a reply.
    Message {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
    /// A person, by user id: their direct message, or their card.
    User(String),
}

/// A link into Slack, and which workspace it is for: by id from a deep
/// link, by its web address from a permalink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub team: Option<String>,
    /// The workspace's Slack address, without `.slack.com` (`acme`, or
    /// `acme.enterprise` for an Enterprise Grid one).
    pub domain: Option<String>,
    pub target: Target,
}

impl Link {
    /// Whether the link is for the workspace with this id and address.
    pub fn is_for(&self, team: &str, domain: &str) -> bool {
        if let Some(id) = &self.team {
            return id == team;
        }
        // Grid workspaces also answer under their organisation's name.
        self.domain.as_deref().is_some_and(|own| {
            !domain.is_empty()
                && (own.eq_ignore_ascii_case(domain)
                    || own
                        .to_ascii_lowercase()
                        .starts_with(&format!("{}.", domain.to_ascii_lowercase())))
        })
    }
}

/// The permalink of message `ts` in `channel` of the workspace at
/// `domain`, as Slack writes it; a reply carries its parent `thread`.
pub fn permalink(domain: &str, channel: &str, ts: &Ts, thread: Option<&Ts>) -> Option<String> {
    if domain.is_empty() || ts.is_local() || ts.seconds().is_none() {
        return None;
    }
    let digits: String = ts.as_str().chars().filter(char::is_ascii_digit).collect();
    let mut link = format!("https://{domain}.slack.com/archives/{channel}/p{digits}");
    if let Some(parent) = thread.filter(|parent| *parent != ts) {
        link.push_str(&format!("?thread_ts={}&cid={channel}", parent.as_str()));
    }
    Some(link)
}

/// Whether a message's text, in Slack's markup, links to a Slack message:
/// a shared message, or a permalink pasted in. Slack unfurls such a link
/// as a quote of the message, which a post through the API asks for with
/// `unfurl_links`.
pub fn has_message_link(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || c == '<' || c == '>')
        .map(|word| word.split('|').next().unwrap_or_default())
        .filter(|word| word.starts_with("https://"))
        .any(|word| {
            parse_web(&word.replace("&amp;", "&"))
                .is_some_and(|link| matches!(link.target, Target::Message { .. }))
        })
}

/// Reads a web link to a conversation or message of a Slack workspace:
/// `https://<domain>.slack.com/archives/<channel>[/p<ts>][?thread_ts=…]`,
/// or the web app's `https://app.slack.com/client/<team>/<channel>`.
pub fn parse_web(url: &str) -> Option<Link> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let query = query.split('#').next().unwrap_or_default();
    let parts: Vec<&str> = path.split('#').next()?.split('/').collect();
    if host == "app.slack.com" {
        return match parts.as_slice() {
            ["client", team, channel, ..] if is_id(team) && is_id(channel) => Some(Link {
                team: Some((*team).to_owned()),
                domain: None,
                target: Target::Conversation((*channel).to_owned()),
            }),
            ["client", team, ..] if is_id(team) => Some(Link {
                team: Some((*team).to_owned()),
                domain: None,
                target: Target::Workspace,
            }),
            _ => None,
        };
    }
    let domain = host.strip_suffix(".slack.com")?;
    if domain.is_empty()
        || !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return None;
    }
    let target = match parts.as_slice() {
        ["archives", channel] | ["archives", channel, ""] if is_id(channel) => {
            Target::Conversation((*channel).to_owned())
        }
        ["archives", channel, message, ..] if is_id(channel) => {
            let ts = message_ts(message)?;
            let thread = param(query, "thread_ts").and_then(|t| real_ts(&t));
            Target::Message {
                channel: (*channel).to_owned(),
                ts,
                thread,
            }
        }
        _ => return None,
    };
    Some(Link {
        team: None,
        domain: Some(domain.to_owned()),
        target,
    })
}

/// Reads a `slack://` deep link: `slack://channel?team=T&id=C`,
/// `slack://user?team=T&id=U` or `slack://open?team=T`. Anything else,
/// such as a sign-in link, is not one.
pub fn parse_deep(url: &str) -> Option<Link> {
    let rest = url.strip_prefix("slack://")?;
    let (kind, query) = rest.split_once('?').unwrap_or((rest, ""));
    let team = param(query, "team").filter(|t| is_id(t))?;
    let id = param(query, "id").filter(|id| is_id(id));
    let target = match kind.trim_end_matches('/') {
        "channel" => match (id, param(query, "message").and_then(|t| real_ts(&t))) {
            (Some(channel), Some(ts)) => Target::Message {
                channel,
                ts,
                thread: param(query, "thread_ts").and_then(|t| real_ts(&t)),
            },
            (Some(channel), None) => Target::Conversation(channel),
            (None, _) => return None,
        },
        "user" => Target::User(id?),
        "open" => match id {
            Some(channel) => Target::Conversation(channel),
            None => Target::Workspace,
        },
        _ => return None,
    };
    Some(Link {
        team: Some(team),
        domain: None,
        target,
    })
}

/// Whether `id` looks like a Slack id (`C0123ABC`, `T01`, `U9`): capital
/// letters and digits only, so nothing else can ride along in it.
fn is_id(id: &str) -> bool {
    id.len() >= 2
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `p1700000000123456` as the timestamp `1700000000.123456`.
fn message_ts(segment: &str) -> Option<Ts> {
    let digits = segment.strip_prefix('p')?;
    if digits.len() <= 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (secs, micros) = digits.split_at(digits.len() - 6);
    Some(Ts::new(format!("{secs}.{micros}")))
}

/// A timestamp from a query, if it is a real one.
fn real_ts(text: &str) -> Option<Ts> {
    let ts = Ts::new(text);
    let (secs, micros) = text.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    (digits(secs) && digits(micros)).then_some(ts)
}

/// The decoded value of `key` in a query string.
fn param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, value)| crate::percent::decode(value).ok())
        .map(|value| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(channel: &str, ts: &str, thread: Option<&str>) -> Target {
        Target::Message {
            channel: channel.into(),
            ts: Ts::new(ts),
            thread: thread.map(Ts::new),
        }
    }

    #[test]
    fn permalinks_are_written_as_slack_writes_them() {
        let ts = Ts::new("1700000000.123456");
        assert_eq!(
            permalink("acme", "C1", &ts, None).as_deref(),
            Some("https://acme.slack.com/archives/C1/p1700000000123456")
        );
        assert_eq!(
            permalink("acme", "C1", &ts, Some(&Ts::new("1699999999.000100"))).as_deref(),
            Some(
                "https://acme.slack.com/archives/C1/p1700000000123456\
                 ?thread_ts=1699999999.000100&cid=C1"
            )
        );
        // A thread's parent links as itself.
        assert_eq!(
            permalink("acme", "C1", &ts, Some(&ts)),
            permalink("acme", "C1", &ts, None)
        );
        assert_eq!(permalink("", "C1", &ts, None), None);
        assert_eq!(permalink("acme", "C1", &Ts::new("local-1"), None), None);
    }

    #[test]
    fn message_links_are_found_in_markup() {
        assert!(has_message_link(
            "Look\n<https://acme.slack.com/archives/C1/p1700000000123456>"
        ));
        assert!(has_message_link(
            "<https://acme.slack.com/archives/C1/p1700000500000200?thread_ts=1700000000.000100&amp;cid=C1|this>"
        ));
        assert!(has_message_link(
            "see https://acme.slack.com/archives/C1/p1700000000123456 too"
        ));
        assert!(!has_message_link(
            "<https://acme.slack.com/archives/C1> is a channel"
        ));
        assert!(!has_message_link("<https://example.com/archives/C1/p17>"));
        assert!(!has_message_link("no links"));
    }

    #[test]
    fn permalinks_read_back() {
        for thread in [None, Some(Ts::new("1699999999.000100"))] {
            let ts = Ts::new("1700000000.123456");
            let link = permalink("acme", "C1", &ts, thread.as_ref()).expect("a link");
            let parsed = parse_web(&link).expect("parsed");
            assert_eq!(parsed.domain.as_deref(), Some("acme"));
            assert_eq!(
                parsed.target,
                Target::Message {
                    channel: "C1".into(),
                    ts,
                    thread
                }
            );
        }
    }

    #[test]
    fn web_links_name_a_conversation_or_message() {
        let link = parse_web("https://Acme.slack.com/archives/C0123ABC").expect("a link");
        assert_eq!(link.target, Target::Conversation("C0123ABC".into()));
        assert!(link.is_for("T1", "acme"));
        assert!(!link.is_for("T1", "other"));
        let link = parse_web("https://acme.enterprise.slack.com/archives/C1/p1700000000123456#x")
            .expect("a grid link");
        assert_eq!(link.target, message("C1", "1700000000.123456", None));
        assert!(link.is_for("T1", "acme"));
        let link = parse_web("https://app.slack.com/client/T1/C2").expect("the web app");
        assert_eq!(link.target, Target::Conversation("C2".into()));
        assert!(link.is_for("T1", "anything"));
        assert!(!link.is_for("T2", "anything"));
        for bad in [
            "http://acme.slack.com/archives/C1",
            "https://acme.slack.com.evil.example/archives/C1",
            "https://evil.example/acme.slack.com/archives/C1",
            "https://acme.slack.com/archives/c1",
            "https://acme.slack.com/archives/C1/p123",
            "https://acme.slack.com/archives/C1/pabc1234567",
            "https://acme.slack.com/files/U1/F1/a.png",
            "https://.slack.com/archives/C1",
        ] {
            assert_eq!(parse_web(bad), None, "{bad}");
        }
        // A thread_ts that is not a timestamp is dropped, not trusted.
        let link =
            parse_web("https://acme.slack.com/archives/C1/p1700000000123456?thread_ts=x&cid=C1")
                .expect("a link");
        assert_eq!(link.target, message("C1", "1700000000.123456", None));
    }

    #[test]
    fn deep_links_name_a_team_and_what_to_open() {
        let link = parse_deep("slack://channel?team=T1&id=C2").expect("a channel");
        assert_eq!(link.team.as_deref(), Some("T1"));
        assert_eq!(link.target, Target::Conversation("C2".into()));
        let link = parse_deep("slack://channel?id=C2&team=T1&message=1700000000.123456")
            .expect("a message");
        assert_eq!(link.target, message("C2", "1700000000.123456", None));
        assert_eq!(
            parse_deep("slack://user?team=T1&id=U3").map(|l| l.target),
            Some(Target::User("U3".into()))
        );
        assert_eq!(
            parse_deep("slack://open?team=T1").map(|l| l.target),
            Some(Target::Workspace)
        );
        for bad in [
            "slack://open",
            "slack://channel?team=T1",
            "slack://channel?id=C2",
            "slack://T0123ABCD/magic-login/abc?host=acme.slack.com",
            "slack://user?team=T1&id=../x",
            "noslacking://channel?team=T1&id=C2",
        ] {
            assert_eq!(parse_deep(bad), None, "{bad}");
        }
    }
}
