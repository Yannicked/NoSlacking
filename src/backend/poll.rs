//! Watching every conversation of a workspace while its real-time socket
//! is down, so new direct messages and mentions elsewhere still show up
//! and notify.
//!
//! The open conversation has a quicker poll of its own (in the worker).
//! This one is slower and never overlaps itself: one round at a time per
//! workspace, the next one only after a rest, longer after a failure.
//!
//! - A browser session asks `client.counts` once a round for every
//!   conversation's newest message, read marker and mention count. A
//!   conversation whose newest message moved is reported, and its newest
//!   page fetched when it would notify: a direct or group message, or a
//!   channel whose mention count rose.
//! - An OAuth sign-in has no such call. It lists its direct and group
//!   messages now and then, and checks a few of them each round with
//!   `conversations.history` and `limit=1`: the most active ones every
//!   round, the rest in turn. Channels, mentions and all, go unheard until
//!   the socket is back: a call per channel would cost too much.
//!
//! What counts as new is decided by the interface (see
//! `WorkspaceState::polled_new`), which knows what it has shown. The
//! worker's own record of what it last saw only keeps the calls down. A
//! round with no record yet (just after start-up) only takes one: what
//! Slack holds then is not news, and treating it as news would end in a
//! pile of notifications for messages from while the app was closed.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::api::{paginate, with_cursor};
use super::fetch::history_page;
use super::{Event, Sink};
use crate::model::Ts;
use crate::slack::{Client, SlackError, types};

/// The rest between two rounds of a browser session: one cheap call each.
const SESSION_EVERY: Duration = Duration::from_secs(30);
/// The rest between two rounds of an OAuth sign-in, whose rounds cost a
/// call per conversation checked.
const OAUTH_EVERY: Duration = Duration::from_secs(60);
/// The longest rest after failures, so an outage is not hammered.
const MAX_REST: Duration = Duration::from_secs(10 * 60);
/// The most newest pages one round fetches. The rest wait for the next
/// round, direct messages first; a long outage then cannot set off a
/// burst of calls when polling starts.
const FETCHES: usize = 8;
/// How many conversations an OAuth round checks, and how many of those
/// are the most recently active ones.
const CHECKS: usize = 20;
const RECENT: usize = 8;
/// The pause between two calls of a round, so a round is spread out
/// rather than a burst that eats the rate limit the interface needs.
const SESSION_PAUSE: Duration = Duration::from_millis(500);
const OAUTH_PAUSE: Duration = Duration::from_secs(2);
/// An OAuth sign-in lists its direct messages again every this many
/// rounds, to find new ones.
const RELIST_EVERY: u64 = 10;
/// `users.conversations`, 200 conversations a page.
const LIST_PAGES: usize = 20;

/// What a round last saw of one conversation.
#[derive(Clone, Debug, Default, PartialEq)]
struct Known {
    /// The newest message.
    latest: Option<Ts>,
    /// Your read marker, which only `client.counts` tells.
    last_read: Option<Ts>,
    /// Slack's count of unread mentions, likewise.
    mentions: u32,
    /// A direct or group message, every message of which notifies.
    direct: bool,
}

/// One workspace's watch, kept between rounds and between outages: a
/// record made while the socket was down still tells the next outage's
/// first round what changed meanwhile.
#[derive(Debug, Default)]
pub(super) struct State {
    /// Each conversation as last seen; `None` until a round took one.
    known: Option<HashMap<String, Known>>,
    /// When the next round may start; `None` for at once.
    next: Option<Instant>,
    /// Rounds that failed in a row, which lengthen the rest.
    failures: u32,
    /// Rounds run, which order the OAuth checks.
    round: u64,
    /// `client.counts` refused this session, so it is watched as an OAuth
    /// sign-in is.
    no_counts: bool,
    /// An OAuth sign-in's direct and group messages, and the round they
    /// were listed in.
    direct: Vec<String>,
    listed: Option<u64>,
    /// The round each conversation was last checked in.
    checked: HashMap<String, u64>,
}

/// How long after sign-in the first round waits, so a socket that is
/// still connecting is not polled around for nothing.
const GRACE: Duration = Duration::from_secs(15);

impl State {
    /// The watch of a workspace just signed in, whose socket is likely
    /// still connecting.
    pub(super) fn starting(now: Instant) -> Self {
        Self {
            next: Some(now + GRACE),
            ..Self::default()
        }
    }

    /// Whether a round may start now.
    pub(super) fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }
}

/// The rest after a round: `every`, doubled for each failure in a row, up
/// to [`MAX_REST`].
fn rest(every: Duration, failures: u32) -> Duration {
    every
        .saturating_mul(2u32.saturating_pow(failures.min(8)))
        .min(MAX_REST)
}

/// One conversation a round reports, and whether its newest page is
/// fetched first.
#[derive(Debug, PartialEq)]
struct Step {
    channel: String,
    fetch: bool,
}

/// What a session round does with a `client.counts` answer, given what
/// the last round saw (`None` before the first one): every conversation
/// that changed is reported, and its newest page fetched when its newest
/// message moved and it would notify. Fetches come first, direct
/// messages before channels; past [`FETCHES`], a conversation waits for
/// the next round, unreported so it still looks changed then. The open
/// conversation is never fetched: its own poll does that.
fn plan(
    known: Option<&HashMap<String, Known>>,
    now: &HashMap<String, Known>,
    open: Option<&str>,
    cap: usize,
) -> Vec<Step> {
    let Some(known) = known else {
        // The first look is a record of how things are, not news.
        let mut all: Vec<Step> = now
            .keys()
            .map(|channel| Step {
                channel: channel.clone(),
                fetch: false,
            })
            .collect();
        all.sort_by(|a, b| a.channel.cmp(&b.channel));
        return all;
    };
    let mut fetch = Vec::new();
    let mut report = Vec::new();
    for (channel, seen) in now {
        let before = known.get(channel);
        if before == Some(seen) {
            continue;
        }
        let old = before.cloned().unwrap_or_default();
        let moved = seen.latest > old.latest;
        let notifies = seen.direct || seen.mentions > old.mentions;
        if moved && notifies && open != Some(channel.as_str()) {
            fetch.push((seen.direct, channel));
        } else {
            report.push(channel);
        }
    }
    // Direct messages first, then a steady order.
    fetch.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    report.sort();
    fetch
        .into_iter()
        .take(cap)
        .map(|(_, channel)| Step {
            channel: channel.clone(),
            fetch: true,
        })
        .chain(report.into_iter().map(|channel| Step {
            channel: channel.clone(),
            fetch: false,
        }))
        .collect()
}

/// Every conversation in a `client.counts` answer.
fn snapshot(counts: types::ClientCounts) -> HashMap<String, Known> {
    let known = |entry: types::CountEntry, direct: bool| {
        (
            entry.id,
            Known {
                latest: types::real_ts(&entry.latest),
                last_read: types::real_ts(&entry.last_read),
                mentions: entry.mention_count,
                direct,
            },
        )
    };
    counts
        .channels
        .into_iter()
        .map(|entry| known(entry, false))
        .chain(
            counts
                .mpims
                .into_iter()
                .chain(counts.ims)
                .map(|entry| known(entry, true)),
        )
        .filter(|(id, _)| !id.is_empty())
        .collect()
}

/// Which conversations an OAuth round checks: the `recent` most recently
/// active ones (by what the last check saw), then those checked longest
/// ago, never-checked ones first, up to `cap` in all. Busy conversations
/// are heard quickly, and every one comes round in turn.
fn pick(
    direct: &[String],
    known: &HashMap<String, Known>,
    checked: &HashMap<String, u64>,
    cap: usize,
    recent: usize,
) -> Vec<String> {
    let mut by_activity: Vec<&String> = direct
        .iter()
        .filter(|id| known.get(*id).is_some_and(|k| k.latest.is_some()))
        .collect();
    by_activity.sort_by(|a, b| {
        let latest = |id: &String| known.get(id).and_then(|k| k.latest.clone());
        latest(b).cmp(&latest(a)).then_with(|| a.cmp(b))
    });
    let mut picked: Vec<String> = by_activity
        .into_iter()
        .take(recent.min(cap))
        .cloned()
        .collect();
    let mut by_turn: Vec<&String> = direct.iter().filter(|id| !picked.contains(id)).collect();
    by_turn.sort_by_key(|id| (checked.get(*id).copied(), (*id).clone()));
    let room = cap.saturating_sub(picked.len());
    picked.extend(by_turn.into_iter().take(room).cloned());
    picked
}

/// Runs one round for a workspace whose socket is down, then sets when
/// the next may start. `open` is the conversation on screen in it, if
/// any. The caller holds `state` for the whole round, so rounds never
/// overlap.
pub(super) async fn round(
    client: Client,
    team: String,
    open: Option<String>,
    state: &mut State,
    sink: Sink,
) {
    state.round += 1;
    let session = client.token().is_session() && !state.no_counts;
    let result = if session {
        session_round(&client, &team, open.as_deref(), state, &sink).await
    } else {
        direct_round(&client, &team, open.as_deref(), state, &sink).await
    };
    match result {
        Ok(()) => state.failures = 0,
        Err(error) => {
            log::debug!("polling {team} for activity: {error}");
            state.failures = state.failures.saturating_add(1);
        }
    }
    let every = if session { SESSION_EVERY } else { OAUTH_EVERY };
    state.next = Some(Instant::now() + rest(every, state.failures));
}

/// A browser session's round: one `client.counts`, then the newest page
/// of the few conversations that would notify.
async fn session_round(
    client: &Client,
    team: &str,
    open: Option<&str>,
    state: &mut State,
    sink: &Sink,
) -> Result<(), SlackError> {
    let counts = match client
        .call::<types::ClientCounts>("client.counts", &[])
        .await
    {
        Ok(counts) => counts,
        Err(SlackError::Api(code)) => {
            // Not offered to this session: watch its direct messages as an
            // OAuth sign-in does, from the next round on.
            log::info!("client.counts refused ({code}); polling direct messages instead");
            state.no_counts = true;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let now = snapshot(counts);
    let steps = plan(state.known.as_ref(), &now, open, FETCHES);
    let known = state.known.get_or_insert_with(HashMap::new);
    let mut fetched = false;
    for step in steps {
        let Some(seen) = now.get(&step.channel) else {
            continue;
        };
        if step.fetch {
            if fetched {
                tokio::time::sleep(SESSION_PAUSE).await;
            }
            fetched = true;
            // On failure the conversation stays as it was in the record,
            // so the next round tries it again.
            newest_page(client, team, &step.channel, sink).await?;
        }
        report(team, &step.channel, seen, true, sink);
        known.insert(step.channel, seen.clone());
    }
    // Conversations you left are no longer watched.
    known.retain(|channel, _| now.contains_key(channel));
    Ok(())
}

/// An OAuth round (or a session's without `client.counts`): a few direct
/// and group messages, each checked with one call, and the newest page of
/// any whose newest message moved.
async fn direct_round(
    client: &Client,
    team: &str,
    open: Option<&str>,
    state: &mut State,
    sink: &Sink,
) -> Result<(), SlackError> {
    let relist = state
        .listed
        .is_none_or(|listed| state.round.saturating_sub(listed) >= RELIST_EVERY);
    if relist {
        state.direct = list_direct(client).await?;
        state.listed = Some(state.round);
        let listed: HashSet<&String> = state.direct.iter().collect();
        state.checked.retain(|id, _| listed.contains(id));
        if let Some(known) = &mut state.known {
            known.retain(|id, _| listed.contains(id));
        }
    }
    let known = state.known.get_or_insert_with(HashMap::new);
    let picked = pick(&state.direct, known, &state.checked, CHECKS, RECENT);
    for (n, channel) in picked.into_iter().enumerate() {
        if n > 0 || relist {
            tokio::time::sleep(OAUTH_PAUSE).await;
        }
        let page = client
            .call::<types::HistoryPage>(
                "conversations.history",
                &[("channel", channel.clone()), ("limit", "1".into())],
            )
            .await;
        let latest = match page {
            Ok(page) => page.messages.first().and_then(|m| types::real_ts(&m.ts)),
            Err(SlackError::Api(code)) => {
                // Gone, or not ours to read: leave it for the next listing.
                log::debug!("conversations.history {channel}: {code}");
                state.checked.insert(channel, state.round);
                continue;
            }
            Err(error) => return Err(error),
        };
        let seen = Known {
            latest,
            direct: true,
            ..Known::default()
        };
        match known.get(&channel) {
            // First check: a record, not news.
            None => report(team, &channel, &seen, false, sink),
            Some(before) if seen.latest > before.latest => {
                if open != Some(channel.as_str()) {
                    tokio::time::sleep(OAUTH_PAUSE).await;
                    newest_page(client, team, &channel, sink).await?;
                }
                report(team, &channel, &seen, false, sink);
            }
            Some(_) => {}
        }
        state.checked.insert(channel.clone(), state.round);
        known.insert(channel, seen);
    }
    Ok(())
}

/// Tells the interface what a round saw of one conversation. Only a
/// session's `client.counts` knows the read marker and mentions.
fn report(team: &str, channel: &str, seen: &Known, counted: bool, sink: &Sink) {
    sink.send(Event::Activity {
        team: team.to_owned(),
        channel: channel.to_owned(),
        latest: seen.latest.clone(),
        last_read: seen.last_read.clone().filter(|_| counted),
        mentions: counted.then_some(seen.mentions),
    });
}

/// Fetches a conversation's newest page and hands it over as a poll, so
/// the interface announces what is new in it. Sent before the
/// conversation's new state, which would move the line "new" is
/// measured from.
async fn newest_page(
    client: &Client,
    team: &str,
    channel: &str,
    sink: &Sink,
) -> Result<(), SlackError> {
    let page = client
        .call::<types::HistoryPage>(
            "conversations.history",
            &[
                ("channel", channel.to_owned()),
                ("limit", super::fetch::HISTORY_PAGE.to_string()),
                ("include_all_metadata", "false".to_owned()),
            ],
        )
        .await;
    let page = match page {
        Ok(page) => page,
        // Gone or not readable: nothing to announce, nothing to retry.
        Err(SlackError::Api(code)) => {
            log::debug!("conversations.history {channel}: {code}");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let (messages, has_more, cursor) = history_page(page);
    sink.send(Event::History {
        team: team.to_owned(),
        channel: channel.to_owned(),
        messages,
        has_more,
        cursor,
        older: false,
        polled: true,
    });
    Ok(())
}

/// Your direct and group messages.
async fn list_direct(client: &Client) -> Result<Vec<String>, SlackError> {
    let mut ids = Vec::new();
    paginate(
        "users.conversations",
        LIST_PAGES,
        |cursor| {
            let params = with_cursor(
                vec![
                    ("types", "im,mpim".to_owned()),
                    ("exclude_archived", "true".to_owned()),
                    ("limit", "200".to_owned()),
                ],
                cursor,
            );
            async move {
                let page: types::ConversationsPage =
                    client.call("users.conversations", &params).await?;
                let next = page.response_metadata.cursor();
                Ok((page.channels, next))
            }
        },
        |channels| {
            ids.extend(channels.into_iter().map(|c| c.into_model().id));
            true
        },
    )
    .await?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(latest: &str, mentions: u32, direct: bool) -> Known {
        Known {
            latest: Some(Ts::new(latest)),
            last_read: Some(Ts::new("1.0")),
            mentions,
            direct,
        }
    }

    fn map(entries: &[(&str, Known)]) -> HashMap<String, Known> {
        entries
            .iter()
            .map(|(id, k)| ((*id).to_owned(), k.clone()))
            .collect()
    }

    fn fetched(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .filter(|s| s.fetch)
            .map(|s| s.channel.as_str())
            .collect()
    }

    fn reported(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .filter(|s| !s.fetch)
            .map(|s| s.channel.as_str())
            .collect()
    }

    #[test]
    fn the_counts_name_every_conversation_and_its_kind() {
        let counts: types::ClientCounts = serde_json::from_str(
            r#"{"ok":true,
                "channels":[{"id":"C1","last_read":"1.0","latest":"2.0","mention_count":3}],
                "mpims":[{"id":"G1","last_read":"0000000000.000000","latest":""}],
                "ims":[{"id":"D1","last_read":"5.0","latest":"6.0"},{"id":""}]}"#,
        )
        .expect("parses");
        let now = snapshot(counts);
        assert_eq!(now.len(), 3);
        assert_eq!(
            now["C1"],
            Known {
                latest: Some(Ts::new("2.0")),
                last_read: Some(Ts::new("1.0")),
                mentions: 3,
                direct: false,
            }
        );
        assert!(now["G1"].direct && now["G1"].latest.is_none());
        assert!(now["D1"].direct);
    }

    #[test]
    fn the_first_look_is_only_a_record() {
        let now = map(&[
            ("D1", known("5.0", 1, true)),
            ("C1", known("9.0", 2, false)),
        ]);
        let steps = plan(None, &now, None, FETCHES);
        assert!(fetched(&steps).is_empty());
        assert_eq!(reported(&steps), ["C1", "D1"]);
    }

    #[test]
    fn only_what_moved_and_would_notify_is_fetched() {
        let before = map(&[
            ("D1", known("5.0", 0, true)),
            ("D2", known("5.0", 0, true)),
            ("C1", known("5.0", 0, false)),
            ("C2", known("5.0", 1, false)),
            ("C3", known("5.0", 0, false)),
        ]);
        let mut read = known("5.0", 0, false);
        read.last_read = Some(Ts::new("5.0"));
        let now = map(&[
            // A new direct message.
            ("D1", known("6.0", 1, true)),
            // Nothing new.
            ("D2", known("5.0", 0, true)),
            // A new message, with no mention: reported, not fetched.
            ("C1", known("6.0", 0, false)),
            // A new mention.
            ("C2", known("6.0", 2, false)),
            // Read elsewhere: reported, not fetched.
            ("C3", read),
            // New to the record, a direct message: fetched.
            ("D3", known("2.0", 0, true)),
        ]);
        let steps = plan(Some(&before), &now, None, FETCHES);
        assert_eq!(fetched(&steps), ["D1", "D3", "C2"]);
        assert_eq!(reported(&steps), ["C1", "C3"]);
        // Fetches go first, so their pages come before the new state.
        assert!(steps[..3].iter().all(|s| s.fetch));
    }

    #[test]
    fn the_open_conversation_and_the_overflow_are_not_fetched() {
        let before = map(&[("D1", known("5.0", 0, true)), ("D2", known("5.0", 0, true))]);
        let now = map(&[
            ("D1", known("6.0", 1, true)),
            ("D2", known("6.0", 1, true)),
            ("D3", known("6.0", 1, true)),
        ]);
        // D1 is on screen: reported, its own poll fetches it.
        let steps = plan(Some(&before), &now, Some("D1"), 1);
        assert_eq!(fetched(&steps), ["D2"]);
        // D3 waits for the next round, unreported, so it still looks moved.
        assert_eq!(reported(&steps), ["D1"]);
    }

    #[test]
    fn checks_take_the_busiest_then_each_in_turn() {
        let direct: Vec<String> = ["D1", "D2", "D3", "D4", "D5"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let known = map(&[
            ("D1", known("1.0", 0, true)),
            ("D2", known("9.0", 0, true)),
            ("D3", known("5.0", 0, true)),
        ]);
        let checked: HashMap<String, u64> = [("D1", 3), ("D2", 3), ("D3", 3), ("D4", 2)]
            .iter()
            .map(|(id, round)| ((*id).to_owned(), *round))
            .collect();
        // The busiest, then never checked (D5), then longest ago (D4).
        assert_eq!(pick(&direct, &known, &checked, 3, 1), ["D2", "D5", "D4"]);
        assert_eq!(pick(&direct, &known, &checked, 2, 2), ["D2", "D3"]);
        assert_eq!(pick(&direct, &known, &checked, 10, 2).len(), 5);
        assert!(pick(&[], &known, &checked, 10, 2).is_empty());
    }

    #[test]
    fn failures_lengthen_the_rest_up_to_a_cap() {
        assert_eq!(rest(SESSION_EVERY, 0), SESSION_EVERY);
        assert_eq!(rest(SESSION_EVERY, 1), SESSION_EVERY * 2);
        assert_eq!(rest(SESSION_EVERY, 2), SESSION_EVERY * 4);
        assert_eq!(rest(SESSION_EVERY, 50), MAX_REST);
        assert_eq!(rest(OAUTH_EVERY, u32::MAX), MAX_REST);
    }
}
