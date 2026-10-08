//! Lists too long to lay out whole on every frame: where each row starts,
//! and which rows a scrolled view can see, so only those are drawn.

use std::ops::Range;

/// Where each row starts, from the rows' heights: one entry per row and a
/// last one for the bottom of the list, so `tops[i + 1] - tops[i]` is row
/// `i`'s height and `tops.last()` the whole height.
pub fn tops(heights: impl IntoIterator<Item = f32>) -> Vec<f32> {
    let heights = heights.into_iter();
    let mut tops = Vec::with_capacity(heights.size_hint().0 + 1);
    let mut y = 0.0;
    tops.push(y);
    for height in heights {
        y += height.max(0.0);
        tops.push(y);
    }
    tops
}

/// The rows that overlap `min..max` (in the list's own coordinates, top at
/// zero), for `tops` from [`tops`]. A row that only touches the range at an
/// edge is left out.
pub fn visible(tops: &[f32], min: f32, max: f32) -> Range<usize> {
    let rows = tops.len().saturating_sub(1);
    if rows == 0 || max <= min {
        return 0..0;
    }
    // The first row whose bottom is below `min`, and the first row whose
    // top is at or below `max`.
    let first = tops[1..].partition_point(|&bottom| bottom <= min);
    let end = tops[..rows].partition_point(|&top| top < max);
    first.min(rows)..end.max(first.min(rows))
}

/// One row of a list whose rows differ in height: what tells it apart from
/// the others, and a guess at its height for before it has been drawn.
///
/// Rows in and near the view are drawn, and so measured, on every frame,
/// so an edit, a new reaction, a picture loading or a new width corrects
/// a row's height as soon as it comes near the view; until then its last
/// height is the best guess there is. That is why heights are kept by key
/// alone, without a version of what they depend on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    /// Stays with the row as rows come and go around it: a hash of a
    /// message's timestamp, say.
    pub key: u64,
    pub guess: f32,
}

#[derive(Clone, Copy, Debug)]
struct Measured {
    height: f32,
    seen: bool,
}

/// The heights rows had when last drawn, by [`Entry::key`], so a long list
/// can be placed without laying out the rows out of view.
#[derive(Clone, Debug, Default)]
pub struct Heights {
    rows: std::collections::HashMap<u64, Measured>,
    /// What the heights were measured under (the message density, say).
    layout: u64,
}

impl Heights {
    /// Forgets every height when the rows are now drawn differently than
    /// when they were measured: a height from the old layout would place
    /// the rows out of view wrongly until each was drawn again.
    pub fn for_layout(&mut self, layout: u64) {
        if self.layout != layout {
            self.rows.clear();
            self.layout = layout;
        }
    }

    /// The height to place `entry` with: as last drawn, else its guess.
    pub fn planned(&mut self, entry: &Entry) -> f32 {
        match self.rows.get_mut(&entry.key) {
            Some(measured) => {
                measured.seen = true;
                measured.height
            }
            None => entry.guess,
        }
    }

    /// Notes the height `entry` was just drawn at.
    pub fn record(&mut self, entry: &Entry, height: f32) {
        self.rows.insert(entry.key, Measured { height, seen: true });
    }

    /// Forgets the rows not placed since the last sweep: messages that
    /// left the list.
    pub fn sweep(&mut self) {
        self.rows.retain(|_, m| std::mem::take(&mut m.seen));
    }
}

/// Which rows of a list to draw for a view of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Where each row starts, as [`tops`] gives.
    pub tops: Vec<f32>,
    /// The rows to draw: those in view, and a margin either side so a
    /// little scrolling finds them already measured.
    pub draw: Range<usize>,
    /// The first row that starts inside the view, the one a reader's eye
    /// is on: rows above it that turn out taller or shorter than planned
    /// must move the view by as much, or the anchor would jump.
    pub anchor: usize,
}

/// Plans a view from `min` to `max` of rows `heights` tall, drawing
/// `margin` more on either side.
pub fn plan(heights: impl IntoIterator<Item = f32>, min: f32, max: f32, margin: f32) -> Plan {
    let tops = tops(heights);
    let draw = visible(&tops, min - margin, max + margin);
    let rows = tops.len() - 1;
    let anchor = tops[..rows]
        .partition_point(|&top| top < min)
        .max(draw.start);
    Plan { tops, draw, anchor }
}

/// Draws the rows of `entries` that `plan` picked, each with `draw`, in a
/// list whose content is as tall as all the rows, remembering the height
/// each turned out to be. Returns how much taller the rows above the
/// anchor turned out than planned (negative for shorter): what the view
/// must scroll by to keep the anchor still.
///
/// When a row drawn turns out another height than planned (one never drawn
/// before, or one whose picture just loaded), the whole frame is asked to
/// be drawn again before it is shown: placed by guesses, the list would
/// show for one frame where it does not end up, and jump.
pub fn show(
    ui: &mut egui::Ui,
    heights: &mut Heights,
    entries: &[Entry],
    plan: &Plan,
    mut draw: impl FnMut(&mut egui::Ui, usize),
) -> f32 {
    let total = plan.tops.last().copied().unwrap_or(0.0);
    ui.add_space(plan.tops[plan.draw.start]);
    let mut moved = 0.0;
    let mut settled = true;
    for index in plan.draw.clone() {
        let entry = &entries[index];
        let top = ui.cursor().top();
        // Ids that stay with the row, whatever is drawn before it.
        ui.push_id(entry.key, |ui| draw(ui, index));
        let height = ui.cursor().top() - top;
        let planned = plan.tops[index + 1] - plan.tops[index];
        if index < plan.anchor {
            moved += height - planned;
        }
        settled &= (height - planned).abs() < 0.5;
        heights.record(entry, height);
    }
    ui.add_space(total - plan.tops[plan.draw.end]);
    if !settled {
        // egui allows a frame only so many passes, so this cannot loop.
        ui.ctx().request_discard("a list's rows changed height");
    }
    moved
}

/// How far beyond the view rows are still drawn, so they are measured
/// before they scroll in.
pub const MARGIN: f32 = 400.0;

/// What [`virtual_list`] drew.
pub struct Drawn {
    /// Where each row starts, and the list's bottom last, as planned.
    pub tops: Vec<f32>,
    /// How much taller the rows above the one being read turned out than
    /// planned (see [`show`]).
    pub moved: f32,
}

/// Draws the rows of `entries` in and near `viewport` (a scroll area's,
/// inside it) with `draw`, the rest placed by the heights they were last
/// drawn at. The heights are kept between frames under `id`, and
/// forgotten when `layout` (what they were measured under) changes.
pub fn virtual_list(
    ui: &mut egui::Ui,
    id: egui::Id,
    layout: u64,
    viewport: egui::Rect,
    entries: &[Entry],
    draw: impl FnMut(&mut egui::Ui, usize),
) -> Drawn {
    let mut heights: Heights = ui.data_mut(|d| d.remove_temp(id)).unwrap_or_default();
    heights.for_layout(layout);
    let plan = plan(
        entries.iter().map(|entry| heights.planned(entry)),
        viewport.min.y,
        viewport.max.y,
        MARGIN,
    );
    heights.sweep();
    let moved = show(ui, &mut heights, entries, &plan, draw);
    ui.data_mut(|d| d.insert_temp(id, heights));
    Drawn {
        tops: plan.tops,
        moved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heights_remember_what_was_drawn_and_forget_what_left() {
        let mut heights = Heights::default();
        let a = Entry {
            key: 1,
            guess: 50.0,
        };
        assert_eq!(heights.planned(&a), 50.0, "a guess until drawn");
        heights.record(&a, 72.0);
        assert_eq!(heights.planned(&a), 72.0);
        heights.sweep();
        assert_eq!(heights.planned(&a), 72.0, "placed since the sweep");
        heights.sweep();
        heights.sweep();
        assert_eq!(heights.planned(&a), 50.0, "gone once not placed");
    }

    #[test]
    fn a_new_layout_forgets_old_heights() {
        let mut heights = Heights::default();
        let a = Entry {
            key: 1,
            guess: 50.0,
        };
        heights.for_layout(7);
        heights.record(&a, 72.0);
        heights.for_layout(7);
        assert_eq!(heights.planned(&a), 72.0, "same layout keeps them");
        heights.for_layout(8);
        assert_eq!(heights.planned(&a), 50.0, "back to the guess");
    }

    #[test]
    fn a_plan_draws_the_view_and_a_margin() {
        // Ten rows of 100: the view 250..450 sees rows 2, 3 and 4.
        let plan = plan(std::iter::repeat_n(100.0, 10), 250.0, 450.0, 0.0);
        assert_eq!(plan.draw, 2..5);
        assert_eq!(plan.anchor, 3, "row 3 is the first to start in view");
        assert_eq!(plan.tops.last(), Some(&1000.0));
        let wide = super::plan(std::iter::repeat_n(100.0, 10), 250.0, 450.0, 120.0);
        assert_eq!(wide.draw, 1..6);
        assert_eq!(wide.anchor, 3);
        // At the very bottom, the anchor is past the last row started.
        let bottom = super::plan(std::iter::repeat_n(100.0, 3), 150.0, 300.0, 0.0);
        assert_eq!(bottom.draw, 1..3);
        assert_eq!(bottom.anchor, 2);
        let empty = super::plan(std::iter::empty(), 0.0, 300.0, 50.0);
        assert_eq!(empty.draw, 0..0);
        assert_eq!(empty.anchor, 0);
    }

    #[test]
    fn tops_add_up_the_heights() {
        assert_eq!(tops([10.0, 20.0, 5.0]), [0.0, 10.0, 30.0, 35.0]);
        assert_eq!(tops([]), [0.0]);
        // A negative height cannot move later rows up.
        assert_eq!(tops([10.0, -4.0, 1.0]), [0.0, 10.0, 10.0, 11.0]);
    }

    #[test]
    fn only_rows_in_view_are_visible() {
        let tops = tops([10.0, 20.0, 5.0, 40.0]);
        // Rows: 0..10, 10..30, 30..35, 35..75.
        assert_eq!(visible(&tops, 0.0, 10.0), 0..1);
        assert_eq!(visible(&tops, 5.0, 31.0), 0..3);
        assert_eq!(visible(&tops, 10.0, 30.0), 1..2, "edges only touch");
        assert_eq!(visible(&tops, 36.0, 1000.0), 3..4);
        assert_eq!(visible(&tops, 80.0, 90.0), 4..4, "past the end");
        assert_eq!(visible(&tops, -50.0, -10.0), 0..0, "before the start");
        assert_eq!(visible(&tops, 20.0, 20.0), 0..0, "an empty view");
        assert_eq!(visible(&[0.0], 0.0, 10.0), 0..0, "an empty list");
    }
}
