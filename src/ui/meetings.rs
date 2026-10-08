//! The meetings dialog of a Teams workspace: "Meet now", which starts a
//! meeting and joins it (its link to copy from the call bar), and joining
//! one by its link or by its ID and passcode. Opened from the sidebar's
//! header, and by opening a meeting link, which it then holds.

use egui::{Key, Margin, RichText};

use crate::app::App;
use crate::i18n::t;
use crate::meetings::NotAMeeting;
use crate::model::Action;
use crate::theme::{self, Icon};

/// The sidebar header's button for the dialog.
pub fn header_button(ui: &mut egui::Ui, palette: &theme::Palette, actions: &mut Vec<Action>) {
    if theme::icon_button(ui, palette, Icon::Video, 16.0, &t("Meetings")).clicked() {
        actions.push(Action::OpenMeetings);
    }
}

/// Why what was typed is not a meeting, in words.
fn problem_text(problem: NotAMeeting) -> String {
    match problem {
        NotAMeeting::Unreadable => t("That is not a meeting link or ID."),
        NotAMeeting::NoPasscode => t("A meeting ID needs its passcode."),
        NotAMeeting::OldWorkLink => t("Links of this kind cannot be joined here yet."),
    }
    .into_owned()
}

/// What the dialog was told.
enum Answer {
    Cancel,
    MeetNow,
    Join,
}

/// The dialog, while open.
pub fn dialog(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.meeting_dialog.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let mut answer: Option<Answer> = None;
    let frame = super::overlays::modal_frame(app);
    let before = (dialog.link.clone(), dialog.passcode.clone());
    let response = egui::Modal::new(egui::Id::new("meetings-dialog"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(
                RichText::new(t("Meetings"))
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, &palette, &t("Meet now")).clicked() {
                    answer = Some(Answer::MeetNow);
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(t("Start a meeting, then invite others with its link."))
                            .font(theme::regular(12.5))
                            .color(palette.secondary),
                    )
                    .wrap(),
                );
            });
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            let heading = ui.label(
                RichText::new(t("Join a meeting"))
                    .font(theme::semibold(14.0))
                    .color(palette.text),
            );
            ui.add_space(4.0);
            let link = ui
                .add(
                    egui::TextEdit::singleline(&mut dialog.link)
                        .id(egui::Id::new("meeting-link"))
                        .hint_text(t("Meeting link or ID"))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(8, 6)),
                )
                .labelled_by(heading.id);
            if focus {
                link.request_focus();
            }
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut dialog.passcode)
                    .id(egui::Id::new("meeting-passcode"))
                    .hint_text(t("Passcode, for a meeting ID"))
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 6)),
            );
            if let Some(problem) = dialog.problem {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(problem_text(problem))
                        .font(theme::regular(12.5))
                        .color(palette.danger),
                );
            }
            if ui.input(|i| i.key_pressed(Key::Enter)) && !dialog.link.trim().is_empty() {
                answer = Some(Answer::Join);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    answer = Some(Answer::Cancel);
                }
                if theme::primary_button(ui, &palette, &t("Join")).clicked() {
                    answer = Some(Answer::Join);
                }
            });
        });
    if response.should_close() {
        answer = Some(Answer::Cancel);
    }
    // Typing again clears what was wrong.
    if (dialog.link.as_str(), dialog.passcode.as_str()) != (before.0.as_str(), before.1.as_str()) {
        dialog.problem = None;
    }
    let team = dialog.team.clone();
    match answer {
        Some(Answer::Cancel) => {}
        Some(Answer::MeetNow) => {
            app.actions
                .push(Action::Huddle(crate::huddles::Action::JoinMeeting {
                    team,
                    meeting: None,
                }))
        }
        Some(Answer::Join) => match dialog.read() {
            Some(meeting) => {
                app.actions
                    .push(Action::Huddle(crate::huddles::Action::JoinMeeting {
                        team,
                        meeting: Some(meeting),
                    }))
            }
            None => app.meeting_dialog = Some(dialog),
        },
        None => app.meeting_dialog = Some(dialog),
    }
}
