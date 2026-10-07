//! The open conversation: its header, the message list with day
//! separators and a "new" line, and the composer.

use egui::{Align, CornerRadius, Margin, RichText, Stroke, Vec2};

use super::composer::{self, Composer};
use super::message::{self, Lead, Row};
use super::rows;
use crate::app::App;
use crate::backend::Socket;
use crate::i18n::{t, tf};
use crate::model::{Ability, Action, ConversationKind, Ts};
use crate::theme::{self, Icon};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            let Some(team) = app.active_team() else {
                empty(ui, app, &t("No workspace"));
                return;
            };
            let channel = app.active_workspace().and_then(|w| w.active.clone());
            let Some(channel) = channel else {
                let text = if app.active_workspace().is_some_and(|w| w.loaded) {
                    t("Pick a conversation")
                } else {
                    t("Loading conversations…")
                };
                empty(ui, app, &text);
                return;
            };
            if app
                .active_workspace()
                .is_some_and(|w| w.info.offers(Ability::Files))
            {
                composer::drop_target(ui, &palette, None, true, &mut app.actions);
            }
            header(app, ui, &channel);
            footer(app, ui, &team, &channel);
            messages(app, ui, &team, &channel);
        });
}

fn empty(ui: &mut egui::Ui, app: &App, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(
            RichText::new(text)
                .font(theme::regular(15.0))
                .color(app.palette.dim),
        );
    });
}

fn header(app: &mut App, ui: &mut egui::Ui, channel: &str) {
    let palette = app.palette;
    let App {
        workspaces,
        settings,
        actions,
        socket,
        popouts,
        #[cfg(feature = "huddle-audio")]
        huddles,
        ..
    } = app;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        return;
    };
    let Some(conversation) = workspace.conversation(channel) else {
        return;
    };
    // What this workspace's service can do: the rest stays out of the header.
    let offers = |ability| workspace.info.offers(ability);
    let popped_out = popouts
        .iter()
        .any(|p| p.team == workspace.info.team_id && p.channel == conversation.id);
    let inset = theme::titlebar_inset(ui.ctx());
    egui::Panel::top("conversation-header")
        .exact_size(52.0 + inset)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(palette.window)
                .inner_margin(Margin {
                    left: 20,
                    right: 12,
                    top: inset as i8,
                    bottom: 0,
                }),
        )
        .show(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().hline(
                rect.x_range(),
                rect.bottom() - 0.5,
                Stroke::new(1.0, palette.outline),
            );
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let title = workspace.title(conversation);
                match conversation.kind {
                    ConversationKind::Channel => {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(17.0), egui::Sense::hover());
                        Icon::Hash.image(palette.secondary, 17.0).paint_at(ui, icon);
                    }
                    ConversationKind::Private => {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
                        Icon::Lock.image(palette.secondary, 16.0).paint_at(ui, icon);
                    }
                    ConversationKind::Direct => {
                        let user = conversation.user.as_deref().and_then(|id| workspace.user(id));
                        let avatar = super::avatar(
                            ui,
                            user.and_then(|u| u.avatar.as_deref()),
                            &title,
                            conversation.user.as_deref().unwrap_or(&title),
                            22.0,
                        );
                        let presence = conversation.user.as_deref().and_then(|id| workspace.people.presence(id));
                        super::people::dot(ui.painter(), &palette, avatar.rect, presence, palette.window);
                        if let Some(presence) = presence {
                            avatar.on_hover_text(super::people::word(presence));
                        }
                    }
                    ConversationKind::Group => {
                        let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
                        Icon::Users.image(palette.secondary, 16.0).paint_at(ui, icon);
                    }
                }
                let name = ui
                    .add(
                        egui::Label::new(
                            RichText::new(&title)
                                .font(theme::bold(17.0))
                                .color(palette.text),
                        )
                        .sense(egui::Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if crate::people::is_external_conversation(workspace, conversation) {
                    super::people::external_tag(ui, &palette, conversation.kind == ConversationKind::Direct);
                }
                if name.clicked()
                    && let Some(user) = &conversation.user
                {
                    actions.push(Action::OpenProfile(user.clone()));
                } else if name.clicked() && offers(Ability::Details) {
                    actions.push(super::browse::details(&conversation.id, crate::convos::Tab::About));
                }
                if !conversation.topic.is_empty() {
                    ui.add_space(8.0);
                    let topic = crate::mrkdwn::plain(&conversation.topic, |_| None);
                    ui.add(
                        egui::Label::new(
                            RichText::new(topic)
                                .font(theme::regular(13.0))
                                .color(palette.secondary),
                        )
                        .truncate(),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let tip = tf("Search ({shortcut})", &[("shortcut", &super::keys::command("F"))]);
                    if offers(Ability::Search) && theme::icon_button(ui, &palette, Icon::Search, 17.0, &tip).clicked() {
                        actions.push(Action::OpenSearch);
                    }
                    if !popped_out
                        && theme::icon_button(ui, &palette, Icon::ExternalLink, 16.0, &t("Open in new window")).clicked()
                    {
                        actions.push(Action::PopOut(conversation.id.clone()));
                    }
                    if conversation.kind != ConversationKind::Direct
                        && offers(Ability::Details)
                        && theme::icon_button(ui, &palette, Icon::Info, 17.0, &t("Details")).clicked()
                    {
                        actions.push(super::browse::details(&conversation.id, crate::convos::Tab::About));
                    }
                    if offers(Ability::Bookmarks)
                        && theme::icon_button(ui, &palette, Icon::Bookmark, 16.0, &t("Bookmarks")).clicked()
                    {
                        actions.push(super::browse::details(&conversation.id, crate::convos::Tab::Bookmarks));
                    }
                    if offers(Ability::Pins)
                        && theme::icon_button(ui, &palette, Icon::Pin, 16.0, &t("Pinned messages")).clicked()
                    {
                        actions.push(super::browse::details(&conversation.id, crate::convos::Tab::Pins));
                    }
                    match socket {
                        Socket::Connected => {}
                        Socket::Off => {
                            ui.label(
                                RichText::new(t("Live updates off"))
                                    .font(theme::regular(12.0))
                                    .color(palette.dim),
                            )
                            .on_hover_text(t("No live connection: new messages in this conversation are fetched every few seconds."));
                        }
                        Socket::Connecting => {
                            ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
                            ui.label(RichText::new(t("Connecting…")).font(theme::regular(12.0)).color(palette.dim));
                        }
                        Socket::Disconnected(reason) | Socket::Rejected(reason) => {
                            if theme::icon_button(ui, &palette, Icon::Refresh, 15.0, &t("Reconnect")).clicked() {
                                actions.push(Action::Reconnect);
                            }
                            ui.label(
                                RichText::new(t("Offline"))
                                    .font(theme::semibold(12.0))
                                    .color(palette.warning),
                            )
                            .on_hover_text(reason.sentence());
                        }
                    }
                    if let Some(members) = conversation.members
                        && !conversation.kind.is_dm()
                    {
                        let count = ui
                            .add(
                                egui::Label::new(
                                    RichText::new(crate::i18n::tn("{count} member", "{count} members", members))
                                        .font(theme::regular(12.5))
                                        .color(palette.dim),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if count.clicked() && offers(Ability::Details) {
                            actions.push(super::browse::details(&conversation.id, crate::convos::Tab::Members));
                        }
                    }
                    if !offers(Ability::Huddles) {
                        return;
                    }
                    #[cfg(feature = "huddle-audio")]
                    super::people::huddle_button(
                        ui,
                        &palette,
                        workspace,
                        &conversation.id,
                        huddles.listening.as_ref(),
                        actions,
                    );
                    #[cfg(not(feature = "huddle-audio"))]
                    super::people::huddle_button(ui, &palette, workspace, &conversation.id, actions);
                    #[cfg(feature = "huddle-audio")]
                    super::people::start_huddle_menu(
                        ui,
                        &palette,
                        workspace,
                        &conversation.id,
                        huddles.listening.as_ref(),
                        actions,
                    );
                    #[cfg(not(feature = "huddle-audio"))]
                    super::people::start_huddle_menu(ui, &palette, workspace, &conversation.id, actions);
                    #[cfg(feature = "huddle-audio")]
                    super::people::listen_button(
                        ui,
                        &palette,
                        workspace,
                        &conversation.id,
                        huddles.listening.as_ref(),
                        actions,
                    );
                });
            });
        });
}

fn footer(app: &mut App, ui: &mut egui::Ui, team: &str, channel: &str) {
    let palette = app.palette;
    let key = App::draft_key(team, channel, None);
    let mut taken = app.drafts.take(&key);
    let focus =
        std::mem::take(&mut app.focus_composer) && app.picker.is_none() && app.switcher.is_none();
    let App {
        workspaces,
        settings,
        actions,
        transfers,
        ..
    } = app;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        // Put the draft back: it was taken out to be edited.
        app.drafts.put_back(key, taken);
        return;
    };
    let Some(conversation) = workspace.conversation(channel) else {
        app.drafts.put_back(key, taken);
        return;
    };
    let title = workspace.title(conversation);
    let placeholder = match conversation.kind {
        ConversationKind::Channel | ConversationKind::Private => {
            tf("Message #{name}", &[("name", &title)])
        }
        _ => tf("Message {name}", &[("name", &title)]),
    };
    egui::Panel::bottom("composer")
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(palette.window)
                .inner_margin(Margin {
                    left: 20,
                    right: 20,
                    top: 4,
                    // The typing line fills the rest of the bottom space.
                    bottom: 2,
                }),
        )
        .show(ui, |ui| {
            if conversation.archived {
                ui.label(RichText::new(t("This channel is archived.")).color(palette.dim));
                return;
            }
            let composer = Composer {
                palette: &palette,
                workspace,
                key: key.clone(),
                placeholder,
                thread: None,
                enter_sends: settings.enter_sends,
                focus,
                channel_name: None,
                uploads: transfers,
            };
            let before = taken.draft.text.clone();
            composer::show(ui, &composer, &mut taken.draft, actions);
            if crate::people::is_typing(&before, &taken.draft.text) {
                actions.push(Action::People(crate::people::Action::Typing {
                    channel: channel.to_owned(),
                    thread: None,
                }));
            }
            super::people::typing(ui, &palette, workspace, channel, None);
        });
    app.drafts.put_back(key, taken);
}

fn messages(app: &mut App, ui: &mut egui::Ui, team: &str, channel: &str) {
    let palette = app.palette;
    let scroll_key = format!("{team}/{channel}");
    let to_bottom = app.scroll_to_bottom.remove(&scroll_key);
    let mut app_scroll_again = false;
    let overlay = app.overlay_open();
    // An anchor for another list is stale by now: drop it either way.
    let prepended = app.prepended.take().is_some_and(|key| key == scroll_key);
    let App {
        workspaces,
        settings,
        actions,
        editing,
        read_line,
        selected,
        jumps,
        ..
    } = app;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        return;
    };
    let Some(conversation) = workspace.conversation(channel) else {
        return;
    };
    let timeline = workspace.timelines.get(channel);
    // A message being brought into view here: it steers the list, so the
    // end of the list must not pull the view down meanwhile.
    let now = std::time::Instant::now();
    let jump = jumps.iter().find(|j| j.list == scroll_key).cloned();
    let steering = jump.as_ref().is_some_and(crate::jump::Jump::steering);
    let to_bottom = to_bottom && !steering;
    // A list of older history has no end to hold on to.
    let detached = timeline.is_some_and(|t| t.has_newer);
    let mut target: Option<(f32, f32)> = None;
    // Where the "New" line starts, to offer a way back up to it.
    let mut unread_top: Option<f32> = None;
    let height_id = egui::Id::new(("content-height", &scroll_key));
    let previous_height: Option<f32> = ui.data(|d| d.get_temp(height_id));
    let mut area = egui::ScrollArea::vertical()
        .id_salt(("messages", &scroll_key))
        .auto_shrink([false, false])
        .stick_to_bottom(!steering && !detached);
    let offset_id = egui::Id::new(("scroll-offset", &scroll_key));
    if let Some(offset) = ui.data_mut(|d| d.remove_temp::<f32>(offset_id)) {
        area = area.vertical_scroll_offset(offset);
    }
    // The newest message you had read when the conversation opened.
    let read_line: Option<Ts> = read_line
        .as_ref()
        .filter(|line| line.list == scroll_key)
        .and_then(|line| line.read.clone());
    // Heights of the rows as last drawn: only the rows in and near the
    // view are laid out, the rest are placed by these.
    let heights_id = egui::Id::new(("row-heights", &scroll_key));
    let mut heights: rows::Heights = ui
        .data_mut(|d| d.remove_temp(heights_id))
        .unwrap_or_default();
    let look = message::Look::of(settings);
    heights.for_layout(look.key());
    let mut moved = 0.0;
    let output = area.show_viewport(ui, |ui, viewport| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let Some(timeline) = timeline.filter(|t| t.loaded) else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new().size(22.0).color(palette.dim));
            });
            return;
        };
        let row = Row {
            palette: &palette,
            workspace,
            workspaces,
            channel,
            in_thread: false,
            enter_sends: settings.enter_sends,
            look,
            overlay,
            selected: selected
                .as_ref()
                .filter(|s| !s.in_thread && s.channel == channel),
        };
        // What the list holds: its top, then each message with the day
        // separator or "New" line above it, if any.
        let mut items = vec![Item::Top];
        let mut days = Days::default();
        let mut previous = None;
        let mut previous_day: Option<jiff::civil::Date> = None;
        let mut drew_new_line = false;
        for message in timeline.messages.iter().filter(|m| m.in_channel()) {
            let day = days.of(&message.ts);
            let new_day = day.is_some() && day != previous_day;
            if new_day {
                previous_day = day;
            }
            let unread = !drew_new_line
                && !message.ts.is_local()
                && read_line.as_ref().is_some_and(|read| message.ts > *read)
                && message.user.as_deref() != Some(workspace.info.user_id.as_str());
            drew_new_line |= unread;
            let lead = if !new_day && !unread && message::continues(previous, message) {
                Lead::Compact
            } else {
                Lead::Full
            };
            items.push(Item::Message {
                message,
                lead,
                new_day,
                unread,
            });
            previous = Some(message);
        }
        if timeline.has_newer {
            items.push(Item::Bottom);
        }
        let entries: Vec<rows::Entry> = items
            .iter()
            .map(|item| item.entry(timeline.has_more, look))
            .collect();
        let plan = rows::plan(
            entries.iter().map(|entry| heights.planned(entry)),
            viewport.min.y,
            viewport.max.y,
            MARGIN,
        );
        heights.sweep();
        unread_top = items
            .iter()
            .position(|item| matches!(item, Item::Message { unread: true, .. }))
            .map(|index| plan.tops[index]);
        if let Some(jump) = &jump {
            target = items
                .iter()
                .position(
                    |item| matches!(item, Item::Message { message, .. } if message.ts == jump.ts),
                )
                .map(|index| (plan.tops[index], plan.tops[index + 1]));
        }
        let light = jump.as_ref().map_or(0.0, |j| j.light(now));
        moved = rows::show(
            ui,
            &mut heights,
            &entries,
            &plan,
            |ui, index| match &items[index] {
                Item::Top => top(ui, workspace, conversation, timeline, &palette, actions),
                Item::Bottom => bottom_row(ui, timeline, &palette, actions),
                Item::Message {
                    message,
                    lead,
                    new_day,
                    unread,
                } => {
                    if *new_day {
                        day_separator(ui, &palette, &super::day_label(&message.ts));
                    }
                    if *unread {
                        new_line(ui, &palette);
                    }
                    let background = ui.painter().add(egui::Shape::Noop);
                    let top = ui.cursor().top();
                    message::show(ui, &row, message, *lead, editing, actions);
                    if light > 0.0 && jump.as_ref().is_some_and(|j| j.ts == message.ts) {
                        paint_light(ui, background, top, &palette, light);
                    }
                }
            },
        );
        ui.add_space(12.0);
        if to_bottom {
            // Jump, don't glide; the pin below keeps it there as the content
            // settles. The scroll area moves only once this pass is drawn,
            // so the pass is drawn again: shown as it is, a conversation just
            // opened would first sit where the last one was scrolled to.
            ui.scroll_to_cursor_animation(
                Some(Align::BOTTOM),
                egui::style::ScrollAnimation::none(),
            );
            // On the first pass the rows are placed by guesses, so the end
            // it scrolls to is a guess too: the next pass scrolls again, to
            // the end as measured.
            if ui.ctx().current_pass_index() == 0 {
                app_scroll_again = true;
                ui.ctx().request_discard("scrolled to the newest message");
            }
        }
    });
    if app_scroll_again {
        app.scroll_to_bottom.insert(scroll_key.clone());
    }
    let content = output.content_size.y;
    let offset = output.state.offset.y;
    let bottom = (content - output.inner_rect.height()).max(0.0);
    // Whether the reader is at the newest message. egui's own stick-to-end
    // only holds while the offset equals the end exactly, which a jump that
    // overshoots by the item spacing never does, so pictures loading after
    // the first layout used to strand the view. Here: opening a conversation
    // pins it; a move while the content holds still is the reader's and
    // decides; growth (pictures, new messages, older history) keeps the
    // intent, and a pinned view follows the new end.
    let pin_id = egui::Id::new(("pinned", &scroll_key));
    let (mut pinned, last_content) = ui
        .data(|d| d.get_temp::<(bool, f32)>(pin_id))
        .unwrap_or((true, content));
    if to_bottom {
        pinned = true;
    } else if (content - last_content).abs() < 0.5 {
        pinned = offset >= bottom - 2.0;
    }
    // A jump holds the view itself.
    if steering || detached {
        pinned = false;
    }
    let view = output.inner_rect;
    if !steering && timeline.is_some_and(|t| t.loaded) {
        // Away from the newest messages, or from the "New" line above.
        if detached || bottom - offset > view.height() * 0.5 {
            let at = egui::pos2(view.center().x, view.bottom() - 28.0);
            if pill(ui, &palette, at, Icon::ArrowDown, &t("Jump to newest")) {
                actions.push(Action::JumpToNewest);
            }
        }
        if unread_top.is_some_and(|top| top < offset - 1.0) {
            let at = egui::pos2(view.center().x, view.top() + 24.0);
            if pill(ui, &palette, at, Icon::ArrowUp, &t("Jump to unread")) {
                actions.push(Action::JumpToUnread);
            }
        }
    }
    if let Some(index) = jumps.iter().position(|j| j.list == scroll_key) {
        let loading = timeline.is_none_or(|t| t.loading || !t.loaded || t.around.is_some());
        let view = output.inner_rect.height();
        let jump = &mut jumps[index];
        if let Some(wanted) = jump.steer(target, offset, view, bottom, loading, now) {
            ui.data_mut(|d| d.insert_temp(offset_id, wanted));
        }
        if jump.done(now) {
            jumps.remove(index);
        } else {
            ui.ctx().request_repaint();
        }
    }
    if steering {
        // The jump moved the view; nothing else may this frame.
    } else if pinned {
        if offset < bottom - 1.0 {
            ui.data_mut(|d| d.insert_temp(offset_id, bottom));
            ui.ctx().request_repaint();
        }
    } else if prepended && let Some(before) = previous_height {
        // Older history went in above: keep the messages being read in place.
        let anchored = offset + (content - before).max(0.0);
        ui.data_mut(|d| d.insert_temp(offset_id, anchored));
        ui.ctx().request_repaint();
    } else if moved.abs() > 0.5 {
        // Rows above the one being read were drawn for the first time, or
        // changed while out of view, and are not the height they were
        // placed with: move the view with them so the reading stays put.
        let kept = (offset + moved).clamp(0.0, bottom);
        ui.data_mut(|d| d.insert_temp(offset_id, kept));
        ui.ctx().request_repaint();
    }
    ui.data_mut(|d| d.insert_temp(heights_id, heights));
    ui.data_mut(|d| {
        d.insert_temp(pin_id, (pinned, content));
        d.insert_temp(height_id, content);
    });
    // Near the top: fetch the page before. Not on the frame that jumps to the
    // bottom, whose offset still reads from before the jump.
    if !to_bottom
        && !steering
        && !pinned
        && offset < 120.0
        && let Some(timeline) = timeline
        && timeline.loaded
        && timeline.has_more
        && !timeline.loading
        && content > output.inner_rect.height()
    {
        actions.push(Action::LoadOlder);
    }
    // Near the end of older history: read on towards the present.
    if !steering
        && offset > bottom - 120.0
        && let Some(timeline) = timeline
        && timeline.loaded
        && timeline.has_newer
        && !timeline.loading
    {
        actions.push(Action::LoadNewer);
    }
}

/// How far beyond the view rows are still drawn, so they are measured
/// before they scroll in.
const MARGIN: f32 = 400.0;

/// A row of the message list.
enum Item<'a> {
    /// "Load older messages", or the beginning of the conversation.
    Top,
    /// Newer messages to load, in a list of older history.
    Bottom,
    Message {
        message: &'a crate::model::Message,
        lead: Lead,
        new_day: bool,
        unread: bool,
    },
}

impl Item<'_> {
    fn entry(&self, has_more: bool, look: message::Look) -> rows::Entry {
        match self {
            Item::Top => rows::Entry {
                key: egui::Id::new("top").value(),
                guess: if has_more { 52.0 } else { 120.0 },
            },
            Item::Bottom => rows::Entry {
                key: egui::Id::new("bottom").value(),
                guess: 52.0,
            },
            Item::Message {
                message,
                lead,
                new_day,
                unread,
            } => rows::Entry {
                key: egui::Id::new(message.ts.as_str()).value(),
                guess: message::guess_height(message, *lead, look)
                    + if *new_day { DAY_SEPARATOR } else { 0.0 }
                    + if *unread { NEW_LINE } else { 0.0 },
            },
        }
    }
}

/// The height of a day separator.
const DAY_SEPARATOR: f32 = 36.0;
/// The height of the "New" line.
const NEW_LINE: f32 = 20.0;

/// The local day of each message, worked out once per day rather than
/// once per message: messages come in order, so most share the day of the
/// one before.
#[derive(Default)]
struct Days {
    /// The last day found.
    current: Option<Day>,
}

/// A local day, and the seconds it spans.
#[derive(Clone, Copy)]
struct Day {
    /// Its first second.
    start: i64,
    /// The first second of the next day.
    end: i64,
    date: jiff::civil::Date,
}

impl Days {
    fn of(&mut self, ts: &Ts) -> Option<jiff::civil::Date> {
        let seconds = ts.seconds()?;
        if let Some(day) = self.current
            && (day.start..day.end).contains(&seconds)
        {
            return Some(day.date);
        }
        let zoned = ts.zoned()?;
        let date = zoned.date();
        let start = zoned.start_of_day().ok()?.timestamp().as_second();
        let end = zoned
            .tomorrow()
            .ok()
            .and_then(|next| next.start_of_day().ok())
            .map_or(start + 86_400, |next| next.timestamp().as_second());
        self.current = Some(Day { start, end, date });
        Some(date)
    }
}

/// The top of the list: older history to load, or where it begins.
fn top(
    ui: &mut egui::Ui,
    workspace: &crate::app::WorkspaceState,
    conversation: &crate::model::Conversation,
    timeline: &crate::model::Timeline,
    palette: &crate::theme::Palette,
    actions: &mut Vec<Action>,
) {
    if timeline.has_more {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            if timeline.loading {
                ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
            } else if ui
                .add(
                    egui::Button::new(
                        RichText::new(t("Load older messages"))
                            .font(theme::medium(13.0))
                            .color(palette.secondary),
                    )
                    .fill(palette.surface),
                )
                .clicked()
            {
                actions.push(Action::LoadOlder);
            }
        });
        ui.add_space(12.0);
    } else {
        beginning(ui, workspace, conversation, palette);
    }
}

/// A small floating button centred on `at`, over the message list.
/// Returns whether it was clicked.
fn pill(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    at: egui::Pos2,
    icon: Icon,
    label: &str,
) -> bool {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        theme::semibold(12.5),
        egui::Color32::WHITE,
    );
    let size = Vec2::new(galley.size().x + 44.0, 28.0);
    let rect = egui::Rect::from_center_size(at, size);
    let response = ui
        .interact(
            rect,
            egui::Id::new(("jump-pill", label)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = if response.hovered() {
        palette.accent.gamma_multiply(0.85)
    } else {
        palette.accent
    };
    ui.painter().add(
        egui::epaint::Shadow {
            offset: [0, 2],
            blur: 8,
            spread: 0,
            color: palette.shadow,
        }
        .as_shape(rect, CornerRadius::same(14)),
    );
    ui.painter().rect_filled(rect, CornerRadius::same(14), fill);
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        Vec2::splat(14.0),
    );
    icon.image(egui::Color32::WHITE, 14.0)
        .paint_at(ui, icon_rect);
    ui.painter().galley(
        egui::pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0),
        galley,
        egui::Color32::WHITE,
    );
    theme::describe(&response, egui::WidgetType::Button, label);
    response.clicked()
}

/// Lights up the message just drawn from `top` down, behind it in the
/// slot `background`, as strongly as `light` (0 to 1): the message a jump
/// brought into view.
pub(super) fn paint_light(
    ui: &egui::Ui,
    background: egui::layers::ShapeIdx,
    top: f32,
    palette: &crate::theme::Palette,
    light: f32,
) {
    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top());
    ui.painter().set(
        background,
        egui::Shape::rect_filled(
            rect,
            CornerRadius::ZERO,
            palette.accent.gamma_multiply(0.22 * light),
        ),
    );
}

/// The end of a list of older history: newer messages to load.
fn bottom_row(
    ui: &mut egui::Ui,
    timeline: &crate::model::Timeline,
    palette: &crate::theme::Palette,
    actions: &mut Vec<Action>,
) {
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        if timeline.loading {
            ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
        } else if ui
            .add(
                egui::Button::new(
                    RichText::new(t("Load newer messages"))
                        .font(theme::medium(13.0))
                        .color(palette.secondary),
                )
                .fill(palette.surface),
            )
            .clicked()
        {
            actions.push(Action::LoadNewer);
        }
    });
    ui.add_space(12.0);
}

fn beginning(
    ui: &mut egui::Ui,
    workspace: &crate::app::WorkspaceState,
    conversation: &crate::model::Conversation,
    palette: &crate::theme::Palette,
) {
    egui::Frame::new()
        .inner_margin(Margin {
            left: 20,
            right: 20,
            top: 28,
            bottom: 12,
        })
        .show(ui, |ui| {
            let title = workspace.title(conversation);
            let (heading, line) = match conversation.kind {
                ConversationKind::Direct => (
                    title.clone(),
                    tf(
                        "This is the very beginning of your direct message history with {name}.",
                        &[("name", &title)],
                    ),
                ),
                ConversationKind::Group => (
                    title.clone(),
                    t("This is the very beginning of this group conversation.").into_owned(),
                ),
                _ => (
                    format!("#{title}"),
                    tf(
                        "This is the very beginning of #{name}.",
                        &[("name", &title)],
                    ),
                ),
            };
            ui.label(
                RichText::new(heading)
                    .font(theme::bold(24.0))
                    .color(palette.text),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(line)
                    .font(theme::regular(14.0))
                    .color(palette.secondary),
            );
            if !conversation.purpose.is_empty() {
                ui.label(
                    RichText::new(crate::mrkdwn::plain(&conversation.purpose, |_| None))
                        .font(theme::regular(14.0))
                        .color(palette.secondary),
                );
            }
        });
}

fn day_separator(ui: &mut egui::Ui, palette: &crate::theme::Palette, label: &str) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), DAY_SEPARATOR),
        egui::Sense::hover(),
    );
    let y = rect.center().y;
    ui.painter().hline(
        rect.x_range().shrink(16.0),
        y,
        Stroke::new(1.0, palette.outline),
    );
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::semibold(12.5), palette.text);
    let pill = egui::Rect::from_center_size(rect.center(), galley.size() + Vec2::new(24.0, 8.0));
    ui.painter()
        .rect_filled(pill, CornerRadius::same(12), palette.window);
    ui.painter().rect_stroke(
        pill,
        CornerRadius::same(12),
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(pill.center() - galley.size() / 2.0, galley, palette.text);
}

fn new_line(ui: &mut egui::Ui, palette: &crate::theme::Palette) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), NEW_LINE),
        egui::Sense::hover(),
    );
    let y = rect.center().y;
    let range = egui::Rangef::new(rect.left() + 16.0, rect.right() - 16.0);
    ui.painter()
        .hline(range, y, Stroke::new(1.0, palette.badge));
    let galley = ui.painter().layout_no_wrap(
        t("New").into_owned(),
        theme::bold(11.0),
        egui::Color32::WHITE,
    );
    let pill = egui::Rect::from_min_size(
        egui::pos2(
            range.max - galley.size().x - 12.0,
            y - galley.size().y / 2.0 - 2.0,
        ),
        galley.size() + Vec2::new(12.0, 4.0),
    );
    ui.painter()
        .rect_filled(pill, CornerRadius::same(4), palette.badge);
    ui.painter()
        .galley(pill.min + Vec2::new(6.0, 2.0), galley, egui::Color32::WHITE);
}
