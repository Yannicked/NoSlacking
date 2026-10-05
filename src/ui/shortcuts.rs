//! The keyboard shortcut sheet, opened with Ctrl+/ (⌘/) or from the
//! settings, and the one list of shortcuts it shows.
//!
//! The list is data, kept here and nowhere else. Its tests read the key
//! handling in `src/ui` and the shortcuts its docs name, so a key added
//! there without a line here fails `cargo test`.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use crate::app::App;
use crate::i18n::{t, tf};
use crate::theme::{self, Palette};

/// When a shortcut applies, for those that hang on a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Always,
    /// With "Enter sends" on.
    EnterSends,
    /// With "Enter sends" off.
    EnterNewLine,
}

/// One line of the sheet.
#[derive(Debug)]
pub struct Shortcut {
    /// What it does, in English: the sheet shows its translation.
    pub label: &'static str,
    /// The chords shown, each its modifiers and egui's name for the key,
    /// such as `Cmd+Shift+J`: `Cmd` is Ctrl, or ⌘ on macOS.
    pub keys: &'static [&'static str],
    /// Chords handled as well but not worth a cap of their own, such as
    /// Ctrl+Shift+= for zooming in where `+` needs Shift: they show on
    /// hover.
    pub also: &'static [&'static str],
    pub when: When,
}

/// A heading of the sheet and its lines.
#[derive(Debug)]
pub struct Group {
    pub title: &'static str,
    pub shortcuts: &'static [Shortcut],
}

const fn line(label: &'static str, keys: &'static [&'static str]) -> Shortcut {
    Shortcut {
        label,
        keys,
        also: &[],
        when: When::Always,
    }
}

/// Every shortcut the app handles, by where it works.
pub const GROUPS: &[Group] = &[
    Group {
        title: "Navigation",
        shortcuts: &[
            line("Jump to a conversation", &["Cmd+K"]),
            line("Search messages and files", &["Cmd+F"]),
            line("New message", &["Cmd+N"]),
            line("Browse channels", &["Cmd+Shift+L"]),
            line(
                "Previous / next conversation",
                &["Alt+ArrowUp", "Alt+ArrowDown"],
            ),
            line(
                "Previous / next unread conversation",
                &["Alt+Shift+ArrowUp", "Alt+Shift+ArrowDown"],
            ),
            line("Jump to the first unread message", &["Cmd+J"]),
            line("Jump to the newest messages", &["Cmd+Shift+J", "End"]),
            line("Activity", &["Cmd+Shift+M"]),
            line("All unreads", &["Cmd+Shift+A"]),
            line("Threads", &["Cmd+Shift+T"]),
            line("Later", &["Cmd+Shift+S"]),
        ],
    },
    Group {
        title: "Messages",
        shortcuts: &[
            line("Select the last message", &["ArrowUp"]),
            line(
                "Select the last message, from an empty composer",
                &["Shift+ArrowUp"],
            ),
            line("Previous / next message", &["ArrowUp", "ArrowDown"]),
            line("Add a reaction", &["R"]),
            line("Reply in thread", &["T"]),
            line("Edit your message", &["E"]),
            line("Delete your message", &["Delete", "Backspace"]),
            line("Copy the text", &["C"]),
            line("Mark unread from here", &["U"]),
            line("Back to the composer", &["Escape"]),
        ],
    },
    Group {
        title: "Composer",
        shortcuts: &[
            Shortcut {
                when: When::EnterSends,
                ..line("Send, or save an edit", &["Enter"])
            },
            Shortcut {
                when: When::EnterNewLine,
                ..line("Send, or save an edit", &["Cmd+Enter"])
            },
            Shortcut {
                when: When::EnterSends,
                ..line("New line", &["Shift+Enter"])
            },
            Shortcut {
                when: When::EnterNewLine,
                ..line("New line", &["Enter", "Shift+Enter"])
            },
            line(
                "Edit your last message, from an empty composer",
                &["ArrowUp"],
            ),
            line("Bold", &["Cmd+B"]),
            line("Italic", &["Cmd+I"]),
            line("Strikethrough", &["Cmd+Shift+X"]),
            line("Code", &["Cmd+Shift+C"]),
            line("Pick a suggestion", &["ArrowUp", "ArrowDown"]),
            line("Accept a suggestion", &["Tab", "Enter"]),
            line("Close the suggestions, or cancel an edit", &["Escape"]),
        ],
    },
    Group {
        title: "Image viewer",
        shortcuts: &[
            line("Previous / next image", &["ArrowLeft", "ArrowRight"]),
            Shortcut {
                also: &["Shift+Plus", "Equals", "Shift+Equals"],
                ..line("Zoom in / out", &["Plus", "Minus"])
            },
            line("Fit to the window", &["Num0"]),
            line("Close the viewer", &["Escape"]),
        ],
    },
    Group {
        title: "Window",
        shortcuts: &[
            line("Keyboard shortcuts", &["Cmd+Slash"]),
            line("Settings", &["Cmd+Comma"]),
            Shortcut {
                also: &["Cmd+Shift+Equals", "Cmd+Plus"],
                ..line("Zoom in / out", &["Cmd+Equals", "Cmd+Minus"])
            },
            line("Actual size", &["Cmd+Num0"]),
            line(
                "Move through a list: switcher, search, browser",
                &["ArrowUp", "ArrowDown"],
            ),
            line("Open what is picked", &["Enter"]),
            line("Close the thread or the open dialog", &["Escape"]),
        ],
    },
];

/// The lines of `group` that apply with this "Enter sends".
pub fn shown(group: &Group, enter_sends: bool) -> impl Iterator<Item = &Shortcut> {
    group.shortcuts.iter().filter(move |s| match s.when {
        When::Always => true,
        When::EnterSends => enter_sends,
        When::EnterNewLine => !enter_sends,
    })
}

/// How the sheet spells a key, by egui's name for it.
fn key_name(key: &str) -> &str {
    match key {
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        "Escape" => "Esc",
        "Plus" => "+",
        "Minus" => "-",
        "Equals" => "=",
        "Comma" => ",",
        "Slash" => "/",
        "Num0" => "0",
        other => other,
    }
}

/// A chord as the platform writes it: "⇧⌘J" on macOS (`mac`), where
/// modifiers are symbols in Apple's order, and "Ctrl+Shift+J" elsewhere,
/// as the other hints spell them (see [`super::keys::command`]).
pub fn spell(chord: &str, mac: bool) -> String {
    let mut parts: Vec<&str> = chord.split('+').collect();
    let key = key_name(parts.pop().unwrap_or_default());
    let has = |name: &str| parts.contains(&name);
    if mac {
        let mut out = String::new();
        for (name, symbol) in [("Alt", "⌥"), ("Shift", "⇧"), ("Cmd", "⌘")] {
            if has(name) {
                out.push_str(symbol);
            }
        }
        out.push_str(key);
        out
    } else {
        let mut out: Vec<&str> = [("Cmd", "Ctrl"), ("Alt", "Alt"), ("Shift", "Shift")]
            .into_iter()
            .filter(|(name, _)| has(name))
            .map(|(_, word)| word)
            .collect();
        out.push(key);
        out.join("+")
    }
}

/// The sheet, while open. Esc, a click beside it or Close shuts it.
pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.shortcuts {
        return;
    }
    let palette = app.palette;
    let enter_sends = app.settings.enter_sends;
    let mac = cfg!(target_os = "macos");
    let mut close = false;
    let tall = (ctx.content_rect().height() * 0.7).clamp(240.0, 620.0);
    let response = egui::Modal::new(egui::Id::new("shortcuts"))
        .frame(super::overlays::modal_frame(app))
        .show(ctx, |ui| {
            ui.set_width(540.0);
            ui.label(
                RichText::new(t("Keyboard shortcuts"))
                    .font(theme::bold(17.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(tf(
                    "Open this list any time with {shortcut}.",
                    &[("shortcut", &spell("Cmd+Slash", mac))],
                ))
                .font(theme::regular(13.0))
                .color(palette.secondary),
            );
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .max_height(tall)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    // Room for the scroll bar beside the caps.
                    ui.set_max_width(ui.available_width() - 12.0);
                    for group in GROUPS {
                        section(ui, &palette, group, enter_sends, mac);
                    }
                });
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::secondary_button(ui, &palette, &t("Close")).clicked() {
                    close = true;
                }
            });
        });
    if close || response.should_close() {
        app.shortcuts = false;
    }
}

/// A group's heading and its lines, on a card.
fn section(ui: &mut egui::Ui, palette: &Palette, group: &Group, enter_sends: bool, mac: bool) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(t(group.title))
            .font(theme::semibold(13.0))
            .color(palette.secondary),
    );
    ui.add_space(4.0);
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 2))
        .inner_margin(Margin::symmetric(12, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for (index, shortcut) in shown(group, enter_sends).enumerate() {
                if index > 0 {
                    let x = ui.max_rect().x_range();
                    let y = ui.cursor().top();
                    ui.painter()
                        .hline(x, y, Stroke::new(1.0, palette.outline.gamma_multiply(0.6)));
                }
                row(ui, palette, shortcut, mac);
            }
        });
    ui.add_space(6.0);
}

/// What a shortcut does on the left, its keys on the right.
fn row(ui: &mut egui::Ui, palette: &Palette, shortcut: &Shortcut, mac: bool) {
    let keys: Vec<String> = shortcut.keys.iter().map(|k| spell(k, mac)).collect();
    let also: Vec<String> = shortcut.also.iter().map(|k| spell(k, mac)).collect();
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 30.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_height(30.0);
            let caps = Caps::new(ui, palette, &keys);
            let width = (ui.available_width() - caps.size.x - 16.0).max(120.0);
            ui.allocate_ui_with_layout(
                Vec2::new(width, 30.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_width(width);
                    ui.add(
                        egui::Label::new(
                            RichText::new(t(shortcut.label))
                                .font(theme::regular(14.0))
                                .color(palette.text),
                        )
                        .wrap(),
                    );
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                caps.paint(ui, palette, &keys, &also);
            });
        },
    );
}

/// The keys as small raised caps, "/" between alternatives, laid out
/// first so the label beside them knows how much room is left.
struct Caps {
    keys: Vec<std::sync::Arc<egui::Galley>>,
    slash: std::sync::Arc<egui::Galley>,
    size: Vec2,
}

/// Inside a cap, around its text.
const CAP_PAD: Vec2 = Vec2::new(7.0, 3.0);
/// Either side of the "/" between caps.
const CAP_GAP: f32 = 6.0;

impl Caps {
    fn new(ui: &egui::Ui, palette: &Palette, keys: &[String]) -> Self {
        let font = theme::medium(12.5);
        let keys: Vec<_> = keys
            .iter()
            .map(|k| {
                ui.painter()
                    .layout_no_wrap(k.clone(), font.clone(), palette.text)
            })
            .collect();
        let slash = ui
            .painter()
            .layout_no_wrap("/".to_owned(), font, palette.dim);
        let height = keys.iter().map(|g| g.size().y).fold(0.0_f32, f32::max) + CAP_PAD.y * 2.0;
        let width = keys
            .iter()
            .map(|g| g.size().x + CAP_PAD.x * 2.0)
            .sum::<f32>()
            + keys.len().saturating_sub(1) as f32 * (slash.size().x + CAP_GAP * 2.0);
        Self {
            keys,
            slash,
            size: Vec2::new(width, height),
        }
    }

    /// Paints the caps, read out as one label (`spoken`), with the `also`
    /// keys on hover.
    fn paint(self, ui: &mut egui::Ui, palette: &Palette, spoken: &[String], also: &[String]) {
        let (rect, response) = ui.allocate_exact_size(self.size, Sense::hover());
        let mut x = rect.left();
        for (index, galley) in self.keys.into_iter().enumerate() {
            if index > 0 {
                x += CAP_GAP;
                ui.painter().galley(
                    egui::pos2(x, rect.center().y - self.slash.size().y / 2.0),
                    self.slash.clone(),
                    palette.dim,
                );
                x += self.slash.size().x + CAP_GAP;
            }
            let cap = egui::Rect::from_min_size(
                egui::pos2(x, rect.top()),
                Vec2::new(galley.size().x + CAP_PAD.x * 2.0, self.size.y),
            );
            ui.painter().rect(
                cap,
                CornerRadius::same(theme::RADIUS_SMALL),
                palette.overlay,
                Stroke::new(1.0, palette.outline),
                egui::StrokeKind::Inside,
            );
            ui.painter().galley(
                egui::pos2(
                    cap.left() + CAP_PAD.x,
                    cap.center().y - galley.size().y / 2.0,
                ),
                galley,
                palette.text,
            );
            x = cap.right();
        }
        theme::describe(&response, egui::WidgetType::Label, &spoken.join(" / "));
        if !also.is_empty() {
            response.on_hover_text(tf("Also {keys}", &[("keys", &also.join(" / "))]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use std::collections::BTreeSet;

    /// Every chord in the table, shown or not, whatever the setting.
    fn listed() -> BTreeSet<String> {
        GROUPS
            .iter()
            .flat_map(|g| g.shortcuts)
            .flat_map(|s| s.keys.iter().chain(s.also))
            .map(|k| (*k).to_owned())
            .collect()
    }

    /// The key a chord ends with.
    fn key_of(chord: &str) -> &str {
        chord.rsplit('+').next().unwrap_or_default()
    }

    /// Writes modifiers and a key the way the table does.
    fn chord(mods: &BTreeSet<&str>, key: &str) -> String {
        let mut parts: Vec<&str> = ["Cmd", "Alt", "Shift"]
            .into_iter()
            .filter(|m| mods.contains(m))
            .collect();
        parts.push(key);
        parts.join("+")
    }

    /// Each `.rs` file under `src/ui`, without its tests.
    fn ui_sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("read src/ui").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).expect("read file");
                    let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                    out.push((path.display().to_string(), code.to_owned()));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui"),
            &mut out,
        );
        out
    }

    /// The arguments of the call opened just before `rest`, up to its
    /// closing bracket.
    fn arguments(rest: &str) -> &str {
        let mut depth = 1;
        for (at, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &rest[..at];
                    }
                }
                _ => {}
            }
        }
        rest
    }

    /// The names following `prefix` in `text`, as `Key::Name`.
    fn names<'a>(text: &'a str, prefix: &str) -> Vec<&'a str> {
        text.match_indices(prefix)
            .map(|(at, _)| {
                let rest = &text[at + prefix.len()..];
                let end = rest
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .collect()
    }

    #[test]
    fn every_key_the_interface_takes_is_listed() {
        let listed = listed();
        let keys: BTreeSet<&str> = listed.iter().map(|c| key_of(c)).collect();
        let mut missing = Vec::new();
        let mut found = 0;
        for (file, code) in ui_sources() {
            for call in ["consume_key(", "take(input, "] {
                for (at, _) in code.match_indices(call) {
                    let args = arguments(&code[at + call.len()..]);
                    let Some(key) = names(args, "Key::").first().copied() else {
                        // The key is a variable (the views' letters, tested
                        // below) or this is a helper's own body.
                        continue;
                    };
                    found += 1;
                    let mods = names(args, "Modifiers::");
                    if mods.is_empty() {
                        // The modifiers are a variable: the key at least.
                        if !keys.contains(key) {
                            missing.push(format!("{file}: {key}"));
                        }
                        continue;
                    }
                    let mods: BTreeSet<&str> = mods
                        .into_iter()
                        .filter(|m| *m != "NONE")
                        .map(|m| match m {
                            "COMMAND" | "CTRL" | "MAC_CMD" => "Cmd",
                            "ALT" => "Alt",
                            "SHIFT" => "Shift",
                            other => panic!("{file}: unknown modifier {other}"),
                        })
                        .collect();
                    let wanted = chord(&mods, key);
                    if !listed.contains(&wanted) {
                        missing.push(format!("{file}: {wanted}"));
                    }
                }
            }
        }
        assert!(found > 30, "the scan found only {found} keys");
        assert!(missing.is_empty(), "not on the sheet: {missing:#?}");
    }

    #[test]
    fn every_view_shortcut_is_listed() {
        let listed = listed();
        for view in crate::views::View::ALL {
            if let Some(key) = super::super::views::shortcut(view) {
                let wanted = format!("Cmd+Shift+{key:?}");
                assert!(listed.contains(&wanted), "not on the sheet: {wanted}");
            }
        }
    }

    /// The shortcuts named in the docs of the key handlers, such as
    /// "Ctrl+Shift+J" or "Alt+↑", in the table's spelling.
    fn documented() -> Vec<String> {
        let mut out = Vec::new();
        for (file, code) in ui_sources() {
            let docs = code
                .lines()
                .map(str::trim_start)
                .filter(|l| l.starts_with("//!") || l.starts_with("///"));
            for doc in docs {
                let mut rest = doc;
                while let Some(at) = ["Ctrl+", "Cmd+", "Alt+", "Shift+"]
                    .iter()
                    .filter_map(|m| rest.find(m))
                    .min()
                {
                    // Part of a word, such as "Ctrl+Shift" already read.
                    let before = rest[..at].chars().next_back();
                    let mut mods = BTreeSet::new();
                    let mut tail = &rest[at..];
                    loop {
                        let found = [
                            ("Ctrl+", "Cmd"),
                            ("Cmd+", "Cmd"),
                            ("Alt+", "Alt"),
                            ("Shift+", "Shift"),
                        ]
                        .into_iter()
                        .find(|(word, _)| tail.starts_with(word));
                        let Some((word, name)) = found else { break };
                        mods.insert(name);
                        tail = &tail[word.len()..];
                    }
                    let length = if tail.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                        tail.find(|c: char| !c.is_ascii_alphanumeric())
                            .unwrap_or(tail.len())
                    } else {
                        tail.chars().next().map_or(0, char::len_utf8)
                    };
                    let word = &tail[..length];
                    rest = &tail[length..];
                    // "Ctrl+Shift" alone names modifiers, not a shortcut.
                    let modifier = matches!(word, "Ctrl" | "Cmd" | "Alt" | "Shift");
                    if before.is_some_and(char::is_alphanumeric) || word.is_empty() || modifier {
                        continue;
                    }
                    let key = match word {
                        "↑" => "ArrowUp",
                        "↓" => "ArrowDown",
                        "←" => "ArrowLeft",
                        "→" => "ArrowRight",
                        "=" => "Equals",
                        "-" => "Minus",
                        "," => "Comma",
                        "/" => "Slash",
                        "0" => "Num0",
                        "Esc" => "Escape",
                        other => other,
                    };
                    out.push(format!("{}: {}", file, chord(&mods, key)));
                }
            }
        }
        out
    }

    #[test]
    fn every_documented_shortcut_is_listed() {
        let listed = listed();
        let documented = documented();
        assert!(
            documented.iter().any(|d| d.ends_with(": Cmd+K")),
            "the doc scan missed Ctrl+K: {documented:#?}"
        );
        let missing: Vec<&String> = documented
            .iter()
            .filter(|d| {
                let chord = d.rsplit(": ").next().unwrap_or_default();
                !listed.contains(chord)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "documented, not on the sheet: {missing:#?}"
        );
    }

    #[test]
    fn chords_use_real_keys_in_the_tables_order() {
        let real: BTreeSet<String> = egui::Key::ALL.iter().map(|k| format!("{k:?}")).collect();
        for chord in listed() {
            let mut parts: Vec<&str> = chord.split('+').collect();
            let key = parts.pop().unwrap_or_default();
            assert!(real.contains(key), "{chord}: no such key");
            let mods: BTreeSet<&str> = parts.iter().copied().collect();
            assert_eq!(
                self::chord(&mods, key),
                chord,
                "modifiers known and in order"
            );
        }
    }

    #[test]
    fn every_label_has_a_dutch_translation() {
        let po = include_str!("../../assets/i18n/nl.po").replace("\r\n", "\n");
        let labels = GROUPS
            .iter()
            .map(|g| g.title)
            .chain(GROUPS.iter().flat_map(|g| g.shortcuts).map(|s| s.label));
        let missing: Vec<&str> = labels
            .filter(|label| {
                let dutch = fastframe_i18n::gettext(Locale::Dutch, label);
                dutch == *label && !po.contains(&format!("msgid \"{label}\"\n"))
            })
            .collect();
        assert!(missing.is_empty(), "not in nl.po: {missing:#?}");
    }

    #[test]
    fn enter_lines_follow_the_setting() {
        let composer = GROUPS
            .iter()
            .find(|g| g.title == "Composer")
            .expect("a composer group");
        let send = |enter_sends| {
            shown(composer, enter_sends)
                .find(|s| s.label == "Send, or save an edit")
                .map(|s| s.keys)
        };
        assert_eq!(send(true), Some(&["Enter"][..]));
        assert_eq!(send(false), Some(&["Cmd+Enter"][..]));
    }

    #[test]
    fn chords_are_spelled_per_platform() {
        assert_eq!(spell("Cmd+Shift+J", false), "Ctrl+Shift+J");
        assert_eq!(spell("Cmd+Shift+J", true), "⇧⌘J");
        assert_eq!(spell("Alt+Shift+ArrowUp", false), "Alt+Shift+↑");
        assert_eq!(spell("Alt+ArrowUp", true), "⌥↑");
        assert_eq!(spell("Cmd+Slash", false), "Ctrl+/");
        assert_eq!(spell("Escape", true), "Esc");
    }
}
