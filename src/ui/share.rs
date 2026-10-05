//! The "Share message" dialog: pick a conversation, say something with it
//! if you like, and share. What it sends is worked out in
//! [`crate::share`]; this only asks.

use egui::{CornerRadius, Key, KeyboardShortcut, Margin, Modifiers, RichText, Stroke, Vec2};

use crate::app::App;
use crate::i18n::t;
use crate::model::Action;
use crate::theme;

/// How many conversations the picker lists at once.
const SHOWN: usize = 6;

/// Shows the dialog while it is open.
pub fn dialog(app: &mut App, ctx: &egui::Context) {
    let Some(mut share) = app.share.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let frame = super::overlays::modal_frame(app);
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    // The message went away (deleted, or another workspace opened).
    let Some(message) = workspace.find_message(&share.channel, &share.ts) else {
        return;
    };
    let author = workspace.author(message);
    let preview = crate::share::preview(&super::message::plain_text(workspace, message), 3);
    // Nothing can be posted to an archived channel.
    let matches = super::overlays::matching(
        ctx,
        "share",
        workspace,
        &share.query,
        |c| !c.archived,
        SHOWN,
    );
    let comment_id = egui::Id::new("share-comment");
    let in_comment = ctx.memory(|m| m.has_focus(comment_id));
    let (down, up, enter, escape) = ctx.input_mut(|input| {
        // Shift+Enter breaks a line in the comment; Enter alone shares.
        let plain = input.modifiers.is_none();
        (
            !in_comment && input.consume_key(Modifiers::NONE, Key::ArrowDown),
            !in_comment && input.consume_key(Modifiers::NONE, Key::ArrowUp),
            plain && input.consume_key(Modifiers::NONE, Key::Enter),
            input.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    if !matches.is_empty() {
        if down {
            share.selected = (share.selected + 1) % matches.len();
        }
        if up {
            share.selected = (share.selected + matches.len() - 1) % matches.len();
        }
        share.selected = share.selected.min(matches.len() - 1);
    }
    let mut picked = enter.then_some(share.selected);
    let mut close = escape;
    let response = egui::Modal::new(egui::Id::new("share-dialog"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(480.0);
            let heading = ui.label(
                RichText::new(t("Share message"))
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.add_space(8.0);
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut share.query)
                        .id(egui::Id::new("share-query"))
                        .hint_text(t("Search for a channel or person…"))
                        .font(theme::regular(15.0))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 7)),
                )
                .labelled_by(heading.id);
            if focus {
                field.request_focus();
            }
            if field.changed() {
                share.selected = 0;
            }
            ui.add_space(6.0);
            for (index, found) in matches.iter().enumerate() {
                let row =
                    super::overlays::conversation_row(ui, &palette, found, index == share.selected);
                if row.clicked() {
                    share.selected = index;
                }
                if row.double_clicked() {
                    picked = Some(index);
                }
            }
            if matches.is_empty() {
                ui.label(RichText::new(t("Nothing matches.")).color(palette.dim));
            }
            ui.add_space(10.0);
            let label = ui.label(
                RichText::new(t("Add a message, if you like"))
                    .font(theme::medium(13.0))
                    .color(palette.secondary),
            );
            ui.add(
                egui::TextEdit::multiline(&mut share.comment)
                    .id(comment_id)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .return_key(Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)))
                    .margin(Margin::symmetric(8, 6)),
            )
            .labelled_by(label.id);
            ui.add_space(10.0);
            quote(ui, &palette, &author, &preview);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    close = true;
                }
                let button = ui.add_enabled_ui(!matches.is_empty(), |ui| {
                    theme::primary_button(ui, &palette, &t("Share"))
                });
                if button.inner.clicked() {
                    picked = Some(share.selected);
                }
            });
        });
    if response.should_close() {
        close = true;
    }
    if close {
        return;
    }
    if let Some(found) = picked.and_then(|index| matches.get(index)) {
        app.actions.push(Action::ShareTo {
            channel: share.channel,
            ts: share.ts,
            thread: share.thread,
            to: found.id.clone(),
            comment: share.comment,
        });
        return;
    }
    app.share = Some(share);
}

/// The message being shared, as Slack quotes it: a bar down its side, who
/// wrote it, and its first lines.
fn quote(ui: &mut egui::Ui, palette: &theme::Palette, author: &str, text: &str) {
    let inner = egui::Frame::new()
        .fill(palette.surface_hover)
        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL))
        .inner_margin(Margin {
            left: 14,
            right: 10,
            top: 8,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(author)
                    .font(theme::semibold(13.5))
                    .color(palette.text),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(text)
                        .font(theme::regular(13.5))
                        .color(palette.secondary),
                )
                .wrap(),
            );
        });
    let rect = inner.response.rect;
    ui.painter().line_segment(
        [
            rect.left_top() + Vec2::new(4.0, 6.0),
            rect.left_bottom() + Vec2::new(4.0, -6.0),
        ],
        Stroke::new(3.0, palette.dim),
    );
}
