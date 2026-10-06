//! The "Add emoji" dialog: a picture, a name, a preview, and what Slack
//! would refuse said before anything is sent.

use egui::{CornerRadius, Key, Margin, RichText, Stroke, Vec2};

use crate::app::App;
use crate::custom_emoji::{NameProblem, check_name, clean_name};
use crate::i18n::{t, tf};
use crate::model::Action;
use crate::theme;

/// The side of the preview.
const PREVIEW: f32 = 64.0;

pub(super) fn dialog(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.add_emoji.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let custom = app
        .workspaces
        .iter()
        .find(|w| w.info.team_id == dialog.team)
        .map(|w| w.emoji.clone())
        .unwrap_or_default();
    let mut close = false;
    let mut send = false;
    let mut pick = false;
    let frame = super::overlays::modal_frame(app);
    let response = egui::Modal::new(egui::Id::new("add-emoji"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(
                RichText::new(t("Add emoji"))
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                // The picture as it will look, or an empty square.
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(PREVIEW), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(theme::RADIUS), palette.surface);
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::same(theme::RADIUS),
                    Stroke::new(1.0, palette.outline),
                    egui::StrokeKind::Inside,
                );
                if let Some(picked) = dialog.picked.as_ref().filter(|p| p.check.is_ok()) {
                    egui::Image::from_bytes(
                        picked.uri.clone(),
                        egui::load::Bytes::Shared(picked.bytes.clone()),
                    )
                    .fit_to_exact_size(Vec2::splat(PREVIEW - 12.0))
                    .paint_at(ui, rect.shrink(6.0));
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    let label = if dialog.picked.is_some() {
                        t("Choose another picture…")
                    } else {
                        t("Choose a picture…")
                    };
                    if ui
                        .add_enabled(
                            !dialog.busy,
                            egui::Button::new(RichText::new(label).font(theme::medium(14.0))),
                        )
                        .clicked()
                    {
                        pick = true;
                    }
                    match &dialog.picked {
                        Some(picked) => {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&picked.file_name)
                                        .font(theme::regular(13.0))
                                        .color(palette.text),
                                )
                                .truncate(),
                            );
                            match picked.check {
                                Ok(info) => {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} × {} · {}",
                                            info.width,
                                            info.height,
                                            super::file_size(picked.bytes.len() as u64)
                                        ))
                                        .font(theme::regular(12.0))
                                        .color(palette.secondary),
                                    );
                                    if !info.square() {
                                        ui.label(
                                            RichText::new(t(
                                                "Not square: Slack will fit it into a square.",
                                            ))
                                            .font(theme::regular(12.0))
                                            .color(palette.dim),
                                        );
                                    }
                                }
                                Err(problem) => {
                                    ui.label(
                                        RichText::new(problem.message())
                                            .font(theme::regular(12.0))
                                            .color(palette.danger),
                                    );
                                }
                            }
                        }
                        None => {
                            ui.label(
                                RichText::new(t(
                                    "PNG, JPEG or GIF, up to 128 KB. Square pictures look best.",
                                ))
                                .font(theme::regular(12.0))
                                .color(palette.secondary),
                            );
                        }
                    }
                });
            });
            ui.add_space(10.0);
            let heading = ui.label(
                RichText::new(t("Name"))
                    .font(theme::semibold(13.0))
                    .color(palette.text),
            );
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .id(egui::Id::new("add-emoji-name"))
                        .hint_text(t("for example shipit"))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(8, 6)),
                )
                .labelled_by(heading.id);
            if focus {
                field.request_focus();
            }
            // Slack takes lowercase only; capitals are lowered as typed.
            dialog.name.make_ascii_lowercase();
            let name = clean_name(&dialog.name);
            match check_name(&dialog.name, &custom) {
                Ok(name) => {
                    ui.label(
                        RichText::new(tf("Type :{name}: to use it.", &[("name", &name)]))
                            .font(theme::regular(12.0))
                            .color(palette.secondary),
                    );
                }
                // Nothing typed yet is not an error to show.
                Err(NameProblem::Empty) if name.is_empty() => {}
                Err(problem) => {
                    ui.label(
                        RichText::new(problem.message())
                            .font(theme::regular(12.0))
                            .color(palette.danger),
                    );
                }
            }
            if let Some(error) = &dialog.error {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(error)
                        .font(theme::regular(13.0))
                        .color(palette.danger),
                );
            }
            ui.add_space(10.0);
            let ready = dialog.ready(&custom).is_some() && !dialog.busy;
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    close = true;
                }
                let add =
                    ui.add_enabled_ui(ready, |ui| theme::primary_button(ui, &palette, &t("Add")));
                if add.inner.clicked() {
                    send = true;
                }
                if dialog.busy {
                    ui.spinner();
                }
            });
            if ready && ui.input(|i| i.key_pressed(Key::Enter)) {
                send = true;
            }
        });
    if response.should_close() && !dialog.busy {
        close = true;
    }
    if pick {
        app.actions.push(Action::PickEmojiImage);
    }
    if send {
        app.actions.push(Action::SendEmoji);
    }
    if !close {
        app.add_emoji = Some(dialog);
    }
}
