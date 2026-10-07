//! What the interface draws about people beyond their names: whether
//! they are around and typing, and your own status.

use egui::{Color32, Margin, Rect, RichText, Stroke, Vec2};

use crate::app::App;
use crate::i18n::t;
use crate::model::{Ability, Action};
use crate::people::{self, Presence};
use crate::theme::{self, Palette};

/// Slack's "active" green. The same in both palettes, as in Slack: it
/// means one thing everywhere.
pub const ACTIVE: Color32 = Color32::from_rgb(0x2b, 0xac, 0x76);

/// Paints a presence dot on the lower right corner of an avatar at
/// `avatar`: filled green when active, a hollow ring when away, nothing
/// when unknown. `behind` is the colour around the avatar, which rings
/// the dot so it stands apart from the picture.
pub fn dot(
    painter: &egui::Painter,
    palette: &Palette,
    avatar: Rect,
    presence: Option<Presence>,
    behind: Color32,
) {
    let Some(presence) = presence else {
        return;
    };
    let radius = (avatar.width() * 0.2).clamp(3.5, 7.0);
    let center = avatar.right_bottom() - Vec2::splat(radius * 0.6);
    painter.circle_filled(center, radius + 1.5, behind);
    match presence {
        Presence::Active => {
            painter.circle_filled(center, radius, ACTIVE);
        }
        Presence::Away => {
            painter.circle_stroke(center, radius - 0.75, Stroke::new(1.5, palette.dim));
        }
    }
}

/// The height of the line under a composer that says who is typing. It is
/// always there, so the composer does not jump when someone starts.
const TYPING_HEIGHT: f32 = 16.0;

/// Draws who is typing in `channel` (or in its thread `thread`) under the
/// composer, and wakes the window when the line should change.
pub fn typing(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &crate::app::WorkspaceState,
    channel: &str,
    thread: Option<&crate::model::Ts>,
) {
    let now = std::time::Instant::now();
    let (users, until) = workspace.people.typing_in(channel, thread, now);
    let names: Vec<String> = users.iter().map(|u| workspace.user_label(u)).collect();
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), TYPING_HEIGHT),
        egui::Sense::hover(),
    );
    if let Some(until) = until {
        ui.ctx().request_repaint_after(until.duration_since(now));
    }
    let Some(line) = crate::people::typing_line(&names) else {
        return;
    };
    let mut job = egui::text::LayoutJob::simple_singleline(
        line,
        crate::theme::regular(12.0),
        palette.secondary,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 4.0);
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        egui::pos2(rect.left() + 4.0, rect.center().y - galley.size().y / 2.0),
        galley,
        palette.secondary,
    );
}

/// Your own picture at the foot of the rail, with your presence: a click
/// opens a menu to set your status, or to show yourself away or active.
pub fn me_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &crate::app::WorkspaceState,
    stay_active: bool,
    actions: &mut Vec<Action>,
) {
    let me = &workspace.info.user_id;
    let user = workspace.user(me);
    let name = workspace.user_label(me);
    let presence = workspace.people.presence(me);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(32.0), egui::Sense::click());
    super::paint_avatar(ui, rect, user.and_then(|u| u.avatar.as_deref()), &name, me);
    let behind = if palette.dark {
        palette.panel.gamma_multiply(0.8)
    } else {
        palette.surface_active
    };
    dot(ui.painter(), palette, rect, presence, behind);
    let status = user.map(status_line).filter(|s| !s.is_empty());
    let tip = match &status {
        Some(status) => format!("{name} · {status}"),
        None => name.clone(),
    };
    theme::focus_ring(ui, &response, palette, 8);
    theme::describe(&response, egui::WidgetType::Button, &tip);
    let response = response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(&tip);
    egui::Popup::menu(&response).show(|ui| {
        ui.set_min_width(220.0);
        ui.label(
            RichText::new(&name)
                .font(theme::semibold(14.0))
                .color(palette.text),
        );
        if let Some(presence) = presence {
            ui.label(
                RichText::new(word(presence))
                    .font(theme::regular(12.5))
                    .color(palette.dim),
            );
        }
        if let Some(status) = &status {
            ui.label(RichText::new(status).color(palette.secondary));
        }
        // Status and away are Slack's; elsewhere the menu only shows who you are.
        if workspace.info.offers(Ability::Status) {
            ui.separator();
            let edit = if status.is_some() {
                t("Edit status…")
            } else {
                t("Set a status…")
            };
            if ui.button(edit).clicked() {
                actions.push(Action::People(people::Action::EditStatus));
                ui.close();
            }
            if status.is_some() && ui.button(t("Clear status")).clicked() {
                actions.push(clear());
                ui.close();
            }
            let away = presence == Some(Presence::Away);
            let toggle = if away {
                t("Set yourself as active")
            } else {
                t("Set yourself as away")
            };
            if ui.button(toggle).clicked() {
                actions.push(Action::People(people::Action::SetAway(!away)));
                ui.close();
            }
            let mut always = stay_active;
            if ui
                .checkbox(&mut always, t("Always show as active"))
                .on_hover_text(t(
                    "While NoSlacking is connected, even when you are not using it. Browser sign-ins only.",
                ))
                .changed()
            {
                actions.push(Action::People(people::Action::StayActive(always)));
                ui.close();
            }
        }
        ui.separator();
        if ui.button(t("View profile")).clicked() {
            actions.push(Action::OpenProfile(me.clone()));
            ui.close();
        }
    });
}

/// Someone's status as one line: the emoji, then the text.
fn status_line(user: &crate::model::User) -> String {
    let emoji = user.status_emoji.trim_matches(':');
    let shown = crate::emoji::unicode(emoji, None).unwrap_or_default();
    format!("{shown} {}", user.status_text).trim().to_owned()
}

/// Clears your status.
fn clear() -> Action {
    Action::People(people::Action::SetStatus {
        emoji: String::new(),
        text: String::new(),
        expiry: people::Expiry::Never,
    })
}

/// The "Set a status" dialog: an emoji, some words, when it clears, and
/// Slack's usual suggestions.
pub fn status_dialog(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.people.status.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let mut answer: Option<Option<Action>> = None;
    let response = egui::Modal::new(egui::Id::new("status-dialog"))
        .frame(super::overlays::modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(400.0);
            let heading = ui.label(
                RichText::new(t("Set a status"))
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let name = dialog.emoji.trim().trim_matches(':');
                let shown = crate::emoji::unicode(name, None).unwrap_or_else(|| {
                    if name.is_empty() {
                        "🙂".to_owned()
                    } else {
                        "❔".to_owned()
                    }
                });
                ui.label(RichText::new(shown).font(theme::regular(20.0)));
                ui.add(
                    egui::TextEdit::singleline(&mut dialog.emoji)
                        .id(egui::Id::new("status-emoji"))
                        .hint_text(t(":emoji:"))
                        .background_color(palette.window)
                        .desired_width(110.0)
                        .margin(Margin::symmetric(8, 6)),
                )
                .labelled_by(heading.id)
                .on_hover_text(t("A shortcode such as :coffee:, or the emoji itself"));
                let text = ui
                    .add(
                        egui::TextEdit::singleline(&mut dialog.text)
                            .id(egui::Id::new("status-text"))
                            .hint_text(t("What's your status?"))
                            .background_color(palette.window)
                            .char_limit(100)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(8, 6)),
                    )
                    .labelled_by(heading.id);
                if focus {
                    text.request_focus();
                }
            });
            ui.add_space(8.0);
            super::section_label(ui, &palette, &t("For example"));
            for preset in people::presets() {
                let text = people::preset_text(&preset);
                let emoji = crate::emoji::unicode(preset.emoji, None).unwrap_or_default();
                let line = format!("{emoji}  {text} — {}", preset.expiry.label());
                if ui
                    .add(egui::Button::new(RichText::new(line).color(palette.text)).frame(false))
                    .clicked()
                {
                    dialog.emoji = preset.emoji.to_owned();
                    dialog.text = text.into_owned();
                    dialog.expiry = preset.expiry;
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = ui.label(
                    RichText::new(t("Clear after"))
                        .font(theme::medium(13.5))
                        .color(palette.text),
                );
                egui::ComboBox::from_id_salt("status-expiry")
                    .selected_text(dialog.expiry.label())
                    .width(160.0)
                    .show_ui(ui, |ui| {
                        for expiry in people::Expiry::ALL {
                            ui.selectable_value(&mut dialog.expiry, expiry, expiry.label());
                        }
                    })
                    .response
                    .labelled_by(label.id);
            });
            let empty = dialog.emoji.trim().is_empty() && dialog.text.trim().is_empty();
            if !empty && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                answer = Some(Some(save(&dialog)));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    answer = Some(None);
                }
                if theme::secondary_button(ui, &palette, &t("Clear status")).clicked() {
                    answer = Some(Some(clear()));
                }
                ui.add_enabled_ui(!empty, |ui| {
                    if theme::primary_button(ui, &palette, &t("Save")).clicked() {
                        answer = Some(Some(save(&dialog)));
                    }
                });
            });
        });
    if response.should_close() && answer.is_none() {
        answer = Some(None);
    }
    match answer {
        Some(Some(action)) => app.actions.push(action),
        Some(None) => {}
        None => app.people.status = Some(dialog),
    }
}

/// Sets the status the dialog holds.
fn save(dialog: &people::StatusDialog) -> Action {
    Action::People(people::Action::SetStatus {
        emoji: dialog.emoji.clone(),
        text: dialog.text.clone(),
        expiry: dialog.expiry,
    })
}

/// The "huddle" button in a conversation's header, while one goes on: who
/// is in it, and a click to join it: here in a browser sign-in's workspace
/// (with the microphone off), else in Slack.
pub fn huddle_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &crate::app::WorkspaceState,
    channel: &str,
    listening: Option<&crate::huddles::Listening>,
    actions: &mut Vec<Action>,
) {
    let Some(huddle) = workspace.people.huddles.get(channel) else {
        return;
    };
    let names: Vec<String> = huddle
        .participants
        .iter()
        .map(|u| workspace.user_label(u))
        .collect();
    let label = crate::i18n::tn(
        "Huddle · {count} person",
        "Huddle · {count} people",
        huddle.participants.len() as u32,
    );
    let button = egui::Button::image_and_text(
        theme::Icon::Headphones.image(ACTIVE, 14.0),
        RichText::new(label)
            .font(theme::semibold(12.5))
            .color(palette.text),
    )
    .stroke(Stroke::new(1.0, ACTIVE))
    .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL + 2));
    let team = &workspace.info.team_id;
    let here = workspace.info.sign_in == crate::model::SignInKind::Session;
    let tip = crate::i18n::tf(
        if here {
            "Join the huddle here: {names}"
        } else {
            "Join the huddle in Slack: {names}"
        },
        &[("names", &names.join(", "))],
    );
    let response = ui
        .add(button)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(&tip);
    if !response.clicked() {
        return;
    }
    if here {
        // Already in it (or joining): nothing more to join.
        if !listening.is_some_and(|l| l.is(team, channel)) {
            actions.push(Action::Huddle(crate::huddles::Action::Listen {
                team: team.clone(),
                channel: channel.to_owned(),
            }));
        }
        return;
    }
    actions.push(Action::OpenUrl(people::huddle_url(team, channel)));
}

/// "Joining…", then "Leave", beside the huddle button while you are in
/// this huddle.
pub fn listen_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &crate::app::WorkspaceState,
    channel: &str,
    listening: Option<&crate::huddles::Listening>,
    actions: &mut Vec<Action>,
) {
    let team = &workspace.info.team_id;
    // Shown only while in this huddle (joining or live), also before
    // Slack's news of a huddle we started arrives; joining is the huddle
    // button's, and after a failure that joins again.
    let Some(here) = listening.filter(|l| l.is(team, channel)) else {
        return;
    };
    let label = match here.phase {
        crate::huddles::Phase::Joining => t("Joining…"),
        _ => t("Leave"),
    };
    let tip = t("Leave the huddle");
    let button = egui::Button::image_and_text(
        theme::Icon::Headphones.image(palette.secondary, 14.0),
        RichText::new(label)
            .font(theme::regular(12.5))
            .color(palette.text),
    )
    .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL + 2));
    let response = ui
        .add(button)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip);
    if response.clicked() {
        actions.push(Action::Huddle(crate::huddles::Action::Leave));
    }
}

/// A headphones menu in a conversation's header while no huddle goes on
/// there: start one here (in a browser sign-in's workspace, joining
/// muted), or else in Slack. A menu rather than one click, so a huddle
/// that tells a whole channel is never started by accident.
pub fn start_huddle_menu(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &crate::app::WorkspaceState,
    channel: &str,
    listening: Option<&crate::huddles::Listening>,
    actions: &mut Vec<Action>,
) {
    let team = &workspace.info.team_id;
    if workspace.people.huddles.contains_key(channel) {
        return;
    }
    if listening.is_some_and(|l| l.is(team, channel)) {
        return;
    }
    let menu = ui
        .menu_image_text_button(
            theme::Icon::Headphones.image(palette.secondary, 14.0),
            RichText::new(t("Huddle"))
                .font(theme::regular(12.5))
                .color(palette.text),
            |ui| {
                // Only a browser sign-in can start one here; the others
                // open Slack's huddle in the browser.
                if workspace.info.sign_in == crate::model::SignInKind::Session {
                    if ui
                        .button(t("Start a huddle here"))
                        .on_hover_text(t(
                            "Start a huddle and join it here, with your microphone off",
                        ))
                        .clicked()
                    {
                        actions.push(Action::Huddle(crate::huddles::Action::Listen {
                            team: team.clone(),
                            channel: channel.to_owned(),
                        }));
                        ui.close();
                    }
                } else if ui
                    .button(t("Start a huddle in Slack"))
                    .on_hover_text(t("Open Slack's huddle for this conversation"))
                    .clicked()
                {
                    actions.push(Action::OpenUrl(people::huddle_url(team, channel)));
                    ui.close();
                }
            },
        )
        .response;
    menu.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(t("Start a huddle"));
}

/// What the globe beside an external conversation or person says.
pub fn external_tip(direct: bool) -> std::borrow::Cow<'static, str> {
    if direct {
        t("From another organization")
    } else {
        t("Shared with another organization")
    }
}

/// A small globe and "External", beside a name.
pub fn external_tag(ui: &mut egui::Ui, palette: &Palette, direct: bool) {
    let tag = egui::Button::image_and_text(
        theme::Icon::Globe.image(palette.secondary, 12.0),
        RichText::new(t("External"))
            .font(theme::regular(12.0))
            .color(palette.secondary),
    )
    .frame(false)
    .sense(egui::Sense::hover());
    ui.add(tag).on_hover_text(external_tip(direct));
}

/// The word for a presence, for tooltips and screen readers.
pub fn word(presence: Presence) -> std::borrow::Cow<'static, str> {
    match presence {
        Presence::Active => t("Active"),
        Presence::Away => t("Away"),
    }
}

/// The huddle invitations ringing, each a card in the top right corner
/// with Join (here in a browser sign-in, else in Slack) and Decline (see [`crate::huddles`]).
pub fn invites(app: &mut App, ctx: &egui::Context) {
    if app.huddles.invites.list().is_empty() {
        return;
    }
    let palette = app.palette;
    let several = app.workspaces.len() > 1;
    let cards: Vec<(String, String, String, String, bool)> = app
        .huddles
        .invites
        .list()
        .iter()
        .rev()
        .take(3)
        .filter_map(|invite| {
            let workspace = app
                .workspaces
                .iter()
                .find(|w| w.info.team_id == invite.team)
                .filter(|w| w.info.offers(Ability::Huddles))?;
            let place = workspace
                .conversation(&invite.channel)
                .filter(|c| !c.kind.is_dm())
                .map(|c| format!("#{}", workspace.title(c)));
            let (title, mut body) =
                crate::huddles::invite_text(&workspace.user_label(&invite.from), place.as_deref());
            if several {
                body = format!("{body} · {}", workspace.info.name);
            }
            // Only a browser sign-in can listen here.
            let session = workspace.info.sign_in == crate::model::SignInKind::Session;
            Some((
                invite.team.clone(),
                invite.room.clone(),
                title,
                body,
                session,
            ))
        })
        .collect();
    if cards.is_empty() {
        return;
    }
    let mut answers = Vec::new();
    egui::Area::new(egui::Id::new("huddle-invites"))
        .anchor(egui::Align2::RIGHT_TOP, Vec2::new(-16.0, 64.0))
        .order(egui::Order::Foreground)
        .interactable(true)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            for (team, room, title, body, session) in cards {
                egui::Frame::new()
                    .fill(palette.overlay)
                    .stroke(Stroke::new(1.5, ACTIVE))
                    .corner_radius(egui::CornerRadius::same(theme::RADIUS + 4))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 8],
                        blur: 24,
                        spread: 0,
                        color: palette.shadow,
                    })
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.horizontal(|ui| {
                            ui.add(theme::Icon::Headphones.image(ACTIVE, 20.0));
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(&title)
                                        .font(theme::semibold(14.5))
                                        .color(palette.text),
                                );
                                ui.label(
                                    RichText::new(&body)
                                        .font(theme::regular(13.0))
                                        .color(palette.secondary),
                                );
                            });
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            // A browser sign-in joins here (see
                            // `huddles::Action::Join`); else Slack does.
                            let hint = if session {
                                t("Join the huddle here, with your microphone off")
                            } else {
                                t("Join the huddle in Slack")
                            };
                            let join = egui::Button::new(
                                RichText::new(t("Join"))
                                    .font(theme::medium(14.0))
                                    .color(Color32::WHITE),
                            )
                            .fill(ACTIVE)
                            .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL + 2))
                            .min_size(Vec2::new(0.0, 32.0));
                            if ui
                                .add(join)
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(hint)
                                .clicked()
                            {
                                answers.push(crate::huddles::Action::Join {
                                    team: team.clone(),
                                    room: room.clone(),
                                });
                            }
                            if theme::secondary_button(ui, &palette, &t("Decline")).clicked() {
                                answers.push(crate::huddles::Action::Decline {
                                    team: team.clone(),
                                    room: room.clone(),
                                });
                            }
                        });
                    });
            }
        });
    app.actions.extend(answers.into_iter().map(Action::Huddle));
}
