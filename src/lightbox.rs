//! The image viewer: which pictures it steps through, and how far one is
//! zoomed and moved. Drawing is in `ui::lightbox`; what is here is plain
//! arithmetic, so it can be tested.

use egui::Vec2;

use crate::model::{File, Message, Ts};

/// The largest zoom, as a multiple of the size that fits the window.
pub const MAX_ZOOM: f32 = 8.0;
/// The smallest zoom: a little smaller than fitting, never a speck.
pub const MIN_ZOOM: f32 = 0.5;
/// How much one press of + or - zooms.
pub const ZOOM_STEP: f32 = 1.25;
/// Files larger than this are shown from their thumbnail: the image
/// loader refuses anything bigger anyway.
const MAX_FULL_BYTES: u64 = 24 * 1024 * 1024;
/// Animations this large take seconds to decode and much memory; their
/// thumbnail is shown instead, as in the message itself.
const MAX_FULL_GIF_BYTES: u64 = 8 * 1024 * 1024;

/// One picture the viewer can show.
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    /// The image at full size, as an egui image URI.
    pub uri: String,
    /// The thumbnail shown in the message, which is usually loaded
    /// already: it stands in while the full image arrives.
    pub thumb: Option<String>,
    /// The picture's own size in pixels, when Slack said, so it is placed
    /// right before it has loaded.
    pub size: Option<[f32; 2]>,
    pub name: String,
    /// Where to download it from, with your token.
    pub download: Option<String>,
    /// The file's page in Slack, for the browser.
    pub permalink: Option<String>,
    /// The message and file it came from, to find it in the gallery.
    pub source: Option<(Ts, String)>,
}

/// How a picture is zoomed and moved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    /// A multiple of the size that fits the window.
    pub zoom: f32,
    /// How far the picture's centre is from the window's.
    pub pan: Vec2,
}

impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: Vec2::ZERO,
        }
    }
}

impl View {
    /// Zooms by `factor`, keeping the point `at` (from the view's centre)
    /// over the same spot of the picture, as zooming under the pointer
    /// should.
    pub fn zoom_at(&mut self, factor: f32, at: Vec2) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let applied = zoom / self.zoom;
        self.pan = at - (at - self.pan) * applied;
        self.zoom = zoom;
    }

    /// Keeps a picture `size` big (as drawn) from being moved off an area
    /// `area` big: it may move only as far as it overhangs, so one that
    /// fits stays centred.
    pub fn clamp_pan(&mut self, size: Vec2, area: Vec2) {
        let limit = ((size - area) / 2.0).max(Vec2::ZERO);
        self.pan = self.pan.clamp(-limit, limit);
    }
}

/// The scale that fits a picture `natural` pixels big into `area`, never
/// enlarging it past its own size, where it would only blur.
pub fn fit(natural: Vec2, area: Vec2) -> f32 {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return 1.0;
    }
    (area.x / natural.x).min(area.y / natural.y).clamp(0.0, 1.0)
}

/// The open viewer: the pictures, the one shown and its view.
#[derive(Clone, Debug, PartialEq)]
pub struct Lightbox {
    pub pictures: Vec<Picture>,
    pub index: usize,
    pub view: View,
}

impl Lightbox {
    /// A viewer of `pictures` on the one at `index`, or `None` when there
    /// is nothing to show.
    pub fn new(pictures: Vec<Picture>, index: usize) -> Option<Self> {
        if pictures.is_empty() {
            return None;
        }
        let index = index.min(pictures.len() - 1);
        Some(Self {
            pictures,
            index,
            view: View::default(),
        })
    }

    /// The picture shown.
    pub fn current(&self) -> Option<&Picture> {
        self.pictures.get(self.index)
    }

    /// Moves `by` pictures on (back when negative), stopping at either
    /// end, and starts the new one fitted. Returns whether it moved.
    pub fn step(&mut self, by: isize) -> bool {
        let last = self.pictures.len().saturating_sub(1);
        let index = self.index.saturating_add_signed(by).min(last);
        if index == self.index {
            return false;
        }
        self.index = index;
        self.view = View::default();
        true
    }
}

/// The full-size URI of an image file, or its thumbnail when the full
/// file is too large to load.
fn full_uri(team: &str, file: &File) -> Option<String> {
    let thumb = file.thumb.as_deref();
    let too_large = file.size > MAX_FULL_BYTES
        || (file.mimetype.contains("gif") && file.size >= MAX_FULL_GIF_BYTES);
    let url = file
        .url_private
        .as_deref()
        .filter(|_| !too_large)
        .or(thumb)?;
    Some(crate::ui::image_uri(team, url))
}

/// The viewer's picture for an image file in message `ts`.
pub fn picture(team: &str, ts: &Ts, file: &File) -> Option<Picture> {
    if !file.is_image() {
        return None;
    }
    Some(Picture {
        uri: full_uri(team, file)?,
        thumb: file.thumb.as_deref().map(|t| crate::ui::image_uri(team, t)),
        size: file.original_size.or(file.thumb_size),
        name: file.name.clone(),
        download: file.download_url.clone().or(file.url_private.clone()),
        permalink: file.permalink.clone(),
        source: Some((ts.clone(), file.id.clone())),
    })
}

/// Every image file in `messages`, in order: what ← and → step through.
pub fn gallery<'a>(team: &str, messages: impl IntoIterator<Item = &'a Message>) -> Vec<Picture> {
    messages
        .into_iter()
        .flat_map(|message| {
            message
                .files
                .iter()
                .filter_map(|file| picture(team, &message.ts, file))
        })
        .collect()
}

/// A viewer over `messages`' images, opened on file `file` of message `ts`.
pub fn open<'a>(
    team: &str,
    messages: impl IntoIterator<Item = &'a Message>,
    ts: &Ts,
    file: &str,
) -> Option<Lightbox> {
    let pictures = gallery(team, messages);
    let index = pictures
        .iter()
        .position(|p| p.source.as_ref().is_some_and(|(t, f)| t == ts && f == file))?;
    Lightbox::new(pictures, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str, size: u64, mimetype: &str) -> File {
        File {
            id: id.into(),
            name: format!("{id}.png"),
            mimetype: mimetype.into(),
            size,
            url_private: Some(format!("https://files.slack.com/files-pri/T1-{id}/full")),
            thumb: Some(format!("https://files.slack.com/files-tmb/T1-{id}/thumb")),
            thumb_size: Some([360.0, 240.0]),
            ..File::default()
        }
    }

    fn message(ts: &str, files: Vec<File>) -> Message {
        Message {
            ts: Ts::new(ts),
            user: Some("U1".into()),
            username: None,
            bot_icon: None,
            bot_id: None,
            text: String::new(),
            thread_ts: None,
            reply_count: 0,
            replies_known: false,
            reply_users: Vec::new(),
            latest_reply: None,
            reactions: Vec::new(),
            files,
            attachments: Vec::new(),
            blocks: Vec::new(),
            edited: false,
            subtype: None,
            delivery: crate::model::Delivery::Sent,
            broadcast: false,
            pinned: false,
            client_msg_id: None,
            subscribed: None,
        }
    }

    #[test]
    fn the_gallery_holds_the_images_in_order() {
        let pdf = File {
            id: "F9".into(),
            mimetype: "application/pdf".into(),
            ..File::default()
        };
        let messages = [
            message("1.0", vec![image("F1", 10, "image/png"), pdf]),
            message("2.0", vec![]),
            message("3.0", vec![image("F2", 10, "image/jpeg")]),
        ];
        let lightbox = open("T1", &messages, &Ts::new("3.0"), "F2").expect("found");
        assert_eq!(lightbox.pictures.len(), 2);
        assert_eq!(lightbox.index, 1);
        let first = &lightbox.pictures[0];
        assert_eq!(
            first.uri,
            "nsauth:T1:https://files.slack.com/files-pri/T1-F1/full"
        );
        assert!(
            first
                .thumb
                .as_deref()
                .is_some_and(|t| t.ends_with("/thumb"))
        );
        assert!(open("T1", &messages, &Ts::new("1.0"), "F9").is_none());
    }

    #[test]
    fn files_too_large_to_load_show_their_thumbnail() {
        let huge = image("F1", 30 * 1024 * 1024, "image/png");
        let big_gif = image("F2", 9 * 1024 * 1024, "image/gif");
        let small_gif = image("F3", 1024, "image/gif");
        let ts = Ts::new("1.0");
        let uri = |file: &File| picture("T1", &ts, file).map(|p| p.uri);
        assert!(uri(&huge).is_some_and(|u| u.ends_with("/thumb")));
        assert!(uri(&big_gif).is_some_and(|u| u.ends_with("/thumb")));
        assert!(uri(&small_gif).is_some_and(|u| u.ends_with("/full")));
    }

    #[test]
    fn stepping_stops_at_either_end_and_resets_the_view() {
        let messages = [message(
            "1.0",
            vec![image("F1", 1, "image/png"), image("F2", 1, "image/png")],
        )];
        let mut lightbox = open("T1", &messages, &Ts::new("1.0"), "F1").expect("found");
        lightbox.view.zoom = 3.0;
        assert!(!lightbox.step(-1), "already the first");
        assert_eq!(lightbox.view.zoom, 3.0, "a step that goes nowhere keeps it");
        assert!(lightbox.step(1));
        assert_eq!((lightbox.index, lightbox.view), (1, View::default()));
        assert!(!lightbox.step(1), "already the last");
        assert!(Lightbox::new(Vec::new(), 0).is_none());
    }

    #[test]
    fn pictures_fit_without_growing() {
        let area = Vec2::new(800.0, 600.0);
        assert_eq!(fit(Vec2::new(1600.0, 600.0), area), 0.5);
        assert_eq!(fit(Vec2::new(400.0, 1200.0), area), 0.5);
        assert_eq!(fit(Vec2::new(100.0, 100.0), area), 1.0, "small stays small");
        assert_eq!(fit(Vec2::ZERO, area), 1.0);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let mut view = View::default();
        let at = Vec2::new(100.0, -50.0);
        view.zoom_at(2.0, at);
        assert_eq!(view.zoom, 2.0);
        // The spot under the pointer was 100 to the right of the centre at
        // zoom 1; at zoom 2 it would be 200, so the picture moves by -100.
        assert_eq!(view.pan, Vec2::new(-100.0, 50.0));
        view.zoom_at(100.0, Vec2::ZERO);
        assert_eq!(view.zoom, MAX_ZOOM);
        view.zoom_at(0.0001, Vec2::ZERO);
        assert_eq!(view.zoom, MIN_ZOOM);
        let before = view;
        view.zoom_at(f32::NAN, Vec2::ZERO);
        assert_eq!(view, before);
    }

    #[test]
    fn a_picture_moves_only_as_far_as_it_overhangs() {
        let area = Vec2::new(800.0, 600.0);
        let mut view = View {
            zoom: 2.0,
            pan: Vec2::new(500.0, -500.0),
        };
        view.clamp_pan(Vec2::new(1000.0, 400.0), area);
        assert_eq!(view.pan, Vec2::new(100.0, 0.0));
    }
}
