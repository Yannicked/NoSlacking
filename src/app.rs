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
use crate::emoji::EmojiSet;
use crate::i18n::{self, t};
use crate::model::{
    Action, Bot, Conversation, ConversationKind, Delivery, Message, SidebarSection, Timeline, Ts,
    User, Workspace,
};
use crate::mrkdwn;
use crate::paths::AppDirs;
use crate::settings::{Appearance, Settings, WorkspaceMeta};
use crate::theme::{self, Catalog, Palette};

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
}

/// An unsent message.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub text: String,
    /// Mentions picked from the suggestions: the text inserted and the
    /// markup it stands for (`<@U123>`), as [`to_wire`] reads them.
    pub mentions: Vec<(String, String)>,
    pub broadcast: bool,
    pub selected: usize,
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
#[derive(Clone, Debug, Default)]
pub struct SetupForm {
    pub client_id: String,
    pub client_secret: String,
    pub app_token: String,
    pub user_token: String,
    pub show_manual: bool,
    /// Session sign-in: the workspace address and the `d` cookie.
    pub session_workspace: String,
    pub session_cookie: String,
    /// Whether the "use your own Slack app" section is expanded.
    pub show_app: bool,
}

/// One signed-in workspace and everything loaded for it.
pub struct WorkspaceState {
    pub info: Workspace,
    pub conversations: Vec<Conversation>,
    pub users: HashMap<String, User>,
    /// Apps and integrations, by `bot_id`.
    pub bots: HashMap<String, Bot>,
    /// Your Slack sidebar sections, when Slack shares them (sessions).
    pub sections: Option<Vec<SidebarSection>>,
    pub emoji: EmojiSet,
    pub timelines: HashMap<String, Timeline>,
    pub threads: HashMap<(String, Ts), Timeline>,
    pub active: Option<String>,
    /// Why this workspace needs signing in again, if it does.
    pub signed_out: Option<String>,
    pub loaded: bool,
    requested_users: HashSet<String>,
    requested_bots: HashSet<String>,
    requested_conversations: HashSet<String>,
}

impl WorkspaceState {
    fn new(info: Workspace) -> Self {
        Self {
            info,
            conversations: Vec::new(),
            users: HashMap::new(),
            bots: HashMap::new(),
            sections: None,
            emoji: EmojiSet::default(),
            timelines: HashMap::new(),
            threads: HashMap::new(),
            active: None,
            signed_out: None,
            loaded: false,
            requested_users: HashSet::new(),
            requested_bots: HashSet::new(),
            requested_conversations: HashSet::new(),
        }
    }

    pub fn conversation(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn conversation_mut(&mut self, id: &str) -> Option<&mut Conversation> {
        self.conversations.iter_mut().find(|c| c.id == id)
    }

    pub fn user(&self, id: &str) -> Option<&User> {
        self.users.get(id)
    }

    /// A person's name, or their id until it is known.
    pub fn user_label(&self, id: &str) -> String {
        self.users
            .get(id)
            .map_or_else(|| id.to_owned(), |u| u.label().to_owned())
    }

    /// What the sidebar and header call a conversation.
    pub fn title(&self, conversation: &Conversation) -> String {
        match conversation.kind {
            ConversationKind::Direct => conversation
                .user
                .as_deref()
                .map_or_else(|| conversation.name.clone(), |id| self.user_label(id)),
            _ => conversation.name.clone(),
        }
    }

    /// The label for a message's author: the name a bot posted under, the
    /// person, or the app behind the `bot_id`.
    pub fn author(&self, message: &Message) -> String {
        if let Some(name) = message.username.as_ref().filter(|n| !n.is_empty()) {
            return name.clone();
        }
        if let Some(user) = message.user.as_deref().and_then(|id| self.users.get(id)) {
            return user.label().to_owned();
        }
        if let Some(bot) = message
            .bot_id
            .as_deref()
            .and_then(|id| self.bots.get(id))
            .filter(|b| !b.name.is_empty())
        {
            return bot.name.clone();
        }
        match (&message.user, &message.bot_id) {
            (Some(id), _) => id.clone(),
            (None, Some(_)) => t("App").into_owned(),
            (None, None) => t("Unknown").into_owned(),
        }
    }

    /// The picture for a message's author.
    pub fn author_icon<'a>(&'a self, message: &'a Message) -> Option<&'a str> {
        message
            .bot_icon
            .as_deref()
            .or_else(|| {
                message
                    .user
                    .as_deref()
                    .and_then(|id| self.users.get(id))
                    .and_then(|u| u.avatar.as_deref())
            })
            .or_else(|| {
                message
                    .bot_id
                    .as_deref()
                    .and_then(|id| self.bots.get(id))
                    .and_then(|b| b.icon.as_deref())
            })
    }

    fn unknown_bots<'a>(&self, messages: impl Iterator<Item = &'a Message>) -> Vec<String> {
        let mut out: Vec<String> = messages
            .filter_map(|m| m.bot_id.as_deref())
            .filter(|id| !id.is_empty() && !self.bots.contains_key(*id))
            .filter(|id| !self.requested_bots.contains(*id))
            .map(str::to_owned)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Whether a message mentions you (or everyone).
    pub fn mentions_me(&self, message: &Message) -> bool {
        let me = format!("<@{}", self.info.user_id);
        message.text.contains(&me)
            || message.text.contains("<!here")
            || message.text.contains("<!channel")
            || message.text.contains("<!everyone")
    }

    /// A message's text ready to edit, with people and channels named as
    /// you would type them. See [`to_editable`].
    pub fn editable(&self, wire: &str) -> (String, Vec<(String, String)>) {
        to_editable(wire, |sigil, id| match sigil {
            '@' => self.users.get(id).map(|u| u.label().to_owned()),
            _ => self.conversation(id).map(|c| self.title(c)),
        })
    }

    fn unknown_users<'a>(&self, ids: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut out: Vec<String> = ids
            .filter(|id| !id.is_empty() && !self.users.contains_key(*id))
            .filter(|id| !self.requested_users.contains(*id))
            .map(str::to_owned)
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// A file chosen in the picker, and the thread it goes to.
type PickedFile = (Option<Ts>, PathBuf);

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
    pub keyring_error: Option<String>,
    pub workspaces: Vec<WorkspaceState>,
    pub sign_in: Option<SignIn>,
    pub setup: SetupForm,
    pub socket: Socket,
    pub thread: Option<(String, Ts)>,
    pub drafts: HashMap<String, Draft>,
    pub editing: Option<Editing>,
    pub toasts: Vec<Toast>,
    pub actions: Vec<Action>,
    pub switcher: Option<(String, usize)>,
    pub profile: Option<String>,
    pub picker: Option<PickerTarget>,
    pub picker_query: String,
    pub preview: Option<(String, String)>,
    /// A message waiting for "Delete?" to be answered.
    pub confirm_delete: Option<(String, Ts)>,
    pub section_dialog: Option<SectionDialog>,
    /// Where the "New" line goes: the read marker when the open
    /// conversation was opened, by `team/channel`.
    pub read_line: Option<(String, Option<Ts>)>,
    pub sidebar_filter: String,
    pub demo: bool,
    /// Older history arrived: the list keeps its place by this much.
    pub prepended: Option<(String, f32)>,
    /// Scroll the open conversation to the bottom next frame.
    pub scroll_to_bottom: bool,
    /// Focus the composer next frame.
    pub focus_composer: bool,
    local_counter: u64,
    uploads: (mpsc::Sender<PickedFile>, mpsc::Receiver<PickedFile>),
    marks: HashMap<(String, String), (Ts, Instant)>,
    pending_marks: HashMap<(String, String), Ts>,
    window_focused: bool,
    settings_dirty: bool,
    quit: bool,
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
            workspaces.push(state);
        }
        let page = if workspaces.is_empty() && !options.demo {
            Page::SignIn
        } else {
            Page::Main
        };
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
            drafts: HashMap::new(),
            editing: None,
            toasts: Vec::new(),
            actions: Vec::new(),
            switcher: None,
            profile: None,
            picker: None,
            picker_query: String::new(),
            preview: None,
            confirm_delete: None,
            section_dialog: None,
            read_line: None,
            sidebar_filter: String::new(),
            demo: options.demo,
            prepended: None,
            scroll_to_bottom: true,
            focus_composer: true,
            local_counter: 0,
            uploads: mpsc::channel(),
            marks: HashMap::new(),
            pending_marks: HashMap::new(),
            window_focused: true,
            settings_dirty: false,
            quit: false,
        };
        app.start_theme_scan();
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
        let id = self.settings.active_workspace.as_deref();
        self.workspaces
            .iter()
            .find(|w| Some(w.info.team_id.as_str()) == id)
            .or_else(|| self.workspaces.first())
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

    fn save_settings(&mut self) {
        self.settings_dirty = true;
    }

    pub fn settings_changed(&mut self) {
        self.save_settings();
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
        while let Ok((thread, path)) = self.uploads.1.try_recv() {
            self.upload(thread, path, String::new());
        }
        if self.catalog.poll() {
            self.refresh_custom_theme();
        }
        if self.catalog.needs_reload() {
            self.start_theme_scan();
        }
        self.flush_marks();
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if self.settings_dirty {
            self.settings_dirty = false;
            self.settings.save(&self.dirs.settings_file());
        }
    }

    pub fn frame_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused && !self.window_focused {
            self.mark_active_read();
        }
        self.window_focused = focused;
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

    // ---- events from the worker ----------------------------------------

    fn handle(&mut self, event: Event) {
        match event {
            Event::AppLoaded(app) => {
                if let Some(app) = &app {
                    self.setup.client_id = app.client_id.clone();
                    self.setup.client_secret = app.client_secret.clone();
                    self.setup.app_token = app.app_token.clone();
                }
                self.app_credentials = app;
                self.app_loaded = true;
            }
            Event::KeyringError(error) => {
                self.toast(format!("{}: {error}", t("Keyring")), true);
                self.keyring_error = Some(error);
            }
            Event::SignIn(state) => {
                if let SignIn::Done(name) = &state {
                    self.toast(format!("{} {name}", t("Signed in to")), false);
                    self.page = Page::Main;
                    self.setup.user_token.clear();
                }
                self.sign_in = Some(state);
            }
            Event::WorkspaceReady(info) => self.workspace_ready(info),
            Event::SignedOut { team, reason } => match reason {
                Some(reason) => {
                    if let Some(workspace) = self.workspace_mut(&team) {
                        workspace.signed_out = Some(reason);
                    }
                }
                None => {
                    self.workspaces.retain(|w| w.info.team_id != team);
                    self.settings.remove_workspace(&team);
                    self.save_settings();
                    if self.workspaces.is_empty() {
                        self.page = Page::SignIn;
                    }
                }
            },
            Event::Conversations {
                team,
                list,
                complete,
            } => self.conversations(&team, list, complete),
            Event::Conversation { team, conversation } => {
                let Some(workspace) = self.workspace_mut(&team) else {
                    return;
                };
                let mut fetch = Vec::new();
                if let Some(user) = &conversation.user
                    && !workspace.users.contains_key(user)
                {
                    fetch.push(user.clone());
                }
                match workspace.conversation_mut(&conversation.id) {
                    Some(existing) => merge_conversation(existing, conversation),
                    None => workspace.conversations.push(conversation),
                }
                self.fetch_users(&team, fetch);
            }
            Event::ConversationGone { team, channel } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.conversations.retain(|c| c.id != channel);
                    workspace.timelines.remove(&channel);
                    if workspace.active.as_deref() == Some(channel.as_str()) {
                        workspace.active = None;
                    }
                }
            }
            Event::Users { team, users } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    for user in users {
                        workspace.requested_users.remove(&user.id);
                        workspace.users.insert(user.id.clone(), user);
                    }
                }
            }
            Event::Sections { team, sections } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.sections = Some(sections);
                }
            }
            Event::Bots { team, bots } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    for bot in bots {
                        workspace.requested_bots.remove(&bot.id);
                        workspace.bots.insert(bot.id.clone(), bot);
                    }
                }
            }
            Event::Emoji { team, emoji } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.emoji = EmojiSet::new(emoji);
                }
            }
            Event::History {
                team,
                channel,
                messages,
                has_more,
                cursor,
                older,
            } => self.history(&team, &channel, messages, has_more, cursor, older),
            Event::HistoryFailed {
                team,
                channel,
                error,
            } => {
                if let Some(timeline) = self
                    .workspace_mut(&team)
                    .and_then(|w| w.timelines.get_mut(&channel))
                {
                    timeline.loading = false;
                }
                self.toast(format!("{}: {error}", t("Could not load messages")), true);
            }
            Event::Thread {
                team,
                channel,
                ts,
                messages,
            } => {
                let Some(workspace) = self.workspace_mut(&team) else {
                    return;
                };
                let users =
                    workspace.unknown_users(messages.iter().filter_map(|m| m.user.as_deref()));
                let bots = workspace.unknown_bots(messages.iter());
                let replies = messages.iter().filter(|m| m.ts != ts).count() as u32;
                if let Some(parent) = workspace
                    .timelines
                    .get_mut(&channel)
                    .and_then(|t| t.find_mut(&ts))
                {
                    parent.reply_count = parent.reply_count.max(replies);
                }
                let timeline = workspace.threads.entry((channel, ts)).or_default();
                timeline.loading = false;
                timeline.loaded = true;
                // Keep replies still being sent.
                let local: Vec<Message> = timeline
                    .messages
                    .iter()
                    .filter(|m| m.ts.is_local())
                    .cloned()
                    .collect();
                timeline.messages = messages;
                for message in local {
                    timeline.upsert(message);
                }
                self.fetch_users(&team, users);
                self.fetch_bots(&team, bots);
            }
            Event::Message {
                team,
                channel,
                message,
            } => self.message(&team, &channel, message),
            Event::Deleted { team, channel, ts } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    if let Some(timeline) = workspace.timelines.get_mut(&channel) {
                        timeline.remove(&ts);
                    }
                    for ((thread_channel, _), timeline) in &mut workspace.threads {
                        if *thread_channel == channel {
                            timeline.remove(&ts);
                        }
                    }
                }
            }
            Event::Reaction {
                team,
                channel,
                ts,
                name,
                user,
                added,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    if let Some(message) = workspace
                        .timelines
                        .get_mut(&channel)
                        .and_then(|t| t.find_mut(&ts))
                    {
                        message.toggle_reaction(&name, &user, added);
                    }
                    for ((thread_channel, _), timeline) in &mut workspace.threads {
                        if *thread_channel == channel
                            && let Some(message) = timeline.find_mut(&ts)
                        {
                            message.toggle_reaction(&name, &user, added);
                        }
                    }
                }
            }
            Event::Sent {
                team,
                channel,
                local,
                result,
            } => self.sent(&team, &channel, &local, result),
            Event::Read { team, channel, ts } => {
                if let Some(conversation) = self
                    .workspace_mut(&team)
                    .and_then(|w| w.conversation_mut(&channel))
                {
                    conversation.last_read = Some(ts);
                    conversation.unread = 0;
                    conversation.mentions = 0;
                }
            }
            Event::Socket(socket) => {
                if let Socket::Rejected(reason) = &socket {
                    self.toast(
                        format!("{} ({reason})", t("Slack refused the app-level token")),
                        true,
                    );
                }
                self.socket = socket;
            }
            Event::Error(error) => self.toast(error, true),
            Event::Notice(text) => self.toast(text, false),
        }
    }

    fn workspace_ready(&mut self, info: Workspace) {
        let team = info.team_id.clone();
        self.settings.upsert_workspace(WorkspaceMeta {
            team_id: info.team_id.clone(),
            name: info.name.clone(),
            domain: info.domain.clone(),
            icon: info.icon.clone(),
            user_id: info.user_id.clone(),
        });
        match self.workspace_mut(&team) {
            Some(workspace) => {
                workspace.info = info;
                workspace.signed_out = None;
            }
            None => {
                let mut state = WorkspaceState::new(info);
                state.active = self.settings.last_conversation.get(&team).cloned();
                self.workspaces.push(state);
            }
        }
        if self.settings.active_workspace.is_none() {
            self.settings.active_workspace = Some(team);
        }
        self.save_settings();
    }

    fn conversations(&mut self, team: &str, list: Vec<Conversation>, complete: bool) {
        let active_team = self.active_team();
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let mut merged = Vec::with_capacity(list.len());
        for mut conversation in list {
            if let Some(existing) = workspace.conversation(&conversation.id) {
                let mut kept = existing.clone();
                merge_conversation(&mut kept, conversation);
                conversation = kept;
            }
            merged.push(conversation);
        }
        if !complete {
            // A cached list: keep anything already known that it lacks.
            for existing in &workspace.conversations {
                if !merged.iter().any(|c| c.id == existing.id) {
                    merged.push(existing.clone());
                }
            }
        }
        workspace.conversations = merged;
        workspace.loaded = workspace.loaded || complete;
        let users = workspace.unknown_users(
            workspace
                .conversations
                .iter()
                .filter_map(|c| c.user.as_deref()),
        );
        let needs_open = match &workspace.active {
            Some(id) => workspace.conversation(id).is_none() && complete,
            None => true,
        };
        if needs_open {
            let first = workspace
                .conversations
                .iter()
                .filter(|c| !c.kind.is_dm())
                .min_by_key(|c| (c.name != "general", c.name.clone()))
                .or_else(|| workspace.conversations.first())
                .map(|c| c.id.clone());
            workspace.active = None;
            if let Some(first) = first {
                workspace.active = Some(first);
            }
        }
        let open = workspace.active.clone();
        self.fetch_users(team, users);
        if active_team.as_deref() == Some(team)
            && let Some(open) = open
        {
            self.ensure_loaded(team, &open);
        }
    }

    fn history(
        &mut self,
        team: &str,
        channel: &str,
        messages: Vec<Message>,
        has_more: bool,
        cursor: Option<String>,
        older: bool,
    ) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let users = workspace.unknown_users(messages.iter().flat_map(|m| {
            m.user
                .as_deref()
                .into_iter()
                .chain(m.reply_users.iter().map(String::as_str))
        }));
        let bots = workspace.unknown_bots(messages.iter());
        let newest = messages.iter().map(|m| m.ts.clone()).max();
        let timeline = workspace.timelines.entry(channel.to_owned()).or_default();
        let first = !timeline.loaded;
        timeline.merge(messages);
        timeline.loading = false;
        if older || first {
            timeline.has_more = has_more;
            timeline.cursor = cursor;
        }
        timeline.loaded = true;
        if let (Some(newest), Some(conversation)) = (newest, workspace.conversation_mut(channel))
            && conversation.latest.as_ref().is_none_or(|l| *l < newest)
        {
            conversation.latest = Some(newest);
        }
        if older {
            self.prepended = Some((format!("{team}/{channel}"), 0.0));
        }
        if first {
            self.scroll_to_bottom = true;
        }
        self.fetch_users(team, users);
        self.fetch_bots(team, bots);
        if !older {
            self.mark_if_viewing(team, channel);
        }
    }

    fn message(&mut self, team: &str, channel: &str, message: Message) {
        let viewing = self.is_viewing(team, channel);
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let mut users = workspace.unknown_users(message.user.as_deref().into_iter());
        let bots = workspace.unknown_bots(std::iter::once(&message));
        let known = workspace.conversation(channel).is_some();
        let from_me = message.user.as_deref() == Some(workspace.info.user_id.as_str());
        let mentions_me = workspace.mentions_me(&message);
        let is_reply = message.is_reply();
        if is_reply {
            let parent_ts = message.thread_ts.clone().unwrap_or_default();
            if let Some(parent) = workspace
                .timelines
                .get_mut(channel)
                .and_then(|t| t.find_mut(&parent_ts))
            {
                let thread = workspace
                    .threads
                    .get(&(channel.to_owned(), parent_ts.clone()));
                let already = thread.is_some_and(|t| t.messages.iter().any(|m| m.ts == message.ts));
                if !already {
                    parent.reply_count += 1;
                    parent.latest_reply = Some(message.ts.clone());
                    if let Some(user) = &message.user
                        && !parent.reply_users.contains(user)
                    {
                        parent.reply_users.push(user.clone());
                    }
                }
            }
            if let Some(thread) = workspace.threads.get_mut(&(channel.to_owned(), parent_ts)) {
                remove_echoed_local(thread, &message, from_me);
                thread.upsert(message.clone());
            }
        }
        if message.in_channel() {
            let timeline = workspace.timelines.entry(channel.to_owned()).or_default();
            remove_echoed_local(timeline, &message, from_me);
            let new = timeline.find_mut(&message.ts).is_none();
            timeline.upsert(message.clone());
            if new && let Some(conversation) = workspace.conversation_mut(channel) {
                if conversation.latest.as_ref().is_none_or(|l| *l < message.ts) {
                    conversation.latest = Some(message.ts.clone());
                }
                if from_me {
                    conversation.last_read = Some(message.ts.clone());
                    conversation.mentions = 0;
                } else if !viewing && (mentions_me || conversation.kind.is_dm()) {
                    conversation.mentions += 1;
                }
            }
        }
        if !known {
            if workspace.requested_conversations.insert(channel.to_owned()) {
                self.backend.send(Command::FetchConversation {
                    team: team.to_owned(),
                    channel: channel.to_owned(),
                });
            }
            users.clear();
        }
        let team_owned = team.to_owned();
        self.fetch_users(&team_owned, users);
        self.fetch_bots(&team_owned, bots);
        if viewing && !from_me {
            self.mark_if_viewing(team, channel);
        }
        if viewing {
            self.waker.wake();
        }
    }

    fn sent(&mut self, team: &str, channel: &str, local: &Ts, result: Result<Message, String>) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let mut timelines: Vec<&mut Timeline> = Vec::new();
        if let Some(t) = workspace.timelines.get_mut(channel) {
            timelines.push(t);
        }
        for ((thread_channel, _), t) in &mut workspace.threads {
            if thread_channel == channel {
                timelines.push(t);
            }
        }
        let error = result.as_ref().err().cloned();
        for timeline in timelines {
            let Some(position) = timeline.messages.iter().position(|m| &m.ts == local) else {
                continue;
            };
            match &result {
                Ok(message) => {
                    timeline.messages.remove(position);
                    // The echo from Socket Mode may already be there.
                    if timeline.find_mut(&message.ts).is_none() {
                        timeline.upsert(message.clone());
                    }
                }
                Err(error) => {
                    timeline.messages[position].delivery = Delivery::Failed(error.clone());
                }
            }
        }
        if let Ok(message) = &result
            && message.in_channel()
            && let Some(conversation) = workspace.conversation_mut(channel)
        {
            conversation.latest = Some(message.ts.clone());
            conversation.last_read = Some(message.ts.clone());
        }
        if let Some(error) = error {
            self.toast(format!("{}: {error}", t("Message not sent")), true);
        }
    }

    fn fetch_users(&mut self, team: &str, ids: Vec<String>) {
        if ids.is_empty() || self.demo {
            return;
        }
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.requested_users.extend(ids.iter().cloned());
        }
        self.backend.send(Command::FetchUsers {
            team: team.to_owned(),
            ids,
        });
    }

    fn fetch_bots(&mut self, team: &str, ids: Vec<String>) {
        if ids.is_empty() || self.demo {
            return;
        }
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.requested_bots.extend(ids.iter().cloned());
        }
        self.backend.send(Command::FetchBots {
            team: team.to_owned(),
            ids,
        });
    }

    // ---- read state -----------------------------------------------------

    fn is_viewing(&self, team: &str, channel: &str) -> bool {
        self.page == Page::Main
            && self.window_focused
            && self.active_team().as_deref() == Some(team)
            && self.active_workspace().and_then(|w| w.active.as_deref()) == Some(channel)
    }

    fn mark_if_viewing(&mut self, team: &str, channel: &str) {
        if self.is_viewing(team, channel) {
            self.mark_read(team, channel);
        }
    }

    fn mark_active_read(&mut self) {
        if let Some(team) = self.active_team()
            && let Some(channel) = self.active_workspace().and_then(|w| w.active.clone())
        {
            self.mark_read(&team, &channel);
        }
    }

    /// Clears a conversation's unread state here and tells Slack, at most
    /// every few seconds.
    fn mark_read(&mut self, team: &str, channel: &str) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
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

    pub fn open_conversation(&mut self, channel: &str) {
        let Some(team) = self.active_team() else {
            return;
        };
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
        self.scroll_to_bottom = true;
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

    fn next_local(&mut self) -> Ts {
        self.local_counter += 1;
        Ts::new(format!("local-{}", self.local_counter))
    }

    fn send(&mut self, text: String, thread: Option<Ts>, broadcast: bool) {
        let Some(team) = self.active_team() else {
            return;
        };
        let channel = match &thread {
            Some(_) => self.thread.as_ref().map(|(c, _)| c.clone()),
            None => self.active_workspace().and_then(|w| w.active.clone()),
        };
        let Some(channel) = channel else {
            return;
        };
        let key = Self::draft_key(&team, &channel, thread.as_ref());
        let draft = self.drafts.remove(&key).unwrap_or_default();
        let wire = to_wire(&text, &draft.mentions);
        if wire.trim().is_empty() {
            return;
        }
        let local = self.next_local();
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let message = Message {
            ts: local.clone(),
            user: Some(workspace.info.user_id.clone()),
            username: None,
            bot_icon: None,
            bot_id: None,
            text: wire.clone(),
            thread_ts: thread.clone(),
            reply_count: 0,
            reply_users: Vec::new(),
            latest_reply: None,
            reactions: Vec::new(),
            files: Vec::new(),
            attachments: Vec::new(),
            blocks: Vec::new(),
            edited: false,
            subtype: None,
            delivery: Delivery::Sending,
            broadcast,
        };
        match &thread {
            Some(parent) => {
                workspace
                    .threads
                    .entry((channel.clone(), parent.clone()))
                    .or_default()
                    .upsert(message);
            }
            None => workspace
                .timelines
                .entry(channel.clone())
                .or_default()
                .upsert(message),
        }
        self.scroll_to_bottom = true;
        self.backend.send(Command::Send {
            team,
            channel,
            text: wire,
            thread,
            broadcast,
            local,
        });
    }

    fn retry(&mut self, channel: &str, local: &Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let mut found = None;
        let mut timelines: Vec<&mut Timeline> =
            workspace.timelines.get_mut(channel).into_iter().collect();
        timelines.extend(
            workspace
                .threads
                .iter_mut()
                .filter(|((c, _), _)| c == channel)
                .map(|(_, t)| t),
        );
        for timeline in timelines {
            if let Some(message) = timeline.find_mut(local) {
                message.delivery = Delivery::Sending;
                found = Some((
                    message.text.clone(),
                    message.thread_ts.clone(),
                    message.broadcast,
                ));
            }
        }
        if let Some((text, thread, broadcast)) = found {
            self.backend.send(Command::Send {
                team,
                channel: channel.to_owned(),
                text,
                thread,
                broadcast,
                local: local.clone(),
            });
        }
    }

    fn react(&mut self, channel: &str, ts: &Ts, name: &str) {
        let Some(team) = self.active_team() else {
            return;
        };
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let me = workspace.info.user_id.clone();
        let mut add = None;
        let mut timelines: Vec<&mut Timeline> =
            workspace.timelines.get_mut(channel).into_iter().collect();
        timelines.extend(
            workspace
                .threads
                .iter_mut()
                .filter(|((c, _), _)| c == channel)
                .map(|(_, t)| t),
        );
        for timeline in timelines {
            if let Some(message) = timeline.find_mut(ts) {
                let adding = *add.get_or_insert_with(|| {
                    !message
                        .reactions
                        .iter()
                        .any(|r| r.name == name && r.users.contains(&me))
                });
                message.toggle_reaction(name, &me, adding);
            }
        }
        if let Some(add) = add {
            self.backend.send(Command::React {
                team,
                channel: channel.to_owned(),
                ts: ts.clone(),
                name: name.to_owned(),
                add,
            });
        }
    }

    fn upload(&mut self, thread: Option<Ts>, path: PathBuf, comment: String) {
        let Some(team) = self.active_team() else {
            return;
        };
        let channel = match &thread {
            Some(_) => self.thread.as_ref().map(|(c, _)| c.clone()),
            None => self.active_workspace().and_then(|w| w.active.clone()),
        };
        if let Some(channel) = channel {
            self.backend.send(Command::Upload {
                team,
                channel,
                thread,
                path,
                comment,
            });
        }
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::SelectWorkspace(team) => {
                self.settings.active_workspace = Some(team.clone());
                self.save_settings();
                self.thread = None;
                self.page = Page::Main;
                self.scroll_to_bottom = true;
                if let Some(channel) = self.workspace_mut(&team).and_then(|w| w.active.clone()) {
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
            Action::OpenConversation(channel) => self.open_conversation(&channel),
            Action::OpenThread { channel, ts } => {
                let Some(team) = self.active_team() else {
                    return;
                };
                self.thread = Some((channel.clone(), ts.clone()));
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace
                        .threads
                        .entry((channel.clone(), ts.clone()))
                        .or_default()
                        .loading = true;
                }
                self.backend.send(Command::LoadThread { team, channel, ts });
            }
            Action::CloseThread => self.thread = None,
            Action::LoadOlder => {
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
            Action::Send {
                text,
                thread,
                broadcast,
            } => self.send(text, thread, broadcast),
            Action::Retry { channel, local } => self.retry(&channel, &local),
            Action::Edit { channel, ts, text } => {
                let Some(team) = self.active_team() else {
                    return;
                };
                let mentions = self
                    .editing
                    .take()
                    .filter(|e| e.ts == ts && e.channel == channel)
                    .map(|e| e.mentions)
                    .unwrap_or_default();
                let wire = to_wire(&text, &mentions);
                if let Some(workspace) = self.workspace_mut(&team) {
                    let mut timelines: Vec<&mut Timeline> =
                        workspace.timelines.get_mut(&channel).into_iter().collect();
                    timelines.extend(workspace.threads.values_mut());
                    for timeline in timelines {
                        if let Some(message) = timeline.find_mut(&ts) {
                            message.text = wire.clone();
                            message.edited = true;
                        }
                    }
                }
                self.backend.send(Command::Edit {
                    team,
                    channel,
                    ts,
                    text: wire,
                });
            }
            Action::Delete { channel, ts } => {
                let Some(team) = self.active_team() else {
                    return;
                };
                if let Some(workspace) = self.workspace_mut(&team) {
                    if let Some(timeline) = workspace.timelines.get_mut(&channel) {
                        timeline.remove(&ts);
                    }
                    for timeline in workspace.threads.values_mut() {
                        timeline.remove(&ts);
                    }
                }
                if !ts.is_local() {
                    self.backend.send(Command::Delete { team, channel, ts });
                }
            }
            Action::React { channel, ts, name } => self.react(&channel, &ts, &name),
            Action::PickReaction { channel, ts } => {
                self.picker_query.clear();
                self.picker = Some(PickerTarget::Reaction { channel, ts });
            }
            Action::PickEmoji { draft } => {
                self.picker_query.clear();
                self.picker = Some(PickerTarget::Draft(draft));
            }
            Action::StartEdit { channel, ts } => {
                let found = self.active_workspace().and_then(|w| {
                    w.timelines
                        .get(&channel)
                        .and_then(|t| t.messages.iter().find(|m| m.ts == ts))
                        .or_else(|| {
                            w.threads
                                .values()
                                .find_map(|t| t.messages.iter().find(|m| m.ts == ts))
                        })
                        .map(|m| w.editable(&m.text))
                });
                if let Some((text, mentions)) = found {
                    self.editing = Some(Editing {
                        channel,
                        ts,
                        text,
                        mentions,
                    });
                }
            }
            Action::CancelEdit => self.editing = None,
            Action::EditLast => {
                let found = self.active_workspace().and_then(|w| {
                    let channel = w.active.clone()?;
                    let me = w.info.user_id.as_str();
                    let ts = w
                        .timelines
                        .get(&channel)?
                        .messages
                        .iter()
                        .rev()
                        .find(|m| {
                            m.user.as_deref() == Some(me)
                                && m.delivery == Delivery::Sent
                                && !m.is_system()
                        })?
                        .ts
                        .clone();
                    Some((channel, ts))
                });
                if let Some((channel, ts)) = found {
                    self.actions.push(Action::StartEdit { channel, ts });
                }
            }
            Action::AskDelete { channel, ts } => self.confirm_delete = Some((channel, ts)),
            Action::NameSection { rename, channel } => {
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
                self.section_dialog = Some(SectionDialog {
                    rename,
                    channel,
                    name,
                });
            }
            Action::Sidebar(edit) => self.edit_sidebar(edit),
            Action::Preview { uri, name } => self.preview = Some((uri, name)),
            Action::OpenSwitcher => self.switcher = Some((String::new(), 0)),
            Action::Upload {
                thread,
                path,
                comment,
            } => self.upload(thread, path, comment),
            Action::PickUpload { thread } => {
                let sender = self.uploads.0.clone();
                let waker = self.waker.clone();
                std::thread::spawn(move || {
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        let _ = sender.send((thread, path));
                        waker.wake();
                    }
                });
            }
            Action::Download { url, name } => {
                if let Some(team) = self.active_team() {
                    self.backend.send(Command::Download { team, url, name });
                }
            }
            Action::OpenUrl(url) => {
                if let Some(channel) = slack_link_channel(&url, self.active_workspace()) {
                    self.open_conversation(&channel);
                } else if !mrkdwn::is_openable(&url) {
                    // Attachments and blocks carry URLs a bot chose.
                    self.toast(t("Only web and mail links can be opened"), true);
                } else if let Err(error) = open::that_detached(&url) {
                    self.toast(format!("{}: {error}", t("Could not open the link")), true);
                }
            }
            Action::OpenProfile(user) => self.profile = Some(user),
            Action::Copy(text) => {
                ctx.copy_text(text);
                self.toast(t("Copied").into_owned(), false);
            }
            Action::ShowSettings => self.page = Page::Settings,
            Action::HideSettings => {
                self.page = if self.workspaces.is_empty() {
                    Page::SignIn
                } else {
                    Page::Main
                };
            }
            Action::AddWorkspace => {
                self.sign_in = None;
                self.page = Page::SignIn;
            }
            Action::SignOut(team) => {
                self.backend.send(Command::SignOut(team));
            }
            Action::Reconnect => self.backend.send(Command::Reconnect),
            Action::DismissError => self.toasts.clear(),
        }
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

    pub fn save_state(&mut self) {
        self.settings.save(&self.dirs.settings_file());
    }

    pub fn request_quit(&mut self) {
        self.quit = true;
    }
}

/// Fills in what a fresher copy of a conversation lacks, and keeps the
/// newer of each marker.
fn merge_conversation(existing: &mut Conversation, fresh: Conversation) {
    let latest = max_ts(existing.latest.take(), fresh.latest.clone());
    let last_read = max_ts(existing.last_read.take(), fresh.last_read.clone());
    let mentions = existing.mentions;
    let unread = if fresh.unread > 0 {
        fresh.unread
    } else {
        existing.unread
    };
    *existing = Conversation {
        latest,
        last_read,
        mentions,
        unread,
        ..fresh
    };
}

fn max_ts(a: Option<Ts>, b: Option<Ts>) -> Option<Ts> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// A sent message's own echo replaces its optimistic copy.
fn remove_echoed_local(timeline: &mut Timeline, message: &Message, from_me: bool) {
    if !from_me {
        return;
    }
    if let Some(position) = timeline
        .messages
        .iter()
        .position(|m| m.ts.is_local() && m.delivery == Delivery::Sending && m.text == message.text)
    {
        timeline.messages.remove(position);
    }
}

/// What `@here`, `@channel` and `@everyone` become for Slack.
const BROADCASTS: [(&str, &str); 3] = [
    ("@here", "<!here>"),
    ("@channel", "<!channel>"),
    ("@everyone", "<!everyone>"),
];

/// What Slack receives for what you typed: markup characters escaped, and
/// picked mentions and broadcasts turned into Slack's own forms.
///
/// `mentions` pairs the text as typed with the markup it stands for. A
/// label only counts where it stands alone, so "@Ann" leaves "@Annabel"
/// be, and the text is read once from the start, so markup already put in
/// is never matched again.
pub fn to_wire(text: &str, mentions: &[(String, String)]) -> String {
    let escaped = mrkdwn::escape(text.trim_end());
    let mut forms: Vec<(String, &str)> = mentions
        .iter()
        .map(|(label, wire)| (mrkdwn::escape(label), wire.as_str()))
        .chain(
            BROADCASTS
                .iter()
                .map(|(typed, wire)| ((*typed).to_owned(), *wire)),
        )
        .filter(|(label, _)| !label.is_empty())
        .collect();
    // Longest first, so "@Ann Lee" wins over "@Ann".
    forms.sort_by_key(|(label, _)| std::cmp::Reverse(label.len()));
    let mut out = String::with_capacity(escaped.len());
    let mut previous = None;
    let mut rest = escaped.as_str();
    while let Some(c) = rest.chars().next() {
        let found = forms.iter().find(|(label, _)| {
            rest.starts_with(label.as_str()) && is_word_edge(rest[label.len()..].chars().next())
        });
        if is_word_edge(previous)
            && let Some((label, wire)) = found
        {
            out.push_str(wire);
            previous = label.chars().next_back();
            rest = &rest[label.len()..];
        } else {
            out.push(c);
            previous = Some(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Whether a typed label may start or end next to `c`.
fn is_word_edge(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_alphanumeric())
}

/// A piece of a message being made editable.
struct Piece {
    shown: String,
    /// The markup it came from, when typing `shown` alone would not bring
    /// it back.
    wire: Option<String>,
    /// What to show (and its markup) when `shown` is not unique in the
    /// text: [`to_wire`] would otherwise turn every copy into this link.
    fallback: Option<(String, String)>,
}

impl Piece {
    fn text(text: &str) -> Self {
        Self {
            shown: mrkdwn::unescape(text),
            wire: None,
            fallback: None,
        }
    }

    fn markup(shown: String, wire: String) -> Self {
        Self {
            shown,
            wire: Some(wire),
            fallback: None,
        }
    }
}

/// A sent message's text as you would type it, and the mentions that turn
/// it back into the same markup through [`to_wire`].
///
/// People and channels show as `@name` and `#name`. A link shows its label
/// when that is unique in the text, and its address otherwise. `name_of`
/// names a person (`'@'`) or a channel (`'#'`) by id.
pub fn to_editable(
    wire: &str,
    name_of: impl Fn(char, &str) -> Option<String>,
) -> (String, Vec<(String, String)>) {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut rest = wire;
    while let Some(open) = rest.find('<') {
        pieces.push(Piece::text(&rest[..open]));
        let after = &rest[open + 1..];
        let inner = after
            .find('>')
            .map(|close| &after[..close])
            .filter(|inner| !inner.is_empty() && !inner.contains(['<', '\n']));
        let Some(inner) = inner else {
            // Not markup: Slack escapes a typed `<`, so keep it as it is.
            pieces.push(Piece::text("<"));
            rest = after;
            continue;
        };
        rest = &after[inner.len() + 1..];
        let raw = format!("<{inner}>");
        let (target, label) = match inner.split_once('|') {
            Some((target, label)) => (target, Some(mrkdwn::unescape(label))),
            None => (inner, None),
        };
        let label = label.filter(|l| !l.is_empty());
        let name = |sigil: char, id: &str| {
            name_of(sigil, id)
                .or_else(|| {
                    label
                        .as_deref()
                        .map(|l| l.trim_start_matches(sigil).to_owned())
                })
                .unwrap_or_else(|| id.to_owned())
        };
        let piece = if let Some(id) = target.strip_prefix('@') {
            Piece::markup(format!("@{}", name('@', id)), raw)
        } else if let Some(id) = target.strip_prefix('#') {
            Piece::markup(format!("#{}", name('#', id)), raw)
        } else if let Some(command) = target.strip_prefix('!') {
            let word = command.split('^').next().unwrap_or(command);
            match BROADCASTS.iter().find(|(typed, _)| typed[1..] == *word) {
                // Typing these brings them back.
                Some((typed, _)) => Piece::text(typed),
                // User groups and dates: their label stands for them.
                None => Piece::markup(label.clone().unwrap_or_else(|| format!("@{word}")), raw),
            }
        } else {
            let url = mrkdwn::unescape(target);
            let bare = format!("<{target}>");
            match label.clone().filter(|l| *l != url) {
                Some(label) => Piece {
                    shown: label,
                    wire: Some(raw),
                    fallback: Some((url, bare)),
                },
                None => Piece::markup(url, bare),
            }
        };
        pieces.push(piece);
    }
    pieces.push(Piece::text(rest));
    let text: String = pieces.iter().map(|p| p.shown.as_str()).collect();
    let mut mentions: Vec<(String, String)> = Vec::new();
    for piece in &mut pieces {
        if let Some((url, bare)) = piece.fallback.take()
            && text.matches(piece.shown.as_str()).count() > 1
        {
            piece.shown = url;
            piece.wire = Some(bare);
        }
        if let Some(wire) = &piece.wire
            && !mentions.iter().any(|(label, _)| *label == piece.shown)
        {
            mentions.push((piece.shown.clone(), wire.clone()));
        }
    }
    let text = pieces.iter().map(|p| p.shown.as_str()).collect();
    (text, mentions)
}

/// The conversation a link to this workspace's Slack points at
/// (`https://acme.slack.com/archives/C123/p…`).
fn slack_link_channel(url: &str, workspace: Option<&WorkspaceState>) -> Option<String> {
    let workspace = workspace?;
    if workspace.info.domain.is_empty() {
        return None;
    }
    let prefix = format!("https://{}.slack.com/archives/", workspace.info.domain);
    let rest = url.strip_prefix(&prefix)?;
    let channel = rest.split(['/', '?']).next()?;
    workspace.conversation(channel).map(|c| c.id.clone())
}

impl fastframe_shell::Resident for App {
    fn closed(&self) -> Closed {
        Closed::Quit
    }

    fn window_gone(&mut self) {
        self.waker.detach();
    }

    fn headless_frame(&mut self, ctx: &egui::Context) -> Headless {
        self.background_frame(ctx);
        if self.quit {
            Headless::Quit
        } else {
            Headless::Wait
        }
    }

    fn shutdown(&mut self) {
        self.save_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_becomes_slack_markup() {
        let mentions = vec![
            ("@Ann".to_owned(), "<@U1>".to_owned()),
            ("@Ann Lee".to_owned(), "<@U2>".to_owned()),
        ];
        assert_eq!(
            to_wire("hi @Ann Lee & @Ann <3 @here, not @heresy", &mentions),
            "hi <@U2> &amp; <@U1> &lt;3 <!here>, not @heresy"
        );
    }

    #[test]
    fn mention_labels_match_only_whole_words() {
        let mentions = vec![
            ("@Ann".to_owned(), "<@U1>".to_owned()),
            ("@Annabel".to_owned(), "<@U2>".to_owned()),
            ("@Zoë".to_owned(), "<@U3>".to_owned()),
        ];
        assert_eq!(
            to_wire("@Annabel, @Ann and @Annie", &mentions),
            "<@U2>, <@U1> and @Annie"
        );
        assert_eq!(
            to_wire("über @Zoë! ünd @Zoëy mail@Ann", &mentions),
            "über <@U3>! ünd @Zoëy mail@Ann"
        );
        // The boundary is read from the whole text, not from where the last
        // match ended.
        assert_eq!(to_wire("é@here @here", &[]), "é@here <!here>");
        assert_eq!(to_wire("@channel—@everyone", &[]), "<!channel>—<!everyone>");
    }

    #[test]
    fn inserted_markup_is_not_matched_again() {
        // A person called "U2" must not reach into `<@U2>`.
        let mentions = vec![
            ("@Bo".to_owned(), "<@U2>".to_owned()),
            ("@U2".to_owned(), "<@U9>".to_owned()),
        ];
        assert_eq!(to_wire("@Bo", &mentions), "<@U2>");
    }

    fn names(sigil: char, id: &str) -> Option<String> {
        match (sigil, id) {
            ('@', "U1") => Some("Ann Lee".into()),
            ('#', "C1") => Some("general".into()),
            _ => None,
        }
    }

    #[test]
    fn edited_messages_keep_their_markup() {
        let wire = "hi <@U1> and <@U2|bob> in <#C1|general> &amp; <!here>: see \
                    <https://x.y/a?b=1&amp;c=2|the docs>, <https://x.y> or \
                    <mailto:a@x.y|a@x.y> &lt;3 <!subteam^S1|@design> ünï *bold*";
        let (text, mentions) = to_editable(wire, names);
        assert_eq!(
            text,
            "hi @Ann Lee and @bob in #general & @here: see the docs, https://x.y or \
             a@x.y <3 @design ünï *bold*"
        );
        assert_eq!(to_wire(&text, &mentions), wire);
    }

    #[test]
    fn edits_survive_changes_around_the_markup() {
        let (text, mentions) = to_editable("ping <@U1> about <#C9>", names);
        assert_eq!(text, "ping @Ann Lee about #C9");
        let changed = text.replace("ping", "hey") + " & <#C1>";
        assert_eq!(
            to_wire(&changed, &mentions),
            "hey <@U1> about <#C9> &amp; &lt;#C1&gt;"
        );
    }

    #[test]
    fn a_link_label_that_repeats_shows_the_address() {
        // "docs" appears as a word too; keeping the label would link both.
        let wire = "docs: <https://x.y|docs>";
        let (text, mentions) = to_editable(wire, names);
        assert_eq!(text, "docs: https://x.y");
        assert_eq!(to_wire(&text, &mentions), "docs: <https://x.y>");
    }

    #[test]
    fn stray_angle_brackets_stay_text() {
        let (text, mentions) = to_editable("a < b <> c", names);
        assert_eq!(text, "a < b <> c");
        assert!(mentions.is_empty());
    }

    #[test]
    fn fresher_conversations_keep_newer_markers() {
        let mut existing = Conversation {
            id: "C1".into(),
            name: "old".into(),
            kind: ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: Some(Ts::new("5.0")),
            latest: Some(Ts::new("9.0")),
            unread: 0,
            mentions: 2,
        };
        let fresh = Conversation {
            name: "renamed".into(),
            last_read: Some(Ts::new("7.0")),
            latest: None,
            mentions: 0,
            ..existing.clone()
        };
        merge_conversation(&mut existing, fresh);
        assert_eq!(existing.name, "renamed");
        assert_eq!(existing.latest, Some(Ts::new("9.0")));
        assert_eq!(existing.last_read, Some(Ts::new("7.0")));
        assert_eq!(existing.mentions, 2);
    }
}
