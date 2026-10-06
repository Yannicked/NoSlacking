//! Draws parsed mrkdwn: styled runs that wrap like text, clickable links,
//! mentions and channels, colour and custom emoji, code and quotes.

use egui::{Color32, CornerRadius, RichText, Sense, Stroke, Vec2};

use crate::app::WorkspaceState;
use crate::emoji::Resolved;
use crate::model::Action;
use crate::mrkdwn::{self, Block, Inline, Style};
use crate::theme::{self, Palette};

pub struct Rich<'a> {
    pub palette: &'a Palette,
    pub workspace: &'a WorkspaceState,
    pub size: f32,
    /// Text colour, for quieter text such as system messages.
    pub color: Color32,
}

impl<'a> Rich<'a> {
    pub fn new(palette: &'a Palette, workspace: &'a WorkspaceState) -> Self {
        Self {
            palette,
            workspace,
            size: 14.5,
            color: palette.text,
        }
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: Color32) -> Self {
        self.color = color;
        self
    }
}

fn styled(rich: &Rich<'_>, text: &str, style: Style, size: f32) -> RichText {
    let font = if style.bold {
        theme::bold(size)
    } else {
        theme::regular(size)
    };
    let mut text = RichText::new(text).font(font).color(rich.color);
    if style.italic {
        text = text.italics();
    }
    if style.strike {
        text = text.strikethrough();
    }
    text
}

thread_local! {
    /// Parsed text, kept while it stays on screen: parsing every message
    /// on every frame was most of the cost of drawing a long history. The
    /// interface draws on one thread, so one cache per thread is one cache.
    static PARSED: std::cell::RefCell<mrkdwn::ParseCache> =
        std::cell::RefCell::new(mrkdwn::ParseCache::default());
}

/// The blocks of `text`, parsed on this frame or an earlier one.
fn parsed(text: &str) -> std::sync::Arc<[Block]> {
    PARSED.with(|cache| cache.borrow_mut().get(text))
}

/// Forgets the parsed text not drawn since the last call; once a frame.
pub fn end_frame() {
    PARSED.with(|cache| cache.borrow_mut().sweep());
    #[cfg(feature = "highlight")]
    HIGHLIGHTED.with(|cache| cache.borrow_mut().sweep());
}

#[cfg(feature = "highlight")]
thread_local! {
    /// Highlighted code, kept while it stays on screen, like [`PARSED`].
    static HIGHLIGHTED: std::cell::RefCell<crate::highlight::HighlightCache> =
        std::cell::RefCell::new(crate::highlight::HighlightCache::default());
}

/// The colour of a kind of code, from the palette so it reads in every
/// theme: the palette's accent, warning and danger are all chosen to stand
/// out on its surfaces.
#[cfg(feature = "highlight")]
fn code_color(palette: &Palette, base: Color32, kind: crate::highlight::Kind) -> Color32 {
    use crate::highlight::Kind;
    match kind {
        Kind::Plain => base,
        Kind::Keyword => palette.accent,
        // Between the keywords' blue and the numbers' red: a violet that
        // tells types from both.
        Kind::Type => palette.accent.lerp_to_gamma(palette.danger, 0.5),
        Kind::String | Kind::Added => palette.warning,
        Kind::Number | Kind::Removed => palette.danger,
        Kind::Comment => palette.dim,
    }
}

/// A code block's text, coloured when its first line names a language.
/// Also returns the code without that line, which is what Copy copies, and
/// the language's name.
fn code_job(
    rich: &Rich<'_>,
    code: &str,
    font: egui::FontId,
) -> (egui::text::LayoutJob, String, Option<&'static str>) {
    #[cfg(feature = "highlight")]
    {
        let (language, body) = crate::highlight::split_language(code);
        if let Some(language) = language {
            let runs = HIGHLIGHTED.with(|cache| cache.borrow_mut().get(language, body));
            let mut job = egui::text::LayoutJob::default();
            for (range, kind) in runs.iter() {
                let mut format = egui::TextFormat::simple(
                    font.clone(),
                    code_color(rich.palette, rich.color, *kind),
                );
                format.italics = *kind == crate::highlight::Kind::Comment;
                job.append(&body[range.clone()], 0.0, format);
            }
            return (job, body.to_owned(), Some(language.name));
        }
    }
    let job = egui::text::LayoutJob::single_section(
        code.to_owned(),
        egui::TextFormat::simple(font, rich.color),
    );
    (job, code.to_owned(), None)
}

/// A preformatted block: monospace on a surface, coloured when it names
/// its language, with a Copy button while the pointer is over it.
fn code_block(ui: &mut egui::Ui, rich: &Rich<'_>, code: &str, actions: &mut Vec<Action>) {
    let palette = rich.palette;
    let (job, body, language) = code_job(rich, code, theme::mono(rich.size - 1.5));
    let response = egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(job).wrap().selectable(true));
        });
    let rect = response.response.rect;
    // The language's name sits in the corner, quietly; the button joins it
    // on hover, so it never covers code you are reading without the mouse.
    let mut right = rect.right() - 4.0;
    if ui.rect_contains_pointer(rect) {
        let button = egui::Rect::from_min_size(
            egui::pos2(right - 26.0, rect.top() + 4.0),
            Vec2::splat(26.0),
        );
        right = button.left() - 4.0;
        ui.painter().rect(
            button,
            CornerRadius::same(theme::RADIUS_SMALL),
            palette.overlay,
            Stroke::new(1.0, palette.outline),
            egui::StrokeKind::Inside,
        );
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(button));
        let tip = crate::i18n::t("Copy code");
        if theme::icon_button(&mut child, palette, theme::Icon::Copy, 14.0, &tip).clicked() {
            actions.push(Action::Copy(body));
        }
    }
    if let Some(language) = language {
        ui.painter().text(
            egui::pos2(right - 4.0, rect.top() + 6.0),
            egui::Align2::RIGHT_TOP,
            language,
            theme::regular(11.0),
            palette.dim,
        );
    }
}

/// Draws `text`; `edited` adds Slack's quiet "(edited)".
pub fn show(
    ui: &mut egui::Ui,
    rich: &Rich<'_>,
    text: &str,
    edited: bool,
    actions: &mut Vec<Action>,
) {
    show_parsed(ui, rich, &parsed(text), edited, actions);
}

/// Draws a message's words: from Slack's own layout of them when it sent
/// one, else from its mrkdwn.
pub fn message(
    ui: &mut egui::Ui,
    rich: &Rich<'_>,
    message: &crate::model::Message,
    edited: bool,
    actions: &mut Vec<Action>,
) {
    match message.rich_text() {
        Some(blocks) => show_parsed(ui, rich, blocks, edited, actions),
        None => show(ui, rich, &message.text, edited, actions),
    }
}

/// Draws blocks parsed already; `edited` adds Slack's quiet "(edited)".
pub fn show_parsed(
    ui: &mut egui::Ui,
    rich: &Rich<'_>,
    blocks: &[Block],
    edited: bool,
    actions: &mut Vec<Action>,
) {
    // Message text can be selected and copied, across runs, links and
    // mentions alike; the rest of the interface keeps labels inert.
    ui.scope(|ui| {
        ui.style_mut().interaction.selectable_labels = true;
        show_blocks(ui, rich, blocks, edited, actions);
    });
}

fn show_blocks(
    ui: &mut egui::Ui,
    rich: &Rich<'_>,
    blocks: &[Block],
    edited: bool,
    actions: &mut Vec<Action>,
) {
    let size = if mrkdwn::only_emoji(blocks) {
        30.0
    } else {
        rich.size
    };
    let count = blocks.len();
    for (index, block) in blocks.iter().enumerate() {
        let last = index + 1 == count;
        match block {
            Block::Paragraph(inlines) => flow(ui, rich, inlines, size, edited && last, actions),
            Block::Quote(inlines) => {
                let response = egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 0,
                        top: 1,
                        bottom: 1,
                    })
                    .show(ui, |ui| {
                        flow(ui, rich, inlines, size, edited && last, actions)
                    });
                let rect = response.response.rect;
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())),
                    CornerRadius::same(2),
                    rich.palette.outline,
                );
            }
            Block::Preformatted(code) => {
                code_block(ui, rich, code, actions);
                if edited && last {
                    ui.label(
                        RichText::new(crate::i18n::t("(edited)"))
                            .font(theme::regular(12.0))
                            .color(rich.palette.dim),
                    );
                }
            }
        }
    }
    if blocks.is_empty() && edited {
        ui.label(
            RichText::new(crate::i18n::t("(edited)"))
                .font(theme::regular(12.0))
                .color(rich.palette.dim),
        );
    }
}

fn clickable(ui: &mut egui::Ui, text: RichText, tooltip: Option<&str>) -> egui::Response {
    let response = ui
        .add(egui::Label::new(text).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    match tooltip {
        Some(tip) => response.on_hover_text(tip),
        None => response,
    }
}

/// Text that wraps across rows, one widget per run.
fn flow(
    ui: &mut egui::Ui,
    rich: &Rich<'_>,
    inlines: &[Inline],
    size: f32,
    edited: bool,
    actions: &mut Vec<Action>,
) {
    let palette = rich.palette;
    let workspace = rich.workspace;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
        // A line break first is an empty first line, as after a quote and
        // a blank line, which keeps its height too.
        let mut previous_newline = true;
        for inline in inlines {
            let newline = matches!(inline, Inline::Newline);
            match inline {
                Inline::Text(text, style) => {
                    ui.add(egui::Label::new(styled(rich, text, *style, size)));
                }
                Inline::Code(code) => {
                    ui.add(egui::Label::new(
                        RichText::new(code)
                            .font(theme::mono(size - 1.5))
                            .color(palette.danger)
                            .background_color(palette.surface),
                    ));
                }
                Inline::Link { url, label, style } => {
                    let shown = label.clone().unwrap_or_else(|| shorten(url));
                    let text = styled(rich, &shown, *style, size).color(palette.link);
                    let tip = (label.is_some()).then_some(url.as_str());
                    let response = clickable(ui, text, tip);
                    if response.hovered() {
                        super::context::hover(ui, super::context::Target::Link(url.clone()));
                    }
                    if response.clicked() {
                        actions.push(Action::OpenUrl(url.clone()));
                    }
                }
                Inline::User { id, label } => {
                    let name = workspace
                        .user(id)
                        .map(|u| u.label().to_owned())
                        .or_else(|| label.clone())
                        .unwrap_or_else(|| id.clone());
                    let me = *id == workspace.info.user_id;
                    let background = if me {
                        palette.mention
                    } else {
                        palette.link.gamma_multiply(0.15)
                    };
                    let text = RichText::new(format!("@{name}"))
                        .font(theme::medium(size))
                        .color(if me { palette.text } else { palette.link })
                        .background_color(background);
                    if clickable(ui, text, None).clicked() {
                        actions.push(Action::OpenProfile(id.clone()));
                    }
                }
                Inline::Channel { id, label } => {
                    let known = workspace.conversation(id);
                    let name = known
                        .map(|c| workspace.title(c))
                        .or_else(|| label.clone())
                        .unwrap_or_else(|| id.clone());
                    let text = RichText::new(format!("#{name}"))
                        .font(theme::medium(size))
                        .color(palette.link)
                        .background_color(palette.link.gamma_multiply(0.15));
                    if clickable(ui, text, None).clicked() && known.is_some() {
                        actions.push(Action::OpenConversation(id.clone()));
                    }
                }
                Inline::Broadcast(name) => {
                    ui.add(egui::Label::new(
                        RichText::new(format!("@{name}"))
                            .font(theme::medium(size))
                            .color(palette.text)
                            .background_color(palette.mention),
                    ));
                }
                Inline::Group { id, label } => {
                    let name = workspace.group_label(id, label.as_deref());
                    ui.add(egui::Label::new(
                        RichText::new(name)
                            .font(theme::medium(size))
                            .color(palette.link)
                            .background_color(palette.link.gamma_multiply(0.15)),
                    ));
                }
                Inline::Emoji(name) => emoji(ui, rich, name, size),
                Inline::Newline => {
                    if previous_newline {
                        // A blank line keeps its height.
                        ui.label(RichText::new(" ").font(theme::regular(size)));
                    }
                    ui.end_row();
                }
            }
            previous_newline = newline;
        }
        if edited {
            ui.add(egui::Label::new(
                RichText::new(format!(" {}", crate::i18n::t("(edited)")))
                    .font(theme::regular(12.0))
                    .color(palette.dim),
            ));
        }
    });
}

/// Draws `:name:` as the emoji it stands for. Its name shows on hover, and
/// is formatted only then.
pub fn emoji(ui: &mut egui::Ui, rich: &Rich<'_>, name: &str, size: f32) {
    match rich.workspace.emoji.resolve(name) {
        Resolved::Unicode(text) => {
            ui.add(egui::Label::new(
                RichText::new(text).font(theme::regular(size * 1.1)),
            ))
            .on_hover_ui(|ui| {
                ui.label(format!(":{name}:"));
            });
        }
        Resolved::Image(url) => {
            let side = size * 1.3;
            // A square, loaded or not: a wide emoji drawn narrower would
            // move the rest of its line, and could rewrap it.
            super::picture(
                ui,
                url.to_owned(),
                Vec2::splat(side),
                egui::CornerRadius::ZERO,
                egui::Sense::hover(),
            )
            .on_hover_ui(|ui| {
                ui.label(format!(":{name}:"));
            });
        }
        Resolved::Unknown => {
            ui.add(egui::Label::new(
                RichText::new(format!(":{name}:"))
                    .font(theme::regular(size))
                    .color(rich.color),
            ));
        }
    }
}

/// `https://example.com/a/very/long/path?x=1` reads as `example.com/a/very/lo…`.
fn shorten(url: &str) -> String {
    let bare = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("mailto:"))
        .unwrap_or(url);
    let bare = bare.strip_suffix('/').unwrap_or(bare);
    if bare.chars().count() > 60 {
        let cut: String = bare.chars().take(57).collect();
        format!("{cut}…")
    } else {
        bare.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::shorten;
    use crate::app::WorkspaceState;
    use crate::model::{UserGroup, Workspace};

    #[test]
    fn group_mentions_show_their_handle() {
        let mut w = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
            sign_in: Default::default(),
        });
        assert_eq!(w.group_label("S1", None), "@S1");
        assert_eq!(w.group_label("S1", Some("@design")), "@design");
        w.groups.push(UserGroup {
            id: "S1".into(),
            handle: "design-team".into(),
            name: "Design".into(),
            members: None,
        });
        assert_eq!(w.group_label("S1", None), "@design-team");
        assert_eq!(w.group_label("S1", Some("@design")), "@design-team");
    }

    #[test]
    fn links_lose_their_scheme_and_length() {
        assert_eq!(shorten("https://example.com/"), "example.com");
        assert_eq!(shorten("mailto:a@b.c"), "a@b.c");
        assert!(shorten(&format!("https://x.y/{}", "a".repeat(100))).ends_with('…'));
    }
}
