//! NoSlacking's entry point: command line, logging, single instance, and
//! the window.

mod cli;

use std::sync::mpsc;

use cli::Cli;

use noslacking::app::{self, App};
use noslacking::backend::Waker;
use noslacking::paths::{APP_ID, AppDirs};
use noslacking::single_instance::{self, Outcome, Request};
use noslacking::{settings, theme};

fn main() -> eframe::Result<()> {
    let cli = command_line();
    let demo = cli.demo();
    let dirs = if demo {
        // Under this user's own cache, not a shared temp folder another
        // user could create first.
        AppDirs::under(&AppDirs::discover().cache.join("demo"))
    } else {
        AppDirs::discover()
    };
    start_logging(&cli, &dirs);
    if cli.release_slack_links {
        release_slack_links(&dirs.state);
        return Ok(());
    }
    run_probes(&cli, &dirs);
    // The same video logging in the app's own huddles, when asked.
    if cli.video > 0 || cli.video_h264_only || cli.video_dump.is_some() {
        noslacking::huddle_audio::video::set_for_app(noslacking::huddle_audio::video::Options {
            streams: cli.video,
            h264_only: cli.video_h264_only,
            dump: cli.video_dump.clone(),
        });
    }

    let waker = Waker::default();
    let (requests, incoming) = mpsc::channel::<Request>();
    let _instance = if demo {
        None
    } else {
        let Ok(guard) = claim_instance(&cli, &dirs, &waker, &requests) else {
            return Ok(());
        };
        guard
    };

    theme::install_emoji(demo);
    let settings = settings::Settings::load(&dirs.settings_file());
    let state = dirs.state.clone();
    #[cfg(feature = "demo")]
    let settings = if demo { demo_settings(&cli) } else { settings };
    let mut app = App::new(&waker, dirs, settings, app::AppOptions { demo });
    // Later launches ask to show the window, with or without one open.
    app.listen_for_launches(incoming);
    let options = native_options(&cli);
    #[cfg(feature = "demo")]
    let demo_setup = noslacking::demo::setup::Setup::new(demo_options(&cli));
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

/// Creates the app's folders and starts the log in them, kept private.
fn start_logging(cli: &Cli, dirs: &AppDirs) {
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
}

/// Runs the probe the command line asks for, if any, and quits with its
/// exit code; returns when none was asked for.
fn run_probes(cli: &Cli, dirs: &AppDirs) {
    #[cfg(feature = "teams")]
    if let Some([team, callee]) = &cli.teams_call_probe {
        std::process::exit(noslacking::teams::call_probe::run(
            &noslacking::teams::call_probe::Options {
                team: team.clone(),
                callee: callee.clone(),
                seconds: cli.seconds,
                settings: dirs.settings_file(),
            },
        ));
    }
    #[cfg(feature = "teams")]
    if cli.teams_probe {
        std::process::exit(noslacking::teams::probe::run(&dirs.settings_file()));
    }
    if let Some([team, channel]) = &cli.huddle_probe {
        let code =
            noslacking::huddle_audio::probe::run(&noslacking::huddle_audio::probe::Options {
                team: team.clone(),
                channel: channel.clone(),
                seconds: cli.seconds,
                region: cli.huddle_region.clone(),
                settings: dirs.settings_file(),
                send_tone: cli.send_tone,
                send_test_video: cli.send_test_video,
                send_test_share: cli.send_test_share,
                video: noslacking::huddle_audio::video::Options {
                    streams: cli.video,
                    h264_only: cli.video_h264_only,
                    dump: cli.video_dump.clone(),
                },
            });
        std::process::exit(code);
    }
}

/// This launch handed its link to the NoSlacking already running, and
/// quits.
struct HandedOver;

/// Becomes the one NoSlacking running, or hands this launch's link to the
/// one that is. Running, the claim to hold until the end: none when the
/// system offered no way to make one.
fn claim_instance(
    cli: &Cli,
    dirs: &AppDirs,
    waker: &Waker,
    requests: &mpsc::Sender<Request>,
) -> Result<Option<single_instance::Guard>, HandedOver> {
    let request = cli.link.clone().map_or(Request::Show, Request::Open);
    let forward = requests.clone();
    let wake = waker.clone();
    match single_instance::acquire(&dirs.instance_file(), request.clone(), move |request| {
        let _ = forward.send(request);
        wake.wake();
    }) {
        Ok(Outcome::Forwarded) => {
            log::info!("handed over to the running NoSlacking");
            Err(HandedOver)
        }
        Ok(Outcome::Primary(guard)) => {
            if let Request::Open(_) = request {
                let _ = requests.send(request);
            }
            // Be the handler for noslacking:// links, so an OAuth sign-in
            // comes back here by itself. slack:// stays with the official
            // app until a browser sign-in needs it, so a claim a crash (or
            // an older version) left goes back first. Off the main thread:
            // it runs xdg-mime or reg.exe.
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
            Ok(Some(guard))
        }
        Err(error) => {
            log::warn!("single instance unavailable: {error}");
            Ok(None)
        }
    }
}

/// The demo's settings: the defaults in the palette asked for, never the
/// user's own.
#[cfg(feature = "demo")]
fn demo_settings(cli: &Cli) -> settings::Settings {
    settings::Settings {
        appearance: if cli.demo_light {
            settings::Appearance::Light
        } else {
            settings::Appearance::Dark
        },
        // The demo's helper decodes in software, so screenshots don't
        // depend on a GPU; this tries the GPU against its share and
        // cameras. Without the helper built beside the app (`cargo
        // build`), the call window says there is no video.
        hardware_video: std::env::var_os("NOSLACKING_DEMO_HARDWARE_VIDEO").is_some(),
        ..settings::Settings::default()
    }
}

/// The `--demo-…` flags for the demo's script.
#[cfg(feature = "demo")]
fn demo_options(cli: &Cli) -> noslacking::demo::setup::Options {
    noslacking::demo::setup::Options {
        shot: cli.demo_shot.clone(),
        shot_delay: cli.demo_shot_delay,
        size: cli.demo_size.clone(),
        hover: cli.demo_hover.clone(),
        view: cli.demo_view,
        right_click: cli.demo_right_click.clone(),
        click: cli.demo_click.clone(),
        keys: cli.demo_keys.clone(),
        wheel: cli.demo_wheel,
        typing: cli.demo_type.clone(),
        frames: cli.demo_frames.clone(),
    }
}

/// The command line, or the answer to `--help`, `--version` or a mistake
/// in it, and quit: 0 for an answer, 2 for a mistake, as clap had it.
#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the answer to a command typed in a terminal"
)]
fn command_line() -> Cli {
    match cli::parse(std::env::args_os().skip(1)) {
        Ok(cli::Parsed::Run(cli)) => *cli,
        Ok(cli::Parsed::Help) => {
            print!("{}", cli::help());
            std::process::exit(0);
        }
        Ok(cli::Parsed::Version) => {
            println!("{}", cli::version());
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
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
        .and_then(noslacking::demo::setup::size)
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
    demo: noslacking::demo::setup::Setup,
}

impl eframe::App for Window {
    #[cfg(feature = "demo")]
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        self.demo.raw_input(ctx, input);
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
        #[cfg(all(feature = "demo", feature = "huddle-video"))]
        self.demo.note_frame(_frame.info().cpu_usage, &self.app);
        #[cfg(feature = "demo")]
        self.demo.after_frame(ui.ctx(), &mut self.app);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.app.save_state();
    }
}
