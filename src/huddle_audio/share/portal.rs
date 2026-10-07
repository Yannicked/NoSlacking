//! Capture through the ScreenCast portal (xdg-desktop-portal) and
//! PipeWire: how a Wayland session, and any app in a Flatpak, may see the
//! screen.
//!
//! The portal is spoken over D-Bus by `ashpd` (pure Rust, on `zbus`): a
//! session, its sources (screens and windows, the cursor drawn in, one
//! source), then Start, which shows the desktop's own dialog; what the
//! user picked comes back as a PipeWire node, and OpenPipeWireRemote
//! hands over a connection to PipeWire that sees only it (so no Flatpak
//! permission is needed). The frames come through the `pipewire` crate
//! (bindings to the system's libpipewire) on a thread of its own, asked
//! for as packed 8-bit RGB in system memory (no DMA-BUF modifiers are
//! offered, so the compositor copies into shared memory), at most
//! [`super::FPS`] a second.
//!
//! The portal remembers what was picked for as long as the app runs (its
//! persist mode 1 and the restore token it gives back), so sharing again
//! starts at once; [`super::Choice::System`] with `again` forgets it and
//! asks.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pw::spa;

use super::{Capturing, Choice, Ended, Frames, Order, Packed, Picture, ShareError, ShareFrame};

/// The restore token from the last share, for this run of the app.
static RESTORE: Mutex<Option<String>> = Mutex::new(None);

/// The portal and PipeWire.
#[derive(Debug)]
pub struct Portal {
    frames: Frames,
}

impl Portal {
    /// The portal, its frames going to `frames`.
    pub fn new(frames: Frames) -> Self {
        Self { frames }
    }

    /// Asks the portal (its dialog, unless it remembers), then starts
    /// the PipeWire stream. Blocks until the user has chosen: call it off
    /// the async threads.
    pub fn start(&mut self, choice: &Choice, ended: Ended) -> Result<Capturing, ShareError> {
        let again = matches!(choice, Choice::System { again: true });
        if again {
            *RESTORE.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
        let restore = RESTORE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // The portal is async; this runs on a blocking thread of the
        // app's runtime, or makes a small runtime of its own (the probe).
        let (handle, _own) = match tokio::runtime::Handle::try_current() {
            Ok(handle) => (handle, None),
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| ShareError::Failed(format!("no runtime: {e}")))?;
                (runtime.handle().clone(), Some(runtime))
            }
        };
        let picked = handle.block_on(ask(restore))?;
        let Picked {
            proxy,
            session,
            node,
            fd,
            token,
            size,
        } = picked;
        if let Some(token) = token {
            *RESTORE.lock().unwrap_or_else(PoisonError::into_inner) = Some(token);
        }
        log::info!("huddle share: the portal gave PipeWire node {node}, {size:?}");
        let frames = self.frames.clone();
        let stream_ended = ended.clone();
        let running = super::spawn_capture("noslacking-share-pipewire", move |stop, started| {
            stream(fd, node, &frames, &stream_ended, stop, started);
        });
        // Whatever happens next, the portal's session is closed when the
        // capture stops, so the desktop's "sharing" indicator goes away.
        let close = move || {
            let closing = handle.spawn(async move {
                if let Err(error) = session.close().await {
                    log::debug!("huddle share: closing the portal session: {error}");
                }
                drop(proxy);
            });
            drop(closing);
        };
        match running {
            Ok(running) => Ok(Capturing::new(running, Some(Box::new(close)))),
            Err(error) => {
                close();
                Err(error)
            }
        }
    }
}

/// What the portal gave.
struct Picked {
    proxy: Screencast,
    session: ashpd::desktop::Session<Screencast>,
    node: u32,
    fd: OwnedFd,
    token: Option<String>,
    size: Option<(i32, i32)>,
}

/// What an `ashpd` error means for the user.
fn portal_error(error: ashpd::Error) -> ShareError {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => ShareError::Cancelled,
        ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => ShareError::Cancelled,
        ashpd::Error::Portal(ashpd::PortalError::NotAllowed(_)) => ShareError::Denied,
        ashpd::Error::Zbus(error) => {
            log::warn!("huddle share: the ScreenCast portal: {error}");
            ShareError::Unavailable
        }
        other => ShareError::Failed(format!("the ScreenCast portal: {other}")),
    }
}

/// The portal's steps: a session, its sources, Start (the dialog), the
/// PipeWire connection.
async fn ask(restore: Option<String>) -> Result<Picked, ShareError> {
    let proxy = Screencast::new().await.map_err(portal_error)?;
    let types = proxy.available_source_types().await.map_err(portal_error)?;
    let cursors = proxy.available_cursor_modes().await.unwrap_or_default();
    log::info!(
        "huddle share: ScreenCast portal version {}, sources {types:?}, cursors {cursors:?}",
        proxy.version()
    );
    if types.is_empty() {
        return Err(ShareError::Unavailable);
    }
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(portal_error)?;
    // The pointer drawn into the picture, as Slack's own shares show it.
    let cursor = if cursors.contains(CursorMode::Embedded) {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };
    let wanted = types & (SourceType::Monitor | SourceType::Window);
    let mut options = SelectSourcesOptions::default()
        .set_cursor_mode(cursor)
        .set_sources(if wanted.is_empty() { types } else { wanted })
        .set_multiple(false);
    // Persisting needs version 4 of the interface.
    if proxy.version() >= 4 {
        options = options
            .set_persist_mode(PersistMode::Application)
            .set_restore_token(restore.as_deref());
    }
    proxy
        .select_sources(&session, options)
        .await
        .map_err(portal_error)?
        .response()
        .map_err(portal_error)?;
    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(portal_error)?
        .response()
        .map_err(portal_error)?;
    let Some(first) = streams.streams().first() else {
        return Err(ShareError::Failed("the portal gave no stream".into()));
    };
    let node = first.pipe_wire_node_id();
    let size = first.size();
    let token = streams.restore_token().map(str::to_owned);
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(portal_error)?;
    Ok(Picked {
        proxy,
        session,
        node,
        fd,
        token,
        size,
    })
}

/// The stream's format, once PipeWire and the compositor agreed on one.
#[derive(Default)]
struct Format {
    width: usize,
    height: usize,
    order: Option<Order>,
    /// When the last frame was kept, to keep at most FPS a second.
    last: Option<Instant>,
    /// Unreadable buffers seen (DMA-BUF), said once.
    unreadable: u64,
}

/// The EnumFormat the stream offers: 8-bit RGB in system memory, any
/// size, up to 30 frames a second (15 preferred).
fn enum_format() -> Option<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 16,
                height: 16
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction {
                num: super::FPS,
                denom: 1
            },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 30, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .ok()
    .map(|(cursor, _)| cursor.into_inner())
}

/// The PipeWire thread: connects through the portal's `fd` to `node` and
/// puts what comes in `frames` until `stop`, or until the stream ends.
fn stream(
    fd: OwnedFd,
    node: u32,
    frames: &Frames,
    ended: &Ended,
    stop: &AtomicBool,
    started: &std::sync::mpsc::Sender<Result<(), ShareError>>,
) {
    let fail = |why: String| {
        let _ = started.send(Err(ShareError::Failed(why)));
    };
    pw::init();
    let main_loop = match pw::main_loop::MainLoopBox::new(None) {
        Ok(main_loop) => main_loop,
        Err(error) => return fail(format!("PipeWire: no loop: {error}")),
    };
    let context = match pw::context::ContextBox::new(main_loop.loop_(), None) {
        Ok(context) => context,
        Err(error) => return fail(format!("PipeWire: no context: {error}")),
    };
    let core = match context.connect_fd(fd, None) {
        Ok(core) => core,
        Err(error) => return fail(format!("PipeWire: could not connect: {error}")),
    };
    let properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    let stream = match pw::stream::StreamBox::new(&core, "noslacking-share", properties) {
        Ok(stream) => stream,
        Err(error) => return fail(format!("PipeWire: no stream: {error}")),
    };
    let put = frames.clone();
    let state_ended = ended.clone();
    // Set once asked to stop: the stream going unconnected then is ours.
    let quitting = std::sync::Arc::new(AtomicBool::new(false));
    let quit_seen = quitting.clone();
    let listener = stream
        .add_local_listener_with_user_data(Format::default())
        .state_changed(move |_, _, old, new| {
            log::info!("huddle share: PipeWire stream {old:?} → {new:?}");
            if quit_seen.load(Ordering::Relaxed) {
                return;
            }
            match new {
                pw::stream::StreamState::Error(why) => state_ended.end(&format!("PipeWire: {why}")),
                pw::stream::StreamState::Unconnected => state_ended.end("PipeWire: unconnected"),
                _ => {}
            }
        })
        .param_changed(|_, format, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media, subtype)) = spa::param::format_utils::parse_format(param) else {
                return;
            };
            if media != spa::param::format::MediaType::Video
                || subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            use spa::param::video::VideoFormat;
            format.order = match info.format() {
                VideoFormat::BGRx | VideoFormat::BGRA => Some(Order::Bgra),
                VideoFormat::RGBx | VideoFormat::RGBA => Some(Order::Rgba),
                _ => None,
            };
            let size = info.size();
            format.width = usize::try_from(size.width).unwrap_or(0);
            format.height = usize::try_from(size.height).unwrap_or(0);
            log::info!(
                "huddle share: PipeWire format {:?} {}x{} at {}/{}",
                info.format(),
                size.width,
                size.height,
                info.framerate().num,
                info.framerate().denom
            );
        })
        .process(move |stream, format| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let now = Instant::now();
            let every = Duration::from_secs(1) / super::FPS;
            if format.last.is_some_and(|at| now < at + every) {
                return;
            }
            let (Some(order), width, height) = (format.order, format.width, format.height) else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let offset = usize::try_from(chunk.offset()).unwrap_or(0);
            let size = usize::try_from(chunk.size()).unwrap_or(0);
            let stride = usize::try_from(chunk.stride()).unwrap_or(0).max(width * 4);
            let Some(bytes) = data.data() else {
                format.unreadable += 1;
                if format.unreadable == 1 {
                    log::warn!(
                        "huddle share: PipeWire gave a buffer that is not in memory \
                         (DMA-BUF?); waiting for one that is"
                    );
                }
                return;
            };
            // An empty chunk is a frame with no new picture (cursor only).
            if size == 0 || width == 0 || height == 0 {
                return;
            }
            let Some(pixels) = bytes.get(offset..offset.saturating_add(size).min(bytes.len()))
            else {
                return;
            };
            let packed = Packed {
                width,
                height,
                stride,
                order,
                data: pixels.to_vec(),
            };
            if !packed.whole() {
                return;
            }
            format.last = Some(now);
            put.put(ShareFrame {
                picture: Picture::Packed(packed),
                at: now,
            });
        })
        .register();
    let _listener = match listener {
        Ok(listener) => listener,
        Err(error) => return fail(format!("PipeWire: no listener: {error}")),
    };
    let Some(bytes) = enum_format() else {
        return fail("PipeWire: no format to offer".into());
    };
    let Some(pod) = spa::pod::Pod::from_bytes(&bytes) else {
        return fail("PipeWire: the format did not serialize".into());
    };
    let mut params = [pod];
    if let Err(error) = stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ) {
        return fail(format!("PipeWire: could not connect the stream: {error}"));
    }
    let _ = started.send(Ok(()));
    while !stop.load(Ordering::Relaxed) {
        main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }
    quitting.store(true, Ordering::Relaxed);
    let _ = stream.disconnect();
    frames.clear();
}
