//! Palette, typography, icons, and the egui style they make.
//!
//! Every colour on screen comes from [`Palette`], so the built-in dark and
//! light palettes, the shared fastframe presets and the user's own palette
//! files all look consistent.

use std::collections::BTreeSet;

use egui::{Color32, CornerRadius, Stroke, Vec2};

/// A palette file from the themes directory.
pub type CustomTheme = fastframe_theme::CustomTheme<Palette>;
/// The palette files, the shared presets, and Omarchy's palette on Linux.
pub type Catalog = fastframe_theme::Catalog<Palette>;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Palette {
    pub dark: bool,
    pub window: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub surface_active: Color32,
    pub outline: Color32,
    pub text: Color32,
    pub secondary: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub overlay: Color32,
    pub shadow: Color32,
    /// Links in messages.
    pub link: Color32,
    /// Behind a mention of you, `@here` or `@channel`.
    pub mention: Color32,
    /// The unread-mentions badge.
    pub badge: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            dark: true,
            window: Color32::from_rgb(0x1a, 0x1d, 0x21),
            panel: Color32::from_rgb(0x13, 0x15, 0x18),
            surface: Color32::from_rgb(0x22, 0x25, 0x29),
            surface_hover: Color32::from_rgb(0x2a, 0x2e, 0x33),
            surface_active: Color32::from_rgb(0x33, 0x38, 0x3e),
            outline: Color32::from_rgb(0x2e, 0x32, 0x38),
            text: Color32::from_rgb(0xe8, 0xea, 0xed),
            secondary: Color32::from_rgb(0xab, 0xb0, 0xb7),
            dim: Color32::from_rgb(0x76, 0x7c, 0x85),
            accent: Color32::from_rgb(0x1d, 0x9b, 0xd1),
            accent_hover: Color32::from_rgb(0x3d, 0xb0, 0xe0),
            on_accent: Color32::WHITE,
            danger: Color32::from_rgb(0xf0, 0x6a, 0x76),
            warning: Color32::from_rgb(0xe8, 0xb4, 0x5a),
            overlay: Color32::from_rgb(0x22, 0x25, 0x29),
            shadow: Color32::from_black_alpha(150),
            link: Color32::from_rgb(0x4f, 0xb4, 0xe6),
            mention: Color32::from_rgba_unmultiplied(0xe8, 0xb4, 0x5a, 0x38),
            badge: Color32::from_rgb(0xe0, 0x1e, 0x5a),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            window: Color32::WHITE,
            panel: Color32::from_rgb(0xf6, 0xf6, 0xf7),
            surface: Color32::from_rgb(0xee, 0xef, 0xf1),
            surface_hover: Color32::from_rgb(0xe4, 0xe6, 0xe9),
            surface_active: Color32::from_rgb(0xd8, 0xdb, 0xdf),
            outline: Color32::from_rgb(0xdd, 0xdf, 0xe2),
            text: Color32::from_rgb(0x1d, 0x1c, 0x1d),
            secondary: Color32::from_rgb(0x55, 0x58, 0x5d),
            dim: Color32::from_rgb(0x8a, 0x8d, 0x91),
            accent: Color32::from_rgb(0x12, 0x64, 0xa3),
            accent_hover: Color32::from_rgb(0x0b, 0x4c, 0x80),
            on_accent: Color32::WHITE,
            danger: Color32::from_rgb(0xc8, 0x2a, 0x3c),
            warning: Color32::from_rgb(0xa0, 0x6a, 0x10),
            overlay: Color32::WHITE,
            shadow: Color32::from_black_alpha(45),
            link: Color32::from_rgb(0x12, 0x64, 0xa3),
            mention: Color32::from_rgba_unmultiplied(0xf2, 0xc7, 0x44, 0x50),
            badge: Color32::from_rgb(0xe0, 0x1e, 0x5a),
        }
    }
}

impl fastframe_theme::Palette for Palette {
    fn base(base: fastframe_theme::Base) -> Self {
        match base {
            fastframe_theme::Base::Dark => Self::dark(),
            fastframe_theme::Base::Light => Self::light(),
        }
    }

    fn set(&mut self, name: &str, color: Color32) -> bool {
        match name {
            "window" => self.window = color,
            "panel" => self.panel = color,
            "surface" => self.surface = color,
            "surface_hover" => self.surface_hover = color,
            "surface_active" => self.surface_active = color,
            "outline" => self.outline = color,
            "text" => self.text = color,
            "secondary" => self.secondary = color,
            "dim" => self.dim = color,
            "accent" => self.accent = color,
            "accent_hover" => self.accent_hover = color,
            "on_accent" => self.on_accent = color,
            "danger" => self.danger = color,
            "warning" => self.warning = color,
            "overlay" => self.overlay = color,
            "shadow" => self.shadow = color,
            "link" => self.link = color,
            "mention" => self.mention = color,
            "badge" => self.badge = color,
            _ => return false,
        }
        true
    }

    /// A palette written for the sixteen shared colours still gets links,
    /// mentions and badges that match it.
    fn derive(&mut self, given: &BTreeSet<&str>) {
        if !given.contains("link") && given.contains("accent") {
            self.link = self.accent;
        }
        if !given.contains("mention") && given.contains("warning") {
            self.mention = self.warning.gamma_multiply(0.25);
        }
        if !given.contains("badge") && given.contains("danger") {
            self.badge = self.danger;
        }
    }
}

/// Adds the shared palettes and, on Linux, Omarchy's, to a normal launch.
pub fn enable_desktop_themes(catalog: &mut Catalog) {
    catalog.enable_desktop_themes(fastframe_theme::DesktopThemes {
        slug: "noslacking",
        omarchy_template: fastframe_theme::omarchy::BASE_TEMPLATE,
        omarchy_previous_templates: &[],
        presets: true,
    });
}

pub const RADIUS: u8 = 8;
pub const RADIUS_SMALL: u8 = 4;
pub const RAIL_WIDTH: f32 = 64.0;
pub const AVATAR: f32 = 36.0;

/// macOS draws the content under the traffic lights; leave them room.
pub fn titlebar_inset(ctx: &egui::Context) -> f32 {
    if cfg!(target_os = "macos") && !ctx.input(|input| input.viewport().fullscreen.unwrap_or(false))
    {
        28.0
    } else {
        0.0
    }
}

pub fn regular(size: f32) -> egui::FontId {
    fastframe_fonts::Weight::Regular.font_id(size)
}

pub fn medium(size: f32) -> egui::FontId {
    fastframe_fonts::Weight::Medium.font_id(size)
}

pub fn semibold(size: f32) -> egui::FontId {
    fastframe_fonts::Weight::SemiBold.font_id(size)
}

pub fn bold(size: f32) -> egui::FontId {
    fastframe_fonts::Weight::Bold.font_id(size)
}

pub fn mono(size: f32) -> egui::FontId {
    egui::FontId::monospace(size)
}

/// How the desktop renders text, read once per process. Tests use the
/// platform default instead of asking the desktop.
pub fn text_rendering() -> fastframe_text::TextRendering {
    static RENDERING: std::sync::OnceLock<fastframe_text::TextRendering> =
        std::sync::OnceLock::new();
    *RENDERING.get_or_init(|| {
        if cfg!(test) {
            fastframe_text::TextRendering::platform_default()
        } else {
            fastframe_text::detect()
        }
    })
}

/// Fonts, image loaders, icons and colour emoji, once per context.
pub fn install(ctx: &egui::Context) {
    let emoji = egui::FontData::from_static(include_bytes!("../assets/fonts/NotoEmoji.ttf"));
    let mut fonts = fastframe_fonts::FontSetup::default()
        .companion("noto_emoji", std::sync::Arc::new(emoji))
        .definitions();
    text_rendering().apply_to(&mut fonts);
    ctx.set_fonts(fonts);
    egui_extras::install_image_loaders(ctx);
    fastframe_icons::install::<Icon>(ctx);
    ctx.add_plugin(fastframe_emoji::EmojiPlugin::default());
}

/// Noto Color Emoji as a bitmap (CBDT) font, behind the platform's own.
///
/// fastframe-emoji draws colour bitmap fonts (CBDT, sbix). Some Linux
/// desktops (Fedora among them) ship Noto Color Emoji only as a COLRv1
/// vector font, which it cannot draw; without this every emoji would fall
/// back to the monochrome outline glyph.
const BUNDLED_EMOJI: &[u8] = include_bytes!("../assets/fonts/NotoColorEmoji.ttf");

/// Chooses the platform's colour emoji font, with the bundled one for
/// whatever it lacks, and finds it off this thread.
pub fn install_emoji(synchronous: bool) {
    fastframe_emoji::EmojiSetup::default()
        .system(true)
        .bundled(BUNDLED_EMOJI)
        .synchronous(synchronous)
        .install();
    std::thread::spawn(fastframe_emoji::warm_up);
}

/// Applies the palette to egui's own widgets, so menus, text fields and
/// scroll bars agree with the custom views.
pub fn apply(ctx: &egui::Context, palette: &Palette) {
    let mut style = (*ctx.global_style()).clone();
    apply_to_style(&mut style, palette);
    ctx.set_global_style(style);
}

fn apply_to_style(style: &mut egui::Style, palette: &Palette) {
    let visuals = &mut style.visuals;
    *visuals = if palette.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.dark_mode = palette.dark;
    text_rendering().apply_to_visuals(visuals);
    visuals.panel_fill = palette.window;
    visuals.window_fill = palette.overlay;
    visuals.extreme_bg_color = palette.surface;
    visuals.faint_bg_color = palette.surface;
    visuals.code_bg_color = palette.surface;
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.secondary);
    visuals.hyperlink_color = palette.link;
    visuals.selection.bg_fill = palette.accent.gamma_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0, palette.accent);
    visuals.window_stroke = Stroke::new(1.0, palette.outline);
    visuals.window_corner_radius = CornerRadius::same(RADIUS + 2);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: palette.shadow,
    };
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: palette.shadow,
    };
    let corner = CornerRadius::same(RADIUS_SMALL + 2);
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = corner;
        widget.bg_stroke = Stroke::NONE;
        widget.fg_stroke = Stroke::new(1.0, palette.text);
        widget.expansion = 0.0;
    }
    visuals.widgets.noninteractive.corner_radius = corner;
    visuals.widgets.noninteractive.bg_fill = palette.panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.outline);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, palette.text);
    visuals.widgets.inactive.bg_fill = palette.surface;
    visuals.widgets.inactive.weak_bg_fill = palette.surface;
    visuals.widgets.hovered.bg_fill = palette.surface_hover;
    visuals.widgets.hovered.weak_bg_fill = palette.surface_hover;
    visuals.widgets.active.bg_fill = palette.surface_active;
    visuals.widgets.active.weak_bg_fill = palette.surface_active;
    visuals.widgets.open.bg_fill = palette.surface_hover;
    visuals.widgets.open.weak_bg_fill = palette.surface_hover;
    visuals.text_cursor.stroke = Stroke::new(2.0, palette.accent);
    visuals.striped = false;

    use egui::FontFamily::{Monospace, Proportional};
    use egui::{FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Small, FontId::new(11.5, Proportional)),
        (TextStyle::Body, FontId::new(14.5, Proportional)),
        (TextStyle::Button, FontId::new(14.0, Proportional)),
        (TextStyle::Heading, FontId::new(20.0, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, Monospace)),
    ]
    .into();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size = Vec2::new(40.0, 28.0);
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.window_margin = egui::Margin::same(16);
    style.spacing.scroll = egui::style::ScrollStyle {
        bar_width: 8.0,
        floating_width: 6.0,
        floating_allocated_width: 0.0,
        handle_min_length: 28.0,
        bar_inner_margin: 3.0,
        bar_outer_margin: 2.0,
        dormant_background_opacity: 0.0,
        dormant_handle_opacity: 0.0,
        active_background_opacity: 0.0,
        active_handle_opacity: 0.55,
        interact_handle_opacity: 0.85,
        foreground_color: true,
        ..egui::style::ScrollStyle::floating()
    };
    style.interaction.selectable_labels = false;
    style.interaction.tooltip_delay = 0.4;
    style.animation_time = 0.12;
    style.url_in_tooltip = true;
}

fastframe_icons::icons! {
    /// Every icon the interface draws. The shared Lucide icons come from
    /// fastframe-icons; the rest are NoSlacking's own files (also Lucide).
    pub enum Icon {
        prefix: "noslacking-icon-",
        directory: "../assets/icons/",
        Archive => "archive",
        Bookmark => "bookmark",
        ArrowDown => "arrow-down",
        ArrowLeft => lucide "arrow-left",
        ArrowUp => "arrow-up",
        AtSign => "at-sign",
        Bell => "bell",
        BellOff => "bell-off",
        Bold => "bold",
        Check => lucide "check",
        CheckCheck => "check-check",
        ChevronDown => lucide "chevron-down",
        ChevronRight => lucide "chevron-right",
        CircleAlert => lucide "circle-alert",
        Code => "code",
        CodeBlock => "square-code",
        Copy => lucide "copy",
        Download => "download",
        Ellipsis => lucide "ellipsis",
        ExternalLink => lucide "external-link",
        FileText => "file-text",
        Headphones => "headphones",
        Hash => "hash",
        Image => "image",
        Info => lucide "info",
        Link => "link",
        Italic => "italic",
        Lock => lucide "lock",
        LogOut => lucide "log-out",
        MessageCircle => "message-circle",
        Messages => "messages-square",
        Paperclip => "paperclip",
        Pencil => lucide "pencil",
        Pin => lucide "pin",
        PinOff => lucide "pin-off",
        Plus => lucide "plus",
        Refresh => lucide "refresh-cw",
        Reply => "reply",
        Search => lucide "search",
        Send => "send",
        Settings => lucide "settings",
        Smile => "smile",
        SmilePlus => "smile-plus",
        Strike => "strikethrough",
        TextQuote => "text-quote",
        SquarePen => lucide "square-pen",
        Trash => lucide "trash-2",
        Type => "type",
        User => lucide "user",
        Users => lucide "users",
        X => lucide "x",
    }
}

/// A square icon button with a hover background and a tooltip.
pub fn icon_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: Icon,
    size: f32,
    tooltip: &str,
) -> egui::Response {
    let pad = 6.0;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::splat(size + pad * 2.0), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(RADIUS_SMALL),
            palette.surface_hover,
        );
    }
    let tint = if response.hovered() {
        palette.text
    } else {
        palette.secondary
    };
    icon.image(tint, size).paint_at(
        ui,
        egui::Rect::from_center_size(rect.center(), Vec2::splat(size)),
    );
    focus_ring(ui, &response, palette, RADIUS_SMALL);
    describe(&response, egui::WidgetType::Button, tooltip);
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if tooltip.is_empty() {
        response
    } else {
        response.on_hover_text(tooltip)
    }
}

/// Tells screen readers what a custom-painted control is and what it is
/// called. egui's own widgets do this themselves; painted ones are silent
/// without it.
pub fn describe(response: &egui::Response, typ: egui::WidgetType, label: &str) {
    response.widget_info(|| egui::WidgetInfo::labeled(typ, response.enabled(), label));
}

/// [`describe`] for a control that is on or off: the open conversation, a
/// reaction of yours, an unfolded section.
pub fn describe_selected(
    response: &egui::Response,
    typ: egui::WidgetType,
    selected: bool,
    label: &str,
) {
    response.widget_info(|| egui::WidgetInfo::selected(typ, response.enabled(), selected, label));
}

/// An accent ring around a custom-painted control with keyboard focus, so
/// Tab shows where it is.
pub fn focus_ring(ui: &egui::Ui, response: &egui::Response, palette: &Palette, radius: u8) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect,
            CornerRadius::same(radius),
            Stroke::new(2.0, palette.accent),
            egui::StrokeKind::Inside,
        );
    }
}

/// A filled accent button.
pub fn primary_button(ui: &mut egui::Ui, palette: &Palette, label: &str) -> egui::Response {
    let text = egui::RichText::new(label)
        .font(medium(14.0))
        .color(palette.on_accent);
    ui.add(
        egui::Button::new(text)
            .fill(palette.accent)
            .corner_radius(CornerRadius::same(RADIUS_SMALL + 2))
            .min_size(Vec2::new(0.0, 32.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A quiet surface-coloured button.
pub fn secondary_button(ui: &mut egui::Ui, palette: &Palette, label: &str) -> egui::Response {
    let text = egui::RichText::new(label)
        .font(medium(14.0))
        .color(palette.text);
    ui.add(
        egui::Button::new(text)
            .fill(palette.surface)
            .corner_radius(CornerRadius::same(RADIUS_SMALL + 2))
            .min_size(Vec2::new(0.0, 32.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A deterministic colour for someone without an avatar.
pub fn identity_color(id: &str) -> Color32 {
    const COLORS: [Color32; 8] = [
        Color32::from_rgb(0xe0, 0x6c, 0x75),
        Color32::from_rgb(0xd1, 0x9a, 0x66),
        Color32::from_rgb(0x98, 0xc3, 0x79),
        Color32::from_rgb(0x56, 0xb6, 0xc2),
        Color32::from_rgb(0x61, 0xaf, 0xef),
        Color32::from_rgb(0xc6, 0x78, 0xdd),
        Color32::from_rgb(0xbe, 0x50, 0x46),
        Color32::from_rgb(0x2b, 0xa8, 0x8a),
    ];
    let hash = id.bytes().fold(0u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    COLORS[(hash % COLORS.len() as u32) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_palettes_get_matching_app_colours() {
        let palette: Palette = fastframe_theme::parse_palette(
            r##"{"base":"dark","colors":{"accent":"#ff0000","danger":"#00ff00"}}"##,
        )
        .expect("parses");
        assert_eq!(palette.link, Color32::from_rgb(0xff, 0, 0));
        assert_eq!(palette.badge, Color32::from_rgb(0, 0xff, 0));
    }

    #[test]
    fn every_preset_parses() {
        assert!(fastframe_theme::presets::themes::<Palette>().count() > 0);
    }
}
