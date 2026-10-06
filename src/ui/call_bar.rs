//! The call bar: the huddle being listened to (the `huddle-audio`
//! feature), from joining until it is left, at the foot of the sidebar
//! as Slack has it, and at the foot of the settings page, so it is on
//! screen wherever you are.
//!
//! It names the huddle (a click opens its conversation), says whether it
//! is joining, live and for how long, or why it failed, shows who is in
//! it with the speaking ringed and the muted marked (you as your
//! microphone really is), and offers Leave (also Ctrl+Shift+H), the
//! microphone's mute button while live (see [`super::huddle_mic`]) and
//! Open in Slack.

use egui::{Color32, CornerRadius, Margin, Rect, RichText, Sense, Stroke, Vec2};

use super::people::ACTIVE;
use crate::app::WorkspaceState;
use crate::huddles::{self, Listening, Phase, Place};
use crate::i18n::{t, tf, tn};
use crate::model::Action;
use crate::theme::{self, Icon, Palette};

/// A face's side, and the room between faces.
const FACE: f32 = 26.0;
const FACE_GAP: f32 = 5.0;
/// Leave's red: deep enough for white text on both themes (the dark
/// palette's own red is too light for it).
const LEAVE: Color32 = Color32::from_rgb(0xcc, 0x2e, 0x45);

/// One person as the bar draws them.
struct Face {
    name: String,
    avatar: Option<String>,
    seed: String,
    muted: bool,
    speaking: bool,
}

/// What the bar shows, gathered before drawing.
struct Bar {
    team: String,
    channel: String,
    title: String,
    status: String,
    /// The workspace's name, when more than one is signed in.
    workspace: Option<String>,
    faces: Vec<Face>,
}

/// Gathers the bar for `listening` at `now`.
fn gather(
    listening: &Listening,
    workspaces: &[WorkspaceState],
    now: std::time::Instant,
) -> Option<Bar> {
    let workspace = workspaces
        .iter()
        .find(|w| w.info.team_id == listening.team)?;
    let conversation = workspace.conversation(&listening.channel);
    let name = conversation.map_or_else(|| listening.channel.clone(), |c| workspace.title(c));
    let place = match conversation {
        Some(c) if c.kind.is_dm() => Place::Direct(&name),
        _ => Place::Channel(&name),
    };
    let faces = huddles::faces(&listening.roster)
        .into_iter()
        .map(|person| {
            let user = person.user.as_deref().and_then(|id| workspace.user(id));
            let name = match (&person.user, person.me) {
                (_, true) if listening.mic == crate::huddle_mic::Mic::Live => tf(
                    "{name} (you, talking here)",
                    &[("name", &workspace.user_label(&workspace.info.user_id))],
                ),
                (_, true) => tf(
                    "{name} (you, listening here)",
                    &[("name", &workspace.user_label(&workspace.info.user_id))],
                ),
                (Some(id), false) => workspace.user_label(id),
                (None, false) => t("Someone").into_owned(),
            };
            Face {
                name,
                avatar: user.and_then(|u| u.avatar.clone()),
                seed: person.user.clone().unwrap_or_default(),
                // Yours as it is here, not as Chime last said: the
                // microphone is the truth, and Chime hears of it late.
                muted: if person.me {
                    listening.mic != crate::huddle_mic::Mic::Live
                } else {
                    person.muted
                },
                speaking: person.speaking,
            }
        })
        .collect();
    Some(Bar {
        team: listening.team.clone(),
        channel: listening.channel.clone(),
        title: huddles::title_text(place),
        status: huddles::status_text(&listening.phase, now),
        workspace: (workspaces.len() > 1).then(|| workspace.info.name.clone()),
        faces,
    })
}

/// The bar at the foot of a panel, when a huddle is being listened to.
/// `settings` says the settings page is open, which the title's click
/// leaves for the conversation.
pub fn panel(
    ui: &mut egui::Ui,
    id: &str,
    palette: &Palette,
    listening: Option<&Listening>,
    workspaces: &[WorkspaceState],
    settings: bool,
    actions: &mut Vec<Action>,
) {
    let Some(listening) = listening else {
        return;
    };
    egui::Panel::bottom(egui::Id::new(id))
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(Margin::same(8)))
        .show(ui, |ui| {
            // Not wider than the sidebar would make it.
            ui.set_max_width(ui.available_width().min(360.0));
            bar(ui, palette, listening, workspaces, settings, actions);
        });
}

/// The bar itself, as wide as `ui`.
fn bar(
    ui: &mut egui::Ui,
    palette: &Palette,
    listening: &Listening,
    workspaces: &[WorkspaceState],
    settings: bool,
    actions: &mut Vec<Action>,
) {
    let now = std::time::Instant::now();
    let Some(bar) = gather(listening, workspaces, now) else {
        return;
    };
    // The clock moves each second.
    if let Phase::Live { since } = listening.phase {
        let into = now.saturating_duration_since(since).subsec_millis();
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(u64::from(1000 - into)));
    }
    let failed = matches!(listening.phase, Phase::Failed { .. });
    let tint = if failed { palette.danger } else { ACTIVE };
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, tint.gamma_multiply(0.7)))
        .corner_radius(CornerRadius::same(theme::RADIUS + 2))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            title_row(ui, palette, &bar, tint, settings, actions);
            status_row(ui, palette, &bar, &listening.phase, tint);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if failed {
                        if theme::icon_button(ui, palette, Icon::X, 14.0, &t("Close")).clicked() {
                            actions.push(Action::Huddle(huddles::Action::Leave));
                        }
                        if small_button(ui, palette, &t("Try again"), None).clicked() {
                            actions.push(Action::Huddle(huddles::Action::Listen {
                                team: bar.team.clone(),
                                channel: bar.channel.clone(),
                            }));
                        }
                    } else {
                        let shortcut =
                            super::shortcuts::spell("Cmd+Shift+H", cfg!(target_os = "macos"));
                        if small_button(ui, palette, &t("Leave"), Some(LEAVE))
                            .on_hover_text(tf(
                                "Leave the huddle ({shortcut})",
                                &[("shortcut", &shortcut)],
                            ))
                            .clicked()
                        {
                            actions.push(Action::Huddle(huddles::Action::Leave));
                        }
                        if matches!(listening.phase, Phase::Live { .. })
                            && let Some(action) =
                                super::huddle_mic::mute_button(ui, palette, listening.mic)
                        {
                            actions.push(Action::Huddle(huddles::Action::Microphone(action)));
                        }
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        faces(ui, palette, &bar.faces);
                    });
                });
            });
        });
}

/// The headphones, the huddle's name (opening its conversation) and
/// Open in Slack.
fn title_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    bar: &Bar,
    tint: Color32,
    settings: bool,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let open = theme::icon_button(
                ui,
                palette,
                Icon::ExternalLink,
                14.0,
                &t("Open in Slack, to talk"),
            );
            if open.clicked() {
                actions.push(Action::OpenUrl(crate::people::huddle_url(
                    &bar.team,
                    &bar.channel,
                )));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(Icon::Headphones.image(tint, 16.0));
                let title = ui
                    .add(
                        egui::Label::new(
                            RichText::new(&bar.title)
                                .font(theme::semibold(13.5))
                                .color(palette.text),
                        )
                        .truncate()
                        .sense(Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t("Open the conversation"));
                theme::describe(&title, egui::WidgetType::Button, &bar.title);
                if title.clicked() {
                    if settings {
                        actions.push(Action::HideSettings);
                    }
                    actions.push(Action::SelectWorkspace(bar.team.clone()));
                    actions.push(Action::OpenConversation(bar.channel.clone()));
                }
            });
        });
    });
}

/// Joining, live and how long, or the failure; and the workspace.
fn status_row(ui: &mut egui::Ui, palette: &Palette, bar: &Bar, phase: &Phase, tint: Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        match phase {
            Phase::Joining => {
                ui.add(egui::Spinner::new().size(10.0).color(palette.secondary));
            }
            Phase::Live { .. } => {
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, tint);
            }
            Phase::Failed { .. } => {
                ui.add(Icon::CircleAlert.image(tint, 12.0));
            }
        }
        let text = match &bar.workspace {
            Some(name) => format!("{} · {name}", bar.status),
            None => bar.status.clone(),
        };
        let color = if matches!(phase, Phase::Failed { .. }) {
            palette.text
        } else {
            palette.secondary
        };
        ui.add(
            egui::Label::new(RichText::new(text).font(theme::regular(12.0)).color(color)).wrap(),
        );
    });
}

/// A compact button, filled with `fill` (white text) or the surface.
fn small_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    label: &str,
    fill: Option<Color32>,
) -> egui::Response {
    let (fill, text) = match fill {
        Some(fill) => (fill, Color32::WHITE),
        None => (palette.surface_hover, palette.text),
    };
    ui.add(
        egui::Button::new(RichText::new(label).font(theme::medium(13.0)).color(text))
            .fill(fill)
            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
            .min_size(Vec2::new(0.0, 28.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The faces, as many as fit, then "+N"; the speaking ringed, the muted
/// marked.
fn faces(ui: &mut egui::Ui, palette: &Palette, faces: &[Face]) {
    ui.spacing_mut().item_spacing.x = FACE_GAP;
    let room = ui.available_width();
    let fit = ((room + FACE_GAP) / (FACE + FACE_GAP)).floor().max(1.0) as usize;
    let (shown, more) = if faces.len() > fit {
        (fit.saturating_sub(1), faces.len() - fit.saturating_sub(1))
    } else {
        (faces.len(), 0)
    };
    for face in &faces[..shown] {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(FACE), Sense::hover());
        let inner = rect.shrink(2.5);
        super::paint_avatar(ui, inner, face.avatar.as_deref(), &face.name, &face.seed);
        if face.speaking {
            ui.painter().rect_stroke(
                rect.shrink(0.75),
                CornerRadius::same(8),
                Stroke::new(2.0, ACTIVE),
                egui::StrokeKind::Inside,
            );
        }
        if face.muted {
            let badge =
                Rect::from_center_size(rect.right_bottom() - Vec2::splat(4.0), Vec2::splat(13.0));
            ui.painter()
                .circle_filled(badge.center(), 6.5, palette.surface);
            Icon::MicOff
                .image(palette.secondary, 9.0)
                .paint_at(ui, Rect::from_center_size(badge.center(), Vec2::splat(9.0)));
        }
        let mut said = face.name.clone();
        if face.speaking {
            said = tf("{name}, speaking", &[("name", &said)]);
        } else if face.muted {
            said = tf("{name}, muted", &[("name", &said)]);
        }
        theme::describe(&response, egui::WidgetType::Image, &said);
        response.on_hover_text(said);
    }
    if more > 0 {
        let label = format!("+{more}");
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(FACE), Sense::hover());
        ui.painter().rect_filled(
            rect.shrink(2.5),
            CornerRadius::same(6),
            palette.surface_hover,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            theme::semibold(11.0),
            palette.secondary,
        );
        response.on_hover_text(tn(
            "{count} more person",
            "{count} more people",
            more as u32,
        ));
    }
}
