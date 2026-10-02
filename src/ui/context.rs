//! The right-click menu on messages.
//!
//! Widgets inside a message (a picture, a link) say they are under the
//! pointer with [`hover`]; when the message is right-clicked, whatever was
//! under the pointer that frame decides the extra items the menu offers.

use crate::model::Ts;

/// What sits under the pointer inside a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A picture a message shares.
    Image {
        channel: String,
        thread: Option<Ts>,
        ts: Ts,
        file: String,
        name: String,
        /// Where the full file downloads from.
        download: Option<String>,
        /// Its page on Slack, to open in the browser.
        permalink: Option<String>,
        /// The image loader's URIs to copy it from, best first: the full
        /// picture, then the thumbnail on screen.
        copy: Vec<String>,
    },
    /// A link in the text.
    Link(String),
}

fn hover_id() -> egui::Id {
    egui::Id::new("context-hover")
}

/// Notes that `target` is under the pointer this frame.
pub fn hover(ui: &egui::Ui, target: Target) {
    let frame = ui.ctx().cumulative_frame_nr();
    ui.data_mut(|d| d.insert_temp(hover_id(), (frame, target)));
}

/// What [`hover`] noted this frame, if anything.
pub fn hovered(ctx: &egui::Context) -> Option<Target> {
    let frame = ctx.cumulative_frame_nr();
    ctx.data(|d| d.get_temp::<(u64, Target)>(hover_id()))
        .filter(|(at, _)| *at == frame)
        .map(|(_, target)| target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_frames_target_counts() {
        let ctx = egui::Context::default();
        let mut seen = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            hover(ui, Target::Link("https://x.y".into()));
            seen = hovered(ui.ctx());
        });
        out.textures_delta.clear();
        assert_eq!(seen, Some(Target::Link("https://x.y".into())));
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            seen = hovered(ui.ctx());
        });
        out.textures_delta.clear();
        assert_eq!(seen, None);
    }
}
