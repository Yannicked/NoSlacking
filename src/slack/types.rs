//! Slack's JSON, as the Web API and Socket Mode send it, and its translation
//! into [`crate::model`].
//!
//! Every field is optional or defaulted: Slack leaves fields out freely, and
//! a missing field must never lose a whole page of messages.

use serde::Deserialize;
use serde_json::Value;

use crate::model::{self, ConversationKind, Delivery, Ts};

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
    pub url_private: Option<String>,
    pub url_private_download: Option<String>,
    pub permalink: Option<String>,
    pub thumb_360: Option<String>,
    pub thumb_360_w: Option<f32>,
    pub thumb_360_h: Option<f32>,
    pub thumb_480: Option<String>,
    pub thumb_480_w: Option<f32>,
    pub thumb_480_h: Option<f32>,
    pub thumb_720: Option<String>,
    pub thumb_720_w: Option<f32>,
    pub thumb_720_h: Option<f32>,
    pub original_w: Option<f32>,
    pub original_h: Option<f32>,
}

impl File {
    pub fn into_model(self) -> Option<model::File> {
        // Files past the free plan's limit, or deleted ones.
        if self.mode == "tombstone" || self.mode == "hidden_by_limit" || self.id.is_empty() {
            return None;
        }
        let (thumb, size) = [
            (self.thumb_720, self.thumb_720_w, self.thumb_720_h),
            (self.thumb_480, self.thumb_480_w, self.thumb_480_h),
            (self.thumb_360, self.thumb_360_w, self.thumb_360_h),
        ]
        .into_iter()
        .find_map(|(url, w, h)| url.map(|url| (Some(url), w.zip(h).map(|(w, h)| [w, h]))))
        .unwrap_or((None, None));
        // Small GIFs and PNGs come without thumbnails; show the file itself.
        let (thumb, size) = match thumb {
            Some(thumb) => (Some(thumb), size),
            None if self.mimetype.starts_with("image/") && self.size < 4 * 1024 * 1024 => (
                self.url_private.clone(),
                self.original_w.zip(self.original_h).map(|(w, h)| [w, h]),
            ),
            None => (None, None),
        };
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
    pub fields: Vec<AttachmentField>,
    pub blocks: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AttachmentField {
    pub title: String,
    pub value: String,
    pub short: bool,
}

impl Attachment {
    fn into_model(self) -> Option<model::Attachment> {
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
        Some(model::Attachment {
            color: self.color.as_deref().and_then(parse_hex),
            service: non_empty(self.service_name).or(non_empty(self.author_name)),
            pretext: non_empty(self.pretext),
            title,
            title_link: non_empty(self.title_link),
            text,
            fields,
            image,
            thumb: non_empty(self.thumb_url),
            footer: non_empty(self.footer),
            blocks,
        })
    }
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

fn kit_button(value: &Value) -> Option<model::Button> {
    if value.get("type").and_then(Value::as_str) != Some("button") {
        return None;
    }
    Some(model::Button {
        text: value
            .get("text")
            .and_then(|t| t.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("Button")
            .to_owned(),
        url: value.get("url").and_then(Value::as_str).map(str::to_owned),
        style: value
            .get("style")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Block Kit blocks, as far as a reader needs them. Inputs and other
/// interactive elements are left out: they need the app's own server.
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
                        Some("button") => kit_button(a).map(Accessory::Button),
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
                }),
            "actions" => {
                let buttons: Vec<model::Button> = block
                    .get("elements")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(kit_button)
                    .collect();
                (!buttons.is_empty()).then_some(KitBlock::Actions(buttons))
            }
            "rich_text" => {
                let mut text = String::new();
                rich_text(block, &mut text);
                (!text.trim().is_empty()).then_some(KitBlock::RichText(text))
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
    #[serde(rename = "type")]
    pub kind: String,
    pub subtype: Option<String>,
    pub ts: String,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    pub username: Option<String>,
    pub text: String,
    pub thread_ts: Option<String>,
    /// Absent on a message that says nothing about its thread, as some
    /// edits and trimmed answers do; an explicit 0 means no replies.
    pub reply_count: Option<u32>,
    pub reply_users: Vec<String>,
    pub latest_reply: Option<String>,
    pub reactions: Vec<Reaction>,
    pub files: Vec<File>,
    pub attachments: Vec<Attachment>,
    pub blocks: Vec<Value>,
    pub edited: Option<Edited>,
    pub bot_profile: Option<BotProfile>,
    pub icons: Option<Icons>,
    /// Set by Slack on a thread reply also sent to the channel.
    pub root: Option<Value>,
    pub hidden: bool,
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
    pub display_name: String,
    pub real_name: String,
    pub title: String,
    pub status_text: String,
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChannelIdsPage {
    pub channel_ids: Vec<String>,
    /// Set when the section has more channels than were sent.
    pub cursor: Option<String>,
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
    pub channels: Vec<CountEntry>,
    pub mpims: Vec<CountEntry>,
    pub ims: Vec<CountEntry>,
}

/// One conversation in [`ClientCounts`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CountEntry {
    pub id: String,
    pub last_read: String,
    pub latest: String,
    pub mention_count: u32,
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
    (!ts.is_empty() && ts != "0000000000.000000").then(|| Ts::new(ts))
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConnectionsOpen {
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AuthedUser {
    pub id: String,
    pub scope: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OauthTeam {
    pub id: String,
    pub name: String,
}

/// `oauth.v2.access`, for both the code exchange and a refresh.
#[derive(Debug, Default, Deserialize)]
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
        assert!(messages[3].files.is_empty());
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
}
