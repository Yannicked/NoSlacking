//! Searching a workspace's messages and files: what was asked, the pages
//! of results so far, and how they are grouped and highlighted.
//!
//! The query goes to Slack as typed, so its modifiers (`from:@ana`,
//! `in:#general`, `before:2025-01-01`, `after:`, `has:link`, …) work as
//! they do in Slack. Slack marks the words that matched with two private
//! characters, [`MATCH_START`] and [`MATCH_END`]; [`segments`] splits text
//! at them for drawing.

use crate::model::Ts;

/// Where Slack starts and ends a matching word, with `highlight=true`.
pub const MATCH_START: char = '\u{e000}';
pub const MATCH_END: char = '\u{e001}';
/// How many results a page holds.
pub const PAGE_SIZE: u32 = 20;
/// The last page Slack serves.
pub const MAX_PAGE: u32 = 100;

/// What is searched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Messages,
    Files,
}

/// How results are ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    /// Slack's score, grouped by conversation.
    #[default]
    Relevant,
    /// Newest first, grouped by day.
    Newest,
}

/// A file that matched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileHit {
    pub name: String,
    pub title: String,
    pub mimetype: String,
    pub size: u64,
}

/// One result: a message, or a file and the message that shared it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hit {
    /// Tells results apart: the message's timestamp and channel, or the
    /// file's id.
    pub key: String,
    pub channel: Option<String>,
    /// The conversation's name as Slack gave it, for one not known here.
    pub channel_name: String,
    /// The message to jump to; a file may have none.
    pub ts: Option<Ts>,
    /// Its thread's parent, for a reply.
    pub thread: Option<Ts>,
    /// When it was posted.
    pub when: Option<Ts>,
    pub user: Option<String>,
    pub username: Option<String>,
    /// mrkdwn, with the matching words marked.
    pub text: String,
    pub file: Option<FileHit>,
    pub permalink: Option<String>,
}

/// A page of results as Slack answered it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub hits: Vec<Hit>,
    pub page: u32,
    pub pages: u32,
    pub total: u32,
}

/// Why a search failed.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// The sign-in was made without the `search:read` permission.
    NoPermission,
    Other(String),
}

/// A query as it was sent, so a later page asks for the same thing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub team: String,
    pub text: String,
    pub scope: Scope,
    pub sort: Sort,
}

/// The search window and its results.
#[derive(Clone, Debug, Default)]
pub struct Search {
    pub open: bool,
    /// What is typed.
    pub text: String,
    pub scope: Scope,
    pub sort: Sort,
    /// The query the results are for.
    pub query: Option<Query>,
    pub hits: Vec<Hit>,
    /// The last page read, and how many there are.
    pub page: u32,
    pub pages: u32,
    pub total: u32,
    pub loading: bool,
    pub failure: Option<Failure>,
    /// Which request the results must answer: an answer to an older one
    /// is dropped.
    pub request: u64,
    /// The result picked with the keyboard.
    pub selected: usize,
    /// Focus the field when next drawn.
    pub focus: bool,
}

impl Search {
    /// Starts the typed query over in `team`. Returns the query and the
    /// request to send, or `None` when nothing is typed.
    pub fn start(&mut self, team: &str) -> Option<(Query, u64)> {
        let text = self.text.trim();
        if text.is_empty() {
            return None;
        }
        let query = Query {
            team: team.to_owned(),
            text: text.to_owned(),
            scope: self.scope,
            sort: self.sort,
        };
        self.request += 1;
        self.query = Some(query.clone());
        self.hits.clear();
        self.page = 0;
        self.pages = 0;
        self.total = 0;
        self.selected = 0;
        self.failure = None;
        self.loading = true;
        Some((query, self.request))
    }

    /// The next page of the results shown, if there is one and nothing is
    /// on its way.
    pub fn more(&mut self) -> Option<(Query, u32, u64)> {
        let query = self.query.clone()?;
        if self.loading || self.failure.is_some() || self.page >= self.pages.min(MAX_PAGE) {
            return None;
        }
        self.request += 1;
        self.loading = true;
        Some((query, self.page + 1, self.request))
    }

    /// Slack answered request `request`.
    pub fn arrived(&mut self, request: u64, result: Result<Page, Failure>) {
        if request != self.request {
            return;
        }
        self.loading = false;
        match result {
            Ok(page) => {
                for hit in page.hits {
                    if !self.hits.iter().any(|h| h.key == hit.key) {
                        self.hits.push(hit);
                    }
                }
                self.page = page.page.max(self.page + 1);
                self.pages = page.pages;
                self.total = page.total;
            }
            Err(failure) => self.failure = Some(failure),
        }
    }
}

/// What a group of results gathers under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heading {
    /// A conversation, by id (or by Slack's name for it when it has none).
    Conversation(String),
    /// A day, by any timestamp of it.
    Day(Ts),
}

/// Results under one heading, by index into the hits, in Slack's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub heading: Heading,
    pub hits: Vec<usize>,
}

/// Gathers `hits` for showing: by conversation when sorted by relevance,
/// groups ordered by their best result; by local day when newest first.
/// `day_of` gives a timestamp's day as a number.
pub fn groups(hits: &[Hit], sort: Sort, day_of: impl Fn(&Ts) -> Option<i64>) -> Vec<Group> {
    let mut groups: Vec<(Option<i64>, Group)> = Vec::new();
    for (index, hit) in hits.iter().enumerate() {
        let (key, heading) = match sort {
            Sort::Relevant => (
                None,
                Heading::Conversation(
                    hit.channel
                        .clone()
                        .unwrap_or_else(|| hit.channel_name.clone()),
                ),
            ),
            Sort::Newest => {
                let when = hit.when.clone().unwrap_or_default();
                (day_of(&when), Heading::Day(when))
            }
        };
        let found = groups
            .iter_mut()
            .find(|(day, group)| match (&group.heading, &heading) {
                (Heading::Conversation(a), Heading::Conversation(b)) => a == b,
                // Newest first, so a day's results come together.
                (Heading::Day(_), Heading::Day(_)) => *day == key,
                _ => false,
            });
        match found {
            Some((_, group)) => group.hits.push(index),
            None => groups.push((
                key,
                Group {
                    heading,
                    hits: vec![index],
                },
            )),
        }
    }
    groups.into_iter().map(|(_, group)| group).collect()
}

/// Splits text at Slack's match markers: each piece, and whether it
/// matched. Empty pieces are left out, and a marker left open runs to the
/// end.
pub fn segments(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut current = String::new();
    let mut matched = false;
    for c in text.chars() {
        let next = match c {
            MATCH_START => true,
            MATCH_END => false,
            _ => {
                current.push(c);
                continue;
            }
        };
        if !current.is_empty() {
            out.push((std::mem::take(&mut current), matched));
        }
        matched = next;
    }
    if !current.is_empty() {
        out.push((current, matched));
    }
    out
}

/// Text without Slack's match markers.
pub fn unmarked(text: &str) -> String {
    text.chars()
        .filter(|c| *c != MATCH_START && *c != MATCH_END)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(key: &str, channel: &str, when: &str) -> Hit {
        Hit {
            key: key.into(),
            channel: Some(channel.into()),
            when: Some(Ts::new(when)),
            ts: Some(Ts::new(when)),
            ..Hit::default()
        }
    }

    fn page(hits: Vec<Hit>, page: u32, pages: u32) -> Page {
        Page {
            total: hits.len() as u32,
            hits,
            page,
            pages,
        }
    }

    #[test]
    fn markers_split_the_text() {
        let text = format!("the {MATCH_START}plan{MATCH_END} for {MATCH_START}Friday");
        assert_eq!(
            segments(&text),
            [
                ("the ".to_owned(), false),
                ("plan".to_owned(), true),
                (" for ".to_owned(), false),
                ("Friday".to_owned(), true),
            ]
        );
        assert_eq!(segments("plain"), [("plain".to_owned(), false)]);
        assert!(segments("").is_empty());
        assert_eq!(unmarked(&text), "the plan for Friday");
    }

    #[test]
    fn results_group_by_conversation_or_day() {
        let hits = [
            hit("a", "C1", "300.0"),
            hit("b", "C2", "260.0"),
            hit("c", "C1", "100.0"),
        ];
        let relevant = groups(&hits, Sort::Relevant, |_| None);
        assert_eq!(
            relevant,
            [
                Group {
                    heading: Heading::Conversation("C1".into()),
                    hits: vec![0, 2]
                },
                Group {
                    heading: Heading::Conversation("C2".into()),
                    hits: vec![1]
                },
            ]
        );
        // Days of 250 seconds: the first two share one.
        let newest = groups(&hits, Sort::Newest, |ts| ts.seconds().map(|s| s / 250));
        let counts: Vec<Vec<usize>> = newest.into_iter().map(|g| g.hits).collect();
        assert_eq!(counts, [vec![0, 1], vec![2]]);
    }

    #[test]
    fn pages_follow_the_latest_request() {
        let mut search = Search {
            text: "  plan  ".into(),
            ..Search::default()
        };
        assert!(search.more().is_none(), "nothing asked yet");
        let (query, first) = search.start("T1").expect("a query");
        assert_eq!(query.text, "plan");
        assert!(search.more().is_none(), "the first page is on its way");
        search.arrived(first, Ok(page(vec![hit("a", "C1", "1.0")], 1, 2)));
        let (_, number, second) = search.more().expect("a second page");
        assert_eq!(number, 2);
        // The same result again on the next page is shown once.
        search.arrived(
            second,
            Ok(page(
                vec![hit("a", "C1", "1.0"), hit("b", "C1", "2.0")],
                2,
                2,
            )),
        );
        assert_eq!(search.hits.len(), 2);
        assert!(search.more().is_none(), "that was the last page");
        // A new query drops the old results, and late answers to it.
        search.text = "other".into();
        let (_, third) = search.start("T1").expect("a query");
        search.arrived(second, Ok(page(vec![hit("x", "C9", "9.0")], 2, 2)));
        assert!(search.hits.is_empty() && search.loading);
        search.arrived(third, Err(Failure::NoPermission));
        assert_eq!(search.failure, Some(Failure::NoPermission));
        assert!(search.more().is_none());
        search.text = " ".into();
        assert!(search.start("T1").is_none());
    }
}
