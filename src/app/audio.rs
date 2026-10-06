//! Sounds played in the app: carrying out what [`crate::audio::Playback`]
//! decides, with the worker (fetching), the playing thread and toasts.

use super::App;
use crate::audio::{Bytes, Effect, Request};
use crate::backend::Command;
use crate::failure::Problem;
use crate::i18n::tf;

impl App {
    /// A card's play, pause or seek, in the workspace on screen.
    pub(super) fn audio(&mut self, request: Request) {
        let Some(team) = self.active_team() else {
            return;
        };
        let effects = self.playback.request(&team, request);
        self.audio_effects(effects);
        // The cards read the new state next frame.
        self.waker.wake();
    }

    /// The worker's answer to a fetch.
    pub(super) fn audio_fetched(&mut self, id: u64, result: Result<Bytes, Problem>) {
        let effects = self.playback.fetched(id, result);
        self.audio_effects(effects);
    }

    /// Stops the sound of a workspace being signed out of.
    pub(super) fn audio_signed_out(&mut self, team: &str) {
        let effects = self.playback.signed_out(team);
        self.audio_effects(effects);
    }

    /// Hears the playing thread and hands the cards what plays.
    pub(super) fn audio_frame(&mut self, ctx: &egui::Context) {
        while let Some(report) = self.audio_device.try_recv() {
            let effects = self.playback.report(report);
            self.audio_effects(effects);
        }
        crate::audio::publish(ctx, self.playback.now());
    }

    fn audio_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Fetch {
                    team,
                    id,
                    url,
                    name,
                } => self.backend.send(Command::FetchAudio {
                    team,
                    id,
                    url,
                    name,
                }),
                Effect::Device(order) => self.audio_device.send(order),
                Effect::Open { team, url, name } => {
                    self.toast(tf("Opening {name}…", &[("name", &name)]), false);
                    self.backend.send(Command::OpenFile { team, url, name });
                }
                Effect::Tell(why) => {
                    if let Some(text) = why.message() {
                        self.toast(text, false);
                    }
                }
                Effect::Problem(problem) => self.toast(problem.message(), problem.is_error()),
            }
        }
    }
}
