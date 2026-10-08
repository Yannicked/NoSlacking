//! What a demo run does to the window: the view `--demo-view` opens, the
//! pointer, clicks, keys and typing it pretends, and the screenshot or
//! frames it saves before quitting.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{NOW, THREAD, ts};
use crate::app::App;
use crate::model::{Action, Ts};

/// Declares [`View`] from one line per view: its doc, the features it
/// needs, its variant and its `--demo-view` name, so the names, the list
/// in an error and the variants cannot drift apart.
macro_rules! views {
    ($(
        $(#[doc = $doc:literal])*
        $(#[cfg($cfg:meta)])?
        $variant:ident = $name:literal,
    )*) => {
        /// A view `--demo-view` opens before the screenshot.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum View {
            $(
                $(#[doc = $doc])*
                $(#[cfg($cfg)])?
                $variant,
            )*
        }

        impl View {
            /// Every view this build has, in `--demo-view`'s order.
            pub const ALL: &[View] = &[$($(#[cfg($cfg)])? View::$variant,)*];

            /// Its name on the command line.
            pub fn name(self) -> &'static str {
                match self {
                    $($(#[cfg($cfg)])? View::$variant => $name,)*
                }
            }
        }
    };
}

views! {
    /// The thread on #engineering's first message.
    Thread = "thread",
    /// The settings page.
    Settings = "settings",
    /// A new custom emoji, its picture picked and a name typed.
    AddEmoji = "add-emoji",
    /// #engineering's Files tab, asking whether to delete your file.
    DeleteFile = "delete-file",
    /// The keyboard shortcuts.
    Shortcuts = "shortcuts",
    /// Adding a workspace.
    SignIn = "sign-in",
    /// The quick switcher.
    Switcher = "switcher",
    /// The switcher as the command palette, `>` typed.
    Palette = "palette",
    /// The reaction picker on the thread's first message.
    Picker = "picker",
    /// Ana's profile.
    Profile = "profile",
    /// The "Share message" dialog, on the thread's first message.
    Share = "share",
    /// Your own status, being set.
    Status = "status",
    /// Your own message in #engineering, opened for editing.
    Edit = "edit",
    /// The image viewer, on the first of #engineering's pictures.
    Lightbox = "lightbox",
    /// The direct message with Ana.
    Dm = "dm",
    /// A message with every kind of formatting sent to the DM, as its rich
    /// text comes back from the pretend Slack.
    RichSend = "rich-send",
    /// #design, with a huddle going on.
    Huddle = "huddle",
    /// Listening to #design's huddle: the call bar.
    Listening = "listening",
    /// In a Teams-style meeting with your microphone live: the call bar's
    /// meeting title, its join link to copy, and Lee waiting in the lobby
    /// with Admit.
    Meeting = "meeting",
    /// The same, Admit pressed for Lee: being let in.
    MeetingAdmitting = "meeting-admitting",
    /// Calling Bob: the call bar ringing.
    Calling = "calling",
    /// The meeting's call window, drawn inside the main one.
    #[cfg(feature = "huddle-video")]
    MeetingWindow = "meeting-window",
    /// Listening with your microphone live.
    Talking = "talking",
    /// Your camera on (the test picture): the call bar's self-preview.
    #[cfg(feature = "huddle-camera")]
    Camera = "camera",
    /// Ana and Carla share their screens and five have a camera on: the
    /// call bar's rows.
    #[cfg(feature = "huddle-video")]
    Sharing = "sharing",
    /// Ana's share open in the call window. eframe cannot take a
    /// screenshot of a window of its own, so it is drawn inside the main
    /// one.
    #[cfg(feature = "huddle-video")]
    CallWindow = "call-window",
    /// The call window on the cameras alone: the grid of tiles.
    #[cfg(feature = "huddle-video")]
    Cameras = "cameras",
    /// Your camera on and the call window open on it: your own tile, and
    /// the controls with the microphone and camera on.
    #[cfg(all(feature = "huddle-video", feature = "huddle-camera"))]
    CameraWindow = "camera-window",
    /// Sharing your screen: the call bar's "You are sharing your screen"
    /// with Stop sharing, and Share on.
    #[cfg(feature = "huddle-share")]
    SharingSelf = "sharing-self",
    /// Where the system has no dialog of its own: the call bar's screens
    /// and windows to choose from.
    #[cfg(feature = "huddle-share")]
    SharePick = "share-pick",
    /// Sharing your screen while watching Ana's: the call window's
    /// controls with Share on.
    #[cfg(all(feature = "huddle-video", feature = "huddle-share"))]
    ShareWindow = "share-window",
    /// Settings → Huddles: the camera, microphone and speaker pickers, the
    /// speaker a pair of AirPods not connected, and the call bar at the
    /// foot.
    Devices = "devices",
    /// [`View::Devices`] with the speaker's picker open.
    DevicesPick = "devices-pick",
    /// [`View::Devices`] with no video helper.
    DevicesNoHelper = "devices-no-helper",
    /// Talking in #design: the call bar's menu beside Mute open on the
    /// microphones and speakers.
    DeviceMenu = "device-menu",
    /// The call bar's menu beside Video open, with the camera on.
    CameraMenu = "camera-menu",
    /// The call window's controls with the menu beside Mute open.
    #[cfg(feature = "huddle-video")]
    DeviceWindow = "device-window",
    /// #deploys, with the bots' buttons and menus.
    Deploys = "deploys",
    /// The deploy bot's Approve button pressed: its question.
    Approve = "approve",
    /// The rollout bot's select in #deploys, open on its choices.
    Menus = "menus",
    /// #deploys in the workspace signed in by OAuth, where app buttons and
    /// menus only work in Slack.
    DeploysOauth = "deploys-oauth",
    /// #general.
    General = "general",
    /// #engineering in IRC-style rows.
    Compact = "compact",
    /// The media of #design held back until clicked.
    HeldMedia = "held-media",
    /// Link previews, a video, a sound and a PDF.
    Media = "media",
    /// #random's files: Slack's previews of a snippet, a text file, a PDF,
    /// a spreadsheet, a voice clip (playing) and a video.
    Previews = "previews",
    /// The file viewer over #random, on a spreadsheet.
    ViewerSheet = "viewer-sheet",
    /// The file viewer on a CSV file.
    ViewerCsv = "viewer-csv",
    /// The file viewer on a zip archive.
    ViewerZip = "viewer-zip",
    /// The file viewer on source code.
    ViewerText = "viewer-text",
    /// Search results, with a second page to scroll to.
    Search = "search",
    /// An old message of #general, shown in context and lit up.
    Jump = "jump",
    /// A reply: its thread opens beside the conversation.
    JumpReply = "jump-reply",
    /// Half-written messages elsewhere, for the sidebar's pencils.
    Drafts = "drafts",
    /// A file waiting in the composer, not sent yet.
    Attach = "attach",
    /// A file on its way, with its progress and Cancel.
    Upload = "upload",
    /// #engineering's details, on About.
    Details = "details",
    /// #engineering's members.
    Members = "members",
    /// #engineering's files.
    Files = "files",
    /// #engineering's pins.
    Pins = "pins",
    /// #engineering's bookmarks.
    Bookmarks = "bookmarks",
    /// Activity, at the top of the sidebar.
    Activity = "activity",
    /// Unreads.
    Unreads = "unreads",
    /// Threads.
    Threads = "threads",
    /// Saved for later.
    Later = "later",
    /// Scheduled messages.
    Scheduled = "scheduled",
    /// The bookmarks tab with the "Add a bookmark" dialog over it, half
    /// filled in.
    BookmarkAdd = "bookmark-add",
    /// Browsing channels.
    Browse = "browse",
    /// A new channel, its name typed.
    NewChannel = "new-channel",
    /// Picking people for a group message, one already picked.
    NewMessage = "new-message",
}

impl std::str::FromStr for View {
    type Err = String;

    /// A view by its name; a name this build lacks lists the ones it has.
    fn from_str(name: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .copied()
            .find(|view| view.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|view| view.name()).collect();
                format!("no such view; one of {}", names.join(", "))
            })
    }
}

/// `WxH` as a size, for `--demo-size`.
pub fn size(text: &str) -> Option<[f32; 2]> {
    let (w, h) = text.split_once('x')?;
    Some([w.parse().ok()?, h.parse().ok()?])
}

/// `X,Y` as a point.
fn point(text: Option<&str>) -> Option<egui::Pos2> {
    let (x, y) = text?.split_once(',')?;
    Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// One key of `--demo-keys`: an egui key name after any "Shift+", "Alt+"
/// or "Ctrl+".
fn key(spec: &str) -> Option<(egui::Key, egui::Modifiers)> {
    let mut modifiers = egui::Modifiers::NONE;
    let mut rest = spec.trim();
    loop {
        if let Some(after) = rest.strip_prefix("Shift+") {
            modifiers |= egui::Modifiers::SHIFT;
            rest = after;
        } else if let Some(after) = rest.strip_prefix("Alt+") {
            modifiers |= egui::Modifiers::ALT;
            rest = after;
        } else if let Some(after) = rest.strip_prefix("Ctrl+") {
            modifiers |= egui::Modifiers::COMMAND;
            rest = after;
        } else {
            break;
        }
    }
    Some((egui::Key::from_name(rest)?, modifiers))
}

/// The `--demo-…` flags, as the command line gave them.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `--demo-shot`: where to save the screenshot.
    pub shot: Option<PathBuf>,
    /// `--demo-shot-delay`: milliseconds before the screenshot.
    pub shot_delay: u64,
    /// `--demo-size`: `WxH`, which the call window takes too.
    pub size: Option<String>,
    /// `--demo-hover`: `X,Y` where the pointer rests.
    pub hover: Option<String>,
    /// `--demo-view`: the view to open.
    pub view: Option<View>,
    /// `--demo-right-click`: `X,Y` to right-click.
    pub right_click: Option<String>,
    /// `--demo-click`: `X,Y` to click.
    pub click: Option<String>,
    /// `--demo-keys`: keys to press, separated by commas.
    pub keys: Option<String>,
    /// `--demo-wheel`: lines to scroll up.
    pub wheel: Option<f32>,
    /// `--demo-type`: text to type.
    pub typing: Option<String>,
    /// `--demo-frames`: where to save every frame.
    pub frames: Option<PathBuf>,
}

/// How far filming a demo run got.
#[derive(Clone, Default)]
struct Filmed {
    /// Frames asked to be saved.
    asked: u32,
    /// Frames saved.
    saved: u32,
}

/// A demo run's script for the window, and how far it got.
#[derive(Clone)]
pub struct Setup {
    shot: Option<PathBuf>,
    hover: Option<egui::Pos2>,
    view: Option<View>,
    /// `--demo-size`, which the call window takes when the view opens it.
    #[cfg(feature = "huddle-video")]
    call_size: Option<[f32; 2]>,
    frames: u32,
    asked: bool,
    /// When the screenshot is due.
    due: Option<Instant>,
    /// Lines to scroll up, once.
    wheel: Option<f32>,
    /// Where to right-click, once.
    right_click: Option<egui::Pos2>,
    /// Where to click, once.
    click: Option<egui::Pos2>,
    /// Characters still to type, one every other frame.
    typing: VecDeque<char>,
    /// Where to save every frame, and how many were asked for and saved.
    film: Option<PathBuf>,
    filmed: Filmed,
    /// Whether `--demo-type` gave nothing to type.
    typing_none: bool,
    /// How many frames were asked for when the last character was typed:
    /// filming goes on a little after it, to see the field at rest.
    typed_frames: u32,
    /// Keys still to press, one per frame.
    keys: Vec<(egui::Key, egui::Modifiers)>,
    started: Instant,
    /// Your message to open for editing once its history has arrived:
    /// asked for earlier, the edit finds nothing to edit.
    edit: Option<Ts>,
    /// The message whose picture to open in the image viewer once its
    /// history has arrived.
    image: Option<Ts>,
    /// The message to open the "Share message" dialog on once its history
    /// has arrived: the dialog closes on a message it cannot find.
    share: Option<Ts>,
    /// Whether the share dialog's comment was typed in yet.
    commented: bool,
    /// Whether to press the deploy bot's Approve button in #deploys once it
    /// has arrived, which asks the app's question first.
    approve: bool,
    /// Whether to open the rollout bot's select in #deploys once it has
    /// arrived, to show its choices.
    open_select: bool,
    /// A device picker or menu to open once the page has settled (and
    /// scrolled), to show its devices.
    open_popup: Option<egui::Id>,
    /// With `NOSLACKING_DEMO_FRAME_TIMES` set: how long frames took with
    /// and without new video pictures, in the log every ten seconds.
    #[cfg(feature = "huddle-video")]
    frame_times: Option<FrameTimes>,
}

impl Setup {
    /// The script the flags ask for, starting now.
    pub fn new(options: Options) -> Self {
        let started = Instant::now();
        Self {
            #[cfg(feature = "huddle-video")]
            frame_times: std::env::var_os("NOSLACKING_DEMO_FRAME_TIMES")
                .map(|_| FrameTimes::default()),
            due: Some(started + Duration::from_millis(options.shot_delay)),
            shot: options.shot,
            wheel: options.wheel,
            right_click: point(options.right_click.as_deref()),
            click: point(options.click.as_deref()),
            keys: options
                .keys
                .as_deref()
                .unwrap_or_default()
                .split(',')
                .filter_map(key)
                .collect(),
            started,
            typing: options
                .typing
                .as_deref()
                .unwrap_or_default()
                .chars()
                .collect(),
            film: options.frames,
            filmed: Filmed::default(),
            typed_frames: 0,
            typing_none: options.typing.as_deref().is_none_or(str::is_empty),
            hover: point(options.hover.as_deref())
                // A wheel scrolls what is under the pointer: the messages.
                .or(options.wheel.map(|_| egui::pos2(900.0, 450.0))),
            view: options.view,
            #[cfg(feature = "huddle-video")]
            call_size: options.size.as_deref().and_then(size),
            frames: 0,
            asked: false,
            edit: None,
            image: None,
            share: None,
            commented: false,
            approve: false,
            open_select: false,
            open_popup: None,
        }
    }

    /// Adds the pretend pointer, clicks, keys, typing and scrolling to a
    /// frame's input, each when its time comes.
    pub fn raw_input(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        let settled = self.started.elapsed() > Duration::from_millis(2500);
        if let Some(pos) = self.hover
            && ctx.input(|input| input.pointer.latest_pos()) != Some(pos)
        {
            input.events.push(egui::Event::PointerMoved(pos));
        }
        if let Some(pos) = self.right_click
            && self.started.elapsed() > Duration::from_millis(2000)
        {
            self.right_click = None;
            press(input, pos, egui::PointerButton::Secondary);
        }
        if let Some(pos) = self.click
            && settled
        {
            self.click = None;
            press(input, pos, egui::PointerButton::Primary);
        }
        if settled
            && self.frames.is_multiple_of(2)
            && let Some(c) = self.typing.pop_front()
        {
            input.events.push(egui::Event::Text(c.to_string()));
            self.typed_frames = self.filmed.asked;
        }
        if settled && !self.keys.is_empty() {
            let (key, modifiers) = self.keys.remove(0);
            for pressed in [true, false] {
                input.events.push(egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers,
                });
            }
        }
        if let Some(lines) = self.wheel
            && settled
        {
            self.wheel = None;
            input.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, lines),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            });
        }
    }

    /// Before the app draws: opens the view once the pretend workspace has
    /// arrived, and what waits on a message once that message has.
    pub fn before_frame(&mut self, app: &mut App) {
        self.frames += 1;
        // Let the pretend workspace arrive and #engineering open first.
        if self.frames == 3 {
            app.actions.push(Action::OpenConversation("C02".into()));
        }
        self.when_arrived(app);
        // The share dialog, once open, with a comment typed in it.
        if !self.commented
            && let Some(share) = app.share.as_mut()
        {
            share.comment = "Worth a read before Thursday's review".into();
            self.commented = true;
        }
        if self.frames == 6
            && let Some(view) = self.view
        {
            self.open(view, app);
        }
    }

    /// What waits for a message of #engineering or #deploys to arrive.
    fn when_arrived(&mut self, app: &mut App) {
        let arrived = |app: &App, ts: &Ts| {
            app.active_workspace()
                .is_some_and(|w| w.find_message("C02", ts).is_some())
        };
        if let Some(ts) = self.edit.take_if(|ts| arrived(app, ts)) {
            app.actions.push(Action::StartEdit {
                channel: "C02".into(),
                ts,
                in_thread: false,
            });
        }
        if let Some(ts) = self.image.take_if(|ts| arrived(app, ts)) {
            app.actions.push(Action::ViewImage {
                channel: "C02".into(),
                thread: None,
                ts,
                file: "F01".into(),
            });
        }
        if let Some(ts) = self.share.take_if(|ts| arrived(app, ts)) {
            app.actions.push(Action::Share {
                channel: "C02".into(),
                ts: ts.clone(),
                thread: Some(ts),
            });
        }
        if self.approve
            && let Some(workspace) = app.active_workspace()
            && let Some(message) = workspace.find_message("C05", &ts(super::APPROVAL))
        {
            use crate::model::{ButtonUse, KitBlock, KitElement, button_use};
            let pressed = message.blocks.iter().find_map(|block| match block {
                KitBlock::Actions(elements) => match elements.first() {
                    Some(KitElement::Button(button)) => Some(button),
                    _ => None,
                },
                _ => None,
            });
            if let Some(button) = pressed
                && let ButtonUse::Press(press) =
                    button_use(workspace.info.sign_in, "C05", message, button)
            {
                app.actions.push(Action::PressButton {
                    press: Box::new(press),
                    confirm: button.confirm.clone(),
                    confirmed: false,
                    link: None,
                });
            }
            self.approve = false;
        }
    }

    /// Opens `view`.
    fn open(&mut self, view: View, app: &mut App) {
        use crate::huddle_mic::Mic;
        use crate::huddles::{self, Listening};
        let open = |app: &mut App, channel: &str| {
            app.actions.push(Action::OpenConversation(channel.into()));
        };
        match view {
            View::Thread => app.actions.push(Action::OpenThread {
                channel: "C02".into(),
                ts: ts(THREAD),
            }),
            View::Settings => app.actions.push(Action::ShowSettings),
            View::AddEmoji => {
                app.add_emoji = Some(crate::custom_emoji::Dialog {
                    team: super::TEAM.into(),
                    name: "shipit".into(),
                    picked: Some(crate::custom_emoji::Picked::new(
                        "shipit.gif".into(),
                        super::PARROT_BYTES.to_vec(),
                        0,
                    )),
                    ..Default::default()
                });
            }
            View::DeleteFile => {
                app.actions
                    .push(Action::Convos(crate::convos::Action::Details {
                        channel: "C02".into(),
                        tab: crate::convos::Tab::Files,
                    }));
                app.actions.push(Action::AskDeleteFile {
                    file: "F3".into(),
                    name: "sidebar-v2.png".into(),
                });
            }
            View::Shortcuts => app.actions.push(Action::ShowShortcuts),
            View::SignIn => app.actions.push(Action::AddWorkspace),
            View::Switcher => app.actions.push(Action::OpenSwitcher),
            View::Palette => {
                app.switcher = Some(crate::app::Switcher {
                    query: ">".into(),
                    selected: 0,
                });
                app.focus_overlay = true;
            }
            View::Picker => app.actions.push(Action::PickReaction {
                channel: "C02".into(),
                ts: ts(THREAD),
            }),
            View::Profile => app.actions.push(Action::OpenProfile("U01".into())),
            View::Share => self.share = Some(ts(THREAD)),
            View::Status => app
                .actions
                .push(Action::People(crate::people::Action::EditStatus)),
            View::Edit => self.edit = Some(ts(NOW - 2000)),
            View::Lightbox => self.image = Some(ts(NOW - 2400)),
            View::Dm => open(app, "D01"),
            View::RichSend => {
                open(app, "D01");
                let key = App::draft_key(super::TEAM, "D01", None);
                app.drafts.insert(
                    key,
                    crate::app::Draft {
                        mentions: vec![("@Ana Lima".into(), "<@U01>".into())],
                        ..Default::default()
                    },
                );
                app.actions.push(Action::Send {
                    text: "*Release plan* for @Ana Lima :rocket:\n\
                           1. Freeze _Thursday_\n\
                           2. Run `cargo test`\n    \
                           • on ~Windows~ every OS\n\
                           > Ship it when it's green\n\
                           ```\ncargo build --release\n```\n\
                           Thanks! 2*3 < 7, snake_case"
                        .into(),
                    thread: None,
                    broadcast: false,
                });
            }
            View::Huddle | View::Media => open(app, "C03"),
            View::Listening => {
                app.huddles.listening = Some(super::listening());
                open(app, "C03");
            }
            View::Meeting => {
                app.huddles.listening = Some(super::meeting());
                open(app, "C03");
            }
            View::MeetingAdmitting => {
                let mut meeting = super::meeting();
                meeting
                    .admitting
                    .push(("U06".into(), std::time::Instant::now()));
                app.huddles.listening = Some(meeting);
                open(app, "C03");
            }
            View::Calling => {
                app.huddles.listening = Some(Listening {
                    callee: Some("U02".into()),
                    phase: huddles::Phase::Ringing,
                    mic: Mic::Live,
                    ..super::listening()
                });
                open(app, "C03");
            }
            #[cfg(feature = "huddle-video")]
            View::MeetingWindow => {
                let listening = Listening {
                    shares: Vec::new(),
                    ..super::meeting()
                };
                self.call_window(app, listening, "C03", huddles::Action::OpenCall);
            }
            View::Talking => {
                app.huddles.listening = Some(Listening {
                    mic: Mic::Live,
                    ..super::listening()
                });
                open(app, "C03");
            }
            #[cfg(feature = "huddle-camera")]
            View::Camera => {
                super::camera(true);
                app.huddles.listening = Some(super::camera_on());
                open(app, "C03");
            }
            #[cfg(feature = "huddle-video")]
            View::Sharing => {
                app.huddles.listening = Some(super::sharing());
                open(app, "C03");
            }
            // #engineering behind the call windows: #design would ring
            // Bob's invitation over them.
            #[cfg(feature = "huddle-video")]
            View::CallWindow => {
                let listening = super::sharing();
                let first = listening.shares.first().map(|s| s.key.clone());
                self.call_window(app, listening, "C02", huddles::Action::Watch(first));
            }
            #[cfg(feature = "huddle-video")]
            View::Cameras => {
                let listening = Listening {
                    shares: Vec::new(),
                    ..super::sharing()
                };
                self.call_window(app, listening, "C02", huddles::Action::OpenCall);
            }
            #[cfg(all(feature = "huddle-video", feature = "huddle-camera"))]
            View::CameraWindow => {
                super::camera(true);
                let listening = super::camera_on();
                self.call_window(app, listening, "C02", huddles::Action::OpenCall);
            }
            #[cfg(feature = "huddle-share")]
            View::SharingSelf => {
                app.huddles.listening = Some(super::sharing_self());
                open(app, "C03");
            }
            #[cfg(feature = "huddle-share")]
            View::SharePick => {
                app.huddles.listening = Some(Listening {
                    sharing: crate::huddle_share::Sharing::Choosing,
                    share_sources: super::share_sources(),
                    ..super::listening()
                });
                open(app, "C03");
            }
            #[cfg(all(feature = "huddle-video", feature = "huddle-share"))]
            View::ShareWindow => {
                let listening = Listening {
                    mic: Mic::Live,
                    sharing: crate::huddle_share::Sharing::On,
                    ..super::sharing()
                };
                let first = listening.shares.first().map(|s| s.key.clone());
                self.call_window(app, listening, "C02", huddles::Action::Watch(first));
            }
            View::Devices | View::DevicesPick | View::DevicesNoHelper => {
                use crate::devices::{Kind, popup_id};
                if view == View::DevicesNoHelper {
                    super::pretend_no_helper();
                }
                app.settings.devices = super::chosen();
                app.huddles.listening = Some(Listening {
                    mic: Mic::Live,
                    ..super::listening()
                });
                app.actions.push(Action::ShowSettings);
                // Down to Huddles, at the page's foot.
                self.wheel.get_or_insert(-80.0);
                self.hover.get_or_insert(egui::pos2(900.0, 300.0));
                if view == View::DevicesPick {
                    self.open_popup = Some(popup_id("settings", Kind::Speaker));
                }
            }
            View::DeviceMenu | View::CameraMenu => {
                use crate::devices::{Kind, menu_id};
                app.settings.devices = super::chosen();
                let kind = if view == View::CameraMenu {
                    #[cfg(feature = "huddle-camera")]
                    {
                        super::camera(true);
                        app.huddles.listening = Some(super::camera_on());
                    }
                    Kind::Camera
                } else {
                    app.huddles.listening = Some(Listening {
                        mic: Mic::Live,
                        ..super::listening()
                    });
                    Kind::Microphone
                };
                open(app, "C03");
                self.open_popup = Some(menu_id("sidebar-call-bar", kind));
            }
            #[cfg(feature = "huddle-video")]
            View::DeviceWindow => {
                use crate::devices::{Kind, menu_id};
                app.settings.devices = super::chosen();
                let listening = Listening {
                    mic: Mic::Live,
                    shares: Vec::new(),
                    ..super::sharing()
                };
                self.call_window(app, listening, "C02", huddles::Action::OpenCall);
                self.open_popup = Some(menu_id("window", Kind::Microphone));
            }
            View::Deploys => open(app, "C05"),
            View::Approve => {
                open(app, "C05");
                self.approve = true;
            }
            View::Menus => {
                open(app, "C05");
                self.open_select = true;
            }
            View::DeploysOauth => {
                app.actions.push(Action::SelectWorkspace("TDEMO2".into()));
                open(app, "C05");
            }
            View::General => open(app, "C01"),
            View::Compact => app.settings.density = crate::settings::Density::Compact,
            View::HeldMedia => {
                app.settings.inline_media = false;
                open(app, "C03");
            }
            // The voice clip plays, to show the card mid-play.
            View::Previews => {
                open(app, "C04");
                if let Some(track) = super::voice_clip()
                    .as_ref()
                    .and_then(crate::audio::Track::of)
                {
                    app.actions
                        .push(Action::Audio(crate::audio::Request::Toggle(track)));
                }
            }
            View::ViewerSheet | View::ViewerCsv | View::ViewerZip | View::ViewerText => {
                use crate::viewer::Kind;
                let (id, name, filetype, kind, size) = match view {
                    View::ViewerSheet => ("F23", "Q4 budget.xlsx", "xlsx", Kind::Sheet, 75_813),
                    View::ViewerCsv => ("F26", "deploys.csv", "csv", Kind::Csv, 7_412),
                    View::ViewerZip => ("F27", "logs.zip", "zip", Kind::Zip, 98_220),
                    _ => ("F20", "backoff.rs", "rust", Kind::Text, 742),
                };
                open(app, "C04");
                app.actions.push(Action::ViewFile {
                    url: format!("https://files.slack.com/files-pri/TDEMO-{id}/{name}"),
                    name: name.into(),
                    filetype: filetype.into(),
                    kind,
                    size,
                });
            }
            View::Search => {
                app.search.text = "standup in:#general".into();
                app.actions.push(Action::OpenSearch);
                app.actions.push(Action::RunSearch);
            }
            View::Jump => app.actions.push(Action::JumpTo {
                channel: "C01".into(),
                ts: super::long_history_ts(20),
                thread: None,
            }),
            View::JumpReply => app.actions.push(Action::JumpTo {
                channel: "C02".into(),
                ts: ts(NOW - 2500),
                thread: Some(ts(THREAD)),
            }),
            View::Drafts => {
                for channel in ["C03", "D01"] {
                    app.drafts.insert(
                        format!("{}/{channel}", super::TEAM),
                        crate::app::Draft {
                            text: "Half a thought…".into(),
                            ..Default::default()
                        },
                    );
                }
            }
            View::Attach | View::Upload => {
                app.actions.push(Action::Upload {
                    thread: None,
                    path: "release-notes.pdf".into(),
                    comment: String::new(),
                });
                // Files wait in the composer until the message is sent.
                if view == View::Upload {
                    app.actions.push(Action::Send {
                        text: String::new(),
                        thread: None,
                        broadcast: false,
                    });
                }
            }
            View::Details | View::Members | View::Files | View::Pins | View::Bookmarks => {
                use crate::convos::{Action as Convos, Tab};
                let tab = match view {
                    View::Members => Tab::Members,
                    View::Files => Tab::Files,
                    View::Pins => Tab::Pins,
                    View::Bookmarks => Tab::Bookmarks,
                    _ => Tab::About,
                };
                app.actions.push(Action::Convos(Convos::Details {
                    channel: "C02".into(),
                    tab,
                }));
            }
            View::Activity | View::Unreads | View::Threads | View::Later | View::Scheduled => {
                use crate::views::{Action as Views, View as Top};
                let top = match view {
                    View::Unreads => Top::Unreads,
                    View::Threads => Top::Threads,
                    View::Later => Top::Later,
                    View::Scheduled => Top::Scheduled,
                    _ => Top::Activity,
                };
                app.actions.push(Action::Views(Views::Open(top)));
            }
            View::BookmarkAdd => {
                use crate::convos::{Action as Convos, BookmarkDialog, Tab};
                app.actions.push(Action::Convos(Convos::Details {
                    channel: "C02".into(),
                    tab: Tab::Bookmarks,
                }));
                app.convos.bookmark = Some(BookmarkDialog {
                    link: "docs.example.com/release".into(),
                    title: "Release guide".into(),
                    emoji: "books".into(),
                    ..BookmarkDialog::new("C02".into(), None)
                });
            }
            View::Browse => app
                .actions
                .push(Action::Convos(crate::convos::Action::Browse)),
            View::NewChannel => {
                app.convos.new_channel = Some(crate::convos::NewChannel {
                    name: "Release Notes".into(),
                    ..Default::default()
                });
            }
            View::NewMessage => {
                let mut dialog = crate::convos::NewMessage::default();
                dialog.pick("U02".into());
                dialog.query = "a".into();
                app.convos.new_message = Some(dialog);
            }
        }
    }

    /// In `listening`'s call, its window open with `then` and drawn inside
    /// the main one (eframe cannot take a screenshot of a window of its
    /// own), over `channel`.
    #[cfg(feature = "huddle-video")]
    fn call_window(
        &self,
        app: &mut App,
        listening: crate::huddles::Listening,
        channel: &str,
        then: crate::huddles::Action,
    ) {
        app.huddles.listening = Some(listening);
        app.huddles.picture.embed = true;
        app.huddles.picture.size = self.call_size;
        app.actions.push(Action::OpenConversation(channel.into()));
        app.actions.push(Action::Huddle(then));
    }

    /// With `NOSLACKING_DEMO_FRAME_TIMES`: notes the last frame's time
    /// `took`, and the video pixels this frame hands to textures.
    #[cfg(feature = "huddle-video")]
    pub fn note_frame(&mut self, took: Option<f32>, app: &App) {
        if let Some(times) = &mut self.frame_times {
            times.note(took, app.huddles.picture.uploaded());
        }
    }

    /// After the app draws: opens the popups that wait on the page, and
    /// saves the frames or the screenshot asked for, then quits.
    pub fn after_frame(&mut self, ctx: &egui::Context, app: &mut App) {
        self.film(ctx, app);
        // Once the page has settled and scrolled; the devices are asked
        // for again as it opens.
        if let Some(id) = self.open_popup
            && self.started.elapsed() > Duration::from_millis(3000)
        {
            egui::Popup::open_id(ctx, id);
            self.open_popup = None;
        }
        if self.open_select
            && let Some(workspace) = app.active_workspace()
            && let Some(message) = workspace.find_message("C05", &ts(super::ROLLOUT))
        {
            use crate::model::{Accessory, KitBlock};
            let select = message.blocks.iter().find_map(|block| match block {
                KitBlock::Section {
                    accessory: Some(Accessory::Menu(menu)),
                    ..
                } => Some(menu),
                _ => None,
            });
            if let Some(select) = select {
                egui::Popup::open_id(ctx, select.popup_id("C05", &message.ts, false));
            }
            self.open_select = false;
        }
        let Some(path) = self.shot.clone() else {
            return;
        };
        ctx.request_repaint();
        let due = self.due.is_none_or(|due| Instant::now() >= due);
        if !self.asked && self.frames > 40 && due {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.asked = true;
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else {
            return;
        };
        let [width, height] = [image.size[0] as u32, image.size[1] as u32];
        match rgba(&image) {
            Some(buffer) => match buffer.save(&path) {
                Ok(()) => log::info!("wrote {width}x{height} to {}", path.display()),
                Err(error) => log::error!("could not write {}: {error}", path.display()),
            },
            None => log::error!("the frame did not match {width}x{height}"),
        }
        self.shot = None;
        // Quit, not just close: with close-to-tray on, closing would only
        // hide the window, and the shot would never end.
        app.request_quit();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// With `--demo-frames`: asks for a screenshot of every frame from
    /// 2.5 s in and saves each one as it comes, quitting once the typing is
    /// done and every frame asked for is saved.
    fn film(&mut self, ctx: &egui::Context, app: &mut App) {
        let Some(dir) = self.film.clone() else {
            return;
        };
        ctx.request_repaint();
        let Filmed { asked, saved } = &mut self.filmed;
        for image in ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        }) {
            let path = dir.join(format!("frame-{saved:03}.png"));
            if let Some(buffer) = rgba(&image)
                && let Err(error) = buffer.save(&path)
            {
                log::error!("could not write {}: {error}", path.display());
            }
            *saved += 1;
        }
        // With nothing to type, from the first frame: to catch what moves
        // while the window loads.
        let started = self.typed_frames == 0 && self.typing.is_empty() && self.typing_none
            || self.started.elapsed() > Duration::from_millis(2500);
        // A little past the typing; longer with none, for loading to end.
        let after = if self.typing_none { 150 } else { 20 };
        if started && (!self.typing.is_empty() || *asked < self.typed_frames + after) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            *asked += 1;
        } else if started && *saved >= *asked {
            log::info!("saved {saved} frames to {}", dir.display());
            app.request_quit();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// A pointer button pressed and let go at `pos`.
fn press(input: &mut egui::RawInput, pos: egui::Pos2, button: egui::PointerButton) {
    input.events.push(egui::Event::PointerMoved(pos));
    for pressed in [true, false] {
        input.events.push(egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
    }
}

/// A screenshot as an image to save.
fn rgba(image: &egui::ColorImage) -> Option<image::RgbaImage> {
    let [width, height] = [image.size[0] as u32, image.size[1] as u32];
    let pixels = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_srgba_unmultiplied())
        .collect();
    image::RgbaImage::from_raw(width, height, pixels)
}

/// How long the demo's frames took, those that uploaded video pictures
/// apart from the rest, so a picture's upload can be told from drawing.
#[cfg(feature = "huddle-video")]
#[derive(Clone, Default)]
struct FrameTimes {
    since: Option<(Instant, f64)>,
    /// Pixels the last frame handed to textures: the next frame's time
    /// (eframe's, which covers painting) is that frame's.
    last: usize,
    /// Frames that uploaded pictures: how many, seconds, pixels.
    with: (u32, f64, usize),
    /// Frames that uploaded none: how many, seconds.
    without: (u32, f64),
}

#[cfg(feature = "huddle-video")]
impl FrameTimes {
    /// The main thread's CPU time so far, in seconds, from /proc (Linux
    /// only; 0 elsewhere).
    fn cpu() -> f64 {
        let Ok(stat) = std::fs::read_to_string("/proc/thread-self/stat") else {
            return 0.0;
        };
        let Some(close) = stat.rfind(')') else {
            return 0.0;
        };
        let fields: Vec<&str> = stat[close + 2..].split(' ').collect();
        let tick = |n: usize| fields.get(n).and_then(|f| f.parse::<f64>().ok());
        (tick(11).unwrap_or(0.0) + tick(12).unwrap_or(0.0)) / 100.0
    }

    /// Notes the last frame's time `took`, and `uploaded`, the pixels this
    /// frame hands to textures.
    fn note(&mut self, took: Option<f32>, uploaded: usize) {
        let now = Instant::now();
        let (since, cpu) = *self.since.get_or_insert((now, Self::cpu()));
        if let Some(took) = took {
            if self.last > 0 {
                self.with.0 += 1;
                self.with.1 += f64::from(took);
                self.with.2 += self.last;
            } else {
                self.without.0 += 1;
                self.without.1 += f64::from(took);
            }
        }
        self.last = uploaded;
        let seconds = now.duration_since(since).as_secs_f64();
        if seconds < 10.0 {
            return;
        }
        let mean = |n: u32, total: f64| total * 1000.0 / f64::from(n.max(1));
        log::info!(
            "demo frames in {seconds:.0} s: {} with pictures ({:.2} ms, {:.2} Mpixel each), {} \
             without ({:.2} ms); main thread {:.1} % of a core",
            self.with.0,
            mean(self.with.0, self.with.1),
            self.with.2 as f64 / 1e6 / f64::from(self.with.0.max(1)),
            self.without.0,
            mean(self.without.0, self.without.1),
            (Self::cpu() - cpu) * 100.0 / seconds
        );
        *self = Self {
            last: self.last,
            ..Self::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_reads_back_from_its_name() {
        for view in View::ALL {
            assert_eq!(view.name().parse::<View>(), Ok(*view));
        }
    }

    #[test]
    fn the_names_are_unique() {
        let mut names: Vec<&str> = View::ALL.iter().map(|view| view.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), View::ALL.len());
    }

    #[test]
    fn a_wrong_name_lists_the_views() {
        let Err(why) = "thraed".parse::<View>() else {
            panic!("a wrong name was taken");
        };
        assert!(why.contains("thread, settings,"), "{why}");
    }

    #[test]
    fn sizes_and_points() {
        assert_eq!(size("800x600"), Some([800.0, 600.0]));
        assert_eq!(size("800"), None);
        assert_eq!(point(Some(" 10, 20")), Some(egui::pos2(10.0, 20.0)));
        assert_eq!(point(Some("10")), None);
        assert_eq!(
            key("Shift+Ctrl+ArrowUp"),
            Some((
                egui::Key::ArrowUp,
                egui::Modifiers::SHIFT | egui::Modifiers::COMMAND
            ))
        );
    }
}
