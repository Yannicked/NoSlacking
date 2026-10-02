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
    /// switcher, picker, profile, upload or drafts.
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
    if let Err(error) = folders {
        log::error!("could not create the app's folders: {error}");
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
                // Be the handler for slack:// and noslacking:// links, so
                // the browser sign-in comes back here by itself. Off the
                // main thread: it runs xdg-mime or reg.exe.
                std::thread::spawn(|| {
                    if let Err(error) = noslacking::auth::register_scheme() {
                        log::warn!("could not register as the slack:// link handler: {error}");
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
    fastframe_shell::Shell::new(app, &waker)
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
        })
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
        self.demo.after_frame(ui.ctx());
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
    /// Keys still to press, one per frame.
    keys: Vec<(egui::Key, egui::Modifiers)>,
    started: std::time::Instant,
    /// Your message to open for editing once its history has arrived:
    /// asked for earlier, the edit finds nothing to edit.
    edit: Option<noslacking::model::Ts>,
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
            Some("sign-in") => app.actions.push(Action::AddWorkspace),
            Some("switcher") => app.actions.push(Action::OpenSwitcher),
            Some("picker") => app.actions.push(Action::PickReaction {
                channel: "C02".into(),
                ts: parent,
            }),
            Some("profile") => app.actions.push(Action::OpenProfile("U01".into())),
            // Your own message in #engineering, opened for editing.
            Some("edit") => self.edit = Some(Ts::new(format!("{}.000100", 1_790_172_000 - 2000))),
            Some("dm") => app.actions.push(Action::OpenConversation("D01".into())),
            Some("deploys") => app.actions.push(Action::OpenConversation("C05".into())),
            Some("general") => app.actions.push(Action::OpenConversation("C01".into())),
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
            Some("upload") => app.actions.push(Action::Upload {
                thread: None,
                path: "release-notes.pdf".into(),
                comment: String::new(),
            }),
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

    fn after_frame(&mut self, ctx: &egui::Context) {
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
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}
