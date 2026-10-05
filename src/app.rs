//! The application state: what is signed in, what is on screen, and how
//! events from the worker and actions from the views change it.
//!
//! Views read [`App`] and push [`Action`]s; [`App::frame_ui`] applies them
//! after drawing, so a frame never sees half an update.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fastframe_shell::{Closed, Headless};

use crate::backend::{self, Backend, Command, Event, SignIn, Socket, Source, Waker};
use crate::credentials::AppCredentials;
use crate::i18n::{self, t, tf};
use crate::model::{Action, Conversation, ConversationKind, Timeline, Ts, Workspace};
use crate::mrkdwn;
use crate::paths::AppDirs;
use crate::settings::{Appearance, Settings};
use crate::theme::{self, Catalog, Palette};

mod compose;
mod desktop;
mod events;
mod hooks;
mod popout;
mod wire;
mod workspace;

pub use popout::Popout;
pub use wire::{to_editable, to_wire};
use workspace::first_unread;
pub use workspace::{WorkspaceState, active_in};

/// How long a toast stays.
const TOAST_FOR: Duration = Duration::from_secs(5);
/// Read markers are sent at most this often per conversation.
const MARK_EVERY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Main,
    /// Adding a workspace, or first run.
    SignIn,
    Settings,
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

/// What the emoji picker adds to.
#[derive(Clone, Debug, PartialEq)]
pub enum PickerTarget {
    Reaction { channel: String, ts: Ts },
    Draft(String),
}

/// A message being edited in place.
#[derive(Clone, Debug)]
pub struct Editing {
    pub channel: String,
    pub ts: Ts,
    pub text: String,
    /// The mentions and links in `text`, as for [`Draft::mentions`].
    pub mentions: Vec<(String, String)>,
    /// Whether the field is in the thread panel, which can show the same
    /// message (a thread's parent) as the conversation.
    pub in_thread: bool,
    /// Focus the field when it is next drawn. Only once, so you can click
    /// or tab away from it.
    pub focus: bool,
}

/// A message picked with the keyboard, whose actions its letter keys run.
#[derive(Clone, Debug, PartialEq)]
pub struct Selected {
    pub channel: String,
    pub ts: Ts,
    /// Whether it is picked in the thread panel rather than the
    /// conversation, which can both show a thread's parent.
    pub in_thread: bool,
    /// Bring it into view and give it focus when it is next drawn: the
    /// selection has just moved.
    pub reveal: bool,
}

/// An unsent message.
#[derive(Clone, Default)]
pub struct Draft {
    pub text: String,
    /// Mentions picked from the suggestions: the text inserted and the
    /// markup it stands for (`<@U123>`), as [`to_wire`] reads them.
    pub mentions: Vec<(String, String)>,
    pub broadcast: bool,
    pub selected: usize,
    /// The word (its start, in chars, and text) whose suggestions Esc
    /// closed. They stay closed until the word changes, so Enter sends.
    pub dismissed: Option<(usize, String)>,
    /// Whether suggestions were showing when last drawn, so Esc closes
    /// them and not the thread.
    pub suggesting: bool,
    /// Files waiting in the composer, sent with the message (its text
    /// becomes their comment) rather than the moment they were added.
    pub attachments: Vec<PathBuf>,
}

/// A file on its way to Slack, shown under the composer it was sent from.
#[derive(Clone, Debug, PartialEq)]
pub struct Upload {
    /// The worker's name for it, for progress and cancelling.
    pub id: u64,
    /// The composer's draft key ([`App::draft_key`]).
    pub key: String,
    pub name: String,
    pub sent: u64,
    /// Zero until the worker has opened the file.
    pub total: u64,
    /// Slack is being told to share it ([`Event::UploadFinishing`]), which
    /// can't be taken back.
    pub finishing: bool,
    /// A pasted image's temporary file, removed once the upload ends.
    pasted: Option<PathBuf>,
    /// Whether the worker said it failed, which it does before it ends.
    failed: bool,
}

impl Upload {
    /// Whether Cancel can still stop it. Not during the last step: the
    /// file is posted whatever the button says.
    pub fn can_cancel(&self) -> bool {
        !self.finishing
    }

    /// How much of it is done, for its progress bar: all of it once only
    /// the sharing is left.
    pub fn fraction(&self) -> f32 {
        if self.finishing {
            1.0
        } else if self.total > 0 {
            (self.sent as f32 / self.total as f32).min(1.0)
        } else {
            0.0
        }
    }
}

/// A draft can say anything; its debug form says only how long it is.
impl std::fmt::Debug for Draft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Draft")
            .field("chars", &self.text.chars().count())
            .field("mentions", &self.mentions.len())
            .field("broadcast", &self.broadcast)
            .finish_non_exhaustive()
    }
}

/// The "name this section" dialog.
#[derive(Clone, Debug, Default)]
pub struct SectionDialog {
    /// The section being renamed; `None` makes a new one.
    pub rename: Option<String>,
    /// A conversation to move into the new section.
    pub channel: Option<String>,
    pub name: String,
}

/// The fields of the sign-in page.
#[derive(Clone, Default)]
pub struct SetupForm {
    pub client_id: String,
    pub client_secret: String,
    pub app_token: String,
    pub user_token: String,
    pub show_manual: bool,
    /// Session sign-in: the workspace address and the `d` cookie.
    pub session_workspace: String,
    pub session_cookie: String,
    /// Browser sign-in: the `slack://` link Slack's page hands over.
    pub session_link: String,
    /// Whether the "use your own Slack app" section is expanded.
    pub show_app: bool,
}

/// The form holds secrets as they are typed; only the plain fields print.
impl std::fmt::Debug for SetupForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupForm")
            .field("client_id", &self.client_id)
            .field("session_workspace", &self.session_workspace)
            .field("show_manual", &self.show_manual)
            .field("show_app", &self.show_app)
            .finish_non_exhaustive()
    }
}

/// Where a file goes: the team, the channel and the thread, if any.
type UploadTarget = (String, String, Option<Ts>);

/// How putting a picture on the clipboard went.
type CopyResult = Result<(), String>;

/// A file chosen in the picker, and where it goes.
type PickedFile = (UploadTarget, PathBuf);

pub struct AppOptions {
    pub demo: bool,
}

pub struct App {
    pub dirs: AppDirs,
    pub settings: Settings,
    pub backend: Backend,
    pub waker: Waker,
    pub palette: Palette,
    applied: Option<(Palette, f32)>,
    pub catalog: Catalog,
    pub page: Page,
    pub app_credentials: Option<AppCredentials>,
    pub app_loaded: bool,
    pub keyring_error: Option<crate::failure::Keyring>,
    pub workspaces: Vec<WorkspaceState>,
    pub sign_in: Option<SignIn>,
    pub setup: SetupForm,
    pub socket: Socket,
    pub thread: Option<(String, Ts)>,
    pub drafts: HashMap<String, Draft>,
    pub editing: Option<Editing>,
    pub selected: Option<Selected>,
    pub toasts: Vec<Toast>,
    pub actions: Vec<Action>,
    /// Files being uploaded, oldest first.
    pub transfers: Vec<Upload>,
    next_upload: u64,
    pub switcher: Option<(String, usize)>,
    pub profile: Option<String>,
    pub picker: Option<PickerTarget>,
    pub picker_query: String,
    /// The image viewer, when open.
    pub preview: Option<crate::lightbox::Lightbox>,
    /// A message waiting for "Delete?" to be answered.
    pub confirm_delete: Option<(String, Ts)>,
    pub section_dialog: Option<SectionDialog>,
    /// Whether the keyboard shortcut sheet is open.
    pub shortcuts: bool,
    /// The "Share message" dialog, when open.
    pub share: Option<crate::share::Share>,
    /// The dialogs and panels for starting and finding conversations.
    pub convos: crate::convos::State,
    /// Watching the people on screen (see [`crate::people`]).
    pub people: crate::people::State,
    /// The views at the top of the sidebar and what they list.
    pub views: crate::views::State,
    /// Where the "New" line goes: the read marker when the open
    /// conversation was opened, by `team/channel`.
    pub read_line: Option<(String, Option<Ts>)>,
    pub sidebar_filter: String,
    pub demo: bool,
    /// Older history arrived: the list keeps its place by this much.
    pub prepended: Option<(String, f32)>,
    /// Lists to scroll to the bottom when next drawn, by
    /// [`App::draft_key`]: a reply sent in a thread must not move the
    /// conversation beside it.
    pub scroll_to_bottom: HashSet<String>,
    /// Focus the composer next frame.
    pub focus_composer: bool,
    /// Focus the thread's reply field next frame: a thread just opened.
    pub focus_thread_composer: bool,
    /// Conversations open in windows of their own.
    pub popouts: Vec<Popout>,
    /// Focus the field of the dialog or picker just opened, once: asking
    /// every frame would keep Tab from reaching its buttons.
    pub focus_overlay: bool,
    /// The search window and its results.
    pub search: crate::search::Search,
    /// Messages being brought into view, at most one per list.
    pub jumps: Vec<crate::jump::Jump>,
    local_counter: u64,
    uploads: (mpsc::Sender<PickedFile>, mpsc::Receiver<PickedFile>),
    /// A picture on its way to the clipboard: the image loader URIs still
    /// to try, best first, and the answers of the threads that copy.
    copying: Vec<String>,
    copied: (mpsc::Sender<CopyResult>, mpsc::Receiver<CopyResult>),
    marks: HashMap<(String, String), (Ts, Instant)>,
    pending_marks: HashMap<(String, String), Ts>,
    /// Drafts sent as a slash command or as an upload's comment, with
    /// their composer's key, kept until it is done: by command (several
    /// can run), and by upload. A failure puts the text back.
    slashing: Vec<(String, String, Draft)>,
    uploading: HashMap<u64, (String, Draft)>,
    window_focused: bool,
    /// When changed settings are next written, and the thread that writes them.
    settings_due: crate::settings::Debounce,
    saver: crate::settings::Saver,
    /// Drafts are kept across restarts (not in the demo): when they are
    /// next written, what they looked like when last checked, and the
    /// thread that writes them.
    keep_drafts: bool,
    drafts_due: crate::settings::Debounce,
    drafts_seen: u64,
    drafts_writer: crate::drafts::Writer,
    quit: bool,
    /// Shows desktop notifications; `None` in the demo or without them.
    notifier: Option<crate::notify::Notifier>,
    /// The desktop's side of the window: requests for it, and what its
    /// title and badge last showed.
    desktop: desktop::Desktop,
}

impl App {
    pub fn new(waker: &Waker, dirs: AppDirs, settings: Settings, options: AppOptions) -> Self {
        let locale = settings.language.unwrap_or_else(i18n::Locale::detect);
        i18n::set_locale(locale);
        let source = if options.demo {
            #[cfg(feature = "demo")]
            {
                Source::Demo
            }
            #[cfg(not(feature = "demo"))]
            {
                Source::Slack {
                    dirs: dirs.clone(),
                    workspaces: Vec::new(),
                    credentials_in_memory: true,
                }
            }
        } else {
            Source::Slack {
                dirs: dirs.clone(),
                workspaces: settings.workspaces.clone(),
                credentials_in_memory: false,
            }
        };
        // Before the worker's first request, so nothing goes around the
        // chosen proxy.
        if let Err(error) = crate::slack::net::configure(&settings.proxy) {
            log::warn!("ignoring the saved proxy setting: {error}");
        }
        crate::spell::configure(&settings.spelling, &dirs.config);
        let backend = backend::spawn(waker, source, dirs.images());
        let mut catalog = Catalog::default();
        if !options.demo {
            theme::enable_desktop_themes(&mut catalog);
        }
        let palette = match (&settings.appearance, &settings.cached_theme) {
            (Appearance::Custom(name), Some(cached)) if cached.filename == *name => cached.palette,
            (Appearance::Light, _) => Palette::light(),
            _ => Palette::dark(),
        };
        let mut workspaces = Vec::new();
        for meta in &settings.workspaces {
            let mut state = WorkspaceState::new(Workspace {
                team_id: meta.team_id.clone(),
                name: meta.name.clone(),
                domain: meta.domain.clone(),
                icon: meta.icon.clone(),
                user_id: meta.user_id.clone(),
            });
            state.active = settings.last_conversation.get(&meta.team_id).cloned();
            state.desktop = settings.desktop.team_state(&meta.team_id);
            workspaces.push(state);
        }
        let page = if workspaces.is_empty() && !options.demo {
            Page::SignIn
        } else {
            Page::Main
        };
        if !options.demo {
            // Pasted images left by a run that ended mid-upload.
            let _ = std::fs::remove_dir_all(dirs.pasted());
        }
        let drafts: HashMap<String, Draft> = if options.demo {
            HashMap::new()
        } else {
            crate::drafts::load(&dirs.drafts_file())
                .into_iter()
                .map(|(key, saved)| {
                    let draft = Draft {
                        text: saved.text,
                        mentions: saved.mentions,
                        broadcast: saved.broadcast,
                        ..Draft::default()
                    };
                    (key, draft)
                })
                .collect()
        };
        let drafts_seen = crate::drafts::fingerprint(draft_views(&drafts));
        let mut app = Self {
            dirs,
            settings,
            backend,
            waker: waker.clone(),
            palette,
            applied: None,
            catalog,
            page,
            app_credentials: None,
            app_loaded: false,
            keyring_error: None,
            workspaces: if options.demo { Vec::new() } else { workspaces },
            sign_in: None,
            setup: SetupForm::default(),
            socket: Socket::Off,
            thread: None,
            drafts,
            editing: None,
            selected: None,
            toasts: Vec::new(),
            actions: Vec::new(),
            transfers: Vec::new(),
            next_upload: 0,
            switcher: None,
            profile: None,
            picker: None,
            picker_query: String::new(),
            preview: None,
            confirm_delete: None,
            section_dialog: None,
            shortcuts: false,
            share: None,
            convos: crate::convos::State::default(),
            people: crate::people::State::default(),
            views: crate::views::State::default(),
            read_line: None,
            sidebar_filter: String::new(),
            demo: options.demo,
            prepended: None,
            scroll_to_bottom: HashSet::new(),
            focus_composer: true,
            focus_thread_composer: false,
            popouts: Vec::new(),
            focus_overlay: false,
            jumps: Vec::new(),
            search: crate::search::Search::default(),
            local_counter: 0,
            uploads: mpsc::channel(),
            copying: Vec::new(),
            copied: mpsc::channel(),
            marks: HashMap::new(),
            pending_marks: HashMap::new(),
            slashing: Vec::new(),
            uploading: HashMap::new(),
            window_focused: true,
            settings_due: crate::settings::Debounce::default(),
            saver: crate::settings::Saver::new(),
            keep_drafts: !options.demo,
            drafts_due: crate::settings::Debounce::default(),
            drafts_seen,
            drafts_writer: crate::drafts::Writer::new(),
            quit: false,
            notifier: desktop::notifier(waker, options.demo),
            desktop: desktop::Desktop::new(options.demo),
        };
        app.start_theme_scan();
        app.start_tray();
        app.refresh_autostart();
        app
    }

    fn start_theme_scan(&mut self) {
        if self.demo {
            return;
        }
        let selected = match &self.settings.appearance {
            Appearance::Custom(name) => Some(name.clone()),
            _ => None,
        };
        let waker = self.waker.clone();
        self.catalog.start(
            self.dirs.themes(),
            selected,
            &fastframe_theme::Waker::new(move || waker.wake()),
        );
    }

    /// Called once the window's egui context exists.
    pub fn attach(&mut self, ctx: &egui::Context) {
        self.waker.attach(ctx);
        self.window_made();
        theme::install(ctx);
        ctx.add_bytes_loader(std::sync::Arc::new(self.backend.images.clone()));
        #[cfg(feature = "demo")]
        if self.demo {
            ctx.include_bytes(crate::demo::PICTURE, crate::demo::PICTURE_BYTES);
            ctx.include_bytes(crate::demo::PARROT, crate::demo::PARROT_BYTES);
            ctx.add_bytes_loader(std::sync::Arc::new(crate::demo::SlowImages::default()));
        }
        self.applied = None;
    }

    pub fn active_workspace(&self) -> Option<&WorkspaceState> {
        active_in(&self.workspaces, &self.settings)
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut WorkspaceState> {
        let id = self.settings.active_workspace.clone();
        let index = self
            .workspaces
            .iter()
            .position(|w| Some(&w.info.team_id) == id.as_ref())
            .or(if self.workspaces.is_empty() {
                None
            } else {
                Some(0)
            })?;
        self.workspaces.get_mut(index)
    }

    fn workspace_mut(&mut self, team: &str) -> Option<&mut WorkspaceState> {
        self.workspaces.iter_mut().find(|w| w.info.team_id == team)
    }

    pub fn active_team(&self) -> Option<String> {
        self.active_workspace().map(|w| w.info.team_id.clone())
    }

    pub fn active_conversation(&self) -> Option<(&WorkspaceState, &Conversation)> {
        let workspace = self.active_workspace()?;
        let id = workspace.active.as_deref()?;
        Some((workspace, workspace.conversation(id)?))
    }

    /// The key a draft is stored under.
    pub fn draft_key(team: &str, channel: &str, thread: Option<&Ts>) -> String {
        match thread {
            Some(ts) => format!("{team}/{channel}/{}", ts.as_str()),
            None => format!("{team}/{channel}"),
        }
    }

    /// The drafts of the composers on screen: the conversation's and the
    /// open thread's.
    pub fn visible_drafts(&self) -> Vec<String> {
        let Some(workspace) = self.active_workspace() else {
            return Vec::new();
        };
        let team = &workspace.info.team_id;
        let mut keys: Vec<String> = workspace
            .active
            .iter()
            .map(|channel| Self::draft_key(team, channel, None))
            .collect();
        if let Some((channel, ts)) = &self.thread {
            keys.push(Self::draft_key(team, channel, Some(ts)));
        }
        keys
    }

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            log::warn!("{text}");
        }
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(Toast {
            text,
            error,
            until: Instant::now() + TOAST_FOR,
        });
        self.waker.wake_after(TOAST_FOR);
    }

    /// Saves the settings once they hold still, off the interface thread:
    /// a drag changes them every frame.
    fn save_settings(&mut self) {
        self.settings_due.poke(Instant::now());
        self.waker.wake_after(crate::settings::SAVE_AFTER);
    }

    pub fn settings_changed(&mut self) {
        self.save_settings();
    }

    /// Notices drafts that changed since the last frame and writes them
    /// once typing pauses. The composers change drafts in place, so a
    /// fingerprint is how the app hears of it.
    fn watch_drafts(&mut self, now: Instant) {
        if !self.keep_drafts {
            return;
        }
        let seen = crate::drafts::fingerprint(draft_views(&self.drafts));
        if seen != self.drafts_seen {
            self.drafts_seen = seen;
            self.drafts_due.poke(now);
            self.waker.wake_after(crate::settings::SAVE_AFTER);
        }
        if self.drafts_due.take_due(now) {
            let drafts = crate::drafts::snapshot(draft_views(&self.drafts));
            self.drafts_writer.save(drafts, &self.dirs.drafts_file());
        } else if self.drafts_due.pending() {
            self.waker.wake_after(crate::settings::SAVE_AFTER);
        }
    }

    // ---- per-frame work -------------------------------------------------

    /// Everything that must happen whether or not a window is open.
    pub fn background_frame(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.backend.try_recv() {
            let signed_out = matches!(event, Event::SignedOut { reason: None, .. });
            self.handle(event);
            if signed_out {
                // egui holds decoded copies of the workspace's private files.
                ctx.forget_all_images();
            }
        }
        while let Ok((target, path)) = self.uploads.1.try_recv() {
            self.stage(target, path);
        }
        self.copy_image_frame(ctx);
        if self.catalog.poll() {
            self.refresh_custom_theme();
        }
        if self.catalog.needs_reload() {
            self.start_theme_scan();
        }
        self.flush_marks();
        self.desktop_frame();
        let now = Instant::now();
        crate::people::frame(self, now);
        self.toasts.retain(|t| t.until > now);
        self.watch_drafts(now);
        if self.settings_due.take_due(now) {
            self.saver.save(&self.settings, &self.dirs.settings_file());
        } else if self.settings_due.pending() {
            // A newer change pushed the save back past the wake asked for.
            self.waker.wake_after(crate::settings::SAVE_AFTER);
        }
    }

    pub fn frame_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused && !self.window_focused {
            self.mark_active_read();
        }
        // Using the app is what keeps you active in Slack.
        if focused && (!self.window_focused || ctx.input(|i| crate::people::is_activity(&i.events)))
        {
            self.people.saw_input(Instant::now());
        }
        self.window_focused = focused;
        self.desktop_window(&ctx);
        crate::ui::show(self, ui);
        // Applying an action may queue another (editing the last message).
        for _ in 0..4 {
            let actions = std::mem::take(&mut self.actions);
            if actions.is_empty() {
                break;
            }
            for action in actions {
                self.apply(action, &ctx);
            }
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let palette = match &self.settings.appearance {
            Appearance::System => match ctx.system_theme() {
                Some(egui::Theme::Light) => Palette::light(),
                _ => Palette::dark(),
            },
            Appearance::Dark => Palette::dark(),
            Appearance::Light => Palette::light(),
            Appearance::Custom(_) => self.palette,
        };
        self.palette = palette;
        let zoom = self.settings.zoom.clamp(0.6, 2.0);
        if self.applied != Some((palette, zoom)) {
            theme::apply(ctx, &palette);
            ctx.set_zoom_factor(zoom);
            self.applied = Some((palette, zoom));
        }
    }

    fn refresh_custom_theme(&mut self) {
        if let Appearance::Custom(name) = &self.settings.appearance
            && let Some(found) = self.catalog.find(name).cloned()
        {
            if self.settings.cached_theme.as_ref() != Some(&found) {
                self.settings.cached_theme = Some(found.clone());
                self.save_settings();
            }
            self.palette = found.palette;
        }
    }

    pub fn set_appearance(&mut self, appearance: Appearance) {
        self.settings.appearance = appearance;
        if let Appearance::Custom(name) = &self.settings.appearance {
            if let Some(found) = self.catalog.find(name).cloned() {
                self.palette = found.palette;
                self.settings.cached_theme = Some(found);
            }
        } else {
            self.settings.cached_theme = None;
        }
        self.save_settings();
    }

    pub fn set_language(&mut self, locale: i18n::Locale) {
        self.settings.language = Some(locale);
        i18n::set_locale(locale);
        self.save_settings();
    }

    // ---- read state -----------------------------------------------------

    fn is_viewing(&self, team: &str, channel: &str) -> bool {
        self.page == Page::Main
            && self.window_focused
            // A view in place of the conversation hides it.
            && self.views.open.is_none()
            && self.active_team().as_deref() == Some(team)
            && self.active_workspace().and_then(|w| w.active.as_deref()) == Some(channel)
    }

    fn mark_if_viewing(&mut self, team: &str, channel: &str) {
        if self.is_viewing(team, channel) {
            self.mark_seen(team, channel);
        }
    }

    fn mark_active_read(&mut self) {
        if let Some(team) = self.active_team()
            && let Some(channel) = self.active_workspace().and_then(|w| w.active.clone())
        {
            self.mark_seen(&team, &channel);
        }
    }

    /// Reads a conversation because it shows, unless you marked it unread
    /// since you opened it: that stays until you open it anew, as in Slack.
    pub(crate) fn mark_seen(&mut self, team: &str, channel: &str) {
        let held = self
            .workspace_mut(team)
            .is_some_and(|w| w.holds_unread(channel));
        if !held {
            self.mark_read(team, channel);
        }
    }

    /// Clears a conversation's unread state here and tells Slack, at most
    /// every few seconds. Asked for outright (or by opening it), so it
    /// also undoes a "Mark unread".
    pub(crate) fn mark_read(&mut self, team: &str, channel: &str) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        workspace.release_unread(channel);
        let newest = workspace
            .timelines
            .get(channel)
            .and_then(|t| t.newest().cloned());
        let Some(conversation) = workspace.conversation_mut(channel) else {
            return;
        };
        let Some(latest) = newest.or_else(|| conversation.latest.clone()) else {
            return;
        };
        conversation.mentions = 0;
        conversation.unread = 0;
        if conversation
            .last_read
            .as_ref()
            .is_some_and(|read| *read >= latest)
        {
            return;
        }
        conversation.last_read = Some(latest.clone());
        self.pending_marks
            .insert((team.to_owned(), channel.to_owned()), latest);
    }

    /// Makes message `ts` of `channel` in the open workspace and all after
    /// it unread: here at once, in Slack with the next marks sent, and
    /// with the "New" line moved above it when the conversation shows.
    fn mark_unread(&mut self, channel: &str, ts: &Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        let Some(marker) = self
            .workspace_mut(&team)
            .and_then(|w| w.mark_unread(channel, ts))
        else {
            return;
        };
        // Replaces a read mark not sent yet, which would undo this one.
        self.pending_marks
            .insert((team.clone(), channel.to_owned()), marker.clone());
        if self.active_workspace().and_then(|w| w.active.as_deref()) == Some(channel) {
            self.read_line = Some((format!("{team}/{channel}"), Some(marker)));
        }
    }

    fn flush_marks(&mut self) {
        if self.pending_marks.is_empty() || self.demo {
            self.pending_marks.clear();
            return;
        }
        let now = Instant::now();
        let ready: Vec<(String, String)> = self
            .pending_marks
            .keys()
            .filter(|key| {
                self.marks
                    .get(*key)
                    .is_none_or(|(_, at)| now.duration_since(*at) >= MARK_EVERY)
            })
            .cloned()
            .collect();
        for key in ready {
            if let Some(ts) = self.pending_marks.remove(&key) {
                self.backend.send(Command::Mark {
                    team: key.0.clone(),
                    channel: key.1.clone(),
                    ts: ts.clone(),
                });
                self.marks.insert(key, (ts, now));
            }
        }
        if !self.pending_marks.is_empty() {
            self.waker.wake_after(MARK_EVERY);
        }
    }

    // ---- actions from the views ----------------------------------------

    fn ensure_loaded(&mut self, team: &str, channel: &str) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let timeline = workspace.timelines.entry(channel.to_owned()).or_default();
        if !timeline.loaded && !timeline.loading {
            timeline.loading = true;
            self.backend.send(Command::LoadHistory {
                team: team.to_owned(),
                channel: channel.to_owned(),
            });
        }
        self.backend.send(Command::Focus {
            team: team.to_owned(),
            channel: Some(channel.to_owned()),
        });
    }

    /// Hides a direct message from the sidebar until it has something
    /// new, and closes it in Slack too.
    fn close_conversation(&mut self, channel: String) {
        let Some(team) = self.active_team() else {
            return;
        };
        let latest = self
            .workspace_mut(&team)
            .and_then(|w| w.conversation(&channel))
            .and_then(|c| c.latest.clone())
            .map_or_else(|| "0".to_owned(), |ts| ts.0);
        self.settings
            .closed
            .entry(team.clone())
            .or_default()
            .insert(channel.clone(), latest);
        self.save_settings();
        self.backend
            .send(Command::CloseConversation { team, channel });
    }

    pub fn open_conversation(&mut self, channel: &str) {
        let Some(team) = self.active_team() else {
            return;
        };
        // Opening a closed conversation opens it in the sidebar again.
        if let Some(closed) = self.settings.closed.get_mut(&team)
            && closed.remove(channel).is_some()
            && closed.is_empty()
        {
            self.settings.closed.remove(&team);
        }
        if let Some(workspace) = self.workspace_mut(&team) {
            workspace.active = Some(channel.to_owned());
        }
        self.settings
            .last_conversation
            .insert(team.clone(), channel.to_owned());
        self.save_settings();
        self.thread = None;
        self.editing = None;
        self.page = Page::Main;
        self.views.open = None;
        self.scroll_to_bottom
            .insert(Self::draft_key(&team, channel, None));
        // An anchor kept for another conversation's older page.
        self.prepended = None;
        self.focus_composer = true;
        self.remember_read_line(&team, channel);
        self.ensure_loaded(&team, channel);
        self.mark_read(&team, channel);
    }

    fn remember_read_line(&mut self, team: &str, channel: &str) {
        let read = self
            .workspaces
            .iter()
            .find(|w| w.info.team_id == team)
            .and_then(|w| w.conversation(channel))
            .filter(|c| c.has_unread())
            .and_then(|c| c.last_read.clone());
        self.read_line = Some((format!("{team}/{channel}"), read));
    }

    /// Brings a picture asked for with "Copy image" to the clipboard: its
    /// bytes come through the image loader (so a private file is fetched
    /// with the workspace's token, from Slack only), and a thread decodes
    /// and offers them.
    fn copy_image_frame(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.copied.1.try_recv() {
            match result {
                Ok(()) => self.toast(t("Image copied").into_owned(), false),
                Err(error) => self.toast(
                    tf("Could not copy the image: {error}", &[("error", &error)]),
                    true,
                ),
            }
        }
        let Some(uri) = self.copying.first().cloned() else {
            return;
        };
        match ctx.try_load_bytes(&uri) {
            Ok(egui::load::BytesPoll::Ready { bytes, .. }) => {
                self.copying.clear();
                let bytes = bytes.to_vec();
                let answer = self.copied.0.clone();
                let waker = self.waker.clone();
                std::thread::spawn(move || {
                    let pixels = match crate::paste::clipboard_pixels(&bytes) {
                        Ok(pixels) => pixels,
                        Err(error) => {
                            let _ = answer.send(Err(error));
                            waker.wake();
                            return;
                        }
                    };
                    // Said before copying: on Linux the copy waits until
                    // something else takes the clipboard.
                    let _ = answer.send(Ok(()));
                    waker.wake();
                    if let Err(error) = crate::paste::copy_image(pixels) {
                        log::warn!("could not copy the image: {error}");
                    }
                });
            }
            Ok(egui::load::BytesPoll::Pending { .. }) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(error) => {
                // Too large or gone: the thumbnail on screen is next.
                self.copying.remove(0);
                if self.copying.is_empty() {
                    let error = error.to_string();
                    self.toast(
                        tf("Could not copy the image: {error}", &[("error", &error)]),
                        true,
                    );
                }
            }
        }
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            // Where you are.
            Action::SelectWorkspace(team) => self.select_workspace(team),
            Action::PopOut(channel) => self.pop_out(channel),
            Action::CloseConversation(channel) => self.close_conversation(channel),
            Action::OpenConversation(channel) => self.open_conversation(&channel),
            Action::OpenThread { channel, ts } => self.open_thread(channel, ts),
            Action::CloseThread => self.thread = None,
            Action::LoadOlder => self.load_older(),
            Action::LoadNewer => self.load_newer(),
            Action::JumpToNewest => {
                if let Some(team) = self.active_team()
                    && let Some(channel) = self.active_workspace().and_then(|w| w.active.clone())
                {
                    self.show_newest(&team, &channel);
                }
            }
            Action::JumpToUnread => self.jump_to_unread(),
            Action::JumpTo {
                channel,
                ts,
                thread,
            } => {
                if let Some(team) = self.active_team() {
                    self.jump_to(&team, &channel, ts, thread);
                }
            }
            Action::ShowSettings => self.page = Page::Settings,
            Action::ShowShortcuts => self.shortcuts = true,
            Action::HideSettings => {
                self.page = if self.workspaces.is_empty() {
                    Page::SignIn
                } else {
                    Page::Main
                };
            }
            // Messages.
            Action::Send {
                text,
                thread,
                broadcast,
            } => self.send(text, thread, broadcast),
            Action::Retry { channel, local } => self.retry(&channel, &local),
            Action::Edit { channel, ts, text } => self.edit(channel, ts, text),
            Action::Delete { channel, ts } => self.delete(channel, ts),
            Action::React { channel, ts, name } => self.react(&channel, &ts, &name),
            Action::StartEdit { channel, ts } => self.start_edit(channel, ts, false),
            Action::StartEditInThread { channel, ts } => self.start_edit(channel, ts, true),
            Action::CancelEdit => self.editing = None,
            Action::EditLast => {
                if let Some((channel, ts)) = self.active_workspace().and_then(|w| w.last_editable())
                {
                    self.actions.push(Action::StartEdit { channel, ts });
                }
            }
            Action::Upload {
                thread,
                path,
                comment,
            } => self.upload(thread, path, comment),
            Action::PickUpload { thread } => self.pick_upload(thread),
            Action::Unstage(path) => self.unstage(&path),
            Action::PasteImage { thread } => self.paste_image(thread),
            Action::CopyImage(uris) => self.copying = uris,
            // The row stays until the worker answers: it may find the
            // upload already being shared, and then it is not cancelled.
            Action::CancelUpload(id) => {
                if self.transfers.iter().any(|u| u.id == id && u.can_cancel()) {
                    self.backend.send(Command::CancelUpload { id });
                }
            }
            Action::Download { url, name } => {
                if let Some(team) = self.active_team() {
                    self.backend.send(Command::Download { team, url, name });
                }
            }
            Action::OpenFile { url, name } => {
                if let Some(team) = self.active_team() {
                    // Fetching a video can take a while; say it started.
                    self.toast(tf("Opening {name}…", &[("name", &name)]), false);
                    self.backend.send(Command::OpenFile { team, url, name });
                }
            }
            Action::Sidebar(edit) => self.edit_sidebar(edit),
            // What floats over the window.
            Action::PickReaction { channel, ts } => {
                self.open_picker(PickerTarget::Reaction { channel, ts });
            }
            Action::PickEmoji { draft } => self.open_picker(PickerTarget::Draft(draft)),
            Action::MarkUnread { channel, ts } => self.mark_unread(&channel, &ts),
            Action::AskDelete { channel, ts } => self.confirm_delete = Some((channel, ts)),
            Action::NameSection { rename, channel } => self.name_section(rename, channel),
            Action::Preview { uri, name } => {
                let picture = crate::lightbox::Picture {
                    uri,
                    thumb: None,
                    size: None,
                    name,
                    download: None,
                    permalink: None,
                    source: None,
                };
                self.preview = crate::lightbox::Lightbox::new(vec![picture], 0);
            }
            Action::ViewImage {
                channel,
                thread,
                ts,
                file,
            } => self.view_image(&channel, thread.as_ref(), &ts, &file),
            Action::OpenSwitcher => {
                self.focus_overlay = true;
                self.switcher = Some((String::new(), 0));
            }
            Action::OpenProfile(user) => self.profile = Some(user),
            Action::OpenSearch => self.open_search(),
            Action::RunSearch => {
                let started = self.active_team().and_then(|team| self.search.start(&team));
                if let Some((query, request)) = started {
                    self.backend.send(Command::Search {
                        query,
                        page: 1,
                        request,
                    });
                }
            }
            Action::SearchMore => {
                if let Some((query, page, request)) = self.search.more() {
                    self.backend.send(Command::Search {
                        query,
                        page,
                        request,
                    });
                }
            }
            Action::DismissError => self.toasts.clear(),
            // Leaving the app: links, folders and the clipboard.
            Action::OpenUrl(url) => self.open_url(&url),
            Action::NotifyLevel { channel, level } => self.set_notify_level(&channel, level),
            Action::Snooze(choice) => self.snooze(choice),
            Action::Mute { channel, muted } => self.mute(&channel, muted),
            Action::OpenFolder(path) => {
                if let Err(error) = open::that_detached(&path) {
                    let error = error.to_string();
                    self.toast(
                        tf("Could not open the folder: {error}", &[("error", &error)]),
                        true,
                    );
                }
            }
            Action::CopyLink {
                channel,
                ts,
                thread,
            } => self.copy_link(ctx, &channel, &ts, thread.as_ref()),
            Action::Share {
                channel,
                ts,
                thread,
            } => {
                self.focus_overlay = true;
                self.share = Some(crate::share::Share::new(channel, ts, thread));
            }
            Action::ShareTo {
                channel,
                ts,
                thread,
                to,
                comment,
            } => self.share_to(&channel, &ts, thread.as_ref(), to, &comment),
            Action::Copy(text) => {
                ctx.copy_text(text);
                self.toast(t("Copied").into_owned(), false);
            }
            // Accounts and sign-in.
            Action::AddWorkspace => {
                self.sign_in = None;
                self.page = Page::SignIn;
            }
            Action::SignOut(team) => self.backend.send(Command::SignOut(team)),
            Action::Reconnect => self.backend.send(Command::Reconnect),
            Action::ApplySpelling => {
                crate::spell::configure(&self.settings.spelling, &self.dirs.config);
            }
            Action::ApplyProxy => self
                .backend
                .send(Command::SetProxy(self.settings.proxy.clone())),
            Action::SignInSession => {
                self.sign_in = None;
                self.backend.send(Command::SignInSession {
                    cookie: self.setup.session_cookie.trim().to_owned(),
                    workspace_url: self.setup.session_workspace.trim().to_owned(),
                });
            }
            Action::StartBrowserSignIn => {
                self.sign_in = None;
                self.backend.send(Command::StartBrowserSignIn);
            }
            Action::SignInLink => {
                self.sign_in = None;
                // The link is a one-time secret: read it once, then forget it.
                let link = std::mem::take(&mut self.setup.session_link);
                self.backend
                    .send(Command::SignInLink(link.trim().to_owned()));
            }
            Action::PasteToken => {
                let token = self.setup.user_token.trim().to_owned();
                self.backend.send(Command::PasteToken(token));
            }
            Action::SaveApp => self.save_app(),
            Action::StartSignIn => {
                self.sign_in = None;
                self.backend.send(Command::StartSignIn {
                    redirect: self.settings.redirect,
                    port: self.settings.loopback_port,
                });
            }
            Action::CancelSignIn => {
                self.backend.send(Command::CancelSignIn);
                self.sign_in = None;
            }
            Action::Convos(action) => crate::convos::apply(self, action),
            Action::People(action) => crate::people::apply(self, action),
            Action::Views(action) => crate::views::apply(self, action),
        }
    }

    fn select_workspace(&mut self, team: String) {
        self.settings.active_workspace = Some(team.clone());
        self.save_settings();
        self.thread = None;
        self.page = Page::Main;
        self.views.open = None;
        self.prepended = None;
        if let Some(channel) = self.workspace_mut(&team).and_then(|w| w.active.clone()) {
            self.scroll_to_bottom
                .insert(Self::draft_key(&team, &channel, None));
            self.remember_read_line(&team, &channel);
            self.ensure_loaded(&team, &channel);
            self.mark_read(&team, &channel);
        } else {
            self.backend.send(Command::Focus {
                team,
                channel: None,
            });
        }
    }

    fn open_thread(&mut self, channel: String, ts: Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        self.thread = Some((channel.clone(), ts.clone()));
        self.focus_thread_composer = true;
        if let Some(workspace) = self.workspace_mut(&team) {
            workspace
                .threads
                .entry((channel.clone(), ts.clone()))
                .or_default()
                .loading = true;
        }
        self.backend.send(Command::LoadThread { team, channel, ts });
    }

    fn load_older(&mut self) {
        let Some(team) = self.active_team() else {
            return;
        };
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let Some(channel) = workspace.active.clone() else {
            return;
        };
        if let Some(timeline) = workspace.timelines.get_mut(&channel)
            && timeline.has_more
            && !timeline.loading
            && let Some(cursor) = timeline.cursor.clone()
        {
            timeline.loading = true;
            self.backend.send(Command::LoadOlder {
                team,
                channel,
                cursor,
            });
        }
    }

    /// Brings the newest messages of a conversation into view. A list of
    /// older history is dropped for the newest page, read afresh.
    pub fn show_newest(&mut self, team: &str, channel: &str) {
        let list = Self::draft_key(team, channel, None);
        self.jumps.retain(|j| j.list != list);
        self.scroll_to_bottom.insert(list);
        let detached = self
            .workspace_mut(team)
            .and_then(|w| w.timelines.get_mut(channel))
            .filter(|t| t.has_newer);
        if let Some(timeline) = detached {
            timeline.messages.retain(|m| m.ts.is_local());
            *timeline = Timeline {
                messages: std::mem::take(&mut timeline.messages),
                ..Timeline::default()
            };
            self.prepended = None;
            self.ensure_loaded(team, channel);
        }
    }

    /// Opens the search window, as it was left: results for another
    /// workspace than the one on screen are dropped.
    fn open_search(&mut self) {
        let team = self.active_team();
        if self
            .search
            .query
            .as_ref()
            .is_some_and(|q| Some(&q.team) != team.as_ref())
        {
            self.search = crate::search::Search {
                text: std::mem::take(&mut self.search.text),
                scope: self.search.scope,
                sort: self.search.sort,
                ..crate::search::Search::default()
            };
        }
        self.search.open = true;
        self.search.focus = true;
    }

    /// Brings the open conversation's "New" line into view: the first
    /// message after the one you had read when it opened, loading the
    /// history around it when it is further back than the list reaches.
    fn jump_to_unread(&mut self) {
        let Some(team) = self.active_team() else {
            return;
        };
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        let Some(channel) = workspace.active.clone() else {
            return;
        };
        let list = Self::draft_key(&team, &channel, None);
        let Some(read) = self
            .read_line
            .as_ref()
            .filter(|(key, _)| *key == list)
            .and_then(|(_, ts)| ts.clone())
        else {
            return;
        };
        let timeline = workspace.timelines.get(&channel);
        let reaches = timeline
            .is_some_and(|t| !t.has_more || t.messages.first().is_some_and(|m| m.ts <= read));
        let first = timeline.and_then(|t| first_unread(t, &read, &workspace.info.user_id));
        match first {
            Some(ts) if reaches => {
                self.jumps.retain(|j| j.list != list);
                self.scroll_to_bottom.remove(&list);
                self.jumps.push(crate::jump::Jump::new(list, ts, false));
            }
            // Further back than the list goes: the messages around the
            // last one read, with the line just after it.
            _ => {
                self.jump_to(&team, &channel, read, None);
                if let Some(jump) = self.jumps.iter_mut().find(|j| j.list == list) {
                    jump.highlight = false;
                }
            }
        }
    }

    /// Asks for the page after the newest message of the open list, when
    /// it holds older history.
    fn load_newer(&mut self) {
        let Some(team) = self.active_team() else {
            return;
        };
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let Some(channel) = workspace.active.clone() else {
            return;
        };
        if let Some(timeline) = workspace.timelines.get_mut(&channel)
            && timeline.has_newer
            && !timeline.loading
            && let Some(after) = timeline.newest().cloned()
        {
            timeline.loading = true;
            self.backend.send(Command::LoadNewer {
                team,
                channel,
                after,
            });
        }
    }

    /// Shows message `ts` of `channel` in `team` with the messages around
    /// it, and lights it up. A reply (`thread` names its parent) shows its
    /// parent in the conversation and itself in the thread beside it.
    pub fn jump_to(&mut self, team: &str, channel: &str, ts: Ts, thread: Option<Ts>) {
        if self.workspace_mut(team).is_none() {
            return;
        }
        if self.active_team().as_deref() != Some(team) {
            self.select_workspace(team.to_owned());
        }
        let reply = thread.filter(|parent| *parent != ts);
        // What the conversation's own list shows: the message, or for a
        // reply its parent.
        let anchor = reply.clone().unwrap_or_else(|| ts.clone());
        let opening = self.active_workspace().and_then(|w| w.active.as_deref()) != Some(channel);
        if opening {
            if let Some(workspace) = self.workspace_mut(team) {
                workspace.active = Some(channel.to_owned());
            }
            self.settings
                .last_conversation
                .insert(team.to_owned(), channel.to_owned());
            self.save_settings();
            self.thread = None;
            self.editing = None;
            self.prepended = None;
            self.remember_read_line(team, channel);
        }
        self.page = Page::Main;
        self.views.open = None;
        let list = Self::draft_key(team, channel, None);
        // A jump replaces any other in the same list, and the end of the
        // list no longer pulls the view down to it.
        self.scroll_to_bottom.remove(&list);
        self.jumps.retain(|j| j.list != list);
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let timeline = workspace.timelines.entry(channel.to_owned()).or_default();
        let loaded = timeline.loaded && timeline.messages.iter().any(|m| m.ts == anchor);
        if !loaded {
            // What is there stays until the stretch around this message
            // replaces it; meanwhile no newest page is asked for.
            timeline.loading = true;
            timeline.around = Some(anchor.clone());
            self.backend.send(Command::LoadAround {
                team: team.to_owned(),
                channel: channel.to_owned(),
                ts: anchor.clone(),
            });
        }
        self.backend.send(Command::Focus {
            team: team.to_owned(),
            channel: Some(channel.to_owned()),
        });
        self.jumps
            .push(crate::jump::Jump::new(list, anchor, reply.is_none()));
        if let Some(parent) = reply {
            let thread_list = Self::draft_key(team, channel, Some(&parent));
            self.jumps.retain(|j| j.list != thread_list);
            self.jumps
                .push(crate::jump::Jump::new(thread_list, ts, true));
            self.open_thread(channel.to_owned(), parent);
        }
        if opening {
            self.mark_read(team, channel);
        }
    }

    fn open_picker(&mut self, target: PickerTarget) {
        self.focus_overlay = true;
        self.picker_query.clear();
        self.picker = Some(target);
    }

    fn name_section(&mut self, rename: Option<String>, channel: Option<String>) {
        let name = rename
            .as_deref()
            .and_then(|id| {
                self.active_workspace()?
                    .sections
                    .as_ref()?
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.name.clone())
            })
            .unwrap_or_default();
        self.focus_overlay = true;
        self.section_dialog = Some(SectionDialog {
            rename,
            channel,
            name,
        });
    }

    fn open_url(&mut self, url: &str) {
        if crate::links::parse_web(url).is_some_and(|link| self.follow(&link)) {
            // A link into a signed-in workspace opens here.
        } else if !mrkdwn::is_openable(url) {
            // Attachments and blocks carry URLs a bot chose.
            self.toast(t("Only web and mail links can be opened"), true);
        } else if let Err(error) = open::that_detached(url) {
            let error = error.to_string();
            self.toast(
                tf("Could not open the link: {error}", &[("error", &error)]),
                true,
            );
        }
    }

    /// Opens what a link into Slack names, in the workspace it is for.
    /// Returns whether that workspace is signed in here and has it.
    fn follow(&mut self, link: &crate::links::Link) -> bool {
        use crate::links::Target;
        let Some(workspace) = self
            .workspaces
            .iter()
            .find(|w| link.is_for(&w.info.team_id, &w.info.domain))
        else {
            return false;
        };
        let team = workspace.info.team_id.clone();
        let known = |channel: &str| workspace.conversation(channel).is_some();
        match &link.target {
            Target::Workspace => {
                self.select_workspace(team);
            }
            Target::Conversation(channel) if known(channel) => {
                if self.active_team().as_deref() != Some(team.as_str()) {
                    self.select_workspace(team);
                }
                self.open_conversation(channel);
            }
            Target::Message {
                channel,
                ts,
                thread,
            } if known(channel) => {
                self.jump_to(&team, channel, ts.clone(), thread.clone());
            }
            Target::User(user) => {
                if self.active_team().as_deref() != Some(team.as_str()) {
                    self.select_workspace(team);
                }
                match self.direct_message(user) {
                    Some(channel) => self.open_conversation(&channel),
                    None => self.profile = Some(user.clone()),
                }
            }
            Target::Conversation(_) | Target::Message { .. } => return false,
        }
        true
    }

    /// Copies the permalink of a message of the open workspace.
    fn copy_link(&mut self, ctx: &egui::Context, channel: &str, ts: &Ts, thread: Option<&Ts>) {
        let link = self
            .active_workspace()
            .and_then(|w| crate::links::permalink(&w.info.domain, channel, ts, thread));
        match link {
            Some(link) => {
                ctx.copy_text(link);
                self.toast(t("Link copied"), false);
            }
            None => self.toast(t("This message has no link yet"), true),
        }
    }

    fn save_app(&mut self) {
        let form = AppCredentials {
            client_id: self.setup.client_id.trim().to_owned(),
            client_secret: self.setup.client_secret.trim().to_owned(),
            app_token: self.setup.app_token.trim().to_owned(),
        };
        if form.can_sign_in() {
            self.backend.send(Command::SaveApp(form.clone()));
            self.app_credentials = Some(form);
        }
    }

    fn start_edit(&mut self, channel: String, ts: Ts, in_thread: bool) {
        let found = self
            .active_workspace()
            .and_then(|w| w.find_message(&channel, &ts).map(|m| w.editable(&m.text)));
        if let Some((text, mentions)) = found {
            self.editing = Some(Editing {
                channel,
                ts,
                text,
                mentions,
                in_thread,
                focus: true,
            });
        }
    }

    /// Opens the image viewer on a file, with the other images of the same
    /// list (the thread, or the conversation) to step through.
    fn view_image(&mut self, channel: &str, thread: Option<&Ts>, ts: &Ts, file: &str) {
        let Some(workspace) = self.active_workspace() else {
            return;
        };
        let team = workspace.info.team_id.as_str();
        let lightbox = match thread {
            Some(parent) => workspace
                .threads
                .get(&(channel.to_owned(), parent.clone()))
                .and_then(|t| crate::lightbox::open(team, &t.messages, ts, file)),
            None => workspace.timelines.get(channel).and_then(|t| {
                let listed = t.messages.iter().filter(|m| m.in_channel());
                crate::lightbox::open(team, listed, ts, file)
            }),
        };
        // A message found nowhere else (a parent shown before its thread
        // has loaded) still opens, on its own.
        let lightbox = lightbox.or_else(|| {
            let message = workspace.find_message(channel, ts)?;
            crate::lightbox::open(team, std::iter::once(message), ts, file)
        });
        if lightbox.is_some() {
            self.preview = lightbox;
        }
    }

    /// Whether a dialog or picker covers the window, so its keys come
    /// first.
    pub fn overlay_open(&self) -> bool {
        self.switcher.is_some()
            || self.picker.is_some()
            || self.profile.is_some()
            || self.preview.is_some()
            || self.confirm_delete.is_some()
            || self.section_dialog.is_some()
            || self.shortcuts
            || self.share.is_some()
            || self.search.open
            || self.convos.overlay_open()
            || self.people.status.is_some()
            || self.views.dialog.is_some()
    }

    /// Changes the sidebar at once, and in Slack, which then sends back the
    /// sections as they really are.
    fn edit_sidebar(&mut self, edit: crate::sidebar::SidebarEdit) {
        let Some(team) = self.active_team() else {
            return;
        };
        let demo = self.demo;
        let Some(sections) = self.workspace_mut(&team).and_then(|w| w.sections.as_mut()) else {
            return;
        };
        let calls = crate::sidebar::plan(sections, &edit);
        crate::sidebar::apply(sections, &edit);
        if !calls.is_empty() && !demo {
            self.backend.send(Command::Sidebar { team, calls });
        }
    }

    /// Opens the DM with someone, if there is one.
    pub fn direct_message(&self, user: &str) -> Option<String> {
        self.active_workspace()?
            .conversations
            .iter()
            .find(|c| c.kind == ConversationKind::Direct && c.user.as_deref() == Some(user))
            .map(|c| c.id.clone())
    }

    /// Writes the settings now and waits for the disk, for quitting.
    pub fn save_state(&mut self) {
        self.settings_due.clear();
        self.saver
            .save_now(&self.settings, &self.dirs.settings_file());
        if self.keep_drafts {
            self.drafts_due.clear();
            let drafts = crate::drafts::snapshot(draft_views(&self.drafts));
            self.drafts_writer
                .save_now(drafts, &self.dirs.drafts_file());
        }
    }

    /// The conversations in `team` with a draft, in it or one of its
    /// threads, for the sidebar's pencil.
    pub fn channels_with_drafts(&self, team: &str) -> HashSet<String> {
        let prefix = format!("{team}/");
        self.drafts
            .iter()
            .filter(|(_, draft)| !draft.text.trim().is_empty())
            .filter_map(|(key, _)| key.strip_prefix(&prefix))
            .map(|rest| rest.split('/').next().unwrap_or(rest).to_owned())
            .collect()
    }

    pub fn request_quit(&mut self) {
        self.quit = true;
    }
}

/// The drafts as [`crate::drafts`] reads them.
fn draft_views(drafts: &HashMap<String, Draft>) -> impl Iterator<Item = crate::drafts::View<'_>> {
    drafts.iter().map(|(key, draft)| {
        (
            key.as_str(),
            draft.text.as_str(),
            draft.mentions.as_slice(),
            draft.broadcast,
        )
    })
}

impl fastframe_shell::Resident for App {
    fn closed(&self) -> Closed {
        self.closed_action()
    }

    fn window_gone(&mut self) {
        self.waker.detach();
        self.window_left();
    }

    fn headless_frame(&mut self, ctx: &egui::Context) -> Headless {
        self.background_frame(ctx);
        if self.quit {
            Headless::Quit
        } else if self.wants_window() {
            Headless::Show
        } else {
            Headless::Wait
        }
    }

    fn start_hidden(&mut self) -> bool {
        self.can_start_hidden()
    }

    fn shutdown(&mut self) {
        self.save_state();
    }
}
