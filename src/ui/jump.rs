//! Steering a message list towards the message being jumped to (see
//! [`crate::jump`]), the same way for a conversation and a thread.

use std::time::Instant;

use crate::jump::Jump;
use crate::model::Ts;

/// A list's jump as a frame starts, read before the list is drawn.
pub(super) struct Steer {
    jump: Option<Jump>,
    now: Instant,
}

impl Steer {
    /// The jump under way in `list`, if one is.
    pub fn of(jumps: &[Jump], list: &str) -> Self {
        Self {
            jump: jumps.iter().find(|j| j.list == list).cloned(),
            now: Instant::now(),
        }
    }

    /// Whether the jump holds the view this frame: nothing else may move
    /// it, nor the end of the list pull it down.
    pub fn steering(&self) -> bool {
        self.jump.as_ref().is_some_and(Jump::steering)
    }

    /// The message being jumped to.
    pub fn ts(&self) -> Option<&Ts> {
        self.jump.as_ref().map(|j| &j.ts)
    }

    /// How strongly the message `ts` is lit this frame: 0 unless it is the
    /// one jumped to.
    pub fn light(&self, ts: &Ts) -> f32 {
        self.jump
            .as_ref()
            .filter(|j| j.ts == *ts)
            .map_or(0.0, |j| j.light(self.now))
    }

    /// Moves `list`'s jump on by this frame, now that it is drawn in
    /// `output` with the message's row at `target` (top and bottom), if
    /// it holds it, and still `loading` or not: the offset to scroll the
    /// view to, if it must move. A jump that is over is forgotten; one
    /// that is not asks for another frame.
    pub fn drive<R>(
        &self,
        jumps: &mut Vec<Jump>,
        list: &str,
        target: Option<(f32, f32)>,
        loading: bool,
        output: &egui::scroll_area::ScrollAreaOutput<R>,
        ctx: &egui::Context,
    ) -> Option<f32> {
        let index = jumps.iter().position(|j| j.list == list)?;
        let jump = &mut jumps[index];
        let view = output.inner_rect.height();
        let bottom = (output.content_size.y - view).max(0.0);
        let offset = output.state.offset.y;
        let wanted = jump.steer(target, offset, view, bottom, loading, self.now);
        if jump.done(self.now) {
            jumps.remove(index);
        } else {
            ctx.request_repaint();
        }
        wanted
    }
}
