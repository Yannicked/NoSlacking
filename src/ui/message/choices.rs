//! An app's Block Kit menus in its messages: static selects, overflow
//! menus and radio buttons, and the elements only Slack itself can use.
//!
//! A choice is sent as a button press is (see [`crate::model::menu_use`]):
//! from a browser session only, after the app's own question when it asks
//! one, busy until Slack takes it.

use egui::{CornerRadius, RichText, Stroke, Vec2};

use super::Row;
use crate::i18n::t;
use crate::model::{Action, Menu, MenuChoice, MenuKind, Message, NotHere, Press, Unusable};
use crate::theme::{self, Icon};

/// Why a menu cannot be used here, in words.
fn not_here_tip(why: NotHere) -> std::borrow::Cow<'static, str> {
    match why {
        NotHere::NeedsSession => t(
            "Slack lets only its own apps and browser sign-ins use an app's menus. Open the message in Slack to use it.",
        ),
        NotHere::NoApp => t("This menu works only in Slack itself."),
    }
}

/// The frame of a select or an overflow menu, as the app's buttons have.
fn framed<'a>(atoms: impl egui::IntoAtoms<'a>, row: &Row<'_>) -> egui::Button<'a> {
    egui::Button::new(atoms)
        .fill(row.palette.surface)
        .stroke(Stroke::new(1.0, row.palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
        .min_size(Vec2::new(28.0, 28.0))
}

/// Sends `choice` from the menu `press` is on, after the app's question;
/// a choice's link opens only once the question is answered yes.
fn choose(press: &Press, menu: &Menu, choice: &MenuChoice, actions: &mut Vec<Action>) {
    actions.push(Action::PressButton {
        press: Box::new(press.choosing(choice)),
        confirm: menu.confirm.clone(),
        confirmed: false,
        link: choice.url.clone(),
    });
}

/// An app's menu in its message. Returns whether it cannot be used here,
/// so the caller offers to open the message in Slack.
pub(super) fn kit_menu(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    menu: &Menu,
    actions: &mut Vec<Action>,
) -> bool {
    let usable = crate::model::menu_use(row.workspace.info.sign_in, row.channel, message, menu);
    // The choice on its way, which shows while Slack has not taken it.
    let busy = usable.as_ref().ok().and_then(|press| {
        row.workspace
            .pressing
            .iter()
            .find(|p| p.same_control(press))
    });
    let chosen = busy
        .and_then(|p| p.value.as_deref())
        .or_else(|| {
            usable
                .as_ref()
                .ok()
                .and_then(|press| row.workspace.chosen(press))
        })
        .or(menu.initial.as_ref().map(|c| c.value.as_str()));
    let standing = Standing {
        usable: &usable,
        busy,
        chosen,
    };
    match menu.kind {
        MenuKind::Select => select(ui, row, message, menu, standing, actions),
        MenuKind::Overflow => overflow(ui, row, message, menu, standing, actions),
        MenuKind::Radio => radio(ui, row, menu, standing, actions),
    }
}

/// Where a menu stands, worked out once for whichever way it is drawn.
#[derive(Clone, Copy)]
struct Standing<'a> {
    /// What choosing sends, or why nothing can be chosen here.
    usable: &'a Result<Press, NotHere>,
    /// The choice on its way, while Slack has not taken it.
    busy: Option<&'a Press>,
    /// The value chosen: on its way, last made, or the app's initial one.
    chosen: Option<&'a str>,
}

/// A drop-down showing the choice made, or the app's placeholder.
fn select(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    menu: &Menu,
    standing: Standing<'_>,
    actions: &mut Vec<Action>,
) -> bool {
    let Standing {
        usable,
        busy,
        chosen,
    } = standing;
    let palette = row.palette;
    // The chosen label, from the menu or, for a choice it no longer lists,
    // from the press or the app's own initial choice.
    let label = chosen
        .and_then(|value| menu.choice(value))
        .map(|choice| choice.text.as_str())
        .or(busy.map(|p| p.text.as_str()))
        .or(menu.initial.as_ref().map(|c| c.text.as_str()));
    let (text, color) = match label {
        Some(text) => (crate::mrkdwn::unescape(text), palette.text),
        None => (
            menu.placeholder
                .as_deref()
                .map(crate::mrkdwn::unescape)
                .unwrap_or_else(|| t("Select an item").into_owned()),
            palette.secondary,
        ),
    };
    let widget = framed(
        (
            RichText::new(text).font(theme::medium(13.0)).color(color),
            Icon::ChevronDown.image(palette.secondary, 14.0),
        ),
        row,
    );
    let press = match usable {
        Err(why) => {
            ui.add_enabled(false, widget)
                .on_disabled_hover_text(not_here_tip(*why));
            return true;
        }
        Ok(_) if busy.is_some() => {
            ui.add_enabled(false, widget)
                .on_disabled_hover_text(t("Waiting for Slack…"));
            ui.add(egui::Spinner::new().size(14.0).color(palette.secondary));
            return false;
        }
        Ok(press) => press,
    };
    let response = ui
        .add(widget)
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let width = response.rect.width().max(180.0);
    egui::Popup::menu(&response)
        .id(menu.popup_id(row.channel, &message.ts, row.in_thread))
        .show(|ui| {
            ui.set_min_width(width);
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .show(ui, |ui| {
                    for group in &menu.groups {
                        if let Some(label) = &group.label {
                            ui.label(
                                RichText::new(crate::mrkdwn::unescape(label))
                                    .font(theme::medium(12.0))
                                    .color(palette.secondary),
                            );
                        }
                        for choice in &group.choices {
                            let selected = chosen == Some(choice.value.as_str());
                            let item = ui.add(egui::Button::selectable(
                                selected,
                                crate::mrkdwn::unescape(&choice.text),
                            ));
                            let item = match &choice.description {
                                Some(description) => {
                                    item.on_hover_text(crate::mrkdwn::unescape(description))
                                }
                                None => item,
                            };
                            if item.clicked() {
                                // Choosing what is already chosen tells the
                                // app nothing new; Slack's client sends
                                // nothing either.
                                if !selected {
                                    choose(press, menu, choice, actions);
                                }
                                ui.close();
                            }
                        }
                    }
                });
        });
    false
}

/// A "⋯" with the app's list of things to do. A choice with a link opens
/// it wherever this is signed in; the rest go to the app.
fn overflow(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    menu: &Menu,
    standing: Standing<'_>,
    actions: &mut Vec<Action>,
) -> bool {
    let Standing { usable, busy, .. } = standing;
    let palette = row.palette;
    let widget = framed(Icon::Ellipsis.image(palette.text, 16.0), row);
    if busy.is_some() {
        ui.add_enabled(false, widget)
            .on_disabled_hover_text(t("Waiting for Slack…"));
        ui.add(egui::Spinner::new().size(14.0).color(palette.secondary));
        return false;
    }
    let links = menu.choices().any(|choice| choice.url.is_some());
    if let Err(why) = usable
        && !links
    {
        ui.add_enabled(false, widget)
            .on_disabled_hover_text(not_here_tip(*why));
        return true;
    }
    let response = ui
        .add(widget)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(t("More options"));
    egui::Popup::menu(&response)
        .id(menu.popup_id(row.channel, &message.ts, row.in_thread))
        .show(|ui| {
            for choice in menu.choices() {
                let works = choice.url.is_some() || usable.is_ok();
                let label = crate::mrkdwn::unescape(&choice.text);
                let item = ui.add_enabled(works, egui::Button::new(label));
                let item = match (usable, &choice.description) {
                    (Err(why), _) if !works => item.on_disabled_hover_text(not_here_tip(*why)),
                    (_, Some(description)) => {
                        item.on_hover_text(crate::mrkdwn::unescape(description))
                    }
                    _ => item,
                };
                if item.clicked() {
                    // Slack opens the link and tells the app too, both
                    // after the app's question. Where the app cannot be
                    // told, the link alone opens: nothing the question is
                    // about happens.
                    match usable {
                        Ok(press) => choose(press, menu, choice, actions),
                        Err(_) => {
                            if let Some(url) = &choice.url {
                                actions.push(Action::OpenUrl(url.clone()));
                            }
                        }
                    }
                    ui.close();
                }
            }
        });
    usable.is_err()
}

/// Every choice in view, the chosen one marked.
fn radio(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    menu: &Menu,
    standing: Standing<'_>,
    actions: &mut Vec<Action>,
) -> bool {
    let Standing {
        usable,
        busy,
        chosen,
    } = standing;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        for choice in menu.choices() {
            let selected = chosen == Some(choice.value.as_str());
            let label = RichText::new(crate::mrkdwn::unescape(&choice.text))
                .font(theme::regular(14.0))
                .color(row.palette.text);
            let enabled = usable.is_ok() && busy.is_none();
            let item = ui.add_enabled(enabled, egui::RadioButton::new(selected, label));
            let item = match (usable, busy) {
                (Err(why), _) => item.on_disabled_hover_text(not_here_tip(*why)),
                (Ok(_), Some(_)) => item.on_disabled_hover_text(t("Waiting for Slack…")),
                _ => item,
            };
            if let Some(description) = &choice.description {
                ui.label(
                    RichText::new(crate::mrkdwn::unescape(description))
                        .font(theme::regular(12.5))
                        .color(row.palette.secondary),
                );
            }
            if item.clicked()
                && !selected
                && let Ok(press) = usable
            {
                choose(press, menu, choice, actions);
            }
        }
        if busy.is_some() {
            ui.add(egui::Spinner::new().size(14.0).color(row.palette.secondary));
        }
    });
    usable.is_err()
}

/// An element only Slack itself can use, such as a select whose choices
/// come from the app, a date picker or a form field: shown by what it
/// says, never usable. Always wants "Open in Slack" beside it.
pub(super) fn kit_unusable(ui: &mut egui::Ui, row: &Row<'_>, element: &Unusable) -> bool {
    let palette = row.palette;
    let label = element
        .label
        .as_deref()
        .map(crate::mrkdwn::unescape)
        .unwrap_or_else(|| unusable_name(&element.kind).into_owned());
    let mut atoms = egui::Atoms::new(
        RichText::new(label)
            .font(theme::medium(13.0))
            .color(palette.secondary),
    );
    if element.kind.ends_with("_select") {
        atoms.push_right(Icon::ChevronDown.image(palette.secondary, 14.0));
    }
    ui.add_enabled(false, framed(atoms, row))
        .on_disabled_hover_text(t(
            "Only Slack itself can use this here. Open the message in Slack to use it.",
        ));
    true
}

/// What an element is called when the app gave it no label.
fn unusable_name(kind: &str) -> std::borrow::Cow<'static, str> {
    match kind {
        "datepicker" => t("Pick a date"),
        "timepicker" => t("Pick a time"),
        "datetimepicker" => t("Pick a date and time"),
        "checkboxes" => t("Checkboxes"),
        kind if kind.ends_with("_select") => t("Select an item"),
        _ => t("A form field"),
    }
}
