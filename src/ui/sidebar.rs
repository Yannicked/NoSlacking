//! The workspace rail and the conversation list.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use crate::app::{App, Page, WorkspaceState};
use crate::i18n::{t, tf, tn};
use crate::model::{Ability, Action, Conversation, ConversationKind, SectionKind, SidebarSection};
use crate::sidebar::{self, SidebarEdit};
use crate::theme::{self, Icon, Palette};

/// Direct messages listed before the "N more" row.
const DM_LIMIT: usize = 25;

pub fn rail(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let rail_fill = if palette.dark {
        palette.panel.gamma_multiply(0.8)
    } else {
        palette.surface_active
    };
    let inset = theme::titlebar_inset(ui.ctx());
    egui::Panel::left("rail")
        .exact_size(theme::RAIL_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(rail_fill).inner_margin(Margin {
            left: 0,
            right: 0,
            top: 12 + inset as i8,
            bottom: 12,
        }))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                let active = app.active_team();
                for workspace in &app.workspaces {
                    let selected = active.as_deref() == Some(workspace.info.team_id.as_str());
                    let unread = workspace
                        .conversations
                        .iter()
                        .any(|c| workspace.is_unread(c));
                    let mentions: u32 = workspace.conversations.iter().map(|c| c.mentions).sum();
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
                    if selected {
                        ui.painter().rect_stroke(
                            rect.expand(3.0),
                            CornerRadius::same(12),
                            Stroke::new(2.0, palette.text),
                            egui::StrokeKind::Outside,
                        );
                    }
                    super::paint_avatar(
                        ui,
                        rect,
                        workspace.info.icon.as_deref(),
                        &workspace.info.name,
                        &workspace.info.team_id,
                    );
                    if workspace.signed_out.is_some() {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(9), palette.shadow);
                        Icon::CircleAlert.image(palette.warning, 18.0).paint_at(
                            ui,
                            egui::Rect::from_center_size(rect.center(), Vec2::splat(18.0)),
                        );
                    }
                    if mentions > 0 && !selected {
                        let dot = egui::Rect::from_center_size(
                            rect.right_bottom() - Vec2::splat(2.0),
                            // "9+" needs a pill rather than a dot.
                            Vec2::new(if mentions > 9 { 22.0 } else { 16.0 }, 16.0),
                        );
                        ui.painter()
                            .rect_filled(dot, CornerRadius::same(8), palette.badge);
                        ui.painter().text(
                            dot.center(),
                            egui::Align2::CENTER_CENTER,
                            rail_count(mentions),
                            theme::bold(10.0),
                            egui::Color32::WHITE,
                        );
                    } else if unread && !selected {
                        let left = egui::pos2(rect.left() - 11.0, rect.center().y);
                        ui.painter().rect_filled(
                            egui::Rect::from_center_size(left, Vec2::new(4.0, 8.0)),
                            CornerRadius::same(2),
                            palette.text,
                        );
                    }
                    let tip = match &workspace.signed_out {
                        Some(reason) => format!("{} ({})", workspace.info.name, reason.message()),
                        None => workspace.info.name.clone(),
                    };
                    theme::focus_ring(ui, &response, &palette, 12);
                    theme::describe_selected(
                        &response,
                        egui::WidgetType::SelectableLabel,
                        selected,
                        &spoken(&tip, unread, mentions),
                    );
                    let response = response
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(tip);
                    if response.clicked() {
                        app.actions
                            .push(Action::SelectWorkspace(workspace.info.team_id.clone()));
                    }
                }
                let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
                let fill = if response.hovered() {
                    palette.surface_hover
                } else {
                    palette.surface
                };
                ui.painter().rect_filled(rect, CornerRadius::same(9), fill);
                Icon::Plus.image(palette.secondary, 20.0).paint_at(
                    ui,
                    egui::Rect::from_center_size(rect.center(), Vec2::splat(20.0)),
                );
                theme::focus_ring(ui, &response, &palette, 9);
                theme::describe(&response, egui::WidgetType::Button, &t("Add a workspace"));
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t("Add a workspace"))
                    .clicked()
                {
                    app.actions.push(Action::AddWorkspace);
                }
            });
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                let settings_open = app.page == Page::Settings;
                let tip = tf(
                    "Settings ({shortcut})",
                    &[("shortcut", &super::keys::command(","))],
                );
                let response = theme::icon_button(ui, &palette, Icon::Settings, 20.0, &tip);
                if response.clicked() {
                    app.actions.push(if settings_open {
                        Action::HideSettings
                    } else {
                        Action::ShowSettings
                    });
                }
                if let Some(workspace) = crate::app::active_in(&app.workspaces, &app.settings) {
                    super::people::me_button(
                        ui,
                        &palette,
                        workspace,
                        app.settings.desktop.stay_active,
                        &mut app.actions,
                    );
                }
            });
        });
}

/// What a screen reader says for a workspace or a conversation: its name,
/// and whether there is something new in it.
fn spoken(name: &str, unread: bool, mentions: u32) -> String {
    if mentions > 0 {
        tf(
            "{name}, {mentions}",
            &[
                ("name", name),
                (
                    "mentions",
                    &tn("{count} mention", "{count} mentions", mentions),
                ),
            ],
        )
    } else if unread {
        tf("{name}, unread", &[("name", name)])
    } else {
        name.to_owned()
    }
}

/// The mention count on a workspace icon. The dot fits one digit, and
/// "9" for twelve mentions would undercount.
fn rail_count(mentions: u32) -> String {
    if mentions > 9 {
        "9+".to_owned()
    } else {
        mentions.to_string()
    }
}

/// Room between the header's last button and the sidebar's right edge,
/// where the panel's resize handle also sits: enough that a button's hover
/// highlight never touches the edge or hides the handle.
const HEADER_RIGHT: i8 = 12;
/// Room between the header's buttons, so their highlights do not merge.
const HEADER_GAP: f32 = 2.0;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let width = app.settings.sidebar_width;
    let now = app.now_seconds();
    let inset = theme::titlebar_inset(ui.ctx());
    let App {
        workspaces,
        settings,
        actions,
        sidebar_filter,
        socket,
        views,
        huddles,
        ..
    } = app;
    let Some(workspace) = crate::app::active_in(workspaces, settings) else {
        return;
    };
    let response = egui::Panel::left("sidebar")
        .resizable(true)
        .default_size(width)
        .size_range(200.0..=440.0)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(palette.panel))
        .show(ui, |ui| {
            egui::Panel::top("sidebar-header")
                .exact_size(52.0 + inset)
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(Margin {
                    left: 16,
                    right: HEADER_RIGHT,
                    top: inset as i8,
                    bottom: 0,
                }))
                .show(ui, |ui| {
                    let rect = ui.max_rect();
                    ui.painter().hline(
                        rect.x_range(),
                        rect.bottom() - 0.5,
                        Stroke::new(1.0, palette.outline),
                    );
                    ui.horizontal_centered(|ui| {
                        // The buttons are placed first, from the right edge
                        // in, so a long workspace name is cut short instead
                        // of pushing them past the edge.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = HEADER_GAP;
                            if workspace.info.offers(Ability::Channels) {
                                super::browse::header_buttons(ui, &palette, actions);
                            }
                            if workspace.sections.is_some()
                                && workspace.info.offers(Ability::Sections)
                                && theme::icon_button(
                                    ui,
                                    &palette,
                                    Icon::Plus,
                                    16.0,
                                    &t("New section"),
                                )
                                .clicked()
                            {
                                actions.push(Action::NameSection {
                                    rename: None,
                                    channel: None,
                                });
                            }
                            ui.add_space(4.0);
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&workspace.info.name)
                                                .font(theme::bold(16.0))
                                                .color(palette.text),
                                        )
                                        .truncate(),
                                    );
                                    let (color, tip) = match socket {
                                        crate::backend::Socket::Connected => {
                                            (palette.accent, t("Live"))
                                        }
                                        crate::backend::Socket::Off => {
                                            (palette.dim, t("Live updates off"))
                                        }
                                        crate::backend::Socket::Connecting => {
                                            (palette.warning, t("Connecting…"))
                                        }
                                        _ => (palette.danger, t("Offline")),
                                    };
                                    let (dot, response) =
                                        ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                                    ui.painter().circle_filled(dot.center(), 4.0, color);
                                    response.on_hover_text(tip);
                                    super::desktop::dnd_button(ui, &palette, workspace, actions);
                                },
                            );
                        });
                    });
                });
            if let Some(reason) = &workspace.signed_out {
                egui::Frame::new()
                    .fill(palette.warning.gamma_multiply(0.15))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(tf(
                                "Signed out: {reason}",
                                &[("reason", &reason.message())],
                            ))
                            .font(theme::regular(12.5))
                            .color(palette.text),
                        );
                        if ui.link(t("Sign in again")).clicked() {
                            actions.push(Action::AddWorkspace);
                        }
                    });
            }
            // The huddle being listened to, at the foot, as in Slack.
            super::call_bar::panel(
                ui,
                "sidebar-call-bar",
                &palette,
                huddles.listening.as_ref(),
                workspaces,
                false,
                actions,
            );
            egui::Frame::new()
                .inner_margin(Margin {
                    left: 10,
                    right: 10,
                    top: 10,
                    bottom: 4,
                })
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(sidebar_filter)
                            .id(egui::Id::new("sidebar-filter"))
                            // No shortcut here: Ctrl+K opens the switcher.
                            .hint_text(t("Find a conversation"))
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(8, 5)),
                    );
                });
            egui::ScrollArea::vertical()
                .id_salt(("sidebar", &workspace.info.team_id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    if sidebar_filter.trim().is_empty() {
                        super::views::entries(ui, &palette, workspace, views, actions);
                    }
                    list(
                        ui,
                        &palette,
                        workspace,
                        sidebar_filter,
                        settings,
                        now,
                        actions,
                    );
                    ui.add_space(12.0);
                });
        });
    let width = response.response.rect.width();
    if (width - app.settings.sidebar_width).abs() > 1.0 {
        app.settings.sidebar_width = width;
        app.settings_changed();
    }
}

/// Where a workspace's remembered sidebar layout lives in egui's memory.
fn memo_id(team: &str) -> egui::Id {
    egui::Id::new(("sidebar-layout", team))
}

/// The open conversation's place as the sidebar holds it (see
/// [`sidebar::Hold`]), so stepping through the sidebar by keyboard follows
/// the order on screen.
pub(super) fn held(ctx: &egui::Context, team: &str) -> Option<sidebar::Hold> {
    ctx.data(|d| {
        d.get_temp::<sidebar::Memo>(memo_id(team))
            .and_then(|memo| memo.held().cloned())
    })
}

fn matches(workspace: &WorkspaceState, conversation: &Conversation, filter: &str) -> bool {
    filter.is_empty()
        || workspace
            .title(conversation)
            .to_lowercase()
            .contains(&filter.to_lowercase())
}

/// The workspace's conversations, section by section, narrowed to those
/// whose names hold `filter` and arranged as `settings` say; `now` decides
/// which have gone quiet.
fn list(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    filter: &str,
    settings: &crate::settings::Settings,
    now: i64,
    actions: &mut Vec<Action>,
) {
    let filter = filter.trim();
    let closed = settings.closed.get(&workspace.info.team_id);
    let sections = workspace.sections.as_deref();
    let drafts = ui.data(|d| {
        d.get_temp::<std::sync::Arc<std::collections::HashSet<String>>>(super::drafts_id())
    });
    // Remembered per workspace: sorting every conversation each frame is
    // the sidebar's main cost, and the order rarely changes.
    let shown = ui.data_mut(|d| {
        let memo = d.get_temp_mut_or_default::<sidebar::Memo>(memo_id(&workspace.info.team_id));
        let rank = |c: &Conversation| workspace.rank(c);
        let hold = memo.hold(workspace.active.as_deref(), &workspace.conversations, rank);
        memo.layout(
            sections,
            &workspace.conversations,
            &workspace.users,
            |c| workspace.title(c),
            rank,
            &sidebar::Arrange {
                sort: settings.sidebar_sort,
                unread_first: settings.unread_first,
                hold: hold.as_ref(),
                tidy: Some(sidebar::Tidy {
                    after: settings.hide_inactive,
                    now,
                    drafts: drafts.as_deref(),
                }),
                revision: Some(workspace.revision()),
            },
        )
    });
    let ctx = ui.ctx().clone();
    let drawn = drawn(
        workspace,
        &shown,
        closed,
        drafts.as_deref(),
        filter,
        |key| folding(&ctx, &workspace.info.team_id, key),
    );
    for section in &drawn {
        section_view(ui, palette, workspace, section, actions);
    }
    if workspace.conversations.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.add(egui::Spinner::new().size(18.0).color(palette.dim));
        });
    }
}

/// A section as the sidebar draws it.
pub(super) struct Drawn<'s, 'a> {
    pub section: &'s sidebar::Shown<'a>,
    /// What its folded and "Show more" state is remembered under.
    pub key: String,
    /// Whether it is unfolded, as remembered.
    pub open: bool,
    /// Whether it lists all its rows: unfolded, or searched through.
    pub expanded: bool,
    /// The rows on screen, in order.
    pub rows: Vec<&'a Conversation>,
    /// The rows held back behind the "N more" row: quiet ones, and direct
    /// messages past the first [`DM_LIMIT`].
    pub more: usize,
    /// Whether the section was expanded past what it would hold back, so
    /// it ends with "Show less".
    pub less: bool,
}

/// Which rows of `shown` the sidebar draws, section by section, given
/// each section's remembered (open, showing all) state by its key. The
/// list and Alt+↑/↓ both use it, so the keys step through exactly the rows
/// on screen.
///
/// An open section holds back its quiet conversations (see
/// [`sidebar::is_inactive`]) and the direct messages past [`DM_LIMIT`]
/// behind one "N more" row, until it is expanded to show everything.
/// Searching shows every match, quiet or not. Direct messages closed here
/// (`closed`) or in Slack ([`sidebar::is_shut`], which reads `drafts`)
/// are left out, searched for or not, unless open: the switcher finds
/// them.
pub(super) fn drawn<'s, 'a>(
    workspace: &WorkspaceState,
    shown: &'s [sidebar::Shown<'a>],
    closed: Option<&std::collections::BTreeMap<String, String>>,
    drafts: Option<&std::collections::HashSet<String>>,
    filter: &str,
    folding: impl Fn(&str) -> (bool, bool),
) -> Vec<Drawn<'s, 'a>> {
    let filter = filter.trim();
    let active = |c: &Conversation| workspace.active.as_deref() == Some(c.id.as_str());
    let mut drawn = Vec::new();
    for section in shown {
        let rows: Vec<(&Conversation, bool)> = section
            .conversations
            .iter()
            .copied()
            .zip(
                section
                    .inactive
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(false)),
            )
            .filter(|(c, _)| matches(workspace, c, filter))
            // Closed ones stay out until something new arrives, unless open.
            .filter(|(c, _)| active(c) || !sidebar::is_closed(closed, c))
            // As are those Slack has closed, until something brings them back.
            .filter(|(c, _)| {
                let draft = drafts.is_some_and(|drafts| drafts.contains(&c.id));
                active(c) || !sidebar::is_shut(c, c.has_unread(), draft)
            })
            .filter(|(c, _)| {
                // Skip deactivated people's DMs unless they have something new.
                c.user
                    .as_deref()
                    .and_then(|u| workspace.user(u))
                    .is_none_or(|u| !u.deleted)
                    || c.has_unread()
            })
            .collect();
        if !filter.is_empty() && rows.is_empty() {
            continue;
        }
        let key = section
            .id
            .clone()
            .unwrap_or_else(|| format!("{:?}", section.kind));
        let (open, all) = folding(&key);
        let expanded = open || !filter.is_empty();
        let total = rows.len();
        let every = |rows: Vec<(&'a Conversation, bool)>| -> Vec<&'a Conversation> {
            rows.into_iter().map(|(c, _)| c).collect()
        };
        let (rows, more, less) = if !expanded {
            // A folded section still shows what is unread or open.
            let rows = every(rows)
                .into_iter()
                .filter(|c| workspace.is_unread(c) || active(c))
                .collect();
            (rows, 0, false)
        } else if !filter.is_empty() {
            (every(rows), 0, false)
        } else {
            let limit = match section.kind {
                SectionKind::DirectMessages => DM_LIMIT,
                _ => usize::MAX,
            };
            let tidy: Vec<&Conversation> = rows
                .iter()
                .filter(|(_, quiet)| !quiet)
                .map(|(c, _)| *c)
                .take(limit)
                .collect();
            let held_back = total - tidy.len();
            if held_back > 0 && all {
                (every(rows), 0, true)
            } else {
                (tidy, held_back, false)
            }
        };
        drawn.push(Drawn {
            section,
            key,
            open,
            expanded,
            rows,
            more,
            less,
        });
    }
    drawn
}

/// A section's remembered (open, showing all) state in workspace `team`.
pub(super) fn folding(ctx: &egui::Context, team: &str, key: &str) -> (bool, bool) {
    // Both survive restarts, per section.
    let open = ctx
        .data_mut(|d| d.get_persisted::<bool>(open_id(team, key)))
        .unwrap_or(true);
    let all = ctx
        .data_mut(|d| d.get_persisted::<bool>(more_id(team, key)))
        .unwrap_or(false);
    (open, all)
}

/// Where whether a section is unfolded is remembered. With the workspace,
/// as the sections without Slack's ids share their keys across them.
fn open_id(team: &str, key: &str) -> egui::Id {
    egui::Id::new(("section-open", team, key))
}

/// Where whether a section shows all its rows, quiet ones included, is
/// remembered, per workspace like [`open_id`].
fn more_id(team: &str, key: &str) -> egui::Id {
    egui::Id::new(("section-more", team, key))
}

fn section_view(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    drawn: &Drawn<'_, '_>,
    actions: &mut Vec<Action>,
) {
    let Drawn {
        section,
        key,
        open,
        expanded,
        rows,
        more,
        less,
    } = drawn;
    let (open, expanded, more, less) = (*open, *expanded, *more, *less);
    ui.add_space(8.0);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
    let icon = if expanded {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    };
    icon.image(palette.dim, 14.0).paint_at(
        ui,
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            Vec2::splat(14.0),
        ),
    );
    let mut job = egui::text::LayoutJob::simple_singleline(
        section.title.clone(),
        theme::semibold(13.0),
        palette.secondary,
    );
    let room = if section.kind == SectionKind::DirectMessages {
        // Leave the "+" its own room.
        68.0
    } else {
        40.0
    };
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - room);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0),
        galley,
        palette.secondary,
    );
    theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
    theme::describe_selected(
        &response,
        egui::WidgetType::CollapsingHeader,
        expanded,
        &section.title,
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        ui.data_mut(|d| d.insert_persisted(open_id(&workspace.info.team_id, key), !open));
    }
    if let (Some(id), Some(sections)) = (&section.id, workspace.sections.as_deref())
        && workspace.info.offers(Ability::Sections)
    {
        section_menu(&response, id, section.kind, sections, actions);
    }
    // A "+" at the end of the Direct messages line, as in Slack's client:
    // the quickest way to a new conversation with someone is where your
    // conversations with people are. It opens the New message dialog.
    if section.kind == SectionKind::DirectMessages && workspace.info.offers(Ability::NewMessage) {
        let plus = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 18.0, rect.center().y),
            Vec2::splat(24.0),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(plus)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        if theme::icon_button(&mut child, palette, Icon::Plus, 15.0, &t("Message someone"))
            .clicked()
        {
            actions.push(Action::Convos(crate::convos::Action::NewMessage));
        }
    }
    for conversation in rows {
        row(ui, palette, workspace, conversation, section, actions);
    }
    // One last row: "N more" expands the section, "Show less" tidies it
    // again, and the choice is remembered like the folding.
    let last = if more > 0 {
        let count = u32::try_from(more).unwrap_or(u32::MAX);
        Some((tn("{count} more", "{count} more", count), true))
    } else if less {
        Some((t("Show less").into_owned(), false))
    } else {
        None
    };
    if let Some((label, all)) = last {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
        theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
        theme::describe(&response, egui::WidgetType::Button, &label);
        ui.painter().text(
            egui::pos2(rect.left() + 32.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            theme::medium(13.0),
            palette.dim,
        );
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            ui.data_mut(|d| d.insert_persisted(more_id(&workspace.info.team_id, key), all));
        }
    }
}

/// Right-click on a section header: new section, and for your own
/// sections rename, move and delete.
fn section_menu(
    response: &egui::Response,
    id: &str,
    kind: SectionKind,
    sections: &[SidebarSection],
    actions: &mut Vec<Action>,
) {
    response.context_menu(|ui| {
        if ui.button(t("New section…")).clicked() {
            actions.push(Action::NameSection {
                rename: None,
                channel: None,
            });
            ui.close();
        }
        if kind == SectionKind::Custom {
            if ui.button(t("Rename…")).clicked() {
                actions.push(Action::NameSection {
                    rename: Some(id.to_owned()),
                    channel: None,
                });
                ui.close();
            }
            ui.separator();
            let can_up = crate::sidebar::can_shift(sections, id, true);
            let can_down = crate::sidebar::can_shift(sections, id, false);
            if ui
                .add_enabled(can_up, egui::Button::new(t("Move up")))
                .clicked()
            {
                actions.push(Action::Sidebar(SidebarEdit::Shift {
                    section: id.to_owned(),
                    up: true,
                }));
                ui.close();
            }
            if ui
                .add_enabled(can_down, egui::Button::new(t("Move down")))
                .clicked()
            {
                actions.push(Action::Sidebar(SidebarEdit::Shift {
                    section: id.to_owned(),
                    up: false,
                }));
                ui.close();
            }
            ui.separator();
            if ui.button(t("Delete section")).clicked() {
                // Its conversations go back to Channels and Direct messages.
                actions.push(Action::Sidebar(SidebarEdit::Delete {
                    section: id.to_owned(),
                }));
                ui.close();
            }
        }
    });
}

/// Right-click on a conversation: star it, move it to another section, or
/// make a new section for it.
fn row_menu(
    response: &egui::Response,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    section: &sidebar::Shown<'_>,
    actions: &mut Vec<Action>,
) {
    let Some(sections) = workspace
        .sections
        .as_deref()
        .filter(|_| workspace.info.offers(Ability::Sections))
    else {
        // Without Slack's sections there is nothing to move or star.
        response.context_menu(|ui| {
            window_items(ui, conversation, actions);
            super::browse::leave_item(ui, conversation, true, actions);
        });
        return;
    };
    let channel = conversation.id.clone();
    let starred = sidebar::is_starred(Some(sections), &channel);
    response.context_menu(|ui| {
        let star = if starred { t("Unstar") } else { t("Star") };
        if ui.button(star).clicked() {
            actions.push(Action::Sidebar(SidebarEdit::Star {
                channel: channel.clone(),
                starred: !starred,
            }));
            ui.close();
        }
        ui.menu_button(t("Move to section"), |ui| {
            for (id, title) in sidebar::targets(sections, section.id.as_deref()) {
                if ui.button(title).clicked() {
                    actions.push(Action::Sidebar(SidebarEdit::Move {
                        channel: channel.clone(),
                        from: section.id.clone(),
                        to: id,
                    }));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button(t("New section…")).clicked() {
                actions.push(Action::NameSection {
                    rename: None,
                    channel: Some(channel.clone()),
                });
                ui.close();
            }
        });
        if section.kind == SectionKind::Custom {
            let home = if conversation.kind.is_dm() {
                SectionKind::DirectMessages
            } else {
                SectionKind::Channels
            };
            if let Some(home) = sections.iter().find(|s| s.kind == home)
                && ui.button(t("Remove from section")).clicked()
            {
                actions.push(Action::Sidebar(SidebarEdit::Move {
                    channel: channel.clone(),
                    from: section.id.clone(),
                    to: home.id.clone(),
                }));
                ui.close();
            }
        }
        ui.separator();
        super::desktop::conversation_menu(ui, workspace, conversation, actions);
        ui.separator();
        window_items(ui, conversation, actions);
        super::browse::leave_item(ui, conversation, true, actions);
    });
}

/// What every conversation's menu offers, sections or not.
fn window_items(ui: &mut egui::Ui, conversation: &Conversation, actions: &mut Vec<Action>) {
    if ui.button(t("Open in new window")).clicked() {
        actions.push(Action::PopOut(conversation.id.clone()));
        ui.close();
    }
    if conversation.kind.is_dm() && ui.button(t("Close conversation")).clicked() {
        actions.push(Action::CloseConversation(conversation.id.clone()));
        ui.close();
    }
}

fn row(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    section: &sidebar::Shown<'_>,
    actions: &mut Vec<Action>,
) {
    // A view in place of the conversation leaves no row selected.
    let covered = ui
        .data(|d| d.get_temp::<bool>(super::views::open_id()))
        .unwrap_or(false);
    let selected = !covered && workspace.active.as_deref() == Some(conversation.id.as_str());
    let unread = workspace.is_unread(conversation) && !selected;
    let muted = workspace.desktop.is_muted(&conversation.id);
    let (outer, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    // A row scrolled out of view keeps its place, but its title, avatar
    // and badge are not laid out: a big workspace has hundreds of rows.
    if !ui.is_rect_visible(outer) {
        return;
    }
    let rect = outer.shrink2(Vec2::new(8.0, 0.0));
    if selected {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS_SMALL + 2),
            palette.accent,
        );
    } else if response.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS_SMALL + 2),
            palette.surface_hover,
        );
    }
    let text_color = if selected {
        palette.on_accent
    } else if unread {
        palette.text
    } else if muted {
        // Quieter than the rest, as Slack draws muted conversations.
        palette.dim
    } else {
        palette.secondary
    };
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 18.0, rect.center().y),
        Vec2::splat(16.0),
    );
    let title = workspace.title(conversation);
    match conversation.kind {
        ConversationKind::Channel => Icon::Hash.image(text_color, 15.0).paint_at(ui, icon_rect),
        ConversationKind::Private => Icon::Lock.image(text_color, 14.0).paint_at(ui, icon_rect),
        ConversationKind::Group => Icon::Users.image(text_color, 15.0).paint_at(ui, icon_rect),
        ConversationKind::Direct => {
            let user = conversation
                .user
                .as_deref()
                .and_then(|id| workspace.user(id));
            let avatar_rect = egui::Rect::from_center_size(icon_rect.center(), Vec2::splat(20.0));
            super::paint_avatar(
                ui,
                avatar_rect,
                user.and_then(|u| u.avatar.as_deref()),
                &title,
                conversation.user.as_deref().unwrap_or(&title),
            );
            let behind = if selected {
                palette.accent
            } else if response.hovered() {
                palette.surface_hover
            } else {
                palette.panel
            };
            let presence = conversation
                .user
                .as_deref()
                .and_then(|id| workspace.people.presence(id));
            super::people::dot(ui.painter(), palette, avatar_rect, presence, behind);
        }
    }
    let font = if unread {
        theme::bold(14.5)
    } else {
        theme::regular(14.5)
    };
    let badge_width = if conversation.mentions > 0 && !selected {
        30.0
    } else {
        0.0
    };
    // A pencil for what you started writing here, as Slack shows it.
    let drafted = !selected
        && ui
            .data(|d| {
                d.get_temp::<std::sync::Arc<std::collections::HashSet<String>>>(super::drafts_id())
            })
            .is_some_and(|drafts| drafts.contains(&conversation.id));
    let draft_width = if drafted { 20.0 } else { 0.0 };
    // Headphones while a huddle goes on here.
    let huddle = workspace.people.huddles.contains_key(&conversation.id);
    let huddle_width = if huddle { 20.0 } else { 0.0 };
    // A globe for Slack Connect.
    let external = crate::people::is_external_conversation(workspace, conversation);
    let external_width = if external { 18.0 } else { 0.0 };
    let max_text =
        rect.width() - 36.0 - 8.0 - badge_width - draft_width - huddle_width - external_width;
    if external {
        let at = egui::Rect::from_center_size(
            egui::pos2(
                rect.right()
                    - badge_width
                    - draft_width
                    - huddle_width
                    - 4.0
                    - external_width / 2.0,
                rect.center().y,
            ),
            Vec2::splat(12.0),
        );
        Icon::Globe.image(palette.dim, 12.0).paint_at(ui, at);
    }
    if huddle {
        let at = egui::Rect::from_center_size(
            egui::pos2(
                rect.right() - badge_width - draft_width - 4.0 - huddle_width / 2.0,
                rect.center().y,
            ),
            Vec2::splat(14.0),
        );
        let tint = if selected {
            palette.on_accent
        } else {
            super::people::ACTIVE
        };
        Icon::Headphones.image(tint, 14.0).paint_at(ui, at);
    }
    // One line, cut with an ellipsis.
    theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL + 2);
    theme::describe_selected(
        &response,
        egui::WidgetType::SelectableLabel,
        selected,
        &if drafted {
            tf(
                "{name}, draft",
                &[("name", &spoken(&title, unread, conversation.mentions))],
            )
        } else {
            spoken(&title, unread, conversation.mentions)
        },
    );
    if drafted {
        let pencil = egui::Rect::from_center_size(
            egui::pos2(
                rect.right() - badge_width - 4.0 - draft_width / 2.0,
                rect.center().y,
            ),
            Vec2::splat(13.0),
        );
        Icon::Pencil.image(palette.dim, 13.0).paint_at(ui, pencil);
    }
    let mut job = egui::text::LayoutJob::simple_singleline(title, font, text_color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_text);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0),
        galley,
        text_color,
    );
    if conversation.mentions > 0 && !selected {
        let mut badge = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_max(
                    egui::pos2(rect.right() - badge_width - 4.0, rect.top()),
                    rect.right_bottom(),
                ))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        super::badge(&mut badge, palette, conversation.mentions);
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        actions.push(Action::OpenConversation(conversation.id.clone()));
    }
    row_menu(&response, workspace, conversation, section, actions);
    if workspace.sections.is_none() {
        // Without Slack's sections there is no section menu to add to.
        response.context_menu(|ui| {
            super::desktop::conversation_menu(ui, workspace, conversation, actions);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_counts_say_when_there_are_more() {
        assert_eq!(rail_count(3), "3");
        assert_eq!(rail_count(9), "9");
        assert_eq!(rail_count(12), "9+");
    }
}
