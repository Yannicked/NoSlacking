//! The call bar: the huddle being listened to, from joining until it is
//! left, at the foot of the sidebar as Slack has it, and at the foot of the settings page, so it is on
//! screen wherever you are.
//!
//! It names the huddle (a click opens its conversation), says whether it
//! is joining, live and for how long, or why it failed, shows who is in
//! it with the speaking ringed and the muted marked (you as your
//! microphone really is), and offers Leave (also Ctrl+Shift+H) and the
//! microphone's mute button while live (see [`super::huddle_mic`]).
//!
//! With `huddle-video`, a row for each screen shared in the huddle ("Ana
//! is sharing their screen") with Watch, which opens the call window on
//! it (see [`super::call_window`]), and one saying how many have a camera
//! on ("2 cameras on") with Video, which opens it on their tiles.
//!
//! Beside Mute, a small arrow opens a menu of microphones and speakers;
//! beside Video, one of cameras (see [`super::devices`]): switching
//! there takes effect at once, and is remembered as Settings would.
//!
//! With `huddle-camera`, the camera's button beside the microphone's and,
//! while it is on, your self-preview above the buttons (see
//! [`super::huddle_camera`]).
//!
//! With `huddle-share`, Share beside the camera's button, a row saying
//! "You are sharing your screen" with Stop sharing while you do, and the
//! screens and windows to pick from where the system has no dialog of
//! its own (see [`super::huddle_share`]).

use egui::{Color32, CornerRadius, Margin, Rect, RichText, Sense, Stroke, Vec2};

use super::devices::Pickers;
use super::people::ACTIVE;
use crate::app::WorkspaceState;
use crate::huddles::{self, Listening, Phase, Place};
use crate::i18n::{t, tf, tn};
use crate::model::Action;
use crate::theme::{self, Icon, Palette};

/// A face's side, and the room between faces.
const FACE: f32 = 26.0;
const FACE_GAP: f32 = 5.0;
/// The room left beside Leave, in points, under which Mute, Video and
/// Share drop their words (their arrows included).
const COMPACT_BELOW: f32 = 330.0;
/// The same without Share: Mute and Video with their arrows.
const COMPACT_BELOW_NO_SHARE: f32 = 250.0;
/// Between a control and its arrow, in points.
pub const ARROW_GAP: f32 = 2.0;
/// Leave's red: deep enough for white text on both themes (the dark
/// palette's own red is too light for it).
pub const LEAVE: Color32 = Color32::from_rgb(0xcc, 0x2e, 0x45);
/// Watch's green: the call's own colour, deep enough for white text.
const ACTIVE_BUTTON: Color32 = Color32::from_rgb(0x00, 0x7a, 0x5a);

/// What leaving is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaving {
    /// Leave the huddle.
    Huddle,
    /// Hang up.
    Call,
    /// Leave the meeting.
    Meeting,
}

impl Leaving {
    /// What leaving `listening` is.
    pub fn of(listening: &Listening) -> Self {
        if listening.meeting {
            Self::Meeting
        } else if listening.is_call() {
            Self::Call
        } else {
            Self::Huddle
        }
    }
}

/// Someone waiting in a meeting's lobby, as the bar lists them.
struct Waiting {
    /// The id the interface knows them by, which Admit sends.
    user: String,
    text: String,
    /// Admit was pressed for them, and they are not in yet: until when
    /// that shows.
    admitting: Option<std::time::Instant>,
}

/// One person as the bar draws them.
struct Face {
    name: String,
    avatar: Option<String>,
    seed: String,
    muted: bool,
    speaking: bool,
    /// You, listening here: always shown, last.
    me: bool,
}

/// A screen shared in the huddle, as the bar lists it.
#[cfg(feature = "huddle-video")]
struct ShareRow {
    key: String,
    text: String,
    /// Shown in the call window now.
    watched: bool,
}

/// What the bar shows, gathered before drawing.
struct Bar {
    team: String,
    channel: String,
    /// Whether the conversation is here to open: a meeting joined by
    /// link has none until its chat comes.
    openable: bool,
    title: String,
    status: String,
    /// The workspace's name, when more than one is signed in.
    workspace: Option<String>,
    faces: Vec<Face>,
    /// Who waits in the meeting's lobby.
    waiting: Vec<Waiting>,
    /// The meeting's join link, to copy.
    invite: Option<String>,
    #[cfg(feature = "huddle-video")]
    shares: Vec<ShareRow>,
    /// How many others have a camera on.
    #[cfg(feature = "huddle-video")]
    cameras: usize,
    /// Whether the call window is open.
    #[cfg(feature = "huddle-video")]
    window: bool,
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
    // The workspace's name for someone it knows; the call's for a guest.
    let label = |person: &huddles::Person, id: &str| match (&person.name, workspace.user(id)) {
        (Some(name), None) => name.clone(),
        _ => workspace.user_label(id),
    };
    let place = match conversation {
        Some(c) if c.kind.is_dm() => Place::Direct(&name),
        _ => Place::Channel(&name),
    };
    let faces = huddles::faces(&listening.roster)
        .into_iter()
        .map(|person| {
            // You are you, whether or not the call names you.
            let id = match (&person.user, person.me) {
                (Some(id), _) => Some(id.as_str()),
                (None, true) => Some(workspace.info.user_id.as_str()),
                (None, false) => None,
            };
            let user = id.and_then(|id| workspace.user(id));
            let name = match (&person.user, person.me) {
                (_, true) if listening.mic == crate::huddle_mic::Mic::Live => tf(
                    "{name} (you, talking here)",
                    &[("name", &workspace.user_label(&workspace.info.user_id))],
                ),
                (_, true) => tf(
                    "{name} (you, listening here)",
                    &[("name", &workspace.user_label(&workspace.info.user_id))],
                ),
                (Some(id), false) => label(&person, id),
                (None, false) => person
                    .name
                    .clone()
                    .unwrap_or_else(|| t("Someone").into_owned()),
            };
            Face {
                name,
                avatar: user.and_then(|u| u.avatar.clone()),
                seed: id.unwrap_or_default().to_owned(),
                // Yours as it is here, not as Chime last said: the
                // microphone is the truth, and Chime hears of it late.
                muted: if person.me {
                    listening.mic != crate::huddle_mic::Mic::Live
                } else {
                    person.muted
                },
                speaking: person.speaking,
                me: person.me,
            }
        })
        .collect();
    Some(Bar {
        team: listening.team.clone(),
        channel: listening.channel.clone(),
        openable: conversation.is_some(),
        title: if listening.meeting {
            huddles::meeting_title_text(&listening.phase)
        } else if listening.is_call() {
            huddles::call_title_text(&name, &listening.phase, listening.answered)
        } else {
            huddles::title_text(place, listening.mic == crate::huddle_mic::Mic::Live)
        },
        status: huddles::status_text(&listening.phase, listening.is_call(), now),
        workspace: (workspaces.len() > 1).then(|| workspace.info.name.clone()),
        faces,
        waiting: listening
            .waiting()
            .filter_map(|person| {
                let user = person.user.clone()?;
                let name = label(person, &user);
                let admitting = listening
                    .admitting
                    .iter()
                    .find(|(who, _)| *who == user)
                    .map(|(_, at)| *at + huddles::ADMIT_WAIT)
                    .filter(|_| listening.is_admitting(&user, now));
                Some(Waiting {
                    text: tf("{name} is waiting in the lobby", &[("name", &name)]),
                    user,
                    admitting,
                })
            })
            .collect(),
        invite: listening
            .invite
            .as_ref()
            .filter(|_| listening.in_huddle())
            .map(|link| link.0.clone()),
        #[cfg(feature = "huddle-video")]
        shares: listening
            .shares
            .iter()
            .map(|share| ShareRow {
                key: share.key.clone(),
                text: huddles::sharing_text(
                    &share
                        .user
                        .as_deref()
                        .map_or_else(|| t("Someone").into_owned(), |id| workspace.user_label(id)),
                ),
                watched: listening.window
                    && listening.watching.as_deref() == Some(share.key.as_str()),
            })
            .collect(),
        #[cfg(feature = "huddle-video")]
        cameras: listening.cameras.len(),
        #[cfg(feature = "huddle-video")]
        window: listening.window,
    })
}

/// The bar at the foot of a panel, when a huddle is being listened to.
/// `settings` says the settings page is open, which the title's click
/// leaves for the conversation.
#[allow(clippy::too_many_arguments, reason = "each is one thing the bar shows")]
pub fn panel(
    ui: &mut egui::Ui,
    id: &str,
    palette: &Palette,
    listening: Option<&Listening>,
    workspaces: &[WorkspaceState],
    pickers: Pickers<'_>,
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
            bar(
                ui, id, palette, listening, workspaces, pickers, settings, actions,
            );
        });
}

/// The bar itself, as wide as `ui`; `id` tells its menus from the other
/// bar's.
#[allow(clippy::too_many_arguments, reason = "each is one thing the bar shows")]
fn bar(
    ui: &mut egui::Ui,
    id: &str,
    palette: &Palette,
    listening: &Listening,
    workspaces: &[WorkspaceState],
    pickers: Pickers<'_>,
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
            // The faces have a row of their own, so the buttons leave them
            // room.
            if !bar.faces.is_empty() {
                ui.horizontal(|ui| faces(ui, palette, &bar.faces));
            }
            if !failed {
                for waiting in &bar.waiting {
                    waiting_row(ui, palette, waiting, actions);
                }
                if let Some(link) = &bar.invite {
                    invite_row(ui, palette, link, actions);
                }
            }
            #[cfg(feature = "huddle-video")]
            if !failed {
                for share in &bar.shares {
                    share_row(ui, palette, share, actions);
                }
                if bar.cameras > 0 {
                    cameras_row(ui, palette, bar.cameras, bar.window, actions);
                }
            }
            #[cfg(feature = "huddle-camera")]
            if !failed {
                super::huddle_camera::preview(ui, palette, listening.camera);
            }
            // Your share: what is going on, or what to choose from.
            #[cfg(feature = "huddle-share")]
            if !failed {
                let asked = if listening.sharing == crate::huddle_share::Sharing::Choosing {
                    super::huddle_share::picker(ui, palette, &listening.share_sources)
                } else {
                    super::huddle_share::sharing_row(ui, palette, listening.sharing)
                };
                if let Some(action) = asked {
                    actions.push(Action::Huddle(huddles::Action::Share(action)));
                }
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if failed {
                        if theme::icon_button(ui, palette, Icon::X, 14.0, &t("Close")).clicked() {
                            actions.push(Action::Huddle(huddles::Action::Leave));
                        }
                        // A meeting is joined again from its dialog.
                        if !listening.meeting
                            && small_button(ui, palette, &t("Try again"), None).clicked()
                        {
                            let (team, channel) = (bar.team.clone(), bar.channel.clone());
                            actions.push(Action::Huddle(match listening.callee.clone() {
                                Some(user) => huddles::Action::Call {
                                    team,
                                    channel,
                                    user,
                                },
                                None => huddles::Action::Listen { team, channel },
                            }));
                        }
                    } else {
                        // Right to left, so they read Mute, Video, Share,
                        // Leave, as in the call window.
                        if leave_button(ui, palette, Look::BAR, Leaving::of(listening)) {
                            actions.push(Action::Huddle(huddles::Action::Leave));
                        }
                        // Four worded controls do not fit a narrow sidebar:
                        // the microphone, the camera and Share then show
                        // their icons alone (their words in the tooltip and
                        // for a screen reader); Leave keeps its word.
                        let room = if cfg!(feature = "huddle-share") {
                            COMPACT_BELOW
                        } else {
                            COMPACT_BELOW_NO_SHARE
                        };
                        let compact = Look {
                            labelled: ui.available_width() >= room,
                            ..Look::BAR
                        };
                        let live = matches!(listening.phase, Phase::Live { .. });
                        #[cfg(feature = "huddle-share")]
                        if matches!(listening.phase, Phase::Live { .. })
                            && let Some(action) = super::huddle_share::share_button(
                                ui,
                                palette,
                                listening.sharing,
                                compact,
                            )
                        {
                            actions.push(Action::Huddle(huddles::Action::Share(action)));
                        }
                        // Each with its arrow on its right, close by:
                        // right to left, the arrow first.
                        #[cfg(feature = "huddle-camera")]
                        if live {
                            ui.scope(|ui| {
                                ui.spacing_mut().item_spacing.x = ARROW_GAP;
                                super::devices::menu_button(
                                    ui,
                                    palette,
                                    compact,
                                    id,
                                    &super::devices::CAMERA_MENU,
                                    pickers,
                                    actions,
                                );
                                if let Some(action) = super::huddle_camera::camera_button(
                                    ui,
                                    palette,
                                    listening.camera,
                                    compact,
                                ) {
                                    actions.push(Action::Huddle(huddles::Action::Camera(action)));
                                }
                            });
                        }
                        if live {
                            ui.scope(|ui| {
                                ui.spacing_mut().item_spacing.x = ARROW_GAP;
                                super::devices::menu_button(
                                    ui,
                                    palette,
                                    compact,
                                    id,
                                    &super::devices::MIC_MENU,
                                    pickers,
                                    actions,
                                );
                                if let Some(action) = super::huddle_mic::mute_button(
                                    ui,
                                    palette,
                                    listening.mic,
                                    compact,
                                ) {
                                    actions
                                        .push(Action::Huddle(huddles::Action::Microphone(action)));
                                }
                            });
                        }
                    }
                });
            });
        });
}

/// Someone waiting in the meeting's lobby, and Admit, which lets them in.
fn waiting_row(ui: &mut egui::Ui, palette: &Palette, waiting: &Waiting, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(until) = waiting.admitting {
                // Asked: Teams lets them in within seconds, and the roster
                // then takes them out of the lobby. Admit comes back if
                // it does not.
                ui.label(
                    RichText::new(t("Letting in…"))
                        .font(theme::medium(13.0))
                        .color(palette.secondary),
                );
                ui.add(egui::Spinner::new().size(12.0).color(palette.secondary));
                ui.ctx().request_repaint_after(
                    until.saturating_duration_since(std::time::Instant::now()),
                );
            } else if small_button(ui, palette, &t("Admit"), Some(ACTIVE_BUTTON))
                .on_hover_text(&waiting.text)
                .clicked()
            {
                actions.push(Action::Huddle(huddles::Action::Admit {
                    user: waiting.user.clone(),
                }));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(Icon::Clock.image(ACTIVE, 14.0));
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&waiting.text)
                                .font(theme::regular(12.0))
                                .color(palette.text),
                        )
                        .wrap(),
                    );
                });
            });
        });
    });
}

/// The meeting's join link, and Copy, so others can be invited.
fn invite_row(ui: &mut egui::Ui, palette: &Palette, link: &str, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if small_button(ui, palette, &t("Copy link"), None)
                .on_hover_text(t("Copy the link others join the meeting with"))
                .clicked()
            {
                actions.push(Action::Copy(link.to_owned()));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(Icon::Link.image(palette.secondary, 14.0));
                ui.add(
                    egui::Label::new(
                        RichText::new(t("Invite others with the meeting's link"))
                            .font(theme::regular(12.0))
                            .color(palette.secondary),
                    )
                    .wrap(),
                );
            });
        });
    });
}

/// Someone's screen share: who, and Watch (or, while it is open in the
/// call window, Stop watching).
#[cfg(feature = "huddle-video")]
fn share_row(ui: &mut egui::Ui, palette: &Palette, share: &ShareRow, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, wanted) = if share.watched {
                (t("Stop watching"), None)
            } else {
                (t("Watch"), Some(share.key.clone()))
            };
            let fill = (!share.watched).then_some(ACTIVE_BUTTON);
            if small_button(ui, palette, &label, fill)
                .on_hover_text(&share.text)
                .clicked()
            {
                actions.push(Action::Huddle(huddles::Action::Watch(wanted)));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(Icon::Monitor.image(ACTIVE, 14.0));
                // On two lines if it must: the sidebar is narrow.
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&share.text)
                                .font(theme::regular(12.0))
                                .color(palette.text),
                        )
                        .wrap(),
                    );
                });
            });
        });
    });
}

/// How many have a camera on, and Video, which opens the call window on
/// their tiles (or, while it is open, Close video).
#[cfg(feature = "huddle-video")]
fn cameras_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    count: usize,
    open: bool,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (label, action, fill) = if open {
                (t("Close video"), huddles::Action::Watch(None), None)
            } else {
                (t("Video"), huddles::Action::OpenCall, Some(ACTIVE_BUTTON))
            };
            let text = huddles::cameras_text(count);
            if small_button(ui, palette, &label, fill)
                .on_hover_text(&text)
                .clicked()
            {
                actions.push(Action::Huddle(action));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(Icon::Users.image(ACTIVE, 14.0));
                ui.add(
                    egui::Label::new(
                        RichText::new(text)
                            .font(theme::regular(12.0))
                            .color(palette.text),
                    )
                    .wrap(),
                );
            });
        });
    });
}

/// The headphones and the huddle's name, which opens its conversation.
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
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(Icon::Headphones.image(tint, 16.0));
                let label = egui::Label::new(
                    RichText::new(&bar.title)
                        .font(theme::semibold(13.5))
                        .color(palette.text),
                )
                .truncate();
                if !bar.openable {
                    ui.add(label);
                    return;
                }
                let title = ui
                    .add(label.sense(Sense::click()))
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
            Phase::Joining | Phase::Ringing | Phase::Lobby => {
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

/// How a call control is drawn: the call bar's compact buttons, or the
/// call window's larger ones, with their words or as icons alone. The
/// controls themselves ([`leave_button`],
/// [`super::huddle_mic::mute_button`], the camera's) are the same
/// widgets in both places, so what they do lives in one place.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// The button's height, in points.
    pub height: f32,
    /// The icon's side, in points.
    pub icon: f32,
    /// The word's size, in points.
    pub text: f32,
    /// With its word ("Mute"), or the icon alone (a narrow window).
    pub labelled: bool,
    /// Leave shows the hung-up phone beside its word, as calls have it.
    pub leave_icon: bool,
}

impl Look {
    /// The call bar's: compact, worded, Leave a word alone.
    pub const BAR: Self = Self {
        height: 28.0,
        icon: 14.0,
        text: 13.0,
        labelled: true,
        leave_icon: false,
    };
}

/// One call control in `look`: `icon` in its ink, then `label` in `ink`
/// unless the look is icons alone (the label then is what a screen
/// reader says), on `fill`.
pub fn control(
    ui: &mut egui::Ui,
    look: Look,
    icon: (Icon, Color32),
    label: &str,
    ink: Color32,
    fill: Color32,
) -> egui::Response {
    let image = icon.0.image(icon.1, look.icon);
    let button = if look.labelled {
        egui::Button::image_and_text(
            image,
            RichText::new(label)
                .font(theme::medium(look.text))
                .color(ink),
        )
        .min_size(Vec2::new(0.0, look.height))
    } else {
        egui::Button::image(image).min_size(Vec2::splat(look.height))
    };
    let response = ui
        .scope(|ui| {
            // Square when an icon alone: the theme's padding would make
            // each 10 points wider, and three of them would push the call
            // bar's row past the sidebar's edge.
            if !look.labelled {
                ui.spacing_mut().button_padding.x = ((look.height - look.icon) / 2.0).max(0.0);
            }
            ui.add(
                button
                    .fill(fill)
                    .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2)),
            )
        })
        .inner
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if !look.labelled {
        theme::describe(&response, egui::WidgetType::Button, label);
    }
    response
}

/// Leave, in red, in `look` ("Hang up" for a `call`); whether it was
/// clicked. Its chord is [`super::keys::leave_chord`], taken by the
/// window with the focus.
pub fn leave_button(ui: &mut egui::Ui, palette: &Palette, look: Look, leaving: Leaving) -> bool {
    let shortcut = super::shortcuts::spell("Cmd+Shift+H", cfg!(target_os = "macos"));
    let (tip, label) = match leaving {
        Leaving::Call => (
            tf("Hang up ({shortcut})", &[("shortcut", &shortcut)]),
            t("Hang up"),
        ),
        Leaving::Meeting => (
            tf("Leave the meeting ({shortcut})", &[("shortcut", &shortcut)]),
            t("Leave"),
        ),
        Leaving::Huddle => (
            tf("Leave the huddle ({shortcut})", &[("shortcut", &shortcut)]),
            t("Leave"),
        ),
    };
    let response = if look.labelled && !look.leave_icon {
        small_button(ui, palette, &label, Some(LEAVE))
    } else {
        control(
            ui,
            look,
            (Icon::PhoneOff, Color32::WHITE),
            &label,
            Color32::WHITE,
            LEAVE,
        )
    };
    response.on_hover_text(tip).clicked()
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

/// Of `others` and, if `me`, your face, in room for `fit` faces: how many
/// of the others show, and how many "+N" counts. Yours always shows, so
/// the count is of the others alone.
fn fitting(others: usize, me: bool, fit: usize) -> (usize, usize) {
    let room = fit.max(1 + usize::from(me)) - usize::from(me);
    if others <= room {
        (others, 0)
    } else {
        // "+N" takes a face's room.
        let shown = room.saturating_sub(1);
        (shown, others - shown)
    }
}

/// The faces, as many as fit, then "+N" and yours; the speaking ringed,
/// the muted marked.
fn faces(ui: &mut egui::Ui, palette: &Palette, faces: &[Face]) {
    ui.spacing_mut().item_spacing.x = FACE_GAP;
    let room = ui.available_width();
    let fit = ((room + FACE_GAP) / (FACE + FACE_GAP)).floor().max(1.0) as usize;
    let (others, mine): (Vec<&Face>, Vec<&Face>) = faces.iter().partition(|f| !f.me);
    let (shown, more) = fitting(others.len(), !mine.is_empty(), fit);
    for face in &others[..shown] {
        one_face(ui, palette, face);
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
    for face in mine {
        one_face(ui, palette, face);
    }
}

/// One face, ringed if speaking, marked if muted.
fn one_face(ui: &mut egui::Ui, palette: &Palette, face: &Face) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn your_face_always_shows_and_more_counts_the_others() {
        // Room for everyone: all show, no "+N".
        assert_eq!(fitting(3, true, 5), (3, 0));
        assert_eq!(fitting(4, true, 5), (4, 0));
        // Too many: one place goes to "+N", one to you.
        assert_eq!(fitting(9, true, 5), (3, 6));
        assert_eq!(fitting(9, false, 5), (4, 5));
        // Hardly any room: still you, and the others as a count.
        assert_eq!(fitting(4, true, 1), (0, 4));
        assert_eq!(fitting(0, true, 1), (0, 0));
    }
}
