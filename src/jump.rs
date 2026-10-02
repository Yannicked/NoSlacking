//! Bringing one message into view: in a long list whose rows are placed by
//! guessed heights, scrolling once is not enough, since the rows drawn on
//! the way turn out taller or shorter than guessed. A [`Jump`] keeps
//! steering its list towards the message until it stands still there, then
//! lights the message up for a moment.

use std::time::{Duration, Instant};

use crate::model::Ts;

/// How long a message jumped to stays lit, fading over the second half.
pub const HIGHLIGHT_FOR: Duration = Duration::from_millis(2400);
/// Frames in a row the view must stand at the message to be done.
const SETTLED_AFTER: u8 = 3;
/// How long a jump steers at most, found or not: the message may be gone,
/// and the list must never be held for ever.
const GIVE_UP_AFTER: Duration = Duration::from_secs(3);

/// A message being brought into view in one list, and lit up.
#[derive(Clone, Debug, PartialEq)]
pub struct Jump {
    /// The list it is in, by [`crate::app::App::draft_key`]: a
    /// conversation's, or a thread's.
    pub list: String,
    pub ts: Ts,
    /// Whether to light the message up, not only scroll to it.
    pub highlight: bool,
    /// When the view first stood at the message: the light starts then.
    pub arrived: Option<Instant>,
    /// Frames in a row the view has stood at the message.
    pub settled: u8,
    /// When steering began, on the first frame the list was there.
    pub began: Option<Instant>,
    /// Whether it stopped steering without getting there.
    pub gave_up: bool,
}

impl Jump {
    pub fn new(list: String, ts: Ts, highlight: bool) -> Self {
        Self {
            list,
            ts,
            highlight,
            arrived: None,
            settled: 0,
            began: None,
            gave_up: false,
        }
    }

    /// Whether the view still needs steering.
    pub fn steering(&self) -> bool {
        self.arrived.is_none() && !self.gave_up
    }

    /// Notes one frame: whether the view stood at the message (`at`).
    /// Returns whether it still needs steering.
    pub fn step(&mut self, at: bool, now: Instant) -> bool {
        if !self.steering() {
            return false;
        }
        let began = *self.began.get_or_insert(now);
        if now.saturating_duration_since(began) >= GIVE_UP_AFTER {
            self.gave_up = true;
            return false;
        }
        self.settled = if at { self.settled + 1 } else { 0 };
        if self.settled >= SETTLED_AFTER {
            self.arrived = Some(now);
        }
        self.steering()
    }

    /// One frame of steering the list towards the message. `target` is
    /// where its row lies (top and bottom) when the list holds it,
    /// `offset` where the view stands, `view` its height and `max` how
    /// far it scrolls; `loading` says whether the list is still on its
    /// way. Returns the offset to move the view to, if it must move.
    pub fn steer(
        &mut self,
        target: Option<(f32, f32)>,
        offset: f32,
        view: f32,
        max: f32,
        loading: bool,
        now: Instant,
    ) -> Option<f32> {
        if !self.steering() {
            return None;
        }
        match target {
            Some((top, bottom)) => {
                let wanted = offset_for(top, bottom, view, max);
                let at = (offset - wanted).abs() < 1.0;
                self.step(at, now);
                (!at).then_some(wanted)
            }
            None if loading => None,
            // The list is there and the message is not in it.
            None => {
                self.gave_up = true;
                None
            }
        }
    }

    /// How strongly the message is lit now, from 1 down to 0.
    pub fn light(&self, now: Instant) -> f32 {
        if !self.highlight {
            return 0.0;
        }
        let Some(arrived) = self.arrived else {
            // On its way it is lit; a jump that gave up never got there.
            return if self.steering() { 1.0 } else { 0.0 };
        };
        let half = HIGHLIGHT_FOR.as_secs_f32() / 2.0;
        let gone = now.saturating_duration_since(arrived).as_secs_f32();
        (1.0 - (gone - half).max(0.0) / half).clamp(0.0, 1.0)
    }

    /// Whether the jump is over: steered, and no longer lit.
    pub fn done(&self, now: Instant) -> bool {
        !self.steering() && self.light(now) <= 0.0
    }
}

/// The scroll offset that shows a row from `top` to `bottom` (in the
/// list's own coordinates) in a view `view` tall, for a list that scrolls
/// as far as `max`: centred, or for a row taller than most of the view,
/// its top a little below the view's.
pub fn offset_for(top: f32, bottom: f32, view: f32, max: f32) -> f32 {
    let height = bottom - top;
    let wanted = if height > view * 0.8 {
        top - 24.0
    } else {
        top + height / 2.0 - view / 2.0
    };
    wanted.clamp(0.0, max.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_centred_unless_too_tall() {
        // A 40-tall row at 1000 in a 400-tall view: centred.
        assert_eq!(offset_for(1000.0, 1040.0, 400.0, 5000.0), 820.0);
        // Taller than the view: its top shows.
        assert_eq!(offset_for(1000.0, 1600.0, 400.0, 5000.0), 976.0);
        // Near either end the list cannot scroll past itself.
        assert_eq!(offset_for(10.0, 50.0, 400.0, 5000.0), 0.0);
        assert_eq!(offset_for(4990.0, 5030.0, 400.0, 4800.0), 4800.0);
        assert_eq!(offset_for(10.0, 50.0, 400.0, -5.0), 0.0);
    }

    #[test]
    fn a_jump_settles_then_fades() {
        let start = Instant::now();
        let mut jump = Jump::new("T/C".into(), Ts::new("1.0"), true);
        assert_eq!(jump.light(start), 1.0, "lit while on its way");
        assert!(jump.step(false, start));
        assert!(jump.step(true, start));
        assert!(jump.step(true, start));
        // A frame off the mark starts the count again.
        assert!(jump.step(false, start));
        assert!(jump.step(true, start));
        assert!(jump.step(true, start));
        assert!(!jump.step(true, start), "three frames in a row");
        assert_eq!(jump.arrived, Some(start));
        assert_eq!(jump.light(start + Duration::from_millis(1000)), 1.0);
        let fading = jump.light(start + Duration::from_millis(1800));
        assert!(fading > 0.0 && fading < 1.0, "{fading}");
        assert!(!jump.done(start + Duration::from_millis(1800)));
        assert!(jump.done(start + HIGHLIGHT_FOR));
    }

    #[test]
    fn steering_moves_the_view_until_it_stands_at_the_row() {
        let now = Instant::now();
        let mut jump = Jump::new("T/C".into(), Ts::new("1.0"), true);
        // Still loading: wait.
        assert_eq!(jump.steer(None, 0.0, 400.0, 5000.0, true, now), None);
        assert!(jump.steering());
        let row = Some((1000.0, 1040.0));
        assert_eq!(jump.steer(row, 0.0, 400.0, 5000.0, false, now), Some(820.0));
        for _ in 0..3 {
            assert_eq!(jump.steer(row, 820.0, 400.0, 5000.0, false, now), None);
        }
        assert!(!jump.steering());
        // Loaded without the message: give up at once.
        let mut missing = Jump::new("T/C".into(), Ts::new("1.0"), true);
        assert_eq!(missing.steer(None, 0.0, 400.0, 5000.0, false, now), None);
        assert!(missing.done(now));
    }

    #[test]
    fn a_jump_gives_up_and_plain_jumps_never_light() {
        let now = Instant::now();
        let plain = Jump::new("T/C".into(), Ts::new("1.0"), false);
        assert_eq!(plain.light(now), 0.0);
        let mut jump = Jump::new("T/C".into(), Ts::new("1.0"), true);
        assert!(jump.step(false, now));
        assert!(jump.step(false, now + Duration::from_secs(2)));
        assert!(!jump.step(false, now + GIVE_UP_AFTER));
        assert!(jump.done(now + GIVE_UP_AFTER));
    }
}
