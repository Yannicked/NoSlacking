//! NoSlacking's entry point: command line, logging, single instance, and
//! the window.

use std::sync::mpsc;

use clap::Parser;

use noslacking::app::{self, App};
use noslacking::backend::Waker;
use noslacking::paths::{APP_ID, AppDirs};
use noslacking::single_instance::{self, Outcome, Request};
use noslacking::{settings, theme};

/// A native Slack client.
#[derive(Parser, Debug)]
#[command(name = "noslacking", version, about)]
struct Cli {
    /// A noslacking:// link; the desktop passes sign-in redirects this way.
    link: Option<String>,

    /// Start in the tray without a window, as at login. Only with the tray
    /// item and "Keep running in the tray" on; otherwise the window opens.
    #[arg(long)]
    hidden: bool,

    /// Log more (also NOSLACKING_LOG=debug).
    #[arg(long)]
    verbose: bool,

    /// Give slack:// links back to whatever had them before, if a browser
    /// sign-in left them with NoSlacking, and quit. Every start does this
    /// too.
    #[arg(long)]
    release_slack_links: bool,

    /// Join the huddle in a conversation, listen and leave, logging each
    /// step, and quit: a test of huddle audio against real Slack. Takes
    /// the workspace's team id and the channel id, and the browser
    /// sign-in saved for that workspace.
    #[cfg(feature = "huddle-audio")]
    #[arg(long, num_args = 2, value_names = ["TEAM", "CHANNEL"])]
    huddle_probe: Option<Vec<String>>,

    /// How long the huddle probe listens, in seconds.
    #[cfg(feature = "huddle-audio")]
    #[arg(long, value_name = "N", default_value_t = 30)]
    seconds: u64,

    /// The media region the huddle probe asks Slack for.
    #[cfg(feature = "huddle-audio")]
    #[arg(long, value_name = "REGION", default_value = noslacking::huddle_audio::join::DEFAULT_REGION)]
    huddle_region: String,

    /// Run against a pretend Slack, offline, with sample data.
    #[cfg(feature = "demo")]
    #[arg(long)]
    demo: bool,

    /// Save a screenshot of the demo to PATH and quit.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "PATH")]
    demo_shot: Option<std::path::PathBuf>,

    /// The demo window's size as WxH logical points.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "WxH")]
    demo_size: Option<String>,

    /// Hold a pretend pointer at X,Y (logical points), to capture hover states.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "X,Y")]
    demo_hover: Option<String>,

    /// Open a view before the screenshot: thread, settings, sign-in,
    /// switcher, palette, picker, profile, share, upload, drafts, lightbox,
    /// media, previews, viewer-sheet, viewer-csv, viewer-zip, viewer-text,
    /// compact, held-media, shortcuts, delete-file or add-emoji.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "VIEW")]
    demo_view: Option<String>,

    /// Use the light palette in the demo.
    #[cfg(feature = "demo")]
    #[arg(long)]
    demo_light: bool,

    /// Right-click at X,Y (logical points) 2 s in, to show a context menu.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "X,Y")]
    demo_right_click: Option<String>,

    /// Click at X,Y (logical points) 2.5 s in, after any right-click.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "X,Y")]
    demo_click: Option<String>,

    /// Press keys one per frame from 2.5 s in, such as
    /// "Shift+ArrowUp,ArrowUp,R" (egui key names), to capture keyboard
    /// states.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "KEYS")]
    demo_keys: Option<String>,

    /// Scroll the message list up by this many lines, 2.5 s in, as a
    /// reader would.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "LINES")]
    demo_wheel: Option<f32>,

    /// Type TEXT into the focused field from 2.5 s in, a character every
    /// other frame, as a person typing would.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "TEXT")]
    demo_type: Option<String>,

    /// Save every frame from 2.5 s in to DIR (frame-000.png, …) until the
    /// typing is done, then quit: for catching a frame that draws wrong.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "DIR")]
    demo_frames: Option<std::path::PathBuf>,

    /// Wait this long before the demo screenshot (default 1500 ms), for
    /// catching animations at different moments.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "MS", default_value_t = 1500)]
    demo_shot_delay: u64,
}

impl Cli {
    fn demo(&self) -> bool {
        #[cfg(feature = "demo")]
        {
            self.demo
        }
        #[cfg(not(feature = "demo"))]
        {
            false
        }
    }
}

fn main() -> eframe::Result<()> {
    let cli = Cli::parse();
    let demo = cli.demo();
    let dirs = if demo {
        // Under this user's own cache, not a shared temp folder another
        // user could create first.
        AppDirs::under(&AppDirs::discover().cache.join("demo"))
    } else {
        AppDirs::discover()
    };
    // The log file lives in these folders, so they come first; a failure
    // is logged once the logger is up, or it would go nowhere.
    let folders = dirs.ensure();
    let filter = if cli.verbose {
        "noslacking=debug,info".to_owned()
    } else {
        std::env::var("NOSLACKING_LOG").unwrap_or_else(|_| "noslacking=info,warn".to_owned())
    };
    let _ = fastframe_log::Logging::new("noslacking", env!("CARGO_PKG_VERSION"))
        .filter(filter)
        .file(dirs.log_file())
        .panic_log(dirs.panic_log())
        .panic_message(fastframe_log::PanicMessage::Redacted(
            noslacking::redact::panic_message,
        ))
        .redact(noslacking::redact::log_record)
        .init();
    // The logger creates its file with the usual permissions. The folder
    // already keeps others out; this also covers a log an older version
    // left readable.
    for log in [dirs.log_file(), dirs.panic_log()] {
        match noslacking::paths::make_private(&log) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                log::debug!("could not make {} private: {error}", log.display());
            }
            _ => {}
        }
    }
    if let Err(error) = folders {
        log::error!("could not create the app's folders: {error}");
    }
    if cli.release_slack_links {
        release_slack_links(&dirs.state);
        return Ok(());
    }
    #[cfg(feature = "huddle-audio")]
    if let Some([team, channel]) = cli.huddle_probe.as_deref() {
        let code =
            noslacking::huddle_audio::probe::run(&noslacking::huddle_audio::probe::Options {
                team: team.clone(),
                channel: channel.clone(),
                seconds: cli.seconds,
                region: cli.huddle_region.clone(),
                settings: dirs.settings_file(),
            });
        std::process::exit(code);
    }

    let waker = Waker::default();
    let (requests, incoming) = mpsc::channel::<Request>();
    let _instance = if demo {
        None
    } else {
        let request = cli.link.clone().map_or(Request::Show, Request::Open);
        let forward = requests.clone();
        let wake = waker.clone();
        match single_instance::acquire(&dirs.instance_file(), request.clone(), move |request| {
            let _ = forward.send(request);
            wake.wake();
        }) {
            Ok(Outcome::Forwarded) => {
                log::info!("handed over to the running NoSlacking");
                return Ok(());
            }
            Ok(Outcome::Primary(guard)) => {
                if let Request::Open(_) = request {
                    let _ = requests.send(request);
                }
                // Be the handler for noslacking:// links, so an OAuth
                // sign-in comes back here by itself. slack:// stays with
                // the official app until a browser sign-in needs it, so
                // a claim a crash (or an older version) left goes back
                // first. Off the main thread: it runs xdg-mime or reg.exe.
                let state = dirs.state.clone();
                std::thread::spawn(move || {
                    match noslacking::slack_links::release_at_start(&state) {
                        Ok(true) => log::info!("gave back the slack:// links a sign-in left"),
                        Ok(false) => {}
                        Err(error) => {
                            log::warn!("could not give the slack:// links back: {error}");
                        }
                    }
                    if let Err(error) = noslacking::auth::register_scheme() {
                        log::warn!("could not register as the noslacking:// link handler: {error}");
                    }
                });
                Some(guard)
            }
            Err(error) => {
                log::warn!("single instance unavailable: {error}");
                None
            }
        }
    };

    theme::install_emoji(demo);
    let settings = settings::Settings::load(&dirs.settings_file());
    let state = dirs.state.clone();
    #[cfg(feature = "demo")]
    let settings = if demo {
        settings::Settings {
            appearance: if cli.demo_light {
                settings::Appearance::Light
            } else {
                settings::Appearance::Dark
            },
            ..settings::Settings::default()
        }
    } else {
        settings
    };
    let mut app = App::new(&waker, dirs, settings, app::AppOptions { demo });
    // Later launches ask to show the window, with or without one open.
    app.listen_for_launches(incoming);
    let options = native_options(&cli);
    #[cfg(feature = "demo")]
    let demo_setup = DemoSetup::from(&cli);
    let ran = fastframe_shell::Shell::new(app, &waker)
        // On macOS the tray item answers only while AppKit's loop runs.
        .idle(fastframe_tray::idle)
        .start_hidden(cli.hidden)
        .run(|lease| {
            #[cfg(feature = "demo")]
            let demo_setup = demo_setup.clone();
            eframe::run_native(
                "NoSlacking",
                options.clone(),
                Box::new(move |cc| {
                    let mut app = lease.take(&cc.egui_ctx);
                    app.attach(&cc.egui_ctx);
                    Ok(Box::new(Window {
                        app,
                        recovery_checked: false,
                        #[cfg(feature = "demo")]
                        demo: demo_setup,
                    }))
                }),
            )
        });
    // A browser sign-in still waiting when the app quits will not finish:
    // its slack:// links go back. The window is gone by now.
    if !demo {
        match noslacking::slack_links::release(&state) {
            Ok(true) => log::info!("gave the slack:// links back"),
            Ok(false) => {}
            Err(error) => log::warn!("could not give the slack:// links back: {error}"),
        }
    }
    ran
}

/// `--release-slack-links`: gives the links back and says how it went.
#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the answer to a command typed in a terminal"
)]
fn release_slack_links(state: &std::path::Path) {
    match noslacking::slack_links::release_at_start(state) {
        Ok(true) => println!("Gave slack:// links back."),
        Ok(false) => println!("NoSlacking does not hold slack:// links; nothing to give back."),
        Err(error) => eprintln!("Could not give slack:// links back: {error}"),
    }
}

fn native_options(cli: &Cli) -> eframe::NativeOptions {
    #[cfg(feature = "demo")]
    let size = cli
        .demo_size
        .as_deref()
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?]))
        .unwrap_or([1240.0, 800.0]);
    #[cfg(not(feature = "demo"))]
    let size = [1240.0, 800.0];
    let demo = cli.demo();
    let viewport = egui::ViewportBuilder::default()
        .with_title("NoSlacking")
        .with_app_id(std::env::var("FLATPAK_ID").unwrap_or_else(|_| APP_ID.to_owned()))
        .with_inner_size(size)
        .with_min_inner_size([560.0, 380.0])
        .with_icon(app_icon())
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    eframe::NativeOptions {
        viewport,
        persist_window: !demo,
        ..Default::default()
    }
}

fn app_icon() -> egui::IconData {
    let bytes =
        include_bytes!("../packaging/icons/hicolor/256x256/apps/cloud.yannick.NoSlacking.png");
    match image::load_from_memory(bytes) {
        Ok(image) => {
            let rgba = image.to_rgba8();
            egui::IconData {
                width: rgba.width(),
                height: rgba.height(),
                rgba: rgba.into_raw(),
            }
        }
        Err(_) => egui::IconData::default(),
    }
}

/// The eframe adapter around the long-lived [`App`] for one window.
struct Window {
    app: fastframe_shell::Held<App>,
    recovery_checked: bool,
    #[cfg(feature = "demo")]
    demo: DemoSetup,
}

impl eframe::App for Window {
    #[cfg(feature = "demo")]
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if let Some(pos) = self.demo.hover
            && ctx.input(|input| input.pointer.latest_pos()) != Some(pos)
        {
            input.events.push(egui::Event::PointerMoved(pos));
        }
        if let Some(pos) = self.demo.right_click
            && self.demo.started.elapsed() > std::time::Duration::from_millis(2000)
        {
            self.demo.right_click = None;
            input.events.push(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                input.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Secondary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        if let Some(pos) = self.demo.click
            && self.demo.started.elapsed() > std::time::Duration::from_millis(2500)
        {
            self.demo.click = None;
            input.events.push(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                input.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        if self.demo.started.elapsed() > std::time::Duration::from_millis(2500)
            && self.demo.frames.is_multiple_of(2)
            && let Some(c) = self.demo.typing.pop_front()
        {
            input.events.push(egui::Event::Text(c.to_string()));
            self.demo.typed_frames = self.demo.filmed.0;
        }
        if self.demo.started.elapsed() > std::time::Duration::from_millis(2500)
            && !self.demo.keys.is_empty()
        {
            let (key, modifiers) = self.demo.keys.remove(0);
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
        if let Some(lines) = self.demo.wheel
            && self.demo.started.elapsed() > std::time::Duration::from_millis(2500)
        {
            self.demo.wheel = None;
            input.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, lines),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            });
        }
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !std::mem::replace(&mut self.recovery_checked, true) {
            fastframe_shell::window::recover_offscreen(ctx, frame);
        }
        self.app.background_frame(ctx);
        fastframe_macos::align_traffic_lights(frame, ctx, 52.0);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(feature = "demo")]
        self.demo.before_frame(&mut self.app);
        self.app.frame_ui(ui);
        #[cfg(feature = "demo")]
        self.demo.after_frame(ui.ctx(), &mut self.app);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.app.save_state();
    }
}

/// `X,Y` as a point.
#[cfg(feature = "demo")]
fn point(text: Option<&str>) -> Option<egui::Pos2> {
    let (x, y) = text?.split_once(',')?;
    Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// One key of `--demo-keys`: an egui key name after any "Shift+", "Alt+"
/// or "Ctrl+".
#[cfg(feature = "demo")]
fn demo_key(spec: &str) -> Option<(egui::Key, egui::Modifiers)> {
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

/// Screenshot and view options for demo runs.
#[cfg(feature = "demo")]
#[derive(Clone)]
struct DemoSetup {
    shot: Option<std::path::PathBuf>,
    hover: Option<egui::Pos2>,
    view: Option<String>,
    frames: u32,
    asked: bool,
    /// When the screenshot is due.
    due: Option<std::time::Instant>,
    /// Lines to scroll up, once.
    wheel: Option<f32>,
    /// Where to right-click, once.
    right_click: Option<egui::Pos2>,
    /// Where to click, once.
    click: Option<egui::Pos2>,
    /// Characters still to type, one every other frame.
    typing: std::collections::VecDeque<char>,
    /// Where to save every frame, and how many were asked for and saved.
    film: Option<std::path::PathBuf>,
    filmed: (u32, u32),
    /// Whether `--demo-type` gave nothing to type.
    typing_none: bool,
    /// How many frames were asked for when the last character was typed:
    /// filming goes on a little after it, to see the field at rest.
    typed_frames: u32,
    /// Keys still to press, one per frame.
    keys: Vec<(egui::Key, egui::Modifiers)>,
    started: std::time::Instant,
    /// Your message to open for editing once its history has arrived:
    /// asked for earlier, the edit finds nothing to edit.
    edit: Option<noslacking::model::Ts>,
    /// The message whose picture to open in the image viewer once its
    /// history has arrived.
    image: Option<noslacking::model::Ts>,
    /// The message to open the "Share message" dialog on once its history
    /// has arrived: the dialog closes on a message it cannot find.
    share: Option<noslacking::model::Ts>,
    /// Whether the share dialog's comment was typed in yet.
    commented: bool,
    /// Whether to press the deploy bot's Approve button in #deploys once it
    /// has arrived, which asks the app's question first.
    approve: bool,
    /// Whether to open the rollout bot's select in #deploys once it has
    /// arrived, to show its choices.
    open_select: bool,
}

#[cfg(feature = "demo")]
impl DemoSetup {
    fn from(cli: &Cli) -> Self {
        Self {
            due: Some(
                std::time::Instant::now() + std::time::Duration::from_millis(cli.demo_shot_delay),
            ),
            shot: cli.demo_shot.clone(),
            wheel: cli.demo_wheel,
            right_click: point(cli.demo_right_click.as_deref()),
            click: point(cli.demo_click.as_deref()),
            keys: cli
                .demo_keys
                .as_deref()
                .unwrap_or_default()
                .split(',')
                .filter_map(demo_key)
                .collect(),
            started: std::time::Instant::now(),
            typing: cli
                .demo_type
                .as_deref()
                .unwrap_or_default()
                .chars()
                .collect(),
            film: cli.demo_frames.clone(),
            filmed: (0, 0),
            typed_frames: 0,
            typing_none: cli.demo_type.as_deref().is_none_or(str::is_empty),
            hover: cli
                .demo_hover
                .as_deref()
                .and_then(|s| s.split_once(','))
                .and_then(|(x, y)| Some(egui::pos2(x.parse().ok()?, y.parse().ok()?)))
                // A wheel scrolls what is under the pointer: the messages.
                .or(cli.demo_wheel.map(|_| egui::pos2(900.0, 450.0))),
            view: cli.demo_view.clone(),
            frames: 0,
            asked: false,
            edit: None,
            image: None,
            share: None,
            commented: false,
            approve: false,
            open_select: false,
        }
    }

    fn before_frame(&mut self, app: &mut App) {
        use noslacking::model::{Action, Ts};
        self.frames += 1;
        // Let the pretend workspace arrive and #engineering open first.
        if self.frames == 3 {
            app.actions.push(Action::OpenConversation("C02".into()));
        }
        if let Some(ts) = &self.edit
            && app
                .active_workspace()
                .is_some_and(|w| w.find_message("C02", ts).is_some())
        {
            app.actions.push(Action::StartEdit {
                channel: "C02".into(),
                ts: ts.clone(),
            });
            self.edit = None;
        }
        if let Some(ts) = &self.image
            && app
                .active_workspace()
                .is_some_and(|w| w.find_message("C02", ts).is_some())
        {
            app.actions.push(Action::ViewImage {
                channel: "C02".into(),
                thread: None,
                ts: ts.clone(),
                file: "F01".into(),
            });
            self.image = None;
        }
        if let Some(ts) = &self.share
            && app
                .active_workspace()
                .is_some_and(|w| w.find_message("C02", ts).is_some())
        {
            app.actions.push(Action::Share {
                channel: "C02".into(),
                ts: ts.clone(),
                thread: Some(ts.clone()),
            });
            self.share = None;
        }
        if self.approve
            && let Some(workspace) = app.active_workspace()
            && let Some(message) = workspace.find_message(
                "C05",
                &Ts::new(format!("{}.000100", noslacking::demo::APPROVAL)),
            )
        {
            use noslacking::model::{ButtonUse, KitBlock, KitElement, button_use};
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
        // The share dialog, once open, with a comment typed in it.
        if !self.commented
            && let Some(share) = app.share.as_mut()
        {
            share.comment = "Worth a read before Thursday's review".into();
            self.commented = true;
        }
        if self.frames != 6 {
            return;
        }
        let parent = Ts::new(format!("{}.000100", 1_790_172_000 - 3600));
        match self.view.as_deref() {
            Some("thread") => app.actions.push(Action::OpenThread {
                channel: "C02".into(),
                ts: parent,
            }),
            Some("settings") => app.actions.push(Action::ShowSettings),
            // A new custom emoji, its picture picked and a name typed.
            Some("add-emoji") => {
                app.add_emoji = Some(noslacking::custom_emoji::Dialog {
                    team: noslacking::demo::TEAM.into(),
                    name: "shipit".into(),
                    picked: Some(noslacking::custom_emoji::Picked::new(
                        "shipit.gif".into(),
                        noslacking::demo::PARROT_BYTES.to_vec(),
                        0,
                    )),
                    ..Default::default()
                });
            }
            // #engineering's Files tab, asking whether to delete your file.
            Some("delete-file") => {
                app.actions
                    .push(Action::Convos(noslacking::convos::Action::Details {
                        channel: "C02".into(),
                        tab: noslacking::convos::Tab::Files,
                    }));
                app.actions.push(Action::AskDeleteFile {
                    file: "F3".into(),
                    name: "sidebar-v2.png".into(),
                });
            }
            Some("shortcuts") => app.actions.push(Action::ShowShortcuts),
            Some("sign-in") => app.actions.push(Action::AddWorkspace),
            Some("switcher") => app.actions.push(Action::OpenSwitcher),
            // The switcher as the command palette, `>` typed.
            Some("palette") => {
                app.switcher = Some((">".into(), 0));
                app.focus_overlay = true;
            }
            Some("picker") => app.actions.push(Action::PickReaction {
                channel: "C02".into(),
                ts: parent,
            }),
            Some("profile") => app.actions.push(Action::OpenProfile("U01".into())),
            // The "Share message" dialog, on the thread's first message.
            Some("share") => self.share = Some(parent),
            // Your own status, being set.
            Some("status") => app
                .actions
                .push(Action::People(noslacking::people::Action::EditStatus)),
            // Your own message in #engineering, opened for editing.
            Some("edit") => self.edit = Some(Ts::new(format!("{}.000100", 1_790_172_000 - 2000))),
            // The image viewer, on the first of #engineering's pictures.
            Some("lightbox") => {
                self.image = Some(Ts::new(format!("{}.000100", 1_790_172_000 - 2400)));
            }
            Some("dm") => app.actions.push(Action::OpenConversation("D01".into())),
            // A message with every kind of formatting sent to the DM, as
            // its rich text comes back from the pretend Slack.
            Some("rich-send") => {
                app.actions.push(Action::OpenConversation("D01".into()));
                let key = noslacking::app::App::draft_key(noslacking::demo::TEAM, "D01", None);
                app.drafts.insert(
                    key,
                    noslacking::app::Draft {
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
            // #design, with a huddle going on.
            Some("huddle") => app.actions.push(Action::OpenConversation("C03".into())),
            Some("deploys") => app.actions.push(Action::OpenConversation("C05".into())),
            // The deploy bot's Approve button pressed: its question.
            Some("approve") => {
                app.actions.push(Action::OpenConversation("C05".into()));
                self.approve = true;
            }
            // The rollout bot's select in #deploys, open on its choices.
            Some("menus") => {
                app.actions.push(Action::OpenConversation("C05".into()));
                self.open_select = true;
            }
            // #deploys in the workspace signed in by OAuth, where app
            // buttons and menus only work in Slack.
            Some("deploys-oauth") => {
                app.actions.push(Action::SelectWorkspace("TDEMO2".into()));
                app.actions.push(Action::OpenConversation("C05".into()));
            }
            Some("general") => app.actions.push(Action::OpenConversation("C01".into())),
            // #engineering in IRC-style rows.
            Some("compact") => app.settings.density = settings::Density::Compact,
            // The media of #design held back until clicked.
            Some("held-media") => {
                app.settings.inline_media = false;
                app.actions.push(Action::OpenConversation("C03".into()));
            }
            // Link previews, a video, a sound and a PDF.
            Some("media") => app.actions.push(Action::OpenConversation("C03".into())),
            // #random's files: Slack's previews of a snippet, a text file,
            // a PDF, a spreadsheet, a voice clip and a video.
            // The voice clip plays, to show the card mid-play.
            Some("previews") => {
                app.actions.push(Action::OpenConversation("C04".into()));
                if let Some(track) = noslacking::demo::voice_clip()
                    .as_ref()
                    .and_then(noslacking::audio::Track::of)
                {
                    app.actions
                        .push(Action::Audio(noslacking::audio::Request::Toggle(track)));
                }
            }
            // The file viewer over #random, on one of its files.
            Some(view @ ("viewer-sheet" | "viewer-csv" | "viewer-zip" | "viewer-text")) => {
                use noslacking::viewer::Kind;
                let (id, name, filetype, kind, size) = match view {
                    "viewer-sheet" => ("F23", "Q4 budget.xlsx", "xlsx", Kind::Sheet, 75_813),
                    "viewer-csv" => ("F26", "deploys.csv", "csv", Kind::Csv, 7_412),
                    "viewer-zip" => ("F27", "logs.zip", "zip", Kind::Zip, 98_220),
                    _ => ("F20", "backoff.rs", "rust", Kind::Text, 742),
                };
                app.actions.push(Action::OpenConversation("C04".into()));
                app.actions.push(Action::ViewFile {
                    url: format!("https://files.slack.com/files-pri/TDEMO-{id}/{name}"),
                    name: name.into(),
                    filetype: filetype.into(),
                    kind,
                    size,
                });
            }
            // Search results, with a second page to scroll to.
            Some("search") => {
                app.search.text = "standup in:#general".into();
                app.actions.push(Action::OpenSearch);
                app.actions.push(Action::RunSearch);
            }
            // An old message of #general, shown in context and lit up.
            Some("jump") => app.actions.push(Action::JumpTo {
                channel: "C01".into(),
                ts: noslacking::demo::long_history_ts(20),
                thread: None,
            }),
            // A reply: its thread opens beside the conversation.
            Some("jump-reply") => app.actions.push(Action::JumpTo {
                channel: "C02".into(),
                ts: Ts::new(format!("{}.000100", 1_790_172_000 - 2500)),
                thread: Some(parent),
            }),
            // Half-written messages elsewhere, for the sidebar's pencils.
            Some("drafts") => {
                for channel in ["C03", "D01"] {
                    app.drafts.insert(
                        format!("{}/{channel}", noslacking::demo::TEAM),
                        noslacking::app::Draft {
                            text: "Half a thought…".into(),
                            ..Default::default()
                        },
                    );
                }
            }
            // A file on its way, with its progress and Cancel.
            // A file waiting in the composer, not sent yet.
            Some("attach") => app.actions.push(Action::Upload {
                thread: None,
                path: "release-notes.pdf".into(),
                comment: String::new(),
            }),
            Some("upload") => {
                app.actions.push(Action::Upload {
                    thread: None,
                    path: "release-notes.pdf".into(),
                    comment: String::new(),
                });
                // Files wait in the composer until the message is sent.
                app.actions.push(Action::Send {
                    text: String::new(),
                    thread: None,
                    broadcast: false,
                });
            }
            // Picking people for a group message, one already picked.
            Some(view @ ("details" | "members" | "files" | "pins" | "bookmarks")) => {
                use noslacking::convos::{Action as Convos, Tab};
                let tab = match view {
                    "members" => Tab::Members,
                    "files" => Tab::Files,
                    "pins" => Tab::Pins,
                    "bookmarks" => Tab::Bookmarks,
                    _ => Tab::About,
                };
                app.actions.push(Action::Convos(Convos::Details {
                    channel: "C02".into(),
                    tab,
                }));
            }
            // The views at the top of the sidebar.
            Some(view @ ("activity" | "unreads" | "threads" | "later" | "scheduled")) => {
                use noslacking::views::{Action as Views, View};
                let view = match view {
                    "unreads" => View::Unreads,
                    "threads" => View::Threads,
                    "later" => View::Later,
                    "scheduled" => View::Scheduled,
                    _ => View::Activity,
                };
                app.actions.push(Action::Views(Views::Open(view)));
            }
            // The bookmarks tab with the "Add a bookmark" dialog over it,
            // half filled in.
            Some("bookmark-add") => {
                use noslacking::convos::{Action as Convos, BookmarkDialog, Tab};
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
            Some("browse") => app
                .actions
                .push(Action::Convos(noslacking::convos::Action::Browse)),
            Some("new-channel") => {
                app.convos.new_channel = Some(noslacking::convos::NewChannel {
                    name: "Release Notes".into(),
                    ..Default::default()
                });
            }
            Some("new-message") => {
                let mut dialog = noslacking::convos::NewMessage::default();
                dialog.pick("U02".into());
                dialog.query = "a".into();
                app.convos.new_message = Some(dialog);
            }
            _ => {}
        }
    }

    /// With `--demo-frames`: asks for a screenshot of every frame from
    /// 2.5 s in and saves each one as it comes, quitting once the typing is
    /// done and every frame asked for is saved.
    fn film(&mut self, ctx: &egui::Context, app: &mut App) {
        let Some(dir) = self.film.clone() else {
            return;
        };
        ctx.request_repaint();
        let (asked, saved) = &mut self.filmed;
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
            let [width, height] = [image.size[0] as u32, image.size[1] as u32];
            let pixels = image
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_srgba_unmultiplied())
                .collect();
            let path = dir.join(format!("frame-{saved:03}.png"));
            if let Some(buffer) = image::RgbaImage::from_raw(width, height, pixels)
                && let Err(error) = buffer.save(&path)
            {
                log::error!("could not write {}: {error}", path.display());
            }
            *saved += 1;
        }
        // With nothing to type, from the first frame: to catch what moves
        // while the window loads.
        let started = self.typed_frames == 0 && self.typing.is_empty() && self.typing_none
            || self.started.elapsed() > std::time::Duration::from_millis(2500);
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

    fn after_frame(&mut self, ctx: &egui::Context, app: &mut App) {
        self.film(ctx, app);
        if self.open_select
            && let Some(workspace) = app.active_workspace()
            && let Some(message) = workspace.find_message(
                "C05",
                &noslacking::model::Ts::new(format!("{}.000100", noslacking::demo::ROLLOUT)),
            )
        {
            use noslacking::model::{Accessory, KitBlock};
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
        let due = self.due.is_none_or(|due| std::time::Instant::now() >= due);
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
        let pixels: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_srgba_unmultiplied())
            .collect();
        match image::RgbaImage::from_raw(width, height, pixels) {
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
}
