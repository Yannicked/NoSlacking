//! The search window: a query passed to Slack as typed, messages or files,
//! by relevance or newest first, with the matching words lit up. Results
//! load a page at a time as the list is scrolled; picking one shows it in
//! its conversation.

use egui::text::{LayoutJob, TextFormat};
use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Vec2};

use crate::app::{App, WorkspaceState};
use crate::failure::Failure;
use crate::i18n::{t, tf, tn};
use crate::model::{Action, ConversationKind};
use crate::search::{Heading, Hit, Query, Scope, Sort};
use crate::theme::{self, Icon, Palette};

/// The results list's tallest.
const LIST_HEIGHT: f32 = 460.0;
/// How close to the end of the list the next page is asked for.
const MORE_WITHIN: f32 = 240.0;

pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.search.open {
        return;
    }
    let palette = app.palette;
    let frame = super::overlays::modal_frame(app);
    let App {
        workspaces,
        settings,
        search,
        actions,
        ..
    } = app;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        search.open = false;
        return;
    };
    let groups = crate::search::groups(&search.hits, search.sort, |ts| {
        ts.zoned()
            .map(|z| i64::from(z.date().year()) * 1000 + i64::from(z.date().day_of_year()))
    });
    // The results in the order they are shown, for the arrow keys.
    let order: Vec<usize> = groups.iter().flat_map(|g| g.hits.iter().copied()).collect();
    let (down, up, enter, escape) = ctx.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::Enter),
            input.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    let moved = (down || up) && !order.is_empty();
    if !order.is_empty() {
        if down {
            search.selected = (search.selected + 1).min(order.len() - 1);
        }
        if up {
            search.selected = search.selected.saturating_sub(1);
        }
        search.selected = search.selected.min(order.len() - 1);
    }
    // Enter runs what is typed when it is not what the results are for,
    // and otherwise opens the picked result.
    let asked = search.query.as_ref().is_some_and(|q| {
        q.text == search.text.trim()
            && q.scope == search.scope
            && q.sort == search.sort
            && q.team == workspace.info.team_id
    });
    let mut run = enter && !asked;
    let mut open: Option<usize> = (enter && asked)
        .then(|| order.get(search.selected).copied())
        .flatten();
    let mut close = escape;
    let mut more = false;
    let focus = std::mem::take(&mut search.focus);
    let response = egui::Modal::new(egui::Id::new("search"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(640.0);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                Icon::Search.image(palette.dim, 18.0).paint_at(ui, rect);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut search.text)
                        .id(egui::Id::new("search-query"))
                        .hint_text(t("Search messages and files"))
                        .font(theme::regular(16.0))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8)),
                );
                if focus {
                    field.request_focus();
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(t(
                    "Narrow it down with from:@name, in:#channel, before:2025-01-31, after:2025-01-01 or has:link",
                ))
                .font(theme::regular(12.0))
                .color(palette.dim),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                for (scope, label) in [(Scope::Messages, t("Messages")), (Scope::Files, t("Files"))] {
                    if tab(ui, &palette, &label, search.scope == scope) && search.scope != scope {
                        search.scope = scope;
                        run = true;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (sort, label) in [(Sort::Newest, t("Newest")), (Sort::Relevant, t("Most relevant"))] {
                        if tab(ui, &palette, &label, search.sort == sort) && search.sort != sort {
                            search.sort = sort;
                            run = true;
                        }
                    }
                });
            });
            ui.add_space(6.0);
            status(ui, &palette, search);
            if search.hits.is_empty() {
                return;
            }
            let output = egui::ScrollArea::vertical()
                .id_salt("search-results")
                .max_height(LIST_HEIGHT)
                // The window grows with its content, so it would otherwise
                // keep the list as short as it first was.
                .min_scrolled_height(LIST_HEIGHT)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let mut position = 0;
                    for group in &groups {
                        heading(ui, &palette, workspace, &group.heading, &search.hits);
                        for &index in &group.hits {
                            let selected = position == search.selected;
                            let hit = &search.hits[index];
                            let text = prepared(ui.ctx(), workspace, search.query.as_ref(), hit);
                            let row = hit_row(
                                ui,
                                &palette,
                                workspace,
                                hit,
                                &text,
                                selected,
                                search.sort,
                            );
                            if selected && moved {
                                row.scroll_to_me(None);
                            }
                            if row.clicked() {
                                open = Some(index);
                            }
                            position += 1;
                        }
                    }
                    if search.loading {
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
                        });
                        ui.add_space(8.0);
                    }
                });
            let left = output.content_size.y - output.state.offset.y - output.inner_rect.height();
            more = left < MORE_WITHIN;
        });
    if response.should_close() {
        close = true;
    }
    if let Some(hit) = open.and_then(|index| search.hits.get(index)) {
        match (&hit.channel, &hit.ts) {
            (Some(channel), Some(ts)) => actions.push(Action::JumpTo {
                channel: channel.clone(),
                ts: ts.clone(),
                thread: hit.thread.clone(),
            }),
            // A file never shared in a conversation: its page in Slack.
            _ => {
                if let Some(link) = &hit.permalink {
                    actions.push(Action::OpenUrl(link.clone()));
                }
            }
        }
        close = true;
    }
    if close {
        search.open = false;
    } else if run {
        actions.push(Action::RunSearch);
    } else if more {
        actions.push(Action::SearchMore);
    }
}

/// A tab or toggle; returns whether it was clicked.
fn tab(ui: &mut egui::Ui, palette: &Palette, label: &str, on: bool) -> bool {
    let text = RichText::new(label)
        .font(if on {
            theme::semibold(13.0)
        } else {
            theme::regular(13.0)
        })
        .color(if on { palette.text } else { palette.secondary });
    let response = ui.add(
        egui::Button::new(text)
            .fill(if on {
                palette.surface_hover
            } else {
                egui::Color32::TRANSPARENT
            })
            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL)),
    );
    theme::describe_selected(&response, egui::WidgetType::SelectableLabel, on, label);
    response.clicked()
}

/// How many results there are, or why there are none.
fn status(ui: &mut egui::Ui, palette: &Palette, search: &crate::search::Search) {
    let text = match &search.failure {
        Some(Failure::MissingPermission) => Some((
            t("Searching needs the search:read permission, which this sign-in lacks. Sign in to the workspace again to allow it; with your own Slack app, update it from the manifest first.")
                .into_owned(),
            palette.warning,
        )),
        Some(failure) => Some((
            tf("Could not search: {error}", &[("error", &failure.message())]),
            palette.warning,
        )),
        None if search.query.is_none() => None,
        None if search.loading && search.hits.is_empty() => {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
            });
            None
        }
        None if search.hits.is_empty() => Some((t("No results.").into_owned(), palette.dim)),
        None => Some((
            tn("{count} result", "{count} results", search.total),
            palette.dim,
        )),
    };
    if let Some((text, color)) = text {
        ui.label(RichText::new(text).font(theme::regular(12.5)).color(color));
        ui.add_space(4.0);
    }
}

/// What a group of results is under: its conversation, or its day.
fn heading(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    heading: &Heading,
    hits: &[Hit],
) {
    let label = match heading {
        Heading::Conversation(id) => {
            let name = hits
                .iter()
                .find(|h| h.channel.as_deref() == Some(id.as_str()))
                .map_or("", |h| h.channel_name.as_str());
            conversation_label(workspace, Some(id), name)
        }
        Heading::Day(ts) => super::day_label(ts),
    };
    ui.add_space(6.0);
    super::section_label(ui, palette, &label);
    ui.add_space(2.0);
}

/// A conversation as a result names it: `#name` for a channel, the
/// person or people for a direct message.
fn conversation_label(workspace: &WorkspaceState, id: Option<&str>, name: &str) -> String {
    match id.and_then(|id| workspace.conversation(id)) {
        Some(conversation) => match conversation.kind {
            ConversationKind::Channel | ConversationKind::Private => {
                format!("#{}", workspace.title(conversation))
            }
            _ => workspace.title(conversation),
        },
        // Slack names a DM by the other person's id.
        None if name.starts_with('U') && workspace.user(name).is_some() => {
            workspace.user_label(name)
        }
        None if name.is_empty() => t("Unknown").into_owned(),
        None => format!("#{name}"),
    }
}

/// A result's text made ready to draw.
#[derive(Debug)]
struct Prepared {
    /// The text in pieces, the matching ones flagged.
    segments: Vec<(String, bool)>,
    /// The text as a screen reader reads it, without the match markers.
    spoken: String,
}

/// The prepared texts of one search's results, kept in egui's memory.
#[derive(Clone, Default)]
struct Texts {
    /// The query, and the people's count and version, they were made for:
    /// people are named in the text.
    stamp: Option<(Query, usize, u64)>,
    texts: std::collections::HashMap<String, std::sync::Arc<Prepared>>,
}

/// `hit`'s text, worked out once per result rather than on every frame:
/// turning mrkdwn into plain words for each result, each frame, slowed a
/// long list of results.
fn prepared(
    ctx: &egui::Context,
    workspace: &WorkspaceState,
    query: Option<&Query>,
    hit: &Hit,
) -> std::sync::Arc<Prepared> {
    let stamp = query.map(|q| (q.clone(), workspace.users.len(), workspace.users_version()));
    ctx.data_mut(|d| {
        let memo = d.get_temp_mut_or_default::<Texts>(egui::Id::new("search-texts"));
        if memo.stamp != stamp {
            memo.stamp = stamp;
            memo.texts.clear();
        }
        memo.texts
            .entry(hit.key.clone())
            .or_insert_with(|| {
                let text = match &hit.file {
                    Some(file) => {
                        let size = super::file_size(file.size);
                        format!("{} · {size}", hit.text)
                    }
                    None => plain(workspace, &hit.text),
                };
                std::sync::Arc::new(Prepared {
                    segments: crate::search::segments(&text),
                    spoken: crate::search::unmarked(&hit.text),
                })
            })
            .clone()
    })
}

/// One result: who and when, and the text with its matches lit up.
fn hit_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    hit: &Hit,
    text: &Prepared,
    selected: bool,
    sort: Sort,
) -> egui::Response {
    let author = match (&hit.user, &hit.username) {
        (Some(user), _) => workspace.user_label(user),
        (None, Some(name)) => name.clone(),
        (None, None) => t("Unknown").into_owned(),
    };
    let avatar = hit
        .user
        .as_deref()
        .and_then(|id| workspace.user(id))
        .and_then(|u| u.avatar.clone());
    let when = hit.when.as_ref().map(|ts| match sort {
        // Under a day's heading the time says enough.
        Sort::Newest => super::short_time(ts),
        Sort::Relevant => tf(
            "{date} at {time}",
            &[
                ("date", &super::day_label(ts)),
                ("time", &super::short_time(ts)),
            ],
        ),
    });
    let place = match sort {
        Sort::Newest => Some(conversation_label(
            workspace,
            hit.channel.as_deref(),
            &hit.channel_name,
        )),
        Sort::Relevant => None,
    };
    let background = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        .inner_margin(Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                match &hit.file {
                    Some(_) => {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(6),
                            palette.surface_hover,
                        );
                        Icon::FileText
                            .image(palette.secondary, 16.0)
                            .paint_at(ui, rect.shrink(6.0));
                    }
                    None => {
                        super::avatar(
                            ui,
                            avatar.as_deref(),
                            &author,
                            hit.user.as_deref().unwrap_or(&author),
                            28.0,
                        );
                    }
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(
                            RichText::new(&author)
                                .font(theme::semibold(13.5))
                                .color(palette.text),
                        );
                        if let Some(place) = &place {
                            ui.label(
                                RichText::new(place)
                                    .font(theme::regular(12.0))
                                    .color(palette.secondary),
                            );
                        }
                        if let Some(when) = &when {
                            ui.label(
                                RichText::new(when)
                                    .font(theme::regular(12.0))
                                    .color(palette.dim),
                            );
                        }
                        if hit.thread.is_some() {
                            ui.label(
                                RichText::new(t("in a thread"))
                                    .font(theme::regular(12.0))
                                    .color(palette.dim),
                            );
                        }
                    });
                    let mut job = highlighted(&text.segments, palette, ui.available_width());
                    job.wrap.max_rows = 3;
                    ui.label(job);
                });
            });
        });
    let response = ui
        .interact(inner.response.rect, ui.id().with(&hit.key), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = if selected {
        Some(palette.accent.gamma_multiply(0.18))
    } else if response.hovered() {
        Some(palette.surface_hover)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(
                inner.response.rect,
                CornerRadius::same(theme::RADIUS_SMALL),
                fill,
            ),
        );
    }
    theme::describe(
        &response,
        egui::WidgetType::Button,
        &format!("{author}: {}", text.spoken),
    );
    response
}

/// A result's mrkdwn as plain words, people and channels by name, with
/// Slack's match markers kept for [`highlighted`].
fn plain(workspace: &WorkspaceState, text: &str) -> String {
    crate::mrkdwn::plain(text, |inline| match inline {
        crate::mrkdwn::Inline::User { id, .. } => Some(format!("@{}", workspace.user_label(id))),
        crate::mrkdwn::Inline::Channel { id, label } => Some(format!(
            "#{}",
            workspace
                .conversation(id)
                .map(|c| c.name.clone())
                .or_else(|| label.clone())
                .unwrap_or_else(|| id.clone())
        )),
        _ => None,
    })
}

/// Text in `segments` laid out with its matching words lit up.
fn highlighted(segments: &[(String, bool)], palette: &Palette, width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    for (piece, matched) in segments {
        let format = if *matched {
            TextFormat {
                font_id: theme::semibold(13.5),
                color: palette.text,
                background: palette.accent.gamma_multiply(0.35),
                ..TextFormat::default()
            }
        } else {
            TextFormat {
                font_id: theme::regular(13.5),
                color: palette.secondary,
                ..TextFormat::default()
            }
        };
        job.append(piece, 0.0, format);
    }
    job
}
