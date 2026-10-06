//! Links to Slack messages drawn as quotes of those messages: a card with
//! the author, where and when it was posted, and how it starts (see
//! [`crate::quotes`] for which link gets which quote).

use egui::{CornerRadius, Margin, RichText, Sense, Vec2};

use super::Row;
use crate::app::WorkspaceState;
use crate::i18n::t;
use crate::model::{Action, Message, Quote};
use crate::quotes::{self, Lookup};
use crate::theme;
use crate::ui::rich::{self, Rich};

/// How large the author's picture is in a quote.
const AVATAR: f32 = 20.0;

/// The quotes `message` needs of its own: for each link to a message that
/// Slack did not unfurl, in a signed-in workspace, the message as loaded
/// or fetched. One not fetched yet is asked for and shows as its plain
/// link meanwhile; one that cannot be read says so.
pub(super) fn own_quotes(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    actions: &mut Vec<Action>,
) {
    for link in quotes::own_quotes(message) {
        let signed_in = row
            .workspaces
            .iter()
            .filter(|w| w.signed_out.is_none())
            .map(|w| (w.info.team_id.as_str(), w.info.domain.as_str()));
        let Some(key) = quotes::resolve(&link, signed_in) else {
            continue;
        };
        let Some(workspace) = row.workspaces.iter().find(|w| w.info.team_id == key.team) else {
            continue;
        };
        let url = crate::links::permalink(
            &workspace.info.domain,
            &key.channel,
            &key.ts,
            key.thread.as_ref(),
        )
        .unwrap_or_default();
        match quotes::lookup(workspace, &key.channel, &key.ts) {
            Lookup::Found(quoted) => {
                let quote = quotes::from_message(workspace, &url, &key.channel, quoted);
                quote_view(ui, row, workspace, &quote, actions);
            }
            Lookup::Gone => {
                let quote = quotes::unavailable(&url, &key.channel);
                quote_view(ui, row, workspace, &quote, actions);
            }
            Lookup::Waiting => {}
            Lookup::Unasked => actions.push(Action::FetchQuote {
                team: key.team,
                channel: key.channel,
                ts: key.ts,
                thread: key.thread,
            }),
        }
    }
}

/// A quoted message: a bar at the left, the author's picture and name,
/// the conversation and time, the first lines of its text and "View
/// message". The whole card opens the message, in the app when its
/// workspace is signed in here.
pub(super) fn quote_view(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    workspace: &WorkspaceState,
    quote: &Quote,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let background = ui.painter().add(egui::Shape::Noop);
    let author = quote
        .user
        .as_deref()
        .and_then(|id| workspace.user(id))
        .map(|u| u.label().to_owned())
        .or_else(|| quote.author.as_deref().map(crate::mrkdwn::unescape))
        .unwrap_or_default();
    let icon = quote
        .user
        .as_deref()
        .and_then(|id| workspace.user(id))
        .and_then(|u| u.avatar.as_deref())
        .or(quote.author_icon.as_deref());
    // Sensed before its contents are laid out, so a mention or link in the
    // quoted text still takes its own click.
    let scope = ui.scope_builder(
        egui::UiBuilder::new()
            .sense(Sense::click())
            .id_salt(("quote", quote.url.as_str())),
        |ui| {
            egui::Frame::new()
                .inner_margin(Margin {
                    left: 12,
                    right: 8,
                    top: 4,
                    bottom: 4,
                })
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(560.0));
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if quote.unavailable {
                        ui.label(
                            RichText::new(t("Message not available"))
                                .font(theme::regular(13.5))
                                .italics()
                                .color(palette.secondary),
                        );
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.spacing_mut().interact_size.y = AVATAR;
                        if !author.is_empty() {
                            let seed = quote.user.as_deref().unwrap_or(&author);
                            crate::ui::avatar(ui, icon, &author, seed, AVATAR);
                            ui.label(
                                RichText::new(&author)
                                    .font(theme::bold(13.5))
                                    .color(palette.text),
                            );
                        }
                        let place = quotes::place(workspace, quote);
                        let time = quote
                            .ts
                            .as_ref()
                            .and_then(|ts| ts.seconds())
                            .map(crate::ui::moment_label);
                        let details: Vec<String> = place.into_iter().chain(time).collect();
                        if !details.is_empty() {
                            ui.label(
                                RichText::new(details.join(" · "))
                                    .font(theme::regular(12.0))
                                    .color(palette.dim),
                            );
                        }
                    });
                    let text = quotes::excerpt(&quote.text);
                    if !text.is_empty() {
                        let rich = Rich::new(palette, workspace).size(14.0);
                        rich::show(ui, &rich, &text, false, actions);
                    }
                    ui.label(
                        RichText::new(t("View message"))
                            .font(theme::semibold(12.5))
                            .color(palette.link),
                    );
                });
        },
    );
    let rect = scope.response.rect;
    let response = scope
        .response
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.hovered() {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(
                rect,
                CornerRadius::same(theme::RADIUS_SMALL),
                palette.surface.gamma_multiply(0.6),
            ),
        );
    }
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, Vec2::new(4.0, rect.height())),
        CornerRadius::same(2),
        palette.outline,
    );
    let spoken = if quote.unavailable {
        t("Message not available").into_owned()
    } else {
        format!("{author}: {}", crate::mrkdwn::unescape(&quote.text))
    };
    theme::describe(&response, egui::WidgetType::Button, &spoken);
    if response.clicked() && !quote.url.is_empty() {
        actions.push(Action::OpenUrl(quote.url.clone()));
    }
}
