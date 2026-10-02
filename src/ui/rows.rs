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

#[cfg(test)]
mod tests {
    use super::*;

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
