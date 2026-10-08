//! The worker's side of Teams calls: one at a time, placed through
//! [`crate::teams::calling::call::outgoing`] or answered through
//! [`crate::teams::calling::call::incoming`] with the huddle's speaker and
//! microphone, ended when asked, on sign-out, or when another call or
//! huddle starts.
//!
//! It speaks to the interface as a huddle does, through
//! [`crate::people::Event::Listening`] and
//! [`crate::people::Event::Microphone`], so the call bar shows it as it
//! shows a huddle. Unlike a huddle, a call starts unmuted: you are in it
//! to talk. An incoming call rings as a huddle invitation does
//! ([`crate::people::Event::HuddleInvite`], the call id as its room), and
//! stops ringing as one is cancelled. A meeting is joined (or started
//! with "Meet now") the same way, shown in
//! [`crate::meetings::MEETING_CHANNEL`], with who waits in its lobby in
//! the roster.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};

use super::listen::microphone;
use super::{Event, Sink};
use crate::failure::{Failure, HuddleTrouble};
use crate::huddle_audio::media::Uplink;
use crate::huddle_audio::microphone::Wiring;
use crate::huddle_audio::processing::RenderTap;
use crate::huddle_audio::roster::{Person, Roster};
use crate::huddle_audio::speaker::Speaker;
use crate::huddle_audio::uplink::Outgoing;
use crate::huddles::{Left, Listen};
use crate::people;
use crate::teams::calling::call::{
    Answer, Attendee, CallEvent, Control, Incoming, incoming, meeting, outgoing,
};
use crate::teams::calling::media::Audio;
use crate::teams::client::TeamsClient;

/// How often the far end's loudness is looked at.
const LISTEN_EVERY: Duration = Duration::from_millis(100);
/// How loud (RMS, 0 to 1) what the far end sends must be to count as
/// speech: about -36 dBFS, above the hiss Opus lets through between
/// words, below a quiet voice.
const SPEECH_RMS: f32 = 0.016;
/// How long the speaking mark stays after the last loud moment, so it
/// does not flicker between words.
const SPEECH_HOLD: Duration = Duration::from_millis(400);

/// Whether someone speaks, from moments looked at in turn: speaking while
/// loud, and a moment after.
#[derive(Debug, Default)]
struct Speech {
    /// When they were last loud.
    loud_at: Option<Instant>,
    /// What the bar was last told.
    speaking: bool,
}

impl Speech {
    /// Takes whether they were `loud` since the last look, at `now`;
    /// answers whether they speak when that changed.
    fn heard(&mut self, loud: bool, now: Instant) -> Option<bool> {
        if loud {
            self.loud_at = Some(now);
        }
        let speaking = self
            .loud_at
            .is_some_and(|at| now.saturating_duration_since(at) < SPEECH_HOLD);
        (speaking != self.speaking).then(|| {
            self.speaking = speaking;
            speaking
        })
    }
}

/// The call going on.
#[derive(Debug)]
struct Running {
    team: String,
    /// The devices chosen, as the call's microphone and camera follow
    /// them.
    devices: watch::Sender<crate::devices::Chosen>,
    /// The call's id, for one answered here: a second delivery of it
    /// must not ring.
    call_id: Option<String>,
    control: mpsc::UnboundedSender<Control>,
    /// Whether the interface wants the microphone muted.
    muted: watch::Sender<bool>,
    /// Whether the interface wants the camera on.
    #[cfg(feature = "huddle-camera")]
    camera: watch::Sender<bool>,
    /// What the interface asks of the screen share.
    #[cfg(feature = "huddle-share")]
    share: mpsc::UnboundedSender<crate::huddle_share::ShareRequest>,
}

/// An incoming call ringing here, waiting for the person to decide.
#[derive(Debug)]
struct Ringing {
    /// Its call id, the invitation's room.
    call_id: String,
    /// What it becomes once picked up.
    running: Running,
    decide: oneshot::Sender<Decision>,
}

/// What the person decided about an incoming call.
#[derive(Debug)]
enum Decision {
    /// Pick up, the call shown in `channel`.
    Accept {
        channel: String,
    },
    Decline,
}

/// The one Teams call there may be, and the incoming one that rings.
#[derive(Debug, Default)]
pub struct Caller {
    running: Option<Running>,
    ringing: Option<Ringing>,
    /// The devices chosen, for this call and the next, as a huddle's.
    chosen: crate::devices::Chosen,
}

/// What a call's devices are steered by: the microphone and the camera
/// as the interface wants them.
struct Wanted {
    microphone: watch::Receiver<bool>,
    /// The devices chosen.
    chosen: watch::Receiver<crate::devices::Chosen>,
    #[cfg(feature = "huddle-camera")]
    camera: watch::Receiver<bool>,
    /// The screen share's requests, and where the share task tells the
    /// call to renegotiate it.
    #[cfg(feature = "huddle-share")]
    share: (
        mpsc::UnboundedReceiver<crate::huddle_share::ShareRequest>,
        mpsc::UnboundedSender<Control>,
    ),
}

/// A call's controls: for the caller to keep, and for the call's task
/// (hang-up and mute for the call, and what its devices should do).
fn controls(
    chosen: &crate::devices::Chosen,
) -> (Running, mpsc::UnboundedReceiver<Control>, Wanted) {
    let (control, controls) = mpsc::unbounded_channel();
    let (devices, chosen) = watch::channel(chosen.clone());
    let (muted, microphone) = watch::channel(true);
    // Unmuted from the start: the microphone task opens it on this.
    let _ = muted.send(false);
    // The camera starts off.
    #[cfg(feature = "huddle-camera")]
    let (camera_sender, camera) = watch::channel(false);
    #[cfg(feature = "huddle-share")]
    let (share_sender, share_requests) = mpsc::unbounded_channel();
    #[cfg(feature = "huddle-share")]
    let share = (share_requests, control.clone());
    let running = Running {
        team: String::new(),
        devices,
        call_id: None,
        control,
        muted,
        #[cfg(feature = "huddle-camera")]
        camera: camera_sender,
        #[cfg(feature = "huddle-share")]
        share: share_sender,
    };
    let wanted = Wanted {
        microphone,
        chosen,
        #[cfg(feature = "huddle-camera")]
        camera,
        #[cfg(feature = "huddle-share")]
        share,
    };
    (running, controls, wanted)
}

impl Caller {
    /// Calls `callee` (an MRI) from `channel` of `team`, ending the last
    /// call first.
    pub fn start(
        &mut self,
        client: TeamsClient,
        team: String,
        channel: String,
        callee: String,
        sink: Sink,
    ) {
        self.stop();
        let (mut running, controls, wanted) = controls(&self.chosen);
        running.team.clone_from(&team);
        tokio::spawn(run(
            client,
            Place {
                team,
                channel,
                callee,
            },
            controls,
            wanted,
            sink,
        ));
        self.running = Some(running);
    }

    /// Rings for the incoming `call` in `team`: the interface shows it as
    /// an invitation. A call already ringing is declined: one rings at a
    /// time.
    pub fn ring(&mut self, client: TeamsClient, team: String, call: Incoming, sink: Sink) {
        // The connection is registered for chat and for calls, and a call
        // may come once for each: the second delivery is the same call.
        let known = |id: Option<&str>| id == Some(call.call_id.as_str());
        if known(self.ringing.as_ref().map(|r| r.call_id.as_str()))
            || known(self.running.as_ref().and_then(|r| r.call_id.as_deref()))
        {
            log::info!("Teams call: a second delivery of a call already here; not rung again");
            return;
        }
        if let Some(old) = self.ringing.take() {
            let _ = old.decide.send(Decision::Decline);
        }
        let (mut running, controls, wanted) = controls(&self.chosen);
        running.team.clone_from(&team);
        let (decide, decisions) = oneshot::channel();
        let call_id = call.call_id.clone();
        tokio::spawn(ring(client, team, call, decisions, controls, wanted, sink));
        self.ringing = Some(Ringing {
            call_id,
            running,
            decide,
        });
    }

    /// Joins a meeting in `team` (or makes one first, "Meet now"),
    /// ending the last call first.
    pub fn join_meeting(&mut self, client: TeamsClient, team: String, join: Join, sink: Sink) {
        self.stop();
        let (mut running, controls, wanted) = controls(&self.chosen);
        running.team.clone_from(&team);
        tokio::spawn(run_meeting(client, team, join, controls, wanted, sink));
        self.running = Some(running);
    }

    /// What the call window shows now: in a meeting, the sizes of video
    /// asked for follow it.
    #[cfg(feature = "huddle-video")]
    pub fn watch(&mut self, wish: &crate::huddle_audio::cameras::Wish) {
        if let Some(running) = &self.running {
            let view = crate::teams::calling::call::View {
                open: wish.open,
                tile: wish.tile,
                share: wish.share.is_some(),
            };
            let _ = running.control.send(Control::View(view));
        }
    }

    /// Lets `user` (by the id the interface knows them by) in from the
    /// meeting's lobby.
    pub fn admit(&mut self, user: &str) {
        if let Some(running) = &self.running {
            // The call knows their MRI: a guest's is not one the id
            // could be turned back into.
            let _ = running.control.send(Control::Admit(user.to_owned()));
        }
    }

    /// Picks up the incoming call `call_id` of `team`, shown in `channel`,
    /// ending any other call first. Answers whether it was still ringing.
    pub fn answer(&mut self, team: &str, call_id: &str, channel: String) -> bool {
        let Some(ringing) = self.take_ringing(team, call_id) else {
            return false;
        };
        self.stop();
        if ringing.decide.send(Decision::Accept { channel }).is_err() {
            return false;
        }
        let mut running = ringing.running;
        running.call_id = Some(ringing.call_id);
        self.running = Some(running);
        true
    }

    /// Declines the incoming call `call_id` of `team`, if it still rings.
    pub fn decline(&mut self, team: &str, call_id: &str) {
        if let Some(ringing) = self.take_ringing(team, call_id) {
            let _ = ringing.decide.send(Decision::Decline);
        }
    }

    fn take_ringing(&mut self, team: &str, call_id: &str) -> Option<Ringing> {
        self.ringing
            .take_if(|r| r.running.team == team && r.call_id == call_id)
    }

    /// Mutes or unmutes the microphone in the call, if there is one.
    pub fn set_muted(&mut self, muted: bool) {
        if let Some(running) = &self.running {
            let _ = running.muted.send(muted);
            let _ = running.control.send(Control::Mute(muted));
        }
    }

    /// Turns the camera on or off in the call, if there is one.
    #[cfg(feature = "huddle-camera")]
    pub fn set_camera(&mut self, on: bool) {
        if let Some(running) = &self.running {
            let _ = running.camera.send(on);
            // A meeting forwards it only once told.
            let _ = running.control.send(Control::Camera(on));
        }
    }

    /// Starts, picks or stops sharing your screen in the call, if there is
    /// one.
    #[cfg(feature = "huddle-share")]
    pub fn share(&mut self, request: crate::huddle_share::ShareRequest) {
        if let Some(running) = &self.running {
            let _ = running.share.send(request);
        }
    }

    /// Uses the devices `chosen` from now on, in the call going on too.
    pub fn use_devices(&mut self, chosen: crate::devices::Chosen) {
        if let Some(running) = &self.running {
            let _ = running.devices.send(chosen.clone());
        }
        self.chosen = chosen;
    }

    /// Hangs up; the call says when it has ended.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.control.send(Control::HangUp);
        }
    }

    /// Hangs up, and declines what rings, if in `team`, which signed out.
    pub fn signed_out(&mut self, team: &str) {
        if self.running.as_ref().is_some_and(|r| r.team == team) {
            self.stop();
        }
        if let Some(ringing) = self.ringing.take_if(|r| r.running.team == team) {
            let _ = ringing.decide.send(Decision::Decline);
        }
    }
}

/// Which meeting to join.
#[derive(Debug)]
pub enum Join {
    /// One with this link, or ID and passcode.
    Meeting(crate::meetings::Meeting),
    /// One made now, named this.
    Now(String),
}

/// Where a call is, and with whom.
struct Place {
    team: String,
    channel: String,
    /// The one called, or calling: by the id the interface knows.
    callee: String,
}

/// What the bar shows of who is in the call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FarEnd {
    /// The one called is muted, as the far end last said.
    muted: bool,
    /// The one called speaks, as their sound says. Teams sends no levels
    /// (the far end turns down RTP's audio level header), so it is how
    /// loud their sound plays.
    speaking: bool,
    /// You speak, as the microphone's voice detection says (after echo
    /// cancelling, so their voice from the speaker does not count).
    me_speaking: bool,
}

/// Who is in a call with `callee`: you and them, as `far` says.
fn roster(callee: &str, far: FarEnd) -> Roster {
    Roster {
        people: vec![
            Person {
                user: Some(callee.to_owned()),
                me: false,
                muted: far.muted,
                speaking: far.speaking,
                name: None,
                waiting: false,
            },
            Person {
                user: None,
                me: true,
                muted: false,
                speaking: far.me_speaking,
                name: None,
                waiting: false,
            },
        ],
        count: Some(2),
    }
}

/// Who is in a meeting with you: the others as `people` says (by the ids
/// the interface knows people by, their names for any it does not), and
/// you, speaking as `me_speaking` says.
fn meeting_roster(people: &[Attendee], me_speaking: bool) -> Roster {
    let mut everyone: Vec<Person> = people
        .iter()
        .map(|attendee| Person {
            user: Some(
                crate::backend::teams_translate::clean_teams_user_id(&attendee.mri)
                    .unwrap_or_else(|| attendee.mri.clone()),
            ),
            me: false,
            muted: attendee.muted,
            speaking: false,
            name: attendee.name.clone(),
            waiting: attendee.waiting,
        })
        .collect();
    everyone.push(Person {
        user: None,
        me: true,
        muted: false,
        speaking: me_speaking,
        name: None,
        waiting: false,
    });
    let count = everyone.iter().filter(|p| !p.waiting).count();
    Roster {
        people: everyone,
        count: u32::try_from(count).ok(),
    }
}

/// What the interface hears of the call's end.
fn ending(result: Result<(), Failure>) -> Result<Left, Failure> {
    // Hung up here, the interface has already let the call go and drops
    // this; so an ending that reaches it was the far end's.
    result.map(|()| Left::Ended)
}

/// The speaker and microphone of a call, open.
struct Devices {
    speaker: Speaker,
    mic: tokio::task::JoinHandle<()>,
    close_mic: oneshot::Sender<()>,
    passing: tokio::task::JoinHandle<()>,
    #[cfg(feature = "huddle-camera")]
    camera: CameraTask,
    /// The screen share's task, and what ends it.
    #[cfg(feature = "huddle-share")]
    share: (tokio::task::JoinHandle<()>, oneshot::Sender<()>),
    /// How loud the far end plays.
    meter: crate::huddle_audio::speaker::Feed,
    /// Whether you spoke since last asked.
    spoke: Arc<AtomicBool>,
}

/// The camera's task: closed until turned on, its pictures encoded on a
/// thread of its own, its preview for the call bar.
#[cfg(feature = "huddle-camera")]
struct CameraTask {
    task: tokio::task::JoinHandle<()>,
    close: oneshot::Sender<()>,
    /// Held open: a call is never refused video, as a huddle can be, and
    /// the task stops when this closes.
    _refusals: mpsc::Sender<()>,
}

impl Devices {
    /// Opens the speaker and the microphone (as `wanted` says) for the
    /// call in `place`, readies the camera and the far end's tile, and
    /// answers the sound and pictures the call is to use.
    async fn open(
        place: &Place,
        wanted: Wanted,
        sink: &Sink,
        tell: &impl Fn(Listen),
    ) -> Result<(Self, Audio), Failure> {
        let Wanted {
            microphone: wanted,
            chosen,
            #[cfg(feature = "huddle-camera")]
                camera: camera_wanted,
            #[cfg(feature = "huddle-share")]
                share: (share_requests, call),
        } = wanted;
        let tap = RenderTap::default();
        let speaker_tap = tap.clone();
        let speaker_choice = chosen.borrow().speaker.clone();
        let ((speaker, fell_back), feed) = match tokio::task::spawn_blocking(move || {
            Speaker::open(Some(speaker_tap), speaker_choice)
        })
        .await
        {
            Ok(Ok(opened)) => opened,
            Ok(Err(why)) => {
                log::warn!("Teams call: {why}");
                return Err(Failure::Huddle(HuddleTrouble::NoSound));
            }
            Err(error) => {
                log::warn!("Teams call: the device thread failed: {error}");
                return Err(Failure::Huddle(HuddleTrouble::NoSound));
            }
        };
        let (frames, mut spoken) = mpsc::channel::<Outgoing>(25);
        let (to_call, frames_in) = mpsc::channel(25);
        // The microphone's frames pass by on the way to the call, saying
        // whether you spoke.
        let spoke = Arc::new(AtomicBool::new(false));
        let heard_you = spoke.clone();
        let passing = tokio::spawn(async move {
            while let Some(frame) = spoken.recv().await {
                if frame.voice {
                    heard_you.store(true, Ordering::Relaxed);
                }
                if to_call.send(frame).await.is_err() {
                    break;
                }
            }
        });
        let (effective, muted) = watch::channel(true);
        let (close_mic, mic_done) = oneshot::channel();
        let mic_sink = sink.clone();
        let (mic_team, mic_channel) = (place.team.clone(), place.channel.clone());
        if let Some(why) = fell_back {
            log::info!("Teams call: the chosen speaker gave way to the default: {why}");
        }
        let mic = tokio::spawn(microphone(
            wanted,
            chosen.clone(),
            effective,
            Wiring {
                frames,
                render: tap,
            },
            mic_done,
            move |news| {
                mic_sink.send(Event::People {
                    team: mic_team.clone(),
                    event: people::Event::Microphone {
                        channel: mic_channel.clone(),
                        news,
                    },
                });
            },
        ));
        // The far end's camera's tile.
        #[cfg(feature = "huddle-video")]
        let gallery = {
            let waker = sink.waker();
            let gallery = crate::huddle_audio::gallery::Gallery::new(move || waker.wake());
            tell(Listen::Gallery(gallery.clone()));
            gallery
        };
        #[cfg(feature = "huddle-camera")]
        let (camera, camera_feed) = camera_task(place, camera_wanted, chosen.clone(), sink, tell);
        // The far end's screen share.
        #[cfg(feature = "huddle-video")]
        let screen = {
            let waker = sink.waker();
            let screen = crate::huddle_audio::screen::Screen::new(move || waker.wake());
            tell(Listen::Screen(screen.clone()));
            screen
        };
        // Ours: nothing captured until asked.
        #[cfg(feature = "huddle-share")]
        let (share, share_feed) = share_task(place, share_requests, call, sink);
        #[cfg(not(feature = "video-helper"))]
        let _ = tell;
        let video = crate::teams::calling::video::Video {
            #[cfg(feature = "huddle-video")]
            gallery: Some(gallery),
            #[cfg(feature = "huddle-camera")]
            camera: Some(camera_feed),
            #[cfg(feature = "huddle-video")]
            screen: Some(screen),
            #[cfg(feature = "huddle-share")]
            share: Some(share_feed),
        };
        let audio = Audio {
            feed: Some(feed.clone()),
            uplink: Some(Uplink {
                frames: frames_in,
                muted,
            }),
            video,
        };
        let devices = Self {
            speaker,
            mic,
            close_mic,
            passing,
            #[cfg(feature = "huddle-camera")]
            camera,
            #[cfg(feature = "huddle-share")]
            share,
            meter: feed,
            spoke,
        };
        Ok((devices, audio))
    }

    /// Closes the microphone, then the speaker.
    async fn close(self) {
        // The share first: its capture ends with it.
        #[cfg(feature = "huddle-share")]
        {
            let (task, close) = self.share;
            let _ = close.send(());
            let _ = task.await;
        }
        #[cfg(feature = "huddle-camera")]
        {
            let _ = self.camera.close.send(());
            let _ = self.camera.task.await;
        }
        let _ = self.close_mic.send(());
        let _ = self.mic.await;
        self.passing.abort();
        let speaker = self.speaker;
        // Stopping the device waits for its thread; not on this one.
        let _ = tokio::task::spawn_blocking(move || drop(speaker)).await;
    }
}

/// Starts the camera's task for the call in `place`, the camera opening
/// and closing as `wanted` says; answers the task and what the call
/// sends from.
#[cfg(feature = "huddle-camera")]
fn camera_task(
    place: &Place,
    wanted: watch::Receiver<bool>,
    chosen: watch::Receiver<crate::devices::Chosen>,
    sink: &Sink,
    tell: &impl Fn(Listen),
) -> (CameraTask, crate::teams::calling::video::CameraFeed) {
    use crate::huddle_audio::camera_send::{QUEUE, SendControl};

    let waker = sink.waker();
    let preview = crate::huddle_camera::Preview::new(move || waker.wake());
    tell(Listen::Preview(preview.clone()));
    let (frames, frames_in) = mpsc::channel(QUEUE);
    let control = SendControl::default();
    let (refusals_sender, refusals) = mpsc::channel(1);
    let (on, on_rx) = watch::channel(false);
    let (close, done) = oneshot::channel();
    let camera_sink = sink.clone();
    let (team, channel) = (place.team.clone(), place.channel.clone());
    let task = tokio::spawn(super::listen::camera(
        wanted,
        chosen,
        on,
        super::listen::CameraWiring {
            frames,
            control: control.clone(),
            preview,
        },
        refusals,
        done,
        move |news| {
            camera_sink.send(Event::People {
                team: team.clone(),
                event: people::Event::Camera {
                    channel: channel.clone(),
                    news,
                },
            });
        },
    ));
    let task = CameraTask {
        task,
        close,
        _refusals: refusals_sender,
    };
    let feed = crate::teams::calling::video::CameraFeed {
        frames: frames_in,
        on: on_rx,
        control,
    };
    (task, feed)
}

/// The far end's screen share's key in the call bar and window: a 1:1
/// call has one far end, so one share.
#[cfg(feature = "huddle-video")]
const FAR_SHARE: &str = "far-share";

/// Starts the screen share's task for the call in `place`: it captures
/// nothing until asked; then, through the video helper as a huddle's
/// share starts (the system's dialog or a list to pick from), it
/// captures and encodes the screen, and tells the call (`call`) to
/// renegotiate the share line on, and off again when it stops. Answers
/// the task and what the call sends from.
#[cfg(feature = "huddle-share")]
fn share_task(
    place: &Place,
    requests: mpsc::UnboundedReceiver<crate::huddle_share::ShareRequest>,
    call: mpsc::UnboundedSender<Control>,
    sink: &Sink,
) -> (
    (tokio::task::JoinHandle<()>, oneshot::Sender<()>),
    crate::teams::calling::video::CameraFeed,
) {
    use crate::huddle_audio::camera_send::{Limits, QUEUE, SendControl};

    let (frames, frames_in) = mpsc::channel(QUEUE);
    let control = SendControl::new(Limits::SHARE);
    let (on, on_rx) = watch::channel(false);
    let (sink, team, channel) = (sink.clone(), place.team.clone(), place.channel.clone());
    let tell = move |news: crate::huddle_share::ShareNews| {
        sink.send(Event::People {
            team: team.clone(),
            event: people::Event::Share {
                channel: channel.clone(),
                news,
            },
        });
    };
    let (close, done) = oneshot::channel();
    let task = tokio::spawn(share(
        requests,
        done,
        frames,
        control.clone(),
        on,
        call,
        tell,
    ));
    let feed = crate::teams::calling::video::CameraFeed {
        frames: frames_in,
        on: on_rx,
        control,
    };
    ((task, close), feed)
}

/// The screen share of a call, from its first request to the call's end
/// (`done`).
#[cfg(feature = "huddle-share")]
async fn share(
    mut requests: mpsc::UnboundedReceiver<crate::huddle_share::ShareRequest>,
    mut done: oneshot::Receiver<()>,
    frames: mpsc::Sender<crate::huddle_audio::camera_send::VideoFrame>,
    control: crate::huddle_audio::camera_send::SendControl,
    on: watch::Sender<bool>,
    call: mpsc::UnboundedSender<Control>,
    tell: impl Fn(crate::huddle_share::ShareNews),
) {
    use super::listen::share::{Begin, Step, begin};
    use crate::huddle_audio::camera_send::{Encoding, Ending};
    use crate::huddle_share::{ShareNews, ShareRequest};

    let mut encoding: Option<Encoding> = None;
    let mut ended: Option<watch::Receiver<Option<Ending>>> = None;
    // Stops sharing: the capture ends with its sending thread, and the
    // call renegotiates the line off.
    let stop = |encoding: &mut Option<Encoding>,
                ended: &mut Option<watch::Receiver<Option<Ending>>>| {
        let running = encoding.take();
        *ended = None;
        let _ = on.send(false);
        let _ = call.send(Control::Share(false));
        running
    };
    loop {
        let request = tokio::select! {
            _ = &mut done => break,
            request = requests.recv() => match request {
                Some(request) => request,
                None => break,
            },
            ending = async {
                match ended.as_mut() {
                    Some(ended) => loop {
                        if let Some(ending) = ended.borrow_and_update().clone() {
                            return ending;
                        }
                        if ended.changed().await.is_err() {
                            return std::future::pending().await;
                        }
                    },
                    None => std::future::pending().await,
                }
            } => {
                let running = stop(&mut encoding, &mut ended);
                let _ = tokio::task::spawn_blocking(move || drop(running)).await;
                tell(match ending {
                    Ending::Ended => ShareNews::Ended,
                    Ending::Failed(failure) => ShareNews::Failed(failure),
                });
                continue;
            }
        };
        let asked = match request {
            ShareRequest::Stop => {
                let running = stop(&mut encoding, &mut ended);
                let _ = tokio::task::spawn_blocking(move || drop(running)).await;
                tell(ShareNews::Off);
                continue;
            }
            ShareRequest::Start { again } => Begin::Start { again },
            ShareRequest::Pick(id) => Begin::Pick(id),
        };
        // A new choice replaces what is shared.
        if let Some(running) = encoding.take() {
            let _ = tokio::task::spawn_blocking(move || drop(running)).await;
        }
        // The helper, the system's dialog and the user all take their
        // time: not on this thread.
        let step = match tokio::task::spawn_blocking(move || begin(asked)).await {
            Ok(step) => step,
            Err(error) => {
                log::warn!("Teams share: the start failed: {error}");
                tell(ShareNews::Failed(crate::failure::Failure::Huddle(
                    crate::failure::HuddleTrouble::VideoHelperLost,
                )));
                continue;
            }
        };
        match step {
            Step::Choose(sources) => tell(ShareNews::Choose(sources)),
            Step::Started(Err(failure)) => {
                let _ = on.send(false);
                tell(ShareNews::Failed(failure));
            }
            Step::Started(Ok(capture)) => {
                let (ending, ending_rx) = watch::channel(None);
                match crate::huddle_audio::share_send::spawn(
                    capture,
                    frames.clone(),
                    control.clone(),
                    ending,
                ) {
                    Ok(started) => {
                        encoding = Some(started);
                        ended = Some(ending_rx);
                        let _ = on.send(true);
                        let _ = call.send(Control::Share(true));
                        log::info!(
                            "Teams share: capturing; the call renegotiates the share line on"
                        );
                        tell(ShareNews::On);
                    }
                    Err(why) => {
                        log::warn!("Teams share: {why}");
                        tell(ShareNews::Failed(crate::failure::Failure::Huddle(
                            crate::failure::HuddleTrouble::VideoHelperLost,
                        )));
                    }
                }
            }
        }
    }
    let running = encoding.take();
    let _ = tokio::task::spawn_blocking(move || drop(running)).await;
}

/// What tells the interface where the call in `place` is.
fn teller(place: &Place, sink: &Sink) -> impl Fn(Listen) + Clone + Send + 'static {
    let (sink, team, channel) = (sink.clone(), place.team.clone(), place.channel.clone());
    move |state: Listen| {
        sink.send(Event::People {
            team: team.clone(),
            event: people::Event::Listening {
                channel: channel.clone(),
                state,
            },
        });
    }
}

/// What the bar shows of a call, and where its changes are told.
struct Shown<'a, T: Fn(Listen)> {
    far: FarEnd,
    callee: &'a str,
    /// Who else is in the meeting, for a meeting.
    people: Option<Vec<Attendee>>,
    /// Whose camera a meeting sends us on each camera line, by MRI.
    watched: Vec<Option<String>>,
    /// The camera lines whose pictures show now.
    cameras_on: std::collections::BTreeSet<usize>,
    tell: &'a T,
    /// How the call ended, once it has.
    ended: Option<Result<(), Failure>>,
}

impl<T: Fn(Listen)> Shown<'_, T> {
    /// The camera on camera line `line` started or stopped: its tile
    /// comes or goes.
    fn far_video(&mut self, line: usize, on: bool) {
        if on {
            self.cameras_on.insert(line);
        } else {
            self.cameras_on.remove(&line);
        }
        self.tell_cameras();
    }

    /// Tells the bar every camera showing, a tile each.
    fn tell_cameras(&self) {
        #[cfg(feature = "huddle-video")]
        {
            let cameras = self
                .cameras_on
                .iter()
                .map(|&line| crate::huddle_audio::cameras::Camera {
                    key: crate::teams::calling::video::camera_key(line),
                    user: self.far_user(line),
                    paused: false,
                    tile: true,
                })
                .collect();
            (self.tell)(Listen::Cameras(cameras));
        }
    }

    /// The far end's screen share started or stopped: the bar offers it
    /// to watch, or no longer.
    fn far_share(&self, on: bool) {
        #[cfg(feature = "huddle-video")]
        {
            let shares = if on {
                vec![crate::huddle_audio::video::Share {
                    key: FAR_SHARE.to_owned(),
                    user: if self.people.is_some() {
                        None
                    } else {
                        self.far_user(0)
                    },
                }]
            } else {
                Vec::new()
            };
            (self.tell)(Listen::Shares(shares));
        }
        #[cfg(not(feature = "huddle-video"))]
        let _ = on;
    }

    /// Whose camera camera line `line` shows: the one called; in a
    /// meeting, the one asked for on it.
    #[cfg(feature = "huddle-video")]
    fn far_user(&self, line: usize) -> Option<String> {
        if self.people.is_some() {
            return self.watched.get(line)?.as_deref().map(|mri| {
                crate::backend::teams_translate::clean_teams_user_id(mri)
                    .unwrap_or_else(|| mri.to_owned())
            });
        }
        (!self.callee.is_empty()).then(|| self.callee.to_owned())
    }

    /// Who is in the call, as the bar shows it.
    fn roster(&self) -> Roster {
        match &self.people {
            Some(people) => meeting_roster(people, self.far.me_speaking),
            None => roster(self.callee, self.far),
        }
    }

    /// Changes what is shown of who is in the call, telling the bar the
    /// whole of it if that changed anything.
    fn change(&mut self, change: impl FnOnce(&mut FarEnd)) {
        let was = self.far;
        change(&mut self.far);
        if self.far != was {
            (self.tell)(Listen::Roster(self.roster()));
        }
    }

    /// Takes in the call's news.
    fn heard(&mut self, event: CallEvent) {
        match event {
            CallEvent::Ringing => (self.tell)(Listen::Ringing),
            CallEvent::Live => (self.tell)(Listen::Live),
            CallEvent::AudioFlowing => {}
            CallEvent::FarEndMuted(muted) => self.change(|far| far.muted = muted),
            CallEvent::FarEndCamera { line, on } => self.far_video(line, on),
            CallEvent::FarEndShare(on) => self.far_share(on),
            // Only an incoming call's ringing says this, and that is over.
            CallEvent::AnsweredElsewhere => {}
            CallEvent::Lobby => (self.tell)(Listen::Lobby),
            CallEvent::Admitted => (self.tell)(Listen::Admitted),
            CallEvent::Watching(who) => {
                self.watched = who;
                // The tiles' names follow.
                self.tell_cameras();
            }
            CallEvent::People(people) => {
                self.people = Some(people);
                (self.tell)(Listen::Roster(self.roster()));
            }
            CallEvent::Ended { result, .. } => self.ended = Some(result),
        }
    }
}

/// Follows a call with its devices open until `call` (its task) is done:
/// tells the interface its news and who speaks. Answers how it ended.
async fn follow(
    mut call: Pin<&mut impl Future<Output = ()>>,
    events: &mut mpsc::UnboundedReceiver<CallEvent>,
    devices: &Devices,
    callee: &str,
    tell: &impl Fn(Listen),
) -> Result<(), Failure> {
    let mut shown = Shown {
        far: FarEnd::default(),
        callee,
        people: None,
        watched: Vec::new(),
        cameras_on: std::collections::BTreeSet::new(),
        tell,
        ended: None,
    };
    let (mut speech, mut your_speech) = (Speech::default(), Speech::default());
    let mut looks = tokio::time::interval(LISTEN_EVERY);
    looks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = &mut call => break,
            Some(event) = events.recv() => shown.heard(event),
            _ = looks.tick() => {
                let now = Instant::now();
                if let Some(speaking) = speech.heard(devices.meter.take_loudness() >= SPEECH_RMS, now) {
                    shown.change(|far| far.speaking = speaking);
                }
                if let Some(speaking) = your_speech.heard(devices.spoke.swap(false, Ordering::Relaxed), now) {
                    shown.change(|far| far.me_speaking = speaking);
                }
            }
        }
    }
    // The call's last words (its end) came before its task was done.
    while let Ok(event) = events.try_recv() {
        shown.heard(event);
    }
    shown.ended.unwrap_or(Ok(()))
}

/// One call we place, from ringing to its end.
async fn run(
    client: TeamsClient,
    place: Place,
    controls: mpsc::UnboundedReceiver<Control>,
    wanted: Wanted,
    sink: Sink,
) {
    let tell = teller(&place, &sink);
    tell(Listen::Joining);
    tell(Listen::Roster(roster(&place.callee, FarEnd::default())));
    let (devices, audio) = match Devices::open(&place, wanted, &sink, &tell).await {
        Ok(opened) => opened,
        Err(failure) => {
            tell(Listen::Ended(Err(failure)));
            return;
        }
    };
    let (told, mut events) = mpsc::unbounded_channel();
    let call = outgoing(
        client,
        place.callee.clone(),
        audio,
        controls,
        move |event| {
            let _ = told.send(event);
        },
    );
    tokio::pin!(call);
    let result = follow(call, &mut events, &devices, &place.callee, &tell).await;
    // The end is told only once the devices are closed, so nothing of
    // this call follows it.
    devices.close().await;
    tell(Listen::Ended(ending(result)));
}

/// One meeting, from joining (or making it) to its end.
async fn run_meeting(
    client: TeamsClient,
    team: String,
    join: Join,
    controls: mpsc::UnboundedReceiver<Control>,
    wanted: Wanted,
    sink: Sink,
) {
    let place = Place {
        team,
        channel: crate::meetings::MEETING_CHANNEL.to_owned(),
        callee: String::new(),
    };
    let tell = teller(&place, &sink);
    tell(Listen::Joining);
    let found = match join {
        Join::Meeting(found) => found,
        Join::Now(subject) => {
            let made = client.meet_now(&subject).await.and_then(|made| {
                log::info!("Teams meeting: made one to start now");
                crate::meetings::Meeting::parse(&made.join, "")
                    .map_err(|_| Failure::Unexpected("the meeting made has no link".into()))
            });
            match made {
                Ok(made) => made,
                Err(failure) => {
                    tell(Listen::Ended(Err(failure)));
                    return;
                }
            }
        }
    };
    tell(Listen::Invite(crate::meetings::MeetingLink(found.url())));
    tell(Listen::Roster(meeting_roster(&[], false)));
    let (devices, audio) = match Devices::open(&place, wanted, &sink, &tell).await {
        Ok(opened) => opened,
        Err(failure) => {
            tell(Listen::Ended(Err(failure)));
            return;
        }
    };
    let (told, mut events) = mpsc::unbounded_channel();
    let call = meeting(client, found, audio, controls, move |event| {
        let _ = told.send(event);
    });
    tokio::pin!(call);
    let result = follow(call, &mut events, &devices, "", &tell).await;
    devices.close().await;
    tell(Listen::Ended(ending(result)));
}

/// One incoming call, from ringing here to its end.
async fn ring(
    client: TeamsClient,
    team: String,
    call: Incoming,
    decisions: oneshot::Receiver<Decision>,
    controls: mpsc::UnboundedReceiver<Control>,
    wanted: Wanted,
    sink: Sink,
) {
    let call_id = call.call_id.clone();
    // The caller as the interface knows people: `live:…` or an object id.
    let caller = crate::backend::teams_translate::clean_teams_user_id(&call.caller.id)
        .unwrap_or_else(|| call.caller.id.clone());
    let invite = |event: people::Event| {
        sink.send(Event::People {
            team: team.clone(),
            event,
        });
    };
    // Shown in the chat with the caller, which the interface finds.
    invite(people::Event::HuddleInvite {
        channel: caller.clone(),
        room: call_id.clone(),
        from: caller.clone(),
    });
    let stopped_ringing = || {
        invite(people::Event::HuddleInviteCancelled {
            channel: None,
            room: Some(call_id.clone()),
        });
    };
    let (answer, answered) = oneshot::channel();
    let (told, mut events) = mpsc::unbounded_channel();
    let driving = incoming(client, call, answered, controls, move |event| {
        let _ = told.send(event);
    });
    tokio::pin!(driving);
    let mut decisions = decisions;
    let channel = loop {
        tokio::select! {
            // Ended while ringing: the caller gave up, it was picked up
            // elsewhere, or it failed.
            () = &mut driving => {
                let mut elsewhere = false;
                while let Ok(event) = events.try_recv() {
                    elsewhere |= event == CallEvent::AnsweredElsewhere;
                }
                if elsewhere {
                    invite(people::Event::CallTakenElsewhere {
                        call: call_id.clone(),
                    });
                } else {
                    stopped_ringing();
                }
                return;
            }
            decided = &mut decisions => match decided {
                Ok(Decision::Accept { channel }) => break channel,
                Ok(Decision::Decline) | Err(_) => {
                    let _ = answer.send(Answer::Decline);
                    driving.await;
                    stopped_ringing();
                    return;
                }
            },
            // Its news while it rings changes nothing shown.
            Some(_) = events.recv() => {}
        }
    };
    let place = Place {
        team: team.clone(),
        channel,
        callee: caller,
    };
    let tell = teller(&place, &sink);
    tell(Listen::Roster(roster(&place.callee, FarEnd::default())));
    let (devices, audio) = match Devices::open(&place, wanted, &sink, &tell).await {
        Ok(opened) => opened,
        Err(failure) => {
            let _ = answer.send(Answer::Decline);
            driving.await;
            tell(Listen::Ended(Err(failure)));
            return;
        }
    };
    if answer.send(Answer::Accept(audio)).is_err() {
        devices.close().await;
        tell(Listen::Ended(Ok(Left::Ended)));
        return;
    }
    let result = follow(driving, &mut events, &devices, &place.callee, &tell).await;
    devices.close().await;
    tell(Listen::Ended(ending(result)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_you_and_the_one_you_rang() {
        let roster = roster(
            "8:live:ana",
            FarEnd {
                muted: true,
                speaking: false,
                me_speaking: true,
            },
        );
        assert_eq!(roster.people.len(), 2);
        assert!(!roster.alone());
        assert_eq!(roster.people[0].user.as_deref(), Some("8:live:ana"));
        assert!(roster.people[0].muted);
        assert!(roster.people[1].me && roster.people[1].speaking);
    }

    #[test]
    fn speaking_shows_while_loud_and_a_moment_after() {
        let start = Instant::now();
        let mut speech = Speech::default();
        assert_eq!(speech.heard(false, start), None);
        assert_eq!(speech.heard(true, start), Some(true));
        // A pause between words keeps the mark.
        let pause = start + Duration::from_millis(200);
        assert_eq!(speech.heard(false, pause), None);
        let quiet = start + SPEECH_HOLD;
        assert_eq!(speech.heard(false, quiet), Some(false));
        assert_eq!(speech.heard(false, quiet + LISTEN_EVERY), None);
    }

    #[test]
    fn an_ending_that_reaches_the_interface_was_the_far_ends() {
        assert_eq!(ending(Ok(())), Ok(Left::Ended));
        assert_eq!(
            ending(Err(Failure::CallDeclined)),
            Err(Failure::CallDeclined)
        );
    }
}
