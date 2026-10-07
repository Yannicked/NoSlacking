//! Camera tiles: who has a camera on, which of them the call window
//! shows, and at which simulcast layer. All of it is plain data, decided
//! here and tested without a socket; `watch` acts on it.
//!
//! The policy (docs/research/huddle-video.md, Stage 2):
//! - Nothing is received while the call window is closed.
//! - At most [`MAX_TILES`] tiles, fewer if the window has room for fewer
//!   (it says how many in its [`Wish`]).
//! - If everyone fits, everyone gets a tile.
//! - Otherwise, the people with a tile keep it, in its place, so the grid
//!   does not reshuffle; free places go to whoever spoke last, then to
//!   the rest in the order Chime listed them. Someone without a tile who
//!   speaks takes the place of the person with a tile who has been
//!   silent longest, unless that person also spoke within [`RECENT`]:
//!   two people talking in turn both stay.
//! - A paused camera keeps its tile (showing the person's picture or
//!   initials) and its m-line while nobody else needs them, so it comes
//!   back at once on RESUME; any camera that is on takes its place
//!   first.
//! - Our own camera is never received.
//! - Per person, the smallest layer that still covers the tile; the
//!   largest when none does.
//! - A change goes out only once it has stood still for [`DEBOUNCE`] (and
//!   at most every few seconds, as `watch` rate-limits re-SUBSCRIBE).

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use super::video::Index;

/// The most camera tiles the call window shows.
pub const MAX_TILES: usize = 9;
/// Someone who spoke this recently counts as speaking for the tiles.
pub const RECENT: Duration = Duration::from_secs(5);
/// A new choice of streams waits this long without changing before it
/// is asked for, so a window being resized or a quick word does not
/// cost a renegotiation each.
pub const DEBOUNCE: Duration = Duration::from_millis(400);

/// What the call window asks the session to receive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wish {
    /// Whether the window is open. Closed, nothing is received.
    pub open: bool,
    /// The share it shows large, by key, if any.
    pub share: Option<String>,
    /// How many camera tiles it has room for.
    pub tiles: usize,
    /// A tile's size in pixels, for the layer; 0×0 when not known yet.
    pub tile: [u32; 2],
}

impl Wish {
    /// The window closed: nothing at all.
    pub fn closed() -> Self {
        Self::default()
    }
}

/// One simulcast layer of a camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    /// Chime's stream id.
    pub stream_id: u32,
    /// Its width in pixels, as announced.
    pub width: u32,
    /// Its height.
    pub height: u32,
    /// The most it sends, in kbit/s.
    pub max_kbps: u32,
}

/// A camera that is on, as INDEX lists it: someone else's video that is
/// not a screen share, with all its layers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    /// Its sender's attendee id, the key it goes by.
    pub key: String,
    /// The Slack user, when Chime's external id names one.
    pub user: Option<String>,
    /// Its layers, in INDEX's order.
    pub layers: Vec<Layer>,
    /// Whether the sender has paused it (all its layers).
    pub paused: bool,
}

/// A camera as the interface hears of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Camera {
    /// Its sender's attendee id.
    pub key: String,
    /// The Slack user.
    pub user: Option<String>,
    /// Paused by its sender: the tile shows who it is instead.
    pub paused: bool,
    /// Whether it has a tile in the call window now.
    pub tile: bool,
}

/// The streams paused at their source: INDEX's list, then PAUSE and
/// RESUME as they come, until the next INDEX says again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pauses {
    streams: BTreeSet<u32>,
}

impl Pauses {
    /// A new INDEX: its list is what is paused now. Gives the streams
    /// that were paused and no longer are.
    pub fn index(&mut self, index: &Index) -> Vec<u32> {
        let now: BTreeSet<u32> = index.paused.iter().copied().collect();
        let resumed = self.streams.difference(&now).copied().collect();
        self.streams = now;
        resumed
    }

    /// The streams a PAUSE or RESUME names, by stream or by group (each
    /// of its layers).
    fn named(streams: &[u32], groups: &[u32], index: &Index) -> Vec<u32> {
        let mut named = streams.to_vec();
        named.extend(
            index
                .sources
                .iter()
                .filter(|s| groups.contains(&s.group_id))
                .map(|s| s.stream_id),
        );
        named
    }

    /// PAUSE: these streams stopped at their source.
    pub fn pause(&mut self, streams: &[u32], groups: &[u32], index: &Index) {
        self.streams.extend(Self::named(streams, groups, index));
    }

    /// RESUME: these streams go again. Gives those that were paused.
    pub fn resume(&mut self, streams: &[u32], groups: &[u32], index: &Index) -> Vec<u32> {
        Self::named(streams, groups, index)
            .into_iter()
            .filter(|s| self.streams.remove(s))
            .collect()
    }

    /// Whether `stream` is paused.
    pub fn paused(&self, stream: u32) -> bool {
        self.streams.contains(&stream)
    }
}

/// The cameras on now, others' only, one per sender in the order INDEX
/// first lists them.
pub fn feeds(index: &Index, me: &str, pauses: &Pauses) -> Vec<Feed> {
    let mut feeds: Vec<Feed> = Vec::new();
    for source in index
        .sources
        .iter()
        .filter(|s| s.video && !s.is_share() && !s.is_ours(me))
    {
        let layer = Layer {
            stream_id: source.stream_id,
            width: source.width,
            height: source.height,
            max_kbps: source.max_kbps,
        };
        match feeds.iter_mut().find(|f| f.key == source.attendee_id) {
            Some(feed) => {
                if feed.user.is_none() {
                    feed.user.clone_from(&source.user);
                }
                feed.layers.push(layer);
            }
            None => feeds.push(Feed {
                key: source.attendee_id.clone(),
                user: source.user.clone(),
                layers: vec![layer],
                paused: false,
            }),
        }
    }
    for feed in &mut feeds {
        feed.paused = feed.layers.iter().all(|l| pauses.paused(l.stream_id));
    }
    feeds
}

/// The layer to receive for a tile of `tile` pixels: the smallest that
/// covers it both ways, so no more is decoded than is shown; the largest
/// (by size, then bitrate) when none does or the size is not known.
pub fn layer(feed: &Feed, tile: [u32; 2]) -> Option<u32> {
    let area = |l: &Layer| u64::from(l.width) * u64::from(l.height);
    let largest = feed
        .layers
        .iter()
        .max_by_key(|l| (area(l), l.max_kbps, Reverse(l.stream_id)));
    if tile[0] == 0 || tile[1] == 0 {
        return largest.map(|l| l.stream_id);
    }
    feed.layers
        .iter()
        .filter(|l| l.width >= tile[0] && l.height >= tile[1])
        .min_by_key(|l| (area(l), Reverse(l.max_kbps), l.stream_id))
        .or(largest)
        .map(|l| l.stream_id)
}

/// Who gets the `n` tiles (at most [`MAX_TILES`]), in tile order, given
/// who has them now (`shown`) and when each attendee last spoke
/// (`spoke`), at `now`. See the module's notes for the rules.
pub fn pick(
    feeds: &[Feed],
    shown: &[String],
    spoke: &HashMap<String, Instant>,
    n: usize,
    now: Instant,
) -> Vec<String> {
    let n = n.min(MAX_TILES);
    if n == 0 {
        return Vec::new();
    }
    let position = |key: &str| shown.iter().position(|s| s == key);
    if feeds.len() <= n {
        // Everyone fits: those with a tile keep their place, then the
        // rest as INDEX lists them.
        let mut all: Vec<&Feed> = feeds.iter().collect();
        all.sort_by_key(|f| position(&f.key).unwrap_or(usize::MAX));
        return all.into_iter().map(|f| f.key.clone()).collect();
    }
    let last = |feed: &Feed| spoke.get(&feed.key).copied();
    let recent =
        |feed: &Feed| last(feed).is_some_and(|at| now.saturating_duration_since(at) < RECENT);
    let order = |key: &str| {
        feeds
            .iter()
            .position(|f| f.key == key)
            .unwrap_or(usize::MAX)
    };
    // Best first: on, then the latest to speak, then INDEX's order.
    let rank = |a: &Feed, b: &Feed| -> Ordering {
        a.paused
            .cmp(&b.paused)
            .then_with(|| last(b).cmp(&last(a)))
            .then_with(|| order(&a.key).cmp(&order(&b.key)))
    };
    let mut kept: Vec<&Feed> = shown
        .iter()
        .filter_map(|key| feeds.iter().find(|f| f.key == *key))
        .collect();
    // Fewer places than before: the worst go.
    while kept.len() > n {
        let Some(worst) = (0..kept.len()).max_by(|&i, &j| rank(kept[i], kept[j])) else {
            break;
        };
        kept.remove(worst);
    }
    let mut rest: Vec<&Feed> = feeds
        .iter()
        .filter(|f| !kept.iter().any(|k| k.key == f.key))
        .collect();
    rest.sort_by(|a, b| rank(a, b));
    let mut rest = rest.into_iter();
    // Free places go to the best of the rest.
    while kept.len() < n {
        match rest.next() {
            Some(feed) => kept.push(feed),
            None => break,
        }
    }
    // Then the best of the rest may take the worst place, one at a time.
    for candidate in rest {
        if candidate.paused {
            break;
        }
        let Some(worst) = (0..kept.len()).max_by(|&i, &j| rank(kept[i], kept[j])) else {
            break;
        };
        let incumbent = kept[worst];
        let takes = incumbent.paused || (recent(candidate) && !recent(incumbent));
        if !takes {
            break;
        }
        kept[worst] = candidate;
    }
    kept.into_iter().map(|f| f.key.clone()).collect()
}

/// The cameras for the interface: those with a tile first, in tile
/// order, then the others as INDEX lists them.
pub fn cameras(feeds: &[Feed], tiles: &[String]) -> Vec<Camera> {
    let mut all: Vec<Camera> = feeds
        .iter()
        .map(|f| Camera {
            key: f.key.clone(),
            user: f.user.clone(),
            paused: f.paused,
            tile: tiles.contains(&f.key),
        })
        .collect();
    all.sort_by_key(|c| tiles.iter().position(|t| *t == c.key).unwrap_or(usize::MAX));
    all
}

/// Holds a new choice of streams back until it has stood still for
/// [`DEBOUNCE`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Debounce {
    last: Option<(Vec<u32>, Instant)>,
}

impl Debounce {
    /// Whether `wanted` has stood still long enough at `now` to be asked
    /// for; notes it when it is new.
    pub fn settled(&mut self, wanted: &[u32], now: Instant) -> bool {
        match &self.last {
            Some((last, since)) if last == wanted => now >= *since + DEBOUNCE,
            _ => {
                self.last = Some((wanted.to_vec(), now));
                false
            }
        }
    }

    /// When `wanted` will have stood still long enough, if nothing
    /// changes.
    pub fn ready_at(&self, wanted: &[u32], now: Instant) -> Instant {
        match &self.last {
            Some((last, since)) if last == wanted => *since + DEBOUNCE,
            _ => now + DEBOUNCE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddle_audio::chime::proto;

    fn source(
        stream: u32,
        attendee: &str,
        size: (u32, u32),
        kbps: u32,
    ) -> proto::SdkStreamDescriptor {
        proto::SdkStreamDescriptor {
            stream_id: Some(stream),
            group_id: Some(stream / 10),
            attendee_id: Some(attendee.into()),
            external_user_id: Some(format!("T1-R1-U{}", attendee.to_uppercase())),
            media_type: Some(proto::SdkStreamMediaType::Video as i32),
            max_bitrate_kbps: Some(kbps),
            width: Some(size.0),
            height: Some(size.1),
            framerate: Some(22),
            ..Default::default()
        }
    }

    fn index(sources: Vec<proto::SdkStreamDescriptor>, paused: Vec<u32>) -> Index {
        Index::of(&proto::SdkIndexFrame {
            sources,
            paused_at_source_ids: paused,
            ..Default::default()
        })
    }

    fn feed(key: &str) -> Feed {
        Feed {
            key: key.into(),
            user: None,
            layers: vec![Layer {
                stream_id: 1,
                width: 480,
                height: 480,
                max_kbps: 500,
            }],
            paused: false,
        }
    }

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|&k| k.to_owned()).collect()
    }

    #[test]
    fn cameras_are_others_one_per_sender_with_their_layers() {
        let index = index(
            vec![
                source(10, "ana", (480, 480), 500),
                // Bob's share is not a camera; his camera is.
                source(20, "bob#content", (1920, 1080), 900),
                source(30, "bob", (320, 180), 150),
                source(31, "bob", (1280, 720), 1200),
                // Ours never is.
                source(40, "me", (480, 480), 500),
            ],
            vec![10],
        );
        let feeds = feeds(&index, "me", &{
            let mut pauses = Pauses::default();
            pauses.index(&index);
            pauses
        });
        let seen: Vec<(&str, usize, bool)> = feeds
            .iter()
            .map(|f| (f.key.as_str(), f.layers.len(), f.paused))
            .collect();
        assert_eq!(seen, [("ana", 1, true), ("bob", 2, false)]);
        assert_eq!(feeds[1].user.as_deref(), Some("UBOB"));
    }

    #[test]
    fn the_layer_is_the_smallest_that_covers_the_tile() {
        let mut bob = feed("bob");
        bob.layers = vec![
            Layer {
                stream_id: 30,
                width: 320,
                height: 180,
                max_kbps: 150,
            },
            Layer {
                stream_id: 31,
                width: 1280,
                height: 720,
                max_kbps: 1200,
            },
            Layer {
                stream_id: 32,
                width: 640,
                height: 360,
                max_kbps: 500,
            },
        ];
        // Unknown size: the largest.
        assert_eq!(layer(&bob, [0, 0]), Some(31));
        assert_eq!(layer(&bob, [200, 150]), Some(30));
        assert_eq!(layer(&bob, [300, 170]), Some(30));
        assert_eq!(layer(&bob, [300, 225]), Some(32));
        assert_eq!(layer(&bob, [700, 500]), Some(31));
        // Larger than any: the largest.
        assert_eq!(layer(&bob, [2000, 1500]), Some(31));
        // One layer: that one, whatever the tile.
        assert_eq!(layer(&feed("ana"), [100, 100]), Some(1));
        assert_eq!(
            layer(
                &Feed {
                    layers: vec![],
                    ..feed("x")
                },
                [1, 1]
            ),
            None
        );
    }

    #[test]
    fn everyone_fits_and_keeps_their_place() {
        let now = Instant::now();
        let feeds = vec![feed("a"), feed("b"), feed("c")];
        let none = HashMap::new();
        assert_eq!(pick(&feeds, &[], &none, 4, now), keys(&["a", "b", "c"]));
        // C had a tile first: C stays first.
        assert_eq!(
            pick(&feeds, &keys(&["c", "a"]), &none, 9, now),
            keys(&["c", "a", "b"])
        );
        // Closed or no room: nothing.
        assert!(pick(&feeds, &[], &none, 0, now).is_empty());
        // Never more than nine, however large the window.
        let many: Vec<Feed> = (0..12).map(|i| feed(&format!("p{i}"))).collect();
        assert_eq!(pick(&many, &[], &none, 25, now).len(), MAX_TILES);
    }

    #[test]
    fn speakers_come_first_and_take_the_longest_silent_place() {
        let now = Instant::now();
        let feeds: Vec<Feed> = ["a", "b", "c", "d", "e"].iter().map(|k| feed(k)).collect();
        let ago = |s: u64| now - Duration::from_secs(s);
        // D spoke a minute ago, E just now: the first two places are theirs.
        let spoke = HashMap::from([("d".to_owned(), ago(60)), ("e".to_owned(), ago(1))]);
        let first = pick(&feeds, &[], &spoke, 3, now);
        assert_eq!(first, keys(&["e", "d", "a"]));
        // The same again changes nothing.
        assert_eq!(pick(&feeds, &first, &spoke, 3, now), first);
        // B speaks: A, never heard, gives up its place, in place.
        let spoke = HashMap::from([
            ("d".to_owned(), ago(60)),
            ("e".to_owned(), ago(2)),
            ("b".to_owned(), ago(0)),
        ]);
        assert_eq!(pick(&feeds, &first, &spoke, 3, now), keys(&["e", "d", "b"]));
        // C speaks too, while E and B are both still recent: D, silent a
        // minute, goes, not them.
        let spoke = HashMap::from([
            ("d".to_owned(), ago(60)),
            ("e".to_owned(), ago(2)),
            ("b".to_owned(), ago(1)),
            ("c".to_owned(), ago(0)),
        ]);
        let shown = keys(&["e", "d", "b"]);
        assert_eq!(pick(&feeds, &shown, &spoke, 3, now), keys(&["e", "c", "b"]));
        // Everyone shown spoke recently: a new speaker waits.
        let spoke = HashMap::from([
            ("e".to_owned(), ago(2)),
            ("c".to_owned(), ago(1)),
            ("b".to_owned(), ago(1)),
            ("a".to_owned(), ago(0)),
        ]);
        let shown = keys(&["e", "c", "b"]);
        assert_eq!(pick(&feeds, &shown, &spoke, 3, now), shown);
        // Someone who never spoke does not push out someone who has.
        let spoke = HashMap::from([("e".to_owned(), ago(30))]);
        assert_eq!(pick(&feeds, &shown, &spoke, 3, now), shown);
        // Room for one fewer: the one silent longest goes.
        let spoke = HashMap::from([
            ("e".to_owned(), ago(30)),
            ("c".to_owned(), ago(10)),
            ("b".to_owned(), ago(20)),
        ]);
        assert_eq!(pick(&feeds, &shown, &spoke, 2, now), keys(&["c", "b"]));
        assert_eq!(pick(&feeds, &shown, &spoke, 1, now), keys(&["c"]));
    }

    #[test]
    fn a_paused_camera_keeps_its_tile_until_one_that_is_on_needs_it() {
        let now = Instant::now();
        let mut feeds: Vec<Feed> = ["a", "b", "c"].iter().map(|k| feed(k)).collect();
        feeds[0].paused = true;
        let none = HashMap::new();
        // Room for all: A keeps its tile, paused.
        assert_eq!(
            pick(&feeds, &keys(&["a", "b"]), &none, 3, now),
            keys(&["a", "b", "c"])
        );
        // Room for two: C, on, takes paused A's place, even unheard.
        assert_eq!(
            pick(&feeds, &keys(&["a", "b"]), &none, 2, now),
            keys(&["c", "b"])
        );
        // A paused camera never takes a place from one that is on, even
        // if its sender speaks.
        let spoke = HashMap::from([("a".to_owned(), now)]);
        assert_eq!(
            pick(&feeds, &keys(&["b", "c"]), &spoke, 2, now),
            keys(&["b", "c"])
        );
        // Fresh: the cameras that are on first.
        assert_eq!(pick(&feeds, &[], &spoke, 2, now), keys(&["b", "c"]));
    }

    #[test]
    fn pauses_follow_index_then_pause_and_resume() {
        let index = index(
            vec![
                source(10, "ana", (480, 480), 500),
                source(11, "ana", (240, 240), 150),
                source(20, "bob", (480, 480), 500),
            ],
            vec![20],
        );
        let mut pauses = Pauses::default();
        assert!(pauses.index(&index).is_empty());
        assert!(pauses.paused(20) && !pauses.paused(10));
        // Ana pauses, by group: both her layers.
        pauses.pause(&[], &[1], &index);
        assert!(pauses.paused(10) && pauses.paused(11));
        let cameras: Vec<bool> = feeds(&index, "me", &pauses)
            .iter()
            .map(|f| f.paused)
            .collect();
        assert_eq!(cameras, [true, true]);
        // Bob resumes: it says so, once.
        assert_eq!(pauses.resume(&[20], &[], &index), [20]);
        assert!(pauses.resume(&[20], &[], &index).is_empty());
        // The next INDEX says Ana is paused no more.
        let fresh = Index {
            paused: vec![],
            ..index.clone()
        };
        assert_eq!(pauses.index(&fresh), [10, 11]);
        assert!(!pauses.paused(10));
    }

    #[test]
    fn the_interface_hears_tiles_first() {
        let feeds: Vec<Feed> = ["a", "b", "c"].iter().map(|k| feed(k)).collect();
        let told = cameras(&feeds, &keys(&["c", "a"]));
        let seen: Vec<(&str, bool)> = told.iter().map(|c| (c.key.as_str(), c.tile)).collect();
        assert_eq!(seen, [("c", true), ("a", true), ("b", false)]);
        assert!(cameras(&feeds, &[]).iter().all(|c| !c.tile));
    }

    #[test]
    fn a_choice_waits_until_it_stands_still() {
        let now = Instant::now();
        let mut debounce = Debounce::default();
        assert!(!debounce.settled(&[1, 2], now));
        assert_eq!(debounce.ready_at(&[1, 2], now), now + DEBOUNCE);
        assert!(!debounce.settled(&[1, 2], now + DEBOUNCE / 2));
        // It changed: the wait starts over.
        let later = now + DEBOUNCE / 2;
        assert!(!debounce.settled(&[1, 3], later));
        assert!(!debounce.settled(&[1, 3], now + DEBOUNCE));
        assert!(debounce.settled(&[1, 3], later + DEBOUNCE));
        assert_eq!(debounce.ready_at(&[9], later), later + DEBOUNCE);
    }
}
