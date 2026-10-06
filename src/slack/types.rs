//! Slack's JSON, as the Web API and Socket Mode send it, and its translation
//! into [`crate::model`].
//!
//! Every field is optional or defaulted: Slack leaves fields out freely, and
//! a missing field must never lose a whole page of messages.

use serde::Deserialize;
use serde_json::Value;

use crate::model::{self, ConversationKind, Delivery, Ts};

/// A field's value, with an explicit `null` read as its default. Slack
/// sends `null` where it would usually leave a field out, and
/// `#[serde(default)]` only covers a missing field: without this, one
/// `null` would fail a whole page.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ResponseMetadata {
    pub next_cursor: String,
}

impl ResponseMetadata {
    pub fn cursor(&self) -> Option<String> {
        (!self.next_cursor.is_empty()).then(|| self.next_cursor.clone())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AuthTest {
    pub url: String,
    pub team: String,
    pub user: String,
    pub team_id: String,
    pub user_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TeamInfo {
    pub team: Team,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub domain: String,
    pub icon: TeamIcon,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TeamIcon {
    pub image_68: Option<String>,
    pub image_88: Option<String>,
    pub image_132: Option<String>,
    pub image_default: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TextValue {
    pub value: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub is_channel: bool,
    pub is_group: bool,
    pub is_im: bool,
    pub is_mpim: bool,
    pub is_private: bool,
    pub is_archived: bool,
    pub is_member: Option<bool>,
    pub user: Option<String>,
    pub topic: TextValue,
    pub purpose: TextValue,
    pub num_members: Option<u32>,
    pub last_read: Option<String>,
    /// A message object in `conversations.info`, sometimes absent.
    pub latest: Option<Value>,
    pub unread_count: Option<u32>,
    pub unread_count_display: Option<u32>,
    /// Shared with another organization (Slack Connect), or invited to be.
    pub is_ext_shared: bool,
    pub is_pending_ext_shared: bool,
    /// Whether a direct message or group DM is open in your sidebar.
    /// Slack's docs show it on `users.conversations`' group DMs and on
    /// `conversations.info`'s direct messages; absent elsewhere.
    pub is_open: Option<bool>,
}

impl Channel {
    pub fn kind(&self) -> ConversationKind {
        if self.is_im {
            ConversationKind::Direct
        } else if self.is_mpim {
            ConversationKind::Group
        } else if self.is_private || self.is_group {
            ConversationKind::Private
        } else {
            ConversationKind::Channel
        }
    }

    pub fn latest_ts(&self) -> Option<Ts> {
        match self.latest.as_ref()? {
            Value::Object(object) => object.get("ts")?.as_str().map(Ts::new),
            Value::String(ts) => Some(Ts::new(ts.clone())),
            _ => None,
        }
    }

    pub fn into_model(self) -> model::Conversation {
        let kind = self.kind();
        let latest = self.latest_ts();
        let last_read = self
            .last_read
            .filter(|ts| !ts.is_empty() && ts != "0000000000.000000")
            .map(Ts::new);
        model::Conversation {
            name: if kind == ConversationKind::Group {
                group_name(&self.name)
            } else {
                self.name
            },
            id: self.id,
            kind,
            user: self.user,
            topic: self.topic.value,
            purpose: self.purpose.value,
            members: self.num_members,
            archived: self.is_archived,
            last_read,
            latest,
            unread: self.unread_count_display.or(self.unread_count).unwrap_or(0),
            mentions: 0,
            external: self.is_ext_shared || self.is_pending_ext_shared,
            // Only direct messages and group DMs open and close.
            is_open: self.is_open.filter(|_| kind.is_dm()),
            empty: false,
        }
    }
}

/// `mpdm-ana--bob--carla-1` reads as `ana, bob, carla`.
fn group_name(name: &str) -> String {
    let inner = name.strip_prefix("mpdm-").unwrap_or(name);
    let inner = inner
        .rsplit_once('-')
        .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
        .map_or(inner, |(rest, _)| rest);
    inner.split("--").collect::<Vec<_>>().join(", ")
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConversationsPage {
    pub channels: Vec<Channel>,
    pub response_metadata: ResponseMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChannelInfo {
    pub channel: Channel,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    pub has_more: bool,
    pub response_metadata: ResponseMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Reaction {
    pub name: String,
    pub count: u32,
    pub users: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct File {
    pub id: String,
    pub name: String,
    pub title: String,
    pub mimetype: String,
    pub size: u64,
    pub mode: String,
    /// Who uploaded it.
    pub user: Option<String>,
    pub url_private: Option<String>,
    pub url_private_download: Option<String>,
    pub permalink: Option<String>,
    // Sizes are read loosely (see `size_of`): exports carry them as text,
    // or as "" when there is none, and one odd size must not lose the
    // message.
    pub thumb_360: Option<String>,
    pub thumb_360_w: Option<Value>,
    pub thumb_360_h: Option<Value>,
    pub thumb_480: Option<String>,
    pub thumb_480_w: Option<Value>,
    pub thumb_480_h: Option<Value>,
    pub thumb_720: Option<String>,
    pub thumb_720_w: Option<Value>,
    pub thumb_720_h: Option<Value>,
    pub original_w: Option<Value>,
    pub original_h: Option<Value>,
    pub thumb_video: Option<String>,
    pub thumb_video_w: Option<Value>,
    pub thumb_video_h: Option<Value>,
    pub thumb_pdf: Option<String>,
    pub thumb_pdf_w: Option<Value>,
    pub thumb_pdf_h: Option<Value>,
    // What Slack adds for previews, all read loosely: a field missing,
    // `null` or of another type than expected only means no preview.
    /// Slack's kind for it: `python`, `json`, `xlsx`.
    pub filetype: Option<Value>,
    /// `slack_audio` for a voice clip recorded in Slack.
    pub subtype: Option<Value>,
    /// A snippet's or text file's first lines, plain.
    pub preview: Option<Value>,
    pub preview_plain_text: Option<Value>,
    pub preview_is_truncated: Option<Value>,
    pub lines: Option<Value>,
    pub lines_more: Option<Value>,
    /// A PDF Slack made of an Office document.
    pub converted_pdf: Option<Value>,
    /// A smaller copy of a video.
    pub mp4_low: Option<Value>,
    /// An MP4 copy of an old WebM voice clip.
    pub aac: Option<Value>,
    pub duration_ms: Option<Value>,
    pub audio_wave_samples: Option<Value>,
    /// `{ status, locale, preview: { content, has_more } }`.
    pub transcription: Option<Value>,
}

/// A string from loosely typed JSON, when it is one and not blank.
fn loose_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// A count from loosely typed JSON: a whole number, or one written as
/// text.
fn loose_count(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The kinds Slack gives files whose text reads as it is (`filetype`).
const TEXT_TYPES: &[&str] = &[
    "text",
    "markdown",
    "post",
    "csv",
    "tsv",
    "json",
    "yaml",
    "xml",
    "html",
    "css",
    "javascript",
    "typescript",
    "python",
    "ruby",
    "rust",
    "go",
    "java",
    "kotlin",
    "swift",
    "c",
    "cpp",
    "csharp",
    "php",
    "shell",
    "bash",
    "powershell",
    "sql",
    "diff",
    "dockerfile",
    "toml",
    "ini",
    "log",
    "perl",
    "lua",
    "r",
    "scala",
    "groovy",
    "haskell",
    "clojure",
    "elixir",
    "erlang",
    "dart",
    "objc",
    "matlab",
    "ocaml",
    "fsharp",
    "vb",
    "verilog",
    "vhdl",
    "latex",
    "puppet",
    "smalltalk",
    "tcl",
    "apex",
    "coffeescript",
    "d",
    "lisp",
    "pascal",
    "scheme",
    "vbscript",
];

/// Whether a file is text that Slack previews as its first lines: a
/// snippet, or a text, data or code file. Pictures, sound, video and PDFs
/// never are, whatever a stray `preview` field says.
fn is_text_like(mode: &str, mimetype: &str, filetype: &str) -> bool {
    let mimetype = mimetype.to_ascii_lowercase();
    if ["image/", "video/", "audio/"]
        .iter()
        .any(|kind| mimetype.starts_with(kind))
        || mimetype == "application/pdf"
    {
        return false;
    }
    mode == "snippet"
        || mimetype.starts_with("text/")
        || matches!(
            mimetype.as_str(),
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/javascript"
                | "application/x-sh"
                | "application/sql"
        )
        || TEXT_TYPES.contains(&filetype.to_ascii_lowercase().as_str())
}

impl File {
    /// What Slack previews of a text file: `preview`, else
    /// `preview_plain_text`, with how much of the file it leaves out.
    fn text_preview(&self, filetype: &str) -> Option<model::TextPreview> {
        if !is_text_like(&self.mode, &self.mimetype, filetype) {
            return None;
        }
        let text = loose_str(self.preview.as_ref())
            .or_else(|| loose_str(self.preview_plain_text.as_ref()))?
            .replace("\r\n", "\n")
            .replace('\t', "    ");
        let count = |value: &Option<Value>| {
            loose_count(value.as_ref()).map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        };
        Some(model::TextPreview {
            text,
            lines_more: count(&self.lines_more),
            lines: count(&self.lines),
            truncated: self
                .preview_is_truncated
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// A voice clip's loudness over time, each sample clamped to 0..=100;
    /// a sample that is not a number reads as silence, so the rest keep
    /// their place.
    fn wave(&self) -> Vec<u8> {
        let Some(Value::Array(samples)) = &self.audio_wave_samples else {
            return Vec::new();
        };
        samples
            .iter()
            .take(1000)
            .map(|sample| {
                let level = sample.as_f64().filter(|n| n.is_finite()).unwrap_or(0.0);
                level.clamp(0.0, 100.0).round() as u8
            })
            .collect()
    }

    /// The start of what Slack heard, once it has written it down.
    fn transcript(&self) -> Option<String> {
        let content = self.transcription.as_ref()?.get("preview")?.get("content");
        loose_str(content).map(|text| text.trim().to_owned())
    }

    pub fn into_model(self) -> Option<model::File> {
        // Files past the free plan's limit show nothing.
        if self.mode == "hidden_by_limit" || self.id.is_empty() {
            return None;
        }
        // A deleted one keeps its place, as in Slack: "This file was
        // deleted".
        if self.mode == "tombstone" {
            return Some(model::File {
                id: self.id,
                deleted: true,
                ..model::File::default()
            });
        }
        let filetype = loose_str(self.filetype.as_ref()).unwrap_or_default();
        let preview = self.text_preview(&filetype);
        let wave = self.wave();
        let transcript = self.transcript();
        let voice = self.subtype.as_ref().and_then(Value::as_str) == Some("slack_audio");
        let sized = |url: Option<String>, w: Option<Value>, h: Option<Value>| {
            url.map(|url| (Some(url), size_of(w.as_ref(), h.as_ref())))
        };
        let (thumb, size) = [
            (self.thumb_720, self.thumb_720_w, self.thumb_720_h),
            (self.thumb_480, self.thumb_480_w, self.thumb_480_h),
            (self.thumb_360, self.thumb_360_w, self.thumb_360_h),
        ]
        .into_iter()
        .find_map(|(url, w, h)| sized(url, w, h))
        .unwrap_or((None, None));
        // Small GIFs and PNGs come without thumbnails; show the file itself.
        let original_size = size_of(self.original_w.as_ref(), self.original_h.as_ref());
        let (thumb, size) = match thumb {
            Some(thumb) => (Some(thumb), size),
            None if self.mimetype.starts_with("image/") && self.size < 4 * 1024 * 1024 => {
                (self.url_private.clone(), original_size)
            }
            None => (None, None),
        };
        // A still for what is not a picture: a video's frame, a PDF's or
        // Office document's first page, or the ordinary thumbnail Slack
        // made of it.
        let is_image = self.mimetype.starts_with("image/");
        let (poster, poster_size) = [
            (self.thumb_video, self.thumb_video_w, self.thumb_video_h),
            (self.thumb_pdf, self.thumb_pdf_w, self.thumb_pdf_h),
        ]
        .into_iter()
        .find_map(|(url, w, h)| sized(url, w, h))
        .unwrap_or_else(|| {
            if is_image {
                (None, None)
            } else {
                (thumb.clone(), size)
            }
        });
        Some(model::File {
            id: self.id,
            name: self.name,
            title: self.title,
            mimetype: self.mimetype,
            size: self.size,
            url_private: self.url_private,
            download_url: self.url_private_download,
            thumb,
            thumb_size: size,
            permalink: self.permalink,
            original_size,
            poster,
            poster_size,
            user: self.user.filter(|user| !user.is_empty()),
            deleted: false,
            filetype,
            preview,
            converted_pdf: loose_str(self.converted_pdf.as_ref()),
            mp4_low: loose_str(self.mp4_low.as_ref()),
            aac: loose_str(self.aac.as_ref()),
            duration_ms: loose_count(self.duration_ms.as_ref()).filter(|ms| *ms > 0),
            voice,
            wave,
            transcript,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Attachment {
    pub color: Option<String>,
    pub service_name: Option<String>,
    pub author_name: Option<String>,
    pub title: Option<String>,
    pub title_link: Option<String>,
    pub pretext: Option<String>,
    pub text: Option<String>,
    pub fallback: Option<String>,
    pub image_url: Option<String>,
    pub thumb_url: Option<String>,
    pub footer: Option<String>,
    pub is_msg_unfurl: bool,
    pub service_icon: Option<String>,
    pub author_icon: Option<String>,
    pub author_link: Option<String>,
    // Sizes are numbers, but kept loose: one written as text must not
    // lose the whole message.
    pub image_width: Option<Value>,
    pub image_height: Option<Value>,
    pub thumb_width: Option<Value>,
    pub thumb_height: Option<Value>,
    /// An embedded player, for videos; only its presence matters here.
    pub video_html: Option<String>,
    pub video_url: Option<String>,
    pub from_url: Option<String>,
    pub original_url: Option<String>,
    pub fields: Vec<AttachmentField>,
    pub blocks: Vec<Value>,
    // What a message unfurl (`is_msg_unfurl`) says about the message it
    // quotes. `ts` is kept loose: it is text, but a number must not lose
    // the whole message.
    pub author_id: Option<String>,
    pub author_subname: Option<String>,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub ts: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AttachmentField {
    pub title: String,
    pub value: String,
    pub short: bool,
}

impl Attachment {
    /// A message unfurl as the quote it stands for. Slack sends one for a
    /// link to a Slack message: the author (by id, name and picture), the
    /// conversation, the message's own `ts` and text, and the link in
    /// `from_url`. Its footer ("Posted in #general") is left out, as the
    /// quote says where it was posted itself.
    fn into_quote(self) -> Option<model::Attachment> {
        let non_empty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        let url = non_empty(self.from_url)
            .or(non_empty(self.original_url))
            .or(non_empty(self.title_link))?;
        let ts = match self.ts {
            Some(Value::String(ts)) => Some(ts),
            Some(Value::Number(ts)) => Some(ts.to_string()),
            _ => None,
        }
        .filter(|ts| !ts.is_empty())
        .map(Ts::new);
        let text = non_empty(self.text)
            .or(non_empty(self.fallback))
            .unwrap_or_default();
        Some(model::Attachment {
            color: self.color.as_deref().and_then(parse_hex),
            quote: Some(model::Quote {
                url,
                channel: non_empty(self.channel_id),
                channel_name: non_empty(self.channel_name),
                ts,
                user: non_empty(self.author_id),
                author: non_empty(self.author_subname).or(non_empty(self.author_name)),
                author_icon: non_empty(self.author_icon),
                text,
                unavailable: false,
            }),
            ..model::Attachment::default()
        })
    }

    fn into_model(self) -> Option<model::Attachment> {
        if self.is_msg_unfurl {
            return self.into_quote();
        }
        let non_empty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        let fields: Vec<model::Field> = self
            .fields
            .into_iter()
            .filter(|f| !f.title.is_empty() || !f.value.is_empty())
            .map(|f| model::Field {
                title: f.title,
                value: f.value,
                short: f.short,
            })
            .collect();
        let blocks = kit_blocks(&self.blocks);
        let title = non_empty(self.title);
        let mut text = non_empty(self.text).unwrap_or_default();
        let image = non_empty(self.image_url);
        if text.is_empty() && title.is_none() && fields.is_empty() && blocks.is_empty() {
            text = non_empty(self.fallback).unwrap_or_default();
        }
        if text.is_empty()
            && title.is_none()
            && fields.is_empty()
            && blocks.is_empty()
            && image.is_none()
        {
            return None;
        }
        let title_link = non_empty(self.title_link);
        let thumb = non_empty(self.thumb_url);
        // A player: the page to watch it on, which is where the link
        // points. Without a picture to show there is nothing to press.
        let video = (non_empty(self.video_html).is_some() || non_empty(self.video_url).is_some())
            .then(|| {
                title_link
                    .clone()
                    .or(non_empty(self.from_url.clone()))
                    .or(non_empty(self.original_url.clone()))
            })
            .flatten()
            .filter(|_| thumb.is_some());
        Some(model::Attachment {
            color: self.color.as_deref().and_then(parse_hex),
            service: non_empty(self.service_name),
            service_icon: non_empty(self.service_icon),
            author: non_empty(self.author_name),
            author_icon: non_empty(self.author_icon),
            author_link: non_empty(self.author_link),
            image_size: size_of(self.image_width.as_ref(), self.image_height.as_ref()),
            thumb_size: size_of(self.thumb_width.as_ref(), self.thumb_height.as_ref()),
            video,
            pretext: non_empty(self.pretext),
            title,
            title_link,
            text,
            fields,
            image,
            thumb,
            footer: non_empty(self.footer),
            blocks,
            quote: None,
        })
    }
}

/// A picture's width and height from loosely typed JSON (a number, or a
/// number written as text), when both are there and above zero.
fn size_of(width: Option<&Value>, height: Option<&Value>) -> Option<[f32; 2]> {
    let number = |value: &Value| -> Option<f32> {
        let number = match value {
            Value::Number(n) => n.as_f64()?,
            Value::String(s) => s.trim().parse().ok()?,
            _ => return None,
        };
        (number.is_finite() && number > 0.0).then_some(number as f32)
    };
    Some([number(width?)?, number(height?)?])
}

/// A Block Kit text object as mrkdwn: `mrkdwn` as it is, `plain_text`
/// escaped so it reads literally.
fn kit_text(value: &Value) -> Option<String> {
    let text = value.get("text").and_then(Value::as_str)?;
    if text.trim().is_empty() {
        return None;
    }
    Some(match value.get("type").and_then(Value::as_str) {
        Some("plain_text") => crate::mrkdwn::escape(text),
        _ => text.to_owned(),
    })
}

/// A string field, left out when missing or blank.
fn kit_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// A button in block `block` (whose `block_id` a press names), with what
/// pressing it needs: the app's `action_id` and `value`, and the `confirm`
/// dialog the app wants shown first.
fn kit_button(block: &Value, value: &Value) -> Option<model::Button> {
    if value.get("type").and_then(Value::as_str) != Some("button") {
        return None;
    }
    let label = |object: Option<&Value>| object.and_then(|o| kit_str(o, "text"));
    let confirm = value
        .get("confirm")
        .filter(|c| c.is_object())
        .map(|c| model::Confirm {
            title: label(c.get("title")),
            text: c.get("text").and_then(kit_text),
            confirm: label(c.get("confirm")),
            deny: label(c.get("deny")),
            style: kit_str(c, "style"),
        });
    Some(model::Button {
        text: value
            .get("text")
            .and_then(|t| t.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("Button")
            .to_owned(),
        url: kit_str(value, "url"),
        style: kit_str(value, "style"),
        action_id: kit_str(value, "action_id"),
        block_id: kit_str(block, "block_id"),
        value: value
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned),
        confirm,
    })
}

/// Block Kit blocks, as far as a reader needs them. Inputs and other
/// interactive elements other than buttons (selects, menus, pickers) are
/// left out: pressing a button is the one interaction drawn here.
pub fn kit_blocks(blocks: &[Value]) -> Vec<model::KitBlock> {
    use model::{Accessory, ContextItem, KitBlock};
    let mut out = Vec::new();
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        let parsed = match kind {
            "header" => block.get("text").and_then(kit_text).map(KitBlock::Header),
            "section" => {
                let text = block.get("text").and_then(kit_text);
                let fields: Vec<String> = block
                    .get("fields")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(kit_text)
                    .collect();
                let accessory = block.get("accessory").and_then(|a| {
                    match a.get("type").and_then(Value::as_str) {
                        Some("image") => Some(Accessory::Image {
                            url: a.get("image_url").and_then(Value::as_str)?.to_owned(),
                            alt: a
                                .get("alt_text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        }),
                        Some("button") => {
                            kit_button(block, a).map(|b| Accessory::Button(Box::new(b)))
                        }
                        _ => None,
                    }
                });
                (text.is_some() || !fields.is_empty() || accessory.is_some()).then_some(
                    KitBlock::Section {
                        text,
                        fields,
                        accessory,
                    },
                )
            }
            "context" => {
                let items: Vec<ContextItem> = block
                    .get("elements")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| match e.get("type").and_then(Value::as_str) {
                        Some("image") => Some(ContextItem::Image {
                            url: e.get("image_url").and_then(Value::as_str)?.to_owned(),
                            alt: e
                                .get("alt_text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        }),
                        _ => kit_text(e).map(ContextItem::Text),
                    })
                    .collect();
                (!items.is_empty()).then_some(KitBlock::Context(items))
            }
            "divider" => Some(KitBlock::Divider),
            "image" => block
                .get("image_url")
                .and_then(Value::as_str)
                .map(|url| KitBlock::Image {
                    url: url.to_owned(),
                    alt: block
                        .get("alt_text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    title: block.get("title").and_then(kit_text),
                    size: size_of(block.get("image_width"), block.get("image_height")),
                }),
            "actions" => {
                let buttons: Vec<model::Button> = block
                    .get("elements")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|element| kit_button(block, element))
                    .collect();
                (!buttons.is_empty()).then_some(KitBlock::Actions(buttons))
            }
            "rich_text" => {
                let blocks = super::rich::blocks(block);
                if blocks.is_empty() {
                    None
                } else if let Some(KitBlock::RichText(before)) = out.last_mut() {
                    // A message has one rich text block in practice; should
                    // it have more, they read on as one, as its `text` does.
                    *before = before.iter().cloned().chain(blocks).collect();
                    None
                } else {
                    Some(KitBlock::RichText(blocks.into()))
                }
            }
            _ => None,
        };
        out.extend(parsed);
    }
    out
}

fn parse_hex(value: &str) -> Option<egui::Color32> {
    let hex = value.trim_start_matches('#');
    match hex {
        "good" => Some(egui::Color32::from_rgb(0x2e, 0xb6, 0x7d)),
        "warning" => Some(egui::Color32::from_rgb(0xec, 0xb2, 0x2e)),
        "danger" => Some(egui::Color32::from_rgb(0xe0, 0x1e, 0x5a)),
        _ if hex.len() == 6 => u32::from_str_radix(hex, 16).ok().map(|v| {
            let [_, r, g, b] = v.to_be_bytes();
            egui::Color32::from_rgb(r, g, b)
        }),
        _ => None,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Edited {
    pub user: String,
    pub ts: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BotProfile {
    pub name: String,
    pub icons: Icons,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Icons {
    pub image_36: Option<String>,
    pub image_48: Option<String>,
    pub image_72: Option<String>,
    pub emoji: Option<String>,
}

impl Icons {
    fn best(self) -> Option<String> {
        self.image_72.or(self.image_48).or(self.image_36)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Message {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub kind: String,
    pub subtype: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub ts: String,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    pub username: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub text: String,
    pub thread_ts: Option<String>,
    /// Absent on a message that says nothing about its thread, as some
    /// edits and trimmed answers do; an explicit 0 means no replies.
    pub reply_count: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    pub reply_users: Vec<String>,
    pub latest_reply: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub reactions: Vec<Reaction>,
    #[serde(deserialize_with = "null_default")]
    pub files: Vec<File>,
    #[serde(deserialize_with = "null_default")]
    pub attachments: Vec<Attachment>,
    #[serde(deserialize_with = "null_default")]
    pub blocks: Vec<Value>,
    pub edited: Option<Edited>,
    pub bot_profile: Option<BotProfile>,
    pub icons: Option<Icons>,
    /// Set by Slack on a thread reply also sent to the channel.
    pub root: Option<Value>,
    #[serde(deserialize_with = "null_default")]
    pub hidden: bool,
    /// The conversations it is pinned in.
    #[serde(deserialize_with = "null_default")]
    pub pinned_to: Vec<String>,
    /// The huddle a `huddle_thread` message stands for: who is in it, and
    /// whether it has ended.
    pub room: Option<Value>,
    /// The id the sending client gave it, if any.
    pub client_msg_id: Option<String>,
}

impl Message {
    pub fn into_model(self) -> Option<model::Message> {
        if self.ts.is_empty() || self.hidden {
            return None;
        }
        // `text` stays the plain fallback (previews, copying); the layout, if
        // any, is drawn from `blocks`.
        let mut text = self.text;
        if text.trim().is_empty() && !self.blocks.is_empty() {
            text = blocks_text(&self.blocks);
        }
        let blocks = kit_blocks(&self.blocks);
        let username = self
            .bot_profile
            .as_ref()
            .map(|bot| bot.name.clone())
            .filter(|name| !name.is_empty())
            .or(self.username);
        let bot_icon = self
            .icons
            .and_then(Icons::best)
            .or_else(|| self.bot_profile.and_then(|bot| bot.icons.best()));
        let broadcast = self.subtype.as_deref() == Some("thread_broadcast") || self.root.is_some();
        Some(model::Message {
            ts: Ts::new(self.ts),
            user: self.user,
            // A person's message never shows a bot name.
            username: if self.bot_id.is_some() {
                username
            } else {
                None
            },
            bot_icon,
            bot_id: self.bot_id,
            text,
            thread_ts: self.thread_ts.map(Ts::new),
            reply_count: self.reply_count.unwrap_or(0),
            replies_known: self.reply_count.is_some(),
            reply_users: self.reply_users,
            latest_reply: self.latest_reply.map(Ts::new),
            reactions: self
                .reactions
                .into_iter()
                .map(|r| model::Reaction {
                    name: r.name,
                    count: r.count,
                    users: r.users,
                })
                .collect(),
            files: self
                .files
                .into_iter()
                .filter_map(File::into_model)
                .collect(),
            attachments: self
                .attachments
                .into_iter()
                .filter_map(Attachment::into_model)
                .collect(),
            blocks,
            edited: self.edited.is_some(),
            subtype: self.subtype,
            delivery: Delivery::Sent,
            broadcast,
            pinned: !self.pinned_to.is_empty(),
            client_msg_id: self.client_msg_id.filter(|id| !id.is_empty()),
        })
    }
}

/// The text of Block Kit blocks, for messages (mostly from apps) whose
/// `text` is empty: sections, headers, context lines and rich text.
pub fn blocks_text(blocks: &[Value]) -> String {
    let mut lines = Vec::new();
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "section" | "header" => {
                if let Some(text) = block.pointer("/text/text").and_then(Value::as_str) {
                    lines.push(if kind == "header" {
                        format!("*{text}*")
                    } else {
                        text.to_owned()
                    });
                }
                if let Some(fields) = block.get("fields").and_then(Value::as_array) {
                    for field in fields {
                        if let Some(text) = field.get("text").and_then(Value::as_str) {
                            lines.push(text.to_owned());
                        }
                    }
                }
            }
            "context" => {
                let parts: Vec<_> = block
                    .get("elements")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.get("text").and_then(Value::as_str))
                    .collect();
                if !parts.is_empty() {
                    lines.push(parts.join(" "));
                }
            }
            "rich_text" => {
                let mut text = String::new();
                rich_text(block, &mut text);
                lines.push(text);
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// Flattens a rich text block back into mrkdwn.
fn rich_text(node: &Value, out: &mut String) {
    let Some(elements) = node.get("elements").and_then(Value::as_array) else {
        return;
    };
    for element in elements {
        match element.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => out.push_str(element.get("text").and_then(Value::as_str).unwrap_or("")),
            "link" => {
                let url = element.get("url").and_then(Value::as_str).unwrap_or("");
                match element.get("text").and_then(Value::as_str) {
                    Some(text) => out.push_str(&format!("<{url}|{text}>")),
                    None => out.push_str(&format!("<{url}>")),
                }
            }
            "user" => out.push_str(&format!(
                "<@{}>",
                element.get("user_id").and_then(Value::as_str).unwrap_or("")
            )),
            "channel" => out.push_str(&format!(
                "<#{}>",
                element
                    .get("channel_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            )),
            "emoji" => out.push_str(&format!(
                ":{}:",
                element.get("name").and_then(Value::as_str).unwrap_or("")
            )),
            "broadcast" => out.push_str(&format!(
                "<!{}>",
                element
                    .get("range")
                    .and_then(Value::as_str)
                    .unwrap_or("here")
            )),
            "rich_text_preformatted" => {
                let mut inner = String::new();
                rich_text(element, &mut inner);
                out.push_str(&format!("```{inner}```\n"));
            }
            "rich_text_quote" => {
                let mut inner = String::new();
                rich_text(element, &mut inner);
                for line in inner.lines() {
                    out.push_str(&format!("> {line}\n"));
                }
            }
            "rich_text_list" => {
                let mut inner = String::new();
                for item in element
                    .get("elements")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let mut line = String::new();
                    rich_text(item, &mut line);
                    inner.push_str(&format!("• {line}\n"));
                }
                out.push_str(&inner);
            }
            _ => rich_text(element, out),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Profile {
    #[serde(deserialize_with = "null_default")]
    pub display_name: String,
    #[serde(deserialize_with = "null_default")]
    pub real_name: String,
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    #[serde(deserialize_with = "null_default")]
    pub status_text: String,
    #[serde(deserialize_with = "null_default")]
    pub status_emoji: String,
    pub image_72: Option<String>,
    pub image_192: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct User {
    pub id: String,
    pub name: String,
    pub real_name: String,
    pub deleted: bool,
    pub is_bot: bool,
    pub tz: Option<String>,
    pub profile: Profile,
    /// The person's own workspace.
    pub team_id: String,
    /// Someone from outside your organization you share a channel with.
    pub is_stranger: bool,
    /// On Enterprise Grid: the organization the person belongs to.
    pub enterprise_user: Option<EnterpriseUser>,
}

/// A person's place in an Enterprise Grid organization.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EnterpriseUser {
    pub enterprise_id: String,
}

impl User {
    pub fn into_model(self) -> model::User {
        model::User {
            id: self.id,
            name: self.name,
            real_name: if self.profile.real_name.is_empty() {
                self.real_name
            } else {
                self.profile.real_name
            },
            display_name: self.profile.display_name,
            avatar: self.profile.image_72.or(self.profile.image_192),
            is_bot: self.is_bot,
            deleted: self.deleted,
            title: self.profile.title,
            status_text: self.profile.status_text,
            status_emoji: self.profile.status_emoji,
            tz: self.tz,
            team: self.team_id,
            enterprise: self
                .enterprise_user
                .map(|e| e.enterprise_id)
                .unwrap_or_default(),
            stranger: self.is_stranger,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserInfo {
    pub user: User,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UsersPage {
    pub members: Vec<User>,
    pub response_metadata: ResponseMetadata,
}

/// `users.channelSections.list`: your sidebar sections. Undocumented; the
/// web client's own call. Sections form a linked list through
/// `next_channel_section_id`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChannelSectionsPage {
    pub channel_sections: Vec<ChannelSection>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChannelSection {
    pub channel_section_id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub emoji: String,
    pub next_channel_section_id: Option<String>,
    pub last_updated: i64,
    pub is_redacted: bool,
    pub channel_ids_page: ChannelIdsPage,
}

/// A section's channels as `users.channelSections.list` sends them.
///
/// Slack's web client knows no call that pages through the rest (no
/// `users.channelSections.channels.list`: Slack answers `unknown_method`),
/// and `users.channelSections.list` takes no per-section cursor. In
/// practice the list holds every channel you are still in: `cursor` is the
/// last id sent, and `count` also counts channels archived or left since
/// they were filed, which are not sent. A channel left out anyway still
/// shows, under Channels or Direct messages.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChannelIdsPage {
    pub channel_ids: Vec<String>,
    /// The last id sent, when Slack has more ids filed than it sent.
    pub cursor: Option<String>,
    /// Every channel ever filed here, including archived and left ones.
    pub count: Option<usize>,
}

impl ChannelIdsPage {
    /// How many filed channels Slack did not send, when it says there are
    /// more (most often archived or left ones).
    pub fn unsent(&self) -> Option<usize> {
        let more = self.cursor.as_deref().is_some_and(|c| !c.is_empty());
        let count = self.count.unwrap_or(0);
        (more && count > self.channel_ids.len()).then(|| count - self.channel_ids.len())
    }
}

/// Puts sections in the order the linked list gives them, keeping the kinds
/// a sidebar shows. The head is the section nothing points to (the most
/// recently updated, if a broken list has several); a loop stops the walk.
pub fn order_sections(sections: Vec<ChannelSection>) -> Vec<model::SidebarSection> {
    use std::collections::{HashMap, HashSet};
    let pointed: HashSet<String> = sections
        .iter()
        .filter_map(|s| s.next_channel_section_id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    let head = sections
        .iter()
        .filter(|s| !pointed.contains(&s.channel_section_id))
        .max_by_key(|s| s.last_updated)
        .map(|s| s.channel_section_id.clone());
    let by_id: HashMap<String, ChannelSection> = sections
        .into_iter()
        .map(|s| (s.channel_section_id.clone(), s))
        .collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut current = head;
    while let Some(id) = current {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(section) = by_id.get(&id) else {
            break;
        };
        let kind = match section.kind.as_str() {
            "standard" => Some(model::SectionKind::Custom),
            "stars" => Some(model::SectionKind::Starred),
            "channels" => Some(model::SectionKind::Channels),
            "direct_messages" => Some(model::SectionKind::DirectMessages),
            "recent_apps" => Some(model::SectionKind::Apps),
            // Slack Connect, Salesforce records, agents, anything new.
            _ => None,
        };
        if let Some(kind) = kind
            && !section.is_redacted
        {
            out.push(model::SidebarSection {
                id: section.channel_section_id.clone(),
                kind,
                name: section.name.clone(),
                emoji: section.emoji.trim_matches(':').to_owned(),
                channel_ids: section.channel_ids_page.channel_ids.clone(),
            });
        }
        current = section
            .next_channel_section_id
            .clone()
            .filter(|next| !next.is_empty());
    }
    out
}

/// `client.counts`: the read state of every conversation at once.
/// Undocumented; the web client's own call, answered for browser sessions.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ClientCounts {
    #[serde(deserialize_with = "null_default")]
    pub channels: Vec<CountEntry>,
    #[serde(deserialize_with = "null_default")]
    pub mpims: Vec<CountEntry>,
    #[serde(deserialize_with = "null_default")]
    pub ims: Vec<CountEntry>,
}

/// One conversation in [`ClientCounts`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CountEntry {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub last_read: String,
    #[serde(deserialize_with = "null_default")]
    pub latest: String,
    #[serde(deserialize_with = "null_default")]
    pub mention_count: u32,
    #[serde(deserialize_with = "null_default")]
    pub has_unreads: bool,
}

impl ClientCounts {
    /// Every conversation's entry, by id.
    pub fn by_id(self) -> std::collections::HashMap<String, CountEntry> {
        self.channels
            .into_iter()
            .chain(self.mpims)
            .chain(self.ims)
            .filter(|entry| !entry.id.is_empty())
            .map(|entry| (entry.id.clone(), entry))
            .collect()
    }
}

/// A timestamp Slack sent, unless it is empty or Slack's all-zero "never".
pub fn real_ts(ts: &str) -> Option<Ts> {
    (!ts.is_empty() && !is_never(ts)).then(|| Ts::new(ts))
}

/// Whether Slack sent its all-zero "never" rather than a timestamp or
/// nothing. As a conversation's newest message, it says there is none,
/// where an empty one says nothing at all.
pub fn is_never(ts: &str) -> bool {
    ts == "0000000000.000000"
}

/// `stars.list`: what you starred. Only conversations matter here.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StarsList {
    pub items: Vec<StarItem>,
    pub response_metadata: ResponseMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StarItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub channel: Option<String>,
}

impl StarsList {
    /// The starred channels, DMs and group DMs.
    pub fn conversations(self) -> Vec<String> {
        self.items
            .into_iter()
            .filter(|item| matches!(item.kind.as_str(), "channel" | "group" | "im"))
            .filter_map(|item| item.channel)
            .collect()
    }
}

/// `bots.info`: the app or integration behind a `bot_id`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BotInfo {
    pub bot: BotEntry,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BotEntry {
    pub id: String,
    pub name: String,
    pub icons: Icons,
}

impl BotEntry {
    pub fn into_model(self) -> model::Bot {
        model::Bot {
            id: self.id,
            name: self.name,
            icon: self.icons.best(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EmojiList {
    pub emoji: std::collections::HashMap<String, String>,
}

/// `usergroups.list`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserGroupList {
    pub usergroups: Vec<UserGroup>,
}

/// One user group, as `usergroups.list` describes it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserGroup {
    pub id: String,
    pub handle: String,
    pub name: String,
    /// A number, though some answers send it as a string.
    pub user_count: Value,
    /// Non-zero once the group is disabled.
    pub date_delete: i64,
}

impl UserGroupList {
    /// The groups you can mention. Disabled ones and any without a handle
    /// are left out, as typing them would reach no one.
    pub fn into_model(self) -> Vec<model::UserGroup> {
        self.usergroups
            .into_iter()
            .filter(|g| !g.id.is_empty() && !g.handle.is_empty() && g.date_delete == 0)
            .map(|g| model::UserGroup {
                members: g
                    .user_count
                    .as_u64()
                    .or_else(|| g.user_count.as_str().and_then(|s| s.parse().ok()))
                    .and_then(|n| usize::try_from(n).ok()),
                id: g.id,
                handle: g.handle,
                name: g.name,
            })
            .collect()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Posted {
    pub channel: String,
    pub ts: String,
    pub message: Option<Message>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UploadUrl {
    pub upload_url: String,
    pub file_id: String,
}

/// `apps.connections.open`: the socket to open.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct ConnectionsOpen {
    /// The `wss://` URL, whose ticket lets anyone open the socket.
    pub url: String,
}

/// Leaves out the URL, which carries the socket's ticket.
impl std::fmt::Debug for ConnectionsOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionsOpen")
            .field("url", &crate::redact::REDACTED)
            .finish()
    }
}

/// The signed-in person in an `oauth.v2.access` answer, with their token.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct AuthedUser {
    pub id: String,
    pub scope: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

/// Shows whether there are tokens, never the tokens.
impl std::fmt::Debug for AuthedUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthedUser")
            .field("id", &self.id)
            .field("scope", &self.scope)
            .field("access_token", &redacted(&self.access_token))
            .field("refresh_token", &redacted(&self.refresh_token))
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// A secret as Debug shows it: whether there is one, not what it is.
fn redacted(secret: &Option<String>) -> Option<&'static str> {
    secret.as_ref().map(|_| crate::redact::REDACTED)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OauthTeam {
    pub id: String,
    pub name: String,
}

/// `oauth.v2.access`, for both the code exchange and a refresh.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct OauthAccess {
    pub authed_user: AuthedUser,
    pub team: OauthTeam,
    /// A refresh of a user token answers at the top level.
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub token_type: Option<String>,
}

/// Shows whether there are tokens, never the tokens.
impl std::fmt::Debug for OauthAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthAccess")
            .field("authed_user", &self.authed_user)
            .field("team", &self.team)
            .field("access_token", &redacted(&self.access_token))
            .field("refresh_token", &redacted(&self.refresh_token))
            .field("expires_in", &self.expires_in)
            .field("token_type", &self.token_type)
            .finish()
    }
}

/// A Socket Mode frame.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub envelope_id: Option<String>,
    pub payload: Value,
    pub reason: Option<String>,
}

/// The `payload` of an `events_api` envelope.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EventCallback {
    pub team_id: String,
    pub event: Value,
    pub authorizations: Vec<Authorization>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Authorization {
    pub team_id: Option<String>,
    pub user_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_keeps_its_client_id() {
        let page: Vec<Message> = serde_json::from_str(
            r#"[
                {"type":"message","ts":"1.0","user":"U1","text":"hi",
                 "client_msg_id":"4f1e6b2a-0c3d-4e5f-8a9b-1c2d3e4f5a6b"},
                {"type":"message","ts":"2.0","user":"U1","text":"hi","client_msg_id":""},
                {"type":"message","ts":"3.0","user":"U1","text":"hi"}
            ]"#,
        )
        .expect("parses");
        let ids: Vec<Option<String>> = page
            .into_iter()
            .filter_map(Message::into_model)
            .map(|m| m.client_msg_id)
            .collect();
        assert_eq!(
            ids,
            [
                Some("4f1e6b2a-0c3d-4e5f-8a9b-1c2d3e4f5a6b".to_owned()),
                None,
                None
            ]
        );
    }

    #[test]
    fn nulls_do_not_lose_a_page() {
        let page: Vec<Message> = serde_json::from_str(
            r#"[
                {"type":"message","ts":"1.0","user":"U1","text":null,"reactions":null,
                 "files":null,"attachments":null,"blocks":null,"reply_users":null,
                 "pinned_to":null,"hidden":null},
                {"type":null,"ts":"2.0","user":"U2","text":"hi"}
            ]"#,
        )
        .expect("parses");
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].text, "");
        assert!(page[0].reactions.is_empty() && !page[0].hidden);
        assert_eq!(page[1].text, "hi");
        let profile: Profile = serde_json::from_str(
            r#"{"display_name":null,"real_name":"Ada","title":null,"status_text":null,"status_emoji":null}"#,
        )
        .expect("parses");
        assert_eq!(profile.real_name, "Ada");
        assert_eq!(profile.display_name, "");
        let counts: ClientCounts = serde_json::from_str(
            r#"{"channels":[{"id":"C1","last_read":null,"latest":null,"mention_count":null,"has_unreads":null}],"mpims":null}"#,
        )
        .expect("parses");
        let by_id = counts.by_id();
        assert_eq!(by_id["C1"].mention_count, 0);
        assert!(!by_id["C1"].has_unreads);
    }

    #[test]
    fn tokens_and_socket_tickets_never_print() {
        let access: OauthAccess = serde_json::from_str(
            r#"{"ok":true,"access_token":"xoxe.xoxp-top","refresh_token":"xoxe-1-top",
                "authed_user":{"id":"U1","access_token":"xoxp-inner","refresh_token":"xoxe-1-inner"},
                "team":{"id":"T1","name":"Acme"}}"#,
        )
        .expect("parses");
        let shown = format!("{access:?}");
        assert!(!shown.contains("xox"), "{shown}");
        assert!(
            shown.contains("<redacted>") && shown.contains("U1"),
            "{shown}"
        );
        let open: ConnectionsOpen =
            serde_json::from_str(r#"{"ok":true,"url":"wss://wss.slack.com/link/?ticket=secret"}"#)
                .expect("parses");
        let shown = format!("{open:?}");
        assert!(!shown.contains("ticket"), "{shown}");
    }

    #[test]
    fn user_groups_keep_the_ones_you_can_mention() {
        // Shaped like Slack's documented answer, trimmed.
        let list: UserGroupList = serde_json::from_str(
            r#"{"ok":true,"usergroups":[
                {"id":"S0614TZR7","team_id":"T060RNRCH","is_usergroup":true,
                 "name":"Team Admins","description":"A group of all Administrators",
                 "handle":"admins","is_external":false,"date_create":1446598059,
                 "date_update":1446670362,"date_delete":0,"auto_type":"admin",
                 "created_by":"USLACKBOT","updated_by":"U060RNRCZ","deleted_by":null,
                 "prefs":{"channels":[],"groups":[]},"user_count":2},
                {"id":"S06158AV7","name":"Team Owners","handle":"owners","user_count":"1"},
                {"id":"S0615G0KT","name":"Old","handle":"old","date_delete":1446746793},
                {"id":"S0615G0KU","name":"No handle","handle":""}
            ]}"#,
        )
        .expect("parses");
        assert_eq!(
            list.into_model(),
            [
                model::UserGroup {
                    id: "S0614TZR7".into(),
                    handle: "admins".into(),
                    name: "Team Admins".into(),
                    members: Some(2),
                },
                model::UserGroup {
                    id: "S06158AV7".into(),
                    handle: "owners".into(),
                    name: "Team Owners".into(),
                    members: Some(1),
                },
            ]
        );
    }

    #[test]
    fn unsent_section_channels_are_counted() {
        let page = |json: &str| -> ChannelIdsPage { serde_json::from_str(json).expect("parses") };
        let short = page(r#"{"channel_ids":["C1","C2"],"count":70,"cursor":"C2"}"#);
        assert_eq!(short.unsent(), Some(68));
        // All sent: a cursor alone, or a count that matches, is no gap.
        assert_eq!(
            page(r#"{"channel_ids":["C1"],"count":1,"cursor":"C1"}"#).unsent(),
            None
        );
        assert_eq!(page(r#"{"channel_ids":["C1"],"count":5}"#).unsent(), None);
        assert_eq!(page(r#"{"channel_ids":[],"cursor":""}"#).unsent(), None);
    }

    #[test]
    fn group_dm_names_read_as_people() {
        assert_eq!(group_name("mpdm-ana--bob--carla-1"), "ana, bob, carla");
        assert_eq!(group_name("mpdm-x.y--z-12"), "x.y, z");
    }

    /// Parses one message as Slack sends it.
    fn parsed(json: &str) -> model::Message {
        serde_json::from_str::<Message>(json)
            .ok()
            .and_then(Message::into_model)
            .expect("a message")
    }

    #[test]
    fn message_unfurls_become_quotes() {
        // As Slack unfurls a permalink to one of its messages.
        let message = parsed(
            r#"{"type":"message","ts":"2.0","user":"U1",
            "text":"<https://acme.slack.com/archives/C1/p1700000000000100>",
            "attachments":[{"id":1,"ts":"1700000000.000100","channel_id":"C1",
            "channel_name":"general","is_msg_unfurl":true,"author_id":"U2",
            "author_name":"ana","author_subname":"Ana Lima",
            "author_link":"https://acme.slack.com/team/U2",
            "author_icon":"https://avatars.slack-edge.com/ana.png",
            "text":"Hi <@U3> :wave:","fallback":"[Nov 14th] Ana Lima: Hi",
            "from_url":"https://acme.slack.com/archives/C1/p1700000000000100",
            "color":"D0D0D0","footer":"Posted in #general","mrkdwn_in":["text"]},
            {"is_msg_unfurl":true,"text":"no link, so not a quote"}]}"#,
        );
        assert_eq!(message.attachments.len(), 1);
        let quote = message.attachments[0].quote.as_ref().expect("a quote");
        assert_eq!(
            quote.url,
            "https://acme.slack.com/archives/C1/p1700000000000100"
        );
        assert_eq!(quote.channel.as_deref(), Some("C1"));
        assert_eq!(quote.channel_name.as_deref(), Some("general"));
        assert_eq!(quote.ts, Some(Ts::new("1700000000.000100")));
        assert_eq!(quote.user.as_deref(), Some("U2"));
        assert_eq!(quote.author.as_deref(), Some("Ana Lima"));
        assert_eq!(
            quote.author_icon.as_deref(),
            Some("https://avatars.slack-edge.com/ana.png")
        );
        assert_eq!(quote.text, "Hi <@U3> :wave:");
        assert!(!quote.unavailable);
        // The footer is not drawn as a link card's would be.
        assert_eq!(message.attachments[0].footer, None);
        assert!(crate::quotes::own_quotes(&message).is_empty());
    }

    #[test]
    fn video_unfurls_keep_their_player_link_and_sizes() {
        let message = parsed(
            r#"{"type":"message","ts":"1.0","user":"U1","text":"<https://youtu.be/x>",
            "attachments":[{"service_name":"YouTube","service_icon":"https://a.ytimg.com/yt.png",
            "author_name":"Rust Channel","author_link":"https://youtube.com/@rust",
            "title":"Talk","title_link":"https://youtu.be/x","thumb_url":"https://i.ytimg.com/x.jpg",
            "thumb_width":480,"thumb_height":"360","video_html":"<iframe></iframe>",
            "image_width":0,"image_height":10},
            {"service_name":"Blog","title":"Post","title_link":"https://blog.example/p",
            "image_url":"https://blog.example/p.png","image_width":1200,"image_height":630}]}"#,
        );
        let video = &message.attachments[0];
        assert_eq!(video.service.as_deref(), Some("YouTube"));
        assert_eq!(video.author.as_deref(), Some("Rust Channel"));
        assert_eq!(video.video.as_deref(), Some("https://youtu.be/x"));
        assert_eq!(video.thumb_size, Some([480.0, 360.0]), "text numbers too");
        assert_eq!(video.image_size, None, "a zero size is no size");
        let post = &message.attachments[1];
        assert_eq!(post.video, None);
        assert_eq!(post.image_size, Some([1200.0, 630.0]));
    }

    #[test]
    fn picture_files_keep_the_size_of_the_thumbnail_they_show() {
        let message = parsed(
            r#"{"type":"message","ts":"1.0","user":"U1","text":"",
            "files":[{"id":"F1","name":"big.png","mimetype":"image/png","size":900000,
            "original_w":3024,"original_h":4032,
            "thumb_360":"https://files.slack.com/t360.png","thumb_360_w":270,"thumb_360_h":360,
            "thumb_720":"https://files.slack.com/t720.png","thumb_720_w":540,"thumb_720_h":720},
            {"id":"F2","name":"tiny.gif","mimetype":"image/gif","size":2000,
            "url_private":"https://files.slack.com/tiny.gif","original_w":64,"original_h":48}]}"#,
        );
        let [big, tiny] = &message.files[..] else {
            panic!("two files");
        };
        assert_eq!(
            big.thumb.as_deref(),
            Some("https://files.slack.com/t720.png")
        );
        assert_eq!(
            big.thumb_size,
            Some([540.0, 720.0]),
            "the largest thumbnail's"
        );
        assert_eq!(big.original_size, Some([3024.0, 4032.0]));
        // Shown as it is, without a thumbnail: its own size.
        assert_eq!(
            tiny.thumb.as_deref(),
            Some("https://files.slack.com/tiny.gif")
        );
        assert_eq!(tiny.thumb_size, Some([64.0, 48.0]));
    }

    /// The one file of a message carrying `file`'s JSON.
    fn file_of(file: &str) -> model::File {
        let mut message = parsed(&format!(
            r#"{{"type":"message","ts":"1.0","user":"U1","text":"","files":[{file}]}}"#
        ));
        assert_eq!(message.files.len(), 1, "one file");
        message.files.remove(0)
    }

    // The fixtures below are files from public Slack exports, cut down to
    // the fields that matter and with their tokens taken out.

    #[test]
    fn a_snippet_keeps_slacks_preview_of_its_first_lines() {
        let file = file_of(
            r#"{"id":"F07A2TVQ7C0","name":"channels.json","title":"channels.json",
            "mimetype":"text/plain","filetype":"json","pretty_type":"JSON","mode":"snippet",
            "size":1563,"editable":true,
            "preview":"[\n{\n    \"id\": \"C06NRA6JLER\",\n    \"name\": \"random\",\n    \"created\": 1710128735,",
            "preview_highlight":"<div class=\"CodeMirror cm-s-default CodeMirrorServer\">…</div>",
            "edit_link":"https://ds-py62195.slack.com/files/U06NU4E26M9/F07A2TVQ7C0/channels.json/edit",
            "url_private":"https://files.slack.com/files-pri/T06NRA6HM3P-F07A2TVQ7C0/channels.json"}"#,
        );
        assert_eq!(file.filetype, "json");
        let preview = file.preview.expect("a preview");
        assert!(
            preview.text.starts_with("[\n{\n    \"id\""),
            "plain, not the HTML"
        );
        assert_eq!(preview.text.lines().count(), 5);
        assert_eq!(preview.lines_more, None);
        assert!(!preview.truncated);
    }

    #[test]
    fn a_text_file_falls_back_to_the_plain_text_preview_and_reads_counts_loosely() {
        let file = file_of(
            r#"{"id":"F1","name":"server.log","mimetype":"text/plain","filetype":"text",
            "mode":"hosted","size":20480,"preview":null,
            "preview_plain_text":"line one\r\n\tindented\r\nline three",
            "preview_is_truncated":true,"lines":"120","lines_more":117}"#,
        );
        let preview = file.preview.expect("a preview");
        assert_eq!(preview.text, "line one\n    indented\nline three");
        assert_eq!(preview.lines, Some(120));
        assert_eq!(preview.lines_more, Some(117));
        assert!(preview.truncated);
    }

    #[test]
    fn a_snippet_an_export_left_without_a_preview_has_none() {
        let file = file_of(
            r#"{"id":"F0216RZRY7Q","name":"GMT20210507-120844_Recording.txt",
            "title":"GMT20210507-120844_Recording.txt","mimetype":"text/plain",
            "filetype":"text","pretty_type":"Plain Text","mode":"snippet","size":9946,
            "edit_link":"https://data-ft-ber-03-2021.slack.com/files/U01RW140HBP/F0216RZRY7Q/gmt20210507-120844_recording.txt/edit",
            "url_private":"https://files.slack.com/files-pri/T01RBRV5F7H-F0216RZRY7Q/gmt20210507-120844_recording.txt"}"#,
        );
        assert_eq!(file.preview, None);
    }

    #[test]
    fn pictures_never_take_a_text_preview() {
        // Old file objects carry empty text fields on every file.
        let jpg = file_of(
            r#"{"id":"F02PM6A1AUA","name":"Chevy.jpg","mimetype":"image/jpeg","filetype":"jpg",
            "mode":"hosted","size":359002,"original_h":1080,"original_w":1920,
            "url_private":"https://files.slack.com/files-pri/THY5HTZ8U-F02PM6A1AUA/chevy.jpg",
            "edit_link":"","preview":"","preview_highlight":"","lines":0,"lines_more":0}"#,
        );
        assert_eq!(jpg.preview, None);
        let png = file_of(
            r#"{"id":"F1","name":"a.png","mimetype":"image/png","mode":"hosted",
            "preview":"not text","lines":3}"#,
        );
        assert_eq!(png.preview, None);
    }

    #[test]
    fn an_office_file_keeps_its_first_page_and_slacks_pdf_of_it() {
        let file = file_of(
            r#"{"id":"F079SQ721A5","name":"Slack-bot-scopes-List.xlsx",
            "title":"Slack-bot-scopes-List.xlsx",
            "mimetype":"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "filetype":"xlsx","pretty_type":"Excel Spreadsheet","mode":"hosted","size":75813,
            "converted_pdf":"https://files.slack.com/files-tmb/T06NRA6HM3P-F079SQ721A5-315d1a255c/slack-bot-scopes-list_converted.pdf",
            "thumb_pdf":"https://files.slack.com/files-tmb/T06NRA6HM3P-F079SQ721A5-315d1a255c/slack-bot-scopes-list_thumb_pdf.png",
            "thumb_pdf_w":1210,"thumb_pdf_h":935,"media_display_type":"unknown",
            "url_private":"https://files.slack.com/files-pri/T06NRA6HM3P-F079SQ721A5/slack-bot-scopes-list.xlsx",
            "url_private_download":"https://files.slack.com/files-pri/T06NRA6HM3P-F079SQ721A5/download/slack-bot-scopes-list.xlsx"}"#,
        );
        assert_eq!(
            file.poster.as_deref(),
            Some(
                "https://files.slack.com/files-tmb/T06NRA6HM3P-F079SQ721A5-315d1a255c/slack-bot-scopes-list_thumb_pdf.png"
            )
        );
        assert_eq!(file.poster_size, Some([1210.0, 935.0]));
        assert_eq!(file.preview, None, "a spreadsheet is not text");
        let (url, name) = file.as_pdf().expect("a PDF to open");
        assert!(url.ends_with("/slack-bot-scopes-list_converted.pdf"));
        assert!(
            crate::slack::client::is_slack_file_url(&url),
            "fetched with the token"
        );
        assert_eq!(name, "Slack-bot-scopes-List.pdf");
    }

    #[test]
    fn a_video_keeps_its_smaller_copy_its_length_and_its_transcript() {
        let file = file_of(
            r#"{"id":"F0BAXAHN5HB","name":"Logicflow - Technical Overview_1080p.mp4",
            "mimetype":"video/mp4","filetype":"mp4","mode":"hosted","size":13017562,
            "mp4":"https://files.slack.com/files-tmb/T5TCAFTA9-F0BAXAHN5HB-ae07560ca6/logicflow_-_technical_overview_1080p.mp4",
            "mp4_low":"https://files.slack.com/files-tmb/T5TCAFTA9-F0BAXAHN5HB-ae07560ca6/logicflow_-_technical_overview_1080p_trans.mp4",
            "hls":"https://files.slack.com/files-tmb/T5TCAFTA9-F0BAXAHN5HB-ae07560ca6/file.m3u8?_xcb=a1098",
            "vtt":"https://files.slack.com/files-tmb/T5TCAFTA9-F0BAXAHN5HB-ae07560ca6/file.vtt?_xcb=a1098",
            "duration_ms":389322,"media_display_type":"video",
            "transcription":{"status":"complete","locale":"en-GB","preview":{"content":"Logic Flow is a live visual programming environment based on the principles of data transformation through functional pipes. Let's start the demo by","has_more":true}},
            "thumb_video":"https://files.slack.com/files-tmb/T5TCAFTA9-F0BAXAHN5HB-ae07560ca6/logicflow_-_technical_overview_1080p_thumb_video.jpeg",
            "thumb_video_w":1920,"thumb_video_h":1080,
            "url_private":"https://files.slack.com/files-tmb/T5TCAFTA9-F0BAXAHN5HB-ae07560ca6/logicflow_-_technical_overview_1080p.mp4"}"#,
        );
        assert_eq!(file.duration_ms, Some(389_322));
        assert_eq!(file.poster_size, Some([1920.0, 1080.0]));
        assert!(
            file.transcript
                .as_deref()
                .is_some_and(|t| t.starts_with("Logic Flow is"))
        );
        assert!(!file.voice);
        let (url, name) = file.player().expect("something to play");
        assert!(url.ends_with("_trans.mp4"), "the smaller copy");
        assert_eq!(name, "Logicflow - Technical Overview_1080p.mp4");
    }

    #[test]
    fn a_voice_clip_keeps_its_waveform_length_and_transcript() {
        let file = file_of(
            r#"{"id":"F03V9NETH3J","name":"Audio clip (2022-08-22_15-11-01-551).m4a",
            "mimetype":"audio/mp4","filetype":"m4a","mode":"hosted","subtype":"slack_audio",
            "size":171020,"duration_ms":13977,"media_display_type":"audio",
            "vtt":"https://files.slack.com/files-tmb/T03U4J8HMUG-F03V9NETH3J-3231bb718b/file.vtt?_xcb=0c2db",
            "transcription":{"status":"complete","locale":"en-US","preview":{"content":"Get to Work.","has_more":false}},
            "audio_wave_samples":[0,0,2,34,75,57,53,45,46,48,66,89,78,54,68,68,61,48,51,47,47,45,69,72,47,40,46,41,37,34,35,35,36,36,28,36,39,40,39,36,41,40,42,33,51,46,39,32,39,34,37,32,37,36,37,32,34,39,27,41,43,48,68,72,56,66,52,53,53,43,39,42,41,46,49,34,37,39,20,25,40,37,34,76,100,53,89,94,34,48,26,25,59,29,74,71,68,23,54,58],
            "url_private":"https://files.slack.com/files-pri/T03U4J8HMUG-F03V9NETH3J/audio_clip__2022-08-22_15-11-01-551_.m4a"}"#,
        );
        assert!(file.voice);
        assert_eq!(file.wave.len(), 100);
        assert_eq!(file.wave.iter().max(), Some(&100));
        assert_eq!(file.duration_ms, Some(13_977));
        assert_eq!(file.transcript.as_deref(), Some("Get to Work."));
        let (url, name) = file.player().expect("something to play");
        assert!(url.ends_with(".m4a"), "the clip itself");
        assert_eq!(name, "Audio clip (2022-08-22_15-11-01-551).m4a");
    }

    #[test]
    fn an_old_webm_voice_clip_plays_from_its_mp4_copy() {
        let file = file_of(
            r#"{"id":"F03C90NKC9H","name":"audio_message.webm","mimetype":"audio/webm",
            "filetype":"webm","mode":"hosted","subtype":"slack_audio","size":2270608,
            "aac":"https://files.slack.com/files-tmb/TJE58GTJL-F03C90NKC9H-a1c3f4a445/audio_message_audio.mp4",
            "duration_ms":140061,"media_display_type":"audio",
            "transcription":{"status":"complete","locale":"en-US"},
            "audio_wave_samples":[73,73,57,74,56,42,27,38,45,57],
            "url_private":"https://files.slack.com/files-pri/TJE58GTJL-F03C90NKC9H/audio_message.webm"}"#,
        );
        assert_eq!(file.transcript, None, "a transcript without its preview");
        let (url, name) = file.player().expect("something to play");
        assert!(url.ends_with("audio_message_audio.mp4"));
        assert_eq!(name, "audio_message.m4a", "named for what it is");
    }

    #[test]
    fn odd_preview_fields_mean_no_preview_and_keep_the_message() {
        let file = file_of(
            r#"{"id":"F1","name":"clip.mp4","mimetype":"video/mp4","mode":"hosted",
            "subtype":3,"duration_ms":"soon","mp4_low":5,"aac":null,
            "audio_wave_samples":"loud","transcription":"none","converted_pdf":"",
            "thumb_video":"https://files.slack.com/t.jpg","thumb_video_w":"","thumb_video_h":null,
            "preview":["x"],"lines_more":null}"#,
        );
        assert_eq!(file.duration_ms, None);
        assert_eq!(file.mp4_low, None);
        assert_eq!(file.aac, None);
        assert!(file.wave.is_empty());
        assert_eq!(file.transcript, None);
        assert_eq!(file.converted_pdf, None);
        assert!(!file.voice);
        assert_eq!(
            file.poster.as_deref(),
            Some("https://files.slack.com/t.jpg")
        );
        assert_eq!(file.poster_size, None, "an empty size is no size");
        // Samples out of range or not numbers stay in place.
        let clip = file_of(
            r#"{"id":"F2","name":"a.m4a","mimetype":"audio/mp4","subtype":"slack_audio",
            "audio_wave_samples":[50,250,-3,"x",null,12.6]}"#,
        );
        assert_eq!(clip.wave, vec![50, 100, 0, 0, 0, 13]);
    }

    #[test]
    fn image_blocks_keep_the_size_slack_adds() {
        let message = parsed(
            r#"{"type":"message","ts":"1.0","bot_id":"B1","text":"chart",
            "blocks":[{"type":"image","image_url":"https://ci.example/a.png","alt_text":"a",
              "image_width":1024,"image_height":"512","image_bytes":20000},
             {"type":"image","image_url":"https://ci.example/b.png","alt_text":"b"},
             {"type":"image","image_url":"https://ci.example/c.png","alt_text":"c",
              "image_width":0,"image_height":300}]}"#,
        );
        let sizes: Vec<_> = message
            .blocks
            .iter()
            .map(|block| match block {
                model::KitBlock::Image { size, .. } => *size,
                other => panic!("an image block, not {other:?}"),
            })
            .collect();
        assert_eq!(sizes, [Some([1024.0, 512.0]), None, None]);
    }

    #[test]
    fn videos_and_pdfs_get_their_still() {
        let message = parsed(
            r#"{"type":"message","ts":"1.0","user":"U1","text":"",
            "files":[{"id":"F1","name":"clip.mp4","mimetype":"video/mp4","size":10,
            "thumb_video":"https://files.slack.com/v.jpg","thumb_video_w":640,"thumb_video_h":360},
            {"id":"F2","name":"plan.pdf","mimetype":"application/pdf","size":10,
            "thumb_pdf":"https://files.slack.com/p.png","thumb_pdf_w":300,"thumb_pdf_h":400},
            {"id":"F3","name":"a.png","mimetype":"image/png","size":10,
            "thumb_360":"https://files.slack.com/t.png"}]}"#,
        );
        let [video, pdf, image] = &message.files[..] else {
            panic!("three files");
        };
        assert_eq!(video.media(), Some(model::Media::Video));
        assert_eq!(video.poster_size, Some([640.0, 360.0]));
        assert!(pdf.is_pdf());
        assert_eq!(pdf.poster.as_deref(), Some("https://files.slack.com/p.png"));
        assert_eq!(image.poster, None, "a picture shows itself");
    }

    #[test]
    fn edits_without_thread_details_keep_the_counts() {
        let mut timeline = model::Timeline::default();
        timeline.upsert(parsed(
            r#"{"type":"message","ts":"1.0","user":"U1","text":"parent","thread_ts":"1.0",
                "reply_count":2,"reply_users":["U2"],"latest_reply":"3.0"}"#,
        ));
        // A `message_changed` copy with the thread left out, with and
        // without `thread_ts`.
        let edits = [
            r#"{"type":"message","ts":"1.0","user":"U1","text":"edited","edited":{"user":"U1","ts":"4.0"}}"#,
            r#"{"type":"message","ts":"1.0","user":"U1","text":"edited","thread_ts":"1.0","edited":{"user":"U1","ts":"4.0"}}"#,
        ];
        for edit in edits {
            let edit = parsed(edit);
            assert!(!edit.replies_known);
            timeline.upsert(edit);
            let parent = &timeline.messages[0];
            assert_eq!(parent.text, "edited");
            assert_eq!(parent.reply_count, 2);
            assert_eq!(parent.reply_users, ["U2"]);
            assert_eq!(parent.latest_reply.as_ref().map(Ts::as_str), Some("3.0"));
            assert_eq!(parent.thread_ts.as_ref().map(Ts::as_str), Some("1.0"));
        }
        // Slack's copy after the last reply was deleted: an explicit 0,
        // even without `thread_ts`, clears the counters.
        timeline.upsert(parsed(
            r#"{"type":"message","ts":"1.0","user":"U1","text":"edited","reply_count":0}"#,
        ));
        let parent = &timeline.messages[0];
        assert_eq!(parent.reply_count, 0);
        assert!(parent.reply_users.is_empty());
        assert_eq!(parent.latest_reply, None);
    }

    #[test]
    fn history_survives_missing_and_odd_fields() {
        let page: HistoryPage = serde_json::from_str(
            r#"{"ok":true,"messages":[
                {"type":"message","ts":"1.0","user":"U1","text":"hi","reactions":[{"name":"tada","count":1,"users":["U2"]}]},
                {"type":"message","subtype":"bot_message","ts":"2.0","bot_id":"B1","username":"CI","text":"","blocks":[{"type":"section","text":{"type":"mrkdwn","text":"*build* ok"}}]},
                {"type":"message","ts":"3.0","user":"U1","text":"pic","files":[{"id":"F1","name":"a.png","mimetype":"image/png","size":10,"thumb_360":"https://files/t","thumb_360_w":360,"thumb_360_h":200}]},
                {"type":"message","ts":"4.0","user":"U1","text":"gone","files":[{"id":"F2","mode":"tombstone"}]}
            ],"has_more":true,"response_metadata":{"next_cursor":"abc"}}"#,
        )
        .expect("parses");
        assert!(page.has_more);
        assert_eq!(page.response_metadata.cursor().as_deref(), Some("abc"));
        let messages: Vec<_> = page
            .messages
            .into_iter()
            .filter_map(Message::into_model)
            .collect();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].reactions[0].users, ["U2"]);
        assert_eq!(messages[1].text, "*build* ok");
        assert_eq!(messages[1].username.as_deref(), Some("CI"));
        assert!(messages[2].files[0].is_image());
        assert_eq!(messages[2].files[0].thumb_size, Some([360.0, 200.0]));
        assert!(
            messages[3].files[0].deleted,
            "a deleted file keeps its place"
        );
        assert_eq!(messages[3].files[0].id, "F2");
    }

    #[test]
    fn glitchtip_alerts_keep_their_fields() {
        let message: Message = serde_json::from_str(
            r#"{"type":"message","subtype":"bot_message","ts":"1.0","bot_id":"B1","username":"GlitchTip",
                "text":"GlitchTip Alert","attachments":[{"color":"e52b50","fallback":"[no preview available]",
                "title":"ValueError: boom","title_link":"https://gt/issues/1","text":"app.views in get",
                "fields":[{"title":"Project","value":"backend","short":true},
                          {"title":"Release","value":"1.2","short":false}]}]}"#,
        )
        .expect("parses");
        let message = message.into_model().expect("model");
        assert!(!message.uses_blocks());
        let card = &message.attachments[0];
        assert_eq!(card.title.as_deref(), Some("ValueError: boom"));
        assert_eq!(card.text, "app.views in get");
        assert_eq!(card.color, Some(egui::Color32::from_rgb(0xe5, 0x2b, 0x50)));
        assert_eq!(
            card.fields,
            [
                model::Field {
                    title: "Project".into(),
                    value: "backend".into(),
                    short: true
                },
                model::Field {
                    title: "Release".into(),
                    value: "1.2".into(),
                    short: false
                },
            ]
        );
    }

    #[test]
    fn block_kit_layout_replaces_the_fallback_text() {
        use model::{Accessory, ContextItem, KitBlock};
        let blocks: Vec<Value> = serde_json::from_str(
            r#"[{"type":"header","text":{"type":"plain_text","text":"Deploy <prod>"}},
                {"type":"section","text":{"type":"mrkdwn","text":"*done*"},
                 "fields":[{"type":"mrkdwn","text":"*A*\n1"}],
                 "accessory":{"type":"image","image_url":"https://x/i.png","alt_text":"i"}},
                {"type":"context","elements":[{"type":"mrkdwn","text":"by bot"},{"type":"image","image_url":"https://x/a.png","alt_text":"a"}]},
                {"type":"divider"},
                {"type":"actions","elements":[{"type":"button","text":{"type":"plain_text","text":"Open"},"url":"https://x"},
                                              {"type":"static_select"}]},
                {"type":"input"}]"#,
        )
        .expect("parses");
        let kit = kit_blocks(&blocks);
        assert_eq!(
            kit[0],
            KitBlock::Header("Deploy &lt;prod&gt;".into()),
            "plain text is escaped"
        );
        assert!(matches!(
            &kit[1],
            KitBlock::Section { text: Some(t), fields, accessory: Some(Accessory::Image { .. }) }
                if t == "*done*" && fields.len() == 1
        ));
        assert!(
            matches!(&kit[2], KitBlock::Context(items) if matches!(items[1], ContextItem::Image { .. }))
        );
        assert_eq!(kit[3], KitBlock::Divider);
        assert!(
            matches!(&kit[4], KitBlock::Actions(buttons) if buttons.len() == 1 && buttons[0].url.is_some())
        );
        assert_eq!(kit.len(), 5, "inputs and selects are left out");
        assert!(kit.iter().any(KitBlock::is_layout));
        let typed: Vec<Value> = serde_json::from_str(
            r#"[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"hi"}]}]}]"#,
        )
        .expect("parses");
        assert!(
            !kit_blocks(&typed).iter().any(KitBlock::is_layout),
            "people's own messages keep their text"
        );
    }

    #[test]
    fn interactive_buttons_keep_what_a_press_needs() {
        use model::{Accessory, Button, Confirm, KitBlock};
        let message: Message = serde_json::from_str(
            r#"{"type":"message","subtype":"bot_message","ts":"1790171950.000100","bot_id":"B09",
                "text":"Deploy?",
                "blocks":[
                  {"type":"section","block_id":"ask","text":{"type":"mrkdwn","text":"Deploy?"},
                   "accessory":{"type":"button","action_id":"details","text":{"type":"plain_text","text":"Details"}}},
                  {"type":"actions","block_id":"deploy-1288","elements":[
                    {"type":"button","action_id":"approve","value":"1288","style":"primary",
                     "text":{"type":"plain_text","text":"Approve"},
                     "confirm":{"title":{"type":"plain_text","text":"Deploy?"},
                       "text":{"type":"mrkdwn","text":"Goes to *everyone*."},
                       "confirm":{"type":"plain_text","text":"Deploy"},
                       "deny":{"type":"plain_text","text":"Not yet"},
                       "style":"danger"}},
                    {"type":"button","action_id":"reject","text":{"type":"plain_text","text":"Reject"}}]}]}"#,
        )
        .expect("parses");
        let message = message.into_model().expect("a message");
        assert_eq!(message.bot_id.as_deref(), Some("B09"));
        let KitBlock::Section {
            accessory: Some(Accessory::Button(details)),
            ..
        } = &message.blocks[0]
        else {
            panic!("a section with a button: {:?}", message.blocks[0]);
        };
        assert_eq!(details.block_id.as_deref(), Some("ask"), "the section's id");
        assert_eq!(details.action_id.as_deref(), Some("details"));
        let KitBlock::Actions(buttons) = &message.blocks[1] else {
            panic!("an actions block: {:?}", message.blocks[1]);
        };
        assert_eq!(
            buttons[0],
            Button {
                text: "Approve".into(),
                url: None,
                style: Some("primary".into()),
                action_id: Some("approve".into()),
                block_id: Some("deploy-1288".into()),
                value: Some("1288".into()),
                confirm: Some(Confirm {
                    title: Some("Deploy?".into()),
                    text: Some("Goes to *everyone*.".into()),
                    confirm: Some("Deploy".into()),
                    deny: Some("Not yet".into()),
                    style: Some("danger".into()),
                }),
            }
        );
        assert_eq!(buttons[1].confirm, None, "no confirm, no question");
        assert_eq!(buttons[1].value, None);
        assert_eq!(buttons[1].block_id.as_deref(), Some("deploy-1288"));
    }

    #[test]
    fn rich_text_is_kept_as_slack_laid_it_out() {
        let message: Message = serde_json::from_str(
            r#"{"type":"message","ts":"1.0","user":"U1","text":":large_blue_square:1000",
                "blocks":[
                    {"type":"rich_text","elements":[{"type":"rich_text_section","elements":[
                        {"type":"emoji","name":"large_blue_square"},{"type":"text","text":"1000"}]}]},
                    {"type":"rich_text","elements":[{"type":"rich_text_section","elements":[
                        {"type":"text","text":"more"}]}]}]}"#,
        )
        .expect("parses");
        let message = message.into_model().expect("a message");
        assert!(!message.uses_blocks(), "rich text alone is no app layout");
        use crate::mrkdwn::{Block, Inline, Style};
        assert_eq!(
            message.rich_text().map(|blocks| blocks.to_vec()),
            Some(vec![
                Block::Paragraph(vec![
                    Inline::Emoji("large_blue_square".into()),
                    Inline::Text("1000".into(), Style::default()),
                ]),
                Block::Paragraph(vec![Inline::Text("more".into(), Style::default())]),
            ]),
            "two blocks read on as one"
        );
    }

    #[test]
    fn rich_text_flattens_to_mrkdwn() {
        let blocks: Vec<Value> = serde_json::from_str(
            r#"[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[
                {"type":"text","text":"hey "},{"type":"user","user_id":"U1"},
                {"type":"text","text":" see "},{"type":"link","url":"https://x.y","text":"this"},
                {"type":"emoji","name":"wave"}]}]}]"#,
        )
        .expect("parses");
        assert_eq!(
            blocks_text(&blocks),
            "hey <@U1> see <https://x.y|this>:wave:"
        );
    }

    #[test]
    fn channels_map_to_kinds_and_unread_state() {
        let info: ChannelInfo = serde_json::from_str(
            r#"{"channel":{"id":"D1","is_im":true,"user":"U2","last_read":"1.0","latest":{"ts":"2.0"},"unread_count_display":3}}"#,
        )
        .expect("parses");
        let conversation = info.channel.into_model();
        assert_eq!(conversation.kind, ConversationKind::Direct);
        assert_eq!(conversation.latest, Some(Ts::new("2.0")));
        assert_eq!(conversation.unread, 3);
    }

    #[test]
    fn direct_messages_say_whether_they_are_open() {
        let open = |json: &str| {
            serde_json::from_str::<Channel>(json)
                .expect("parses")
                .into_model()
                .is_open
        };
        assert_eq!(
            open(r#"{"id":"G1","is_mpim":true,"is_group":true,"is_open":true}"#),
            Some(true)
        );
        assert_eq!(
            open(r#"{"id":"D1","is_im":true,"user":"U2","is_open":false}"#),
            Some(false)
        );
        // Not said, or said as null: not known, which never hides it.
        assert_eq!(open(r#"{"id":"D2","is_im":true,"user":"U2"}"#), None);
        assert_eq!(open(r#"{"id":"D3","is_im":true,"is_open":null}"#), None);
        // Channels do not open and close.
        assert_eq!(
            open(r#"{"id":"C1","is_channel":true,"is_open":false}"#),
            None
        );
    }

    #[test]
    fn the_all_zero_timestamp_means_never() {
        assert!(is_never("0000000000.000000"));
        assert!(!is_never(""));
        assert!(!is_never("1700000000.000100"));
        assert_eq!(real_ts("0000000000.000000"), None);
    }
}
