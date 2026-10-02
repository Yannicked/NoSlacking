//! What `search.messages` and `search.files` answer, and its translation
//! into [`crate::search`].

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::model::Ts;
use crate::search::{FileHit, Hit, Page};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Paging {
    pub page: u32,
    pub pages: u32,
    pub total: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MatchChannel {
    pub id: String,
    pub name: String,
}

/// One message that matched.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MessageMatch {
    pub channel: MatchChannel,
    pub user: Option<String>,
    pub username: Option<String>,
    pub ts: String,
    pub text: String,
    pub permalink: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Messages {
    pub total: u32,
    pub paging: Paging,
    pub matches: Vec<MessageMatch>,
}

/// `search.messages`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MessagesAnswer {
    pub messages: Messages,
}

/// Where a file was shared: by conversation, the messages that shared it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Shares {
    pub public: BTreeMap<String, Vec<Share>>,
    pub private: BTreeMap<String, Vec<Share>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Share {
    pub ts: String,
    pub thread_ts: Option<String>,
    pub channel_name: String,
}

/// One file that matched.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileMatch {
    pub id: String,
    pub name: String,
    pub title: String,
    pub mimetype: String,
    pub size: u64,
    pub user: Option<String>,
    pub timestamp: Option<i64>,
    pub channels: Vec<String>,
    pub groups: Vec<String>,
    pub ims: Vec<String>,
    pub permalink: Option<String>,
    pub shares: Shares,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Files {
    pub total: u32,
    pub paging: Paging,
    pub matches: Vec<FileMatch>,
}

/// `search.files`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FilesAnswer {
    pub files: Files,
}

/// The parent's timestamp in a reply's permalink (`?thread_ts=…`): search
/// results say nothing else about threads.
fn thread_of(permalink: Option<&str>) -> Option<Ts> {
    let query = permalink?.split_once('?')?.1;
    let value = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("thread_ts="))?;
    let (secs, micros) = value.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(secs) && digits(micros)).then(|| Ts::new(value))
}

impl MessagesAnswer {
    pub fn into_page(self) -> Page {
        let messages = self.messages;
        let hits = messages
            .matches
            .into_iter()
            .filter(|m| !m.ts.is_empty() && !m.channel.id.is_empty())
            .map(|m| {
                let ts = Ts::new(m.ts);
                let thread = thread_of(m.permalink.as_deref()).filter(|parent| *parent != ts);
                Hit {
                    key: format!("{}/{}", m.channel.id, ts.as_str()),
                    channel: Some(m.channel.id),
                    channel_name: m.channel.name,
                    ts: Some(ts.clone()),
                    thread,
                    when: Some(ts),
                    user: m.user.filter(|u| !u.is_empty()),
                    username: m.username.filter(|u| !u.is_empty()),
                    text: m.text,
                    file: None,
                    permalink: m.permalink,
                }
            })
            .collect();
        Page {
            hits,
            page: messages.paging.page,
            pages: messages.paging.pages,
            total: messages.total.max(messages.paging.total),
        }
    }
}

impl FilesAnswer {
    pub fn into_page(self) -> Page {
        let files = self.files;
        let hits = files
            .matches
            .into_iter()
            .filter(|f| !f.id.is_empty())
            .map(|f| {
                // The first message that shared it, to jump to.
                let share = f
                    .shares
                    .public
                    .iter()
                    .chain(f.shares.private.iter())
                    .find_map(|(channel, shares)| Some((channel.clone(), shares.first()?)));
                let channel = share.as_ref().map(|(c, _)| c.clone()).or_else(|| {
                    f.channels
                        .iter()
                        .chain(&f.groups)
                        .chain(&f.ims)
                        .next()
                        .cloned()
                });
                let ts = share
                    .as_ref()
                    .map(|(_, s)| Ts::new(s.ts.clone()))
                    .filter(|ts| ts.seconds().is_some());
                let thread = share
                    .as_ref()
                    .and_then(|(_, s)| s.thread_ts.clone())
                    .map(Ts::new)
                    .filter(|parent| Some(parent) != ts.as_ref());
                Hit {
                    key: f.id,
                    channel,
                    channel_name: share
                        .map(|(_, s)| s.channel_name.clone())
                        .unwrap_or_default(),
                    ts,
                    thread,
                    when: f.timestamp.map(|secs| Ts::new(format!("{secs}.000000"))),
                    user: f.user.filter(|u| !u.is_empty()),
                    username: None,
                    text: if f.title.is_empty() {
                        f.name.clone()
                    } else {
                        f.title.clone()
                    },
                    file: Some(FileHit {
                        name: f.name,
                        title: f.title,
                        mimetype: f.mimetype,
                        size: f.size,
                    }),
                    permalink: f.permalink,
                }
            })
            .collect();
        Page {
            hits,
            page: files.paging.page,
            pages: files.paging.pages,
            total: files.total.max(files.paging.total),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{MATCH_END, MATCH_START};

    #[test]
    fn message_results_keep_their_marks_and_threads() {
        let answer: MessagesAnswer = serde_json::from_str(
            r#"{"ok":true,"query":"plan","messages":{"total":3,
                "paging":{"count":20,"total":3,"page":1,"pages":1},
                "matches":[
                  {"iid":"1","team":"T1","channel":{"id":"C1","name":"general","is_channel":true},
                   "type":"message","user":"U1","username":"ana","ts":"1700000000.000100",
                   "text":"the \ue000plan\ue001",
                   "permalink":"https://acme.slack.com/archives/C1/p1700000000000100"},
                  {"channel":{"id":"C1","name":"general"},"user":"U2","ts":"1700000500.000200",
                   "text":"a reply",
                   "permalink":"https://acme.slack.com/archives/C1/p1700000500000200?thread_ts=1700000000.000100&cid=C1"},
                  {"channel":{"id":"","name":"gone"},"ts":"1.0","text":"no channel"}
                ]}}"#,
        )
        .expect("parsed");
        let page = answer.into_page();
        assert_eq!((page.page, page.pages, page.total), (1, 1, 3));
        assert_eq!(page.hits.len(), 2, "a result without a channel is left out");
        let first = &page.hits[0];
        assert_eq!(first.text, format!("the {MATCH_START}plan{MATCH_END}"));
        assert_eq!(first.channel.as_deref(), Some("C1"));
        assert_eq!(first.channel_name, "general");
        assert_eq!(first.thread, None);
        assert_eq!(first.user.as_deref(), Some("U1"));
        assert_eq!(
            page.hits[1].thread,
            Some(Ts::new("1700000000.000100")),
            "a reply names its parent"
        );
    }

    #[test]
    fn file_results_point_at_the_message_that_shared_them() {
        let answer: FilesAnswer = serde_json::from_str(
            r#"{"ok":true,"files":{"total":2,"paging":{"page":2,"pages":5,"total":90},
                "matches":[
                  {"id":"F1","name":"plan.pdf","title":"Release plan","mimetype":"application/pdf",
                   "size":2048,"user":"U1","timestamp":1700000000,"channels":["C1"],
                   "permalink":"https://acme.slack.com/files/U1/F1/plan.pdf",
                   "shares":{"public":{"C1":[{"ts":"1700000001.000100","channel_name":"general",
                     "thread_ts":"1699999999.000100"}]}}},
                  {"id":"F2","name":"notes.txt","title":"","ims":["D1"],"timestamp":1700000002}
                ]}}"#,
        )
        .expect("parsed");
        let page = answer.into_page();
        assert_eq!((page.page, page.pages, page.total), (2, 5, 90));
        let shared = &page.hits[0];
        assert_eq!(shared.channel.as_deref(), Some("C1"));
        assert_eq!(shared.ts, Some(Ts::new("1700000001.000100")));
        assert_eq!(shared.thread, Some(Ts::new("1699999999.000100")));
        assert_eq!(shared.text, "Release plan");
        assert_eq!(
            shared.when.as_ref().and_then(Ts::seconds),
            Some(1_700_000_000)
        );
        let unshared = &page.hits[1];
        assert_eq!(unshared.channel.as_deref(), Some("D1"));
        assert_eq!(unshared.ts, None, "nothing to jump to");
        assert_eq!(unshared.text, "notes.txt");
    }

    #[test]
    fn only_real_thread_timestamps_are_read() {
        assert_eq!(
            thread_of(Some("https://a/p1?thread_ts=1.2&cid=C")),
            Some(Ts::new("1.2"))
        );
        assert_eq!(thread_of(Some("https://a/p1?thread_ts=x.y")), None);
        assert_eq!(thread_of(Some("https://a/p1")), None);
        assert_eq!(thread_of(None), None);
    }
}
