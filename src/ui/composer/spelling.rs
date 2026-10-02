//! Red squiggles under misspelt words in the composer, and their spellings
//! on a right click.

use std::ops::Range;

use egui::text::{CCursor, CCursorRange};
use egui::{Pos2, Stroke};

use crate::app::Draft;
use crate::i18n::t;
use crate::theme::Palette;

/// Drafts longer than this are not checked as they are typed.
const MAX_CHECKED: usize = 20_000;

/// What a right click landed on: the word's byte range and its spellings.
#[derive(Clone, Debug, Default)]
struct Target {
    range: Range<usize>,
    word: String,
    spellings: Vec<String>,
}

/// Marks the misspelt words of the field `id` drawn as `output`, and on
/// a right click on one offers its spellings, replacing it with the one
/// picked.
pub(super) fn show(
    ui: &egui::Ui,
    output: &egui::text_edit::TextEditOutput,
    id: egui::Id,
    draft: &mut Draft,
    palette: &Palette,
) {
    if draft.text.len() > MAX_CHECKED {
        return;
    }
    let mut misspelt = crate::spell::misspelt(&draft.text);
    // The word being typed is not wrong yet.
    let typing = output
        .cursor_range
        .filter(|_| output.response.has_focus())
        .map(|range| byte_of(&draft.text, range.primary.index.into()));
    misspelt.retain(|range| typing.is_none_or(|at| !(range.start..=range.end).contains(&at)));
    let painter = ui.painter().with_clip_rect(output.text_clip_rect);
    for range in &misspelt {
        squiggle(&painter, output, &draft.text, range, palette);
    }
    let target_id = id.with("spelling-target");
    if output.response.secondary_clicked()
        && let Some(pointer) = output.response.interact_pointer_pos()
    {
        let at = output.galley.cursor_from_pos(pointer - output.galley_pos);
        let byte = byte_of(&draft.text, at.index.into());
        let target = misspelt
            .iter()
            .find(|range| range.start <= byte && byte < range.end)
            .map(|range| Target {
                range: range.clone(),
                word: draft.text[range.clone()].to_owned(),
                spellings: crate::spell::suggestions(&draft.text[range.clone()]),
            });
        ui.data_mut(|data| match target {
            Some(target) => {
                data.insert_temp(target_id, target);
            }
            None => data.remove::<Target>(target_id),
        });
    }
    let Some(target) = ui.data(|data| data.get_temp::<Target>(target_id)) else {
        return;
    };
    let mut picked = None;
    output.response.context_menu(|ui| {
        if target.spellings.is_empty() {
            ui.label(t("No spellings found"));
        }
        for spelling in &target.spellings {
            if ui.button(spelling).clicked() {
                picked = Some(spelling.clone());
                ui.close();
            }
        }
    });
    if let Some(spelling) = picked {
        // Only if the word is still where it was when the menu opened.
        if draft.text.get(target.range.clone()) == Some(target.word.as_str()) {
            draft.text.replace_range(target.range.clone(), &spelling);
            let end = draft.text[..target.range.start + spelling.len()]
                .chars()
                .count();
            let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(end))));
            state.store(ui.ctx(), id);
            ui.memory_mut(|memory| memory.request_focus(id));
        }
        ui.data_mut(|data| data.remove::<Target>(target_id));
    }
}

/// The byte offset of character `index` in `text`.
fn byte_of(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// A wavy line under the characters of `range`, row by row.
fn squiggle(
    painter: &egui::Painter,
    output: &egui::text_edit::TextEditOutput,
    text: &str,
    range: &Range<usize>,
    palette: &Palette,
) {
    let first = text[..range.start].chars().count();
    let last = first + text[range.clone()].chars().count();
    let start = output.galley.pos_from_cursor(CCursor::new(first));
    let end = output.galley.pos_from_cursor(CCursor::new(last));
    // A word wrapped over two rows is rare enough to mark on its first.
    let right = if (end.bottom() - start.bottom()).abs() < 1.0 {
        end.left()
    } else {
        output.galley.rect.right()
    };
    let origin = output.galley_pos.to_vec2();
    let y = start.bottom() + origin.y - 1.0;
    let (left, right) = (start.left() + origin.x, right + origin.x);
    let step = 2.0;
    let mut points = Vec::new();
    let mut x = left;
    let mut up = false;
    while x <= right {
        points.push(Pos2::new(x, if up { y - 1.0 } else { y + 1.0 }));
        up = !up;
        x += step;
    }
    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(1.0, palette.danger)));
    }
}
