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
//! (bindings to the system's libpipewire) on the share's thread.
//!
//! When the share encodes on a GPU that imports dma-bufs, the stream
//! offers packed 8-bit RGB as a dma-buf with a linear layout first (the
//! one every driver here imports; the compositor copies into it on its
//! GPU), and in shared memory second: the compositor picks. A dma-buf
//! goes from PipeWire to the GPU's encoder without the processor reading
//! a pixel ([`super::DmaBuf`]). If the GPU stops taking them, the stream
//! asks again for memory only.
//!
//! The portal remembers what was picked (its persist mode, and the
//! restore token it gives back, which the app keeps for the run), so
//! sharing again starts at once; [`ShareChoice::System`] with `again`
//! forgets it and asks.
//!
//! The one `unsafe` here borrows the file descriptor of a dma-buf
//! PipeWire lends while the frame is handled.

use std::cell::{Cell, RefCell};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use noslacking_video_ipc::{ShareChoice, ShareProblem};
use pipewire as pw;
use pw::spa;

use super::{DmaBuf, Frame, Order, Packed, Trouble};
use crate::share::{Ask, Pipeline, Settings, Share};

/// `DRM_FORMAT_MOD_LINEAR`: rows one after another, as in memory.
const MOD_LINEAR: i64 = 0;
/// `SPA_VIDEO_FLAG_MODIFIER`: the negotiated format has a modifier, so
/// its buffers are dma-bufs.
const VIDEO_FLAG_MODIFIER: u32 = 1 << 2;
/// `SPA_POD_PROP_FLAG_MANDATORY`.
const PROP_MANDATORY: u32 = 1 << 3;
/// `SPA_POD_PROP_FLAG_DONT_FIXATE`.
const PROP_DONT_FIXATE: u32 = 1 << 4;

/// What the portal gave.
struct Picked {
    proxy: Screencast,
    session: ashpd::desktop::Session<Screencast>,
    node: u32,
    fd: OwnedFd,
    token: Option<String>,
    size: Option<(i32, i32)>,
}

/// Asks the portal (its dialog, unless it remembers `restore`), then
/// starts the PipeWire stream on the share's thread. Blocks until the
/// user has chosen.
pub fn start(
    choice: &ShareChoice,
    settings: Settings,
    restore: &str,
) -> Result<(Share, String), Trouble> {
    let again = matches!(choice, ShareChoice::System { again: true });
    let restore = (!again && !restore.is_empty()).then(|| restore.to_owned());
    let picked = futures_lite::future::block_on(ask(restore))?;
    let Picked {
        proxy,
        session,
        node,
        fd,
        token,
        size,
    } = picked;
    eprintln!("noslacking-video: share: the portal gave PipeWire node {node}, {size:?}");
    let (asks, inbox) = pw::channel::channel::<Ask>();
    let (started, result) = mpsc::channel();
    let ended = Arc::new(Mutex::new(None));
    let thread_ended = Arc::clone(&ended);
    let thread = std::thread::Builder::new()
        .name("noslacking-share-pipewire".into())
        .spawn(move || {
            let trouble = stream(fd, node, settings, inbox, &started);
            *thread_ended.lock().unwrap_or_else(PoisonError::into_inner) = trouble;
            // Whatever happened, the portal's session is closed when the
            // capture stops, so the desktop's "sharing" indicator goes.
            if let Err(error) = futures_lite::future::block_on(session.close()) {
                eprintln!("noslacking-video: share: closing the portal session: {error}");
            }
            drop(proxy);
        })
        .map_err(|e| Trouble::failed(format!("no capture thread: {e}")))?;
    let send = move |ask| asks.send(ask).is_ok();
    let share = Share::new(Box::new(send), thread, ended);
    match result.recv() {
        Ok(Ok(())) => Ok((share, token.unwrap_or_default())),
        Ok(Err(trouble)) => Err(trouble),
        Err(_) => Err(Trouble::failed("the capture thread stopped")),
    }
}

/// What an `ashpd` error means for the user.
fn portal_error(error: ashpd::Error) -> Trouble {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
            Trouble::new(ShareProblem::Cancelled, "the dialog was closed")
        }
        ashpd::Error::Portal(ashpd::PortalError::Cancelled(why)) => {
            Trouble::new(ShareProblem::Cancelled, why)
        }
        ashpd::Error::Portal(ashpd::PortalError::NotAllowed(why)) => {
            Trouble::new(ShareProblem::Denied, why)
        }
        ashpd::Error::Zbus(error) => Trouble::new(
            ShareProblem::Unavailable,
            format!("the ScreenCast portal: {error}"),
        ),
        other => Trouble::failed(format!("the ScreenCast portal: {other}")),
    }
}

/// The portal's steps: a session, its sources, Start (the dialog), the
/// PipeWire connection.
async fn ask(restore: Option<String>) -> Result<Picked, Trouble> {
    let proxy = Screencast::new().await.map_err(portal_error)?;
    let types = proxy.available_source_types().await.map_err(portal_error)?;
    let cursors = proxy.available_cursor_modes().await.unwrap_or_default();
    eprintln!(
        "noslacking-video: share: ScreenCast portal version {}, sources {types:?}, cursors \
         {cursors:?}",
        proxy.version()
    );
    if types.is_empty() {
        return Err(Trouble::new(
            ShareProblem::Unavailable,
            "the portal offers no sources",
        ));
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
        return Err(Trouble::failed("the portal gave no stream"));
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
    width: u32,
    height: u32,
    order: Option<Order>,
    alpha: bool,
    /// Its buffers are dma-bufs with this modifier.
    dmabuf: Option<u64>,
    /// Buffers in memory that could not be read, said once.
    unreadable: u64,
}

/// The formats the stream offers, each a serialized EnumFormat: 8-bit
/// RGB as a linear dma-buf first if `dmabuf`, then in memory; any size,
/// up to 30 frames a second (15 preferred).
fn enum_formats(dmabuf: bool) -> Vec<Vec<u8>> {
    let mut formats = Vec::new();
    if dmabuf {
        formats.extend(enum_format(true));
    }
    formats.extend(enum_format(false));
    formats
}

fn enum_format(dmabuf: bool) -> Option<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let mut object = spa::pod::object!(
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
    if dmabuf {
        // Only the linear layout: every VA-API driver imports it, and the
        // compositor's GPU copies into it. The compositor fixes it.
        object.properties.push(spa::pod::Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: spa::pod::PropertyFlags::from_bits_retain(PROP_MANDATORY | PROP_DONT_FIXATE),
            value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Enum {
                    default: MOD_LINEAR,
                    alternatives: vec![MOD_LINEAR],
                },
            ))),
        });
    }
    serialize(object)
}

/// The Buffers parameter: dma-bufs, or memory the stream maps.
fn buffers(dmabuf: bool) -> Option<Vec<u8>> {
    use spa::buffer::DataType;
    let types = if dmabuf {
        1 << DataType::DmaBuf.as_raw()
    } else {
        (1 << DataType::MemPtr.as_raw()) | (1 << DataType::MemFd.as_raw())
    };
    serialize(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![spa::pod::Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            spa::pod::Value::Int(types),
        )],
    })
}

fn serialize(object: spa::pod::Object) -> Option<Vec<u8>> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .ok()
    .map(|(cursor, _)| cursor.into_inner())
}

/// Sets `stream`'s parameters to `params` (serialized pods).
fn update(stream: &pw::stream::Stream, params: &[Vec<u8>]) {
    let mut pods: Vec<&spa::pod::Pod> = params
        .iter()
        .filter_map(|bytes| spa::pod::Pod::from_bytes(bytes))
        .collect();
    if let Err(error) = stream.update_params(&mut pods) {
        eprintln!("noslacking-video: share: PipeWire parameters: {error}");
    }
}

/// The share's thread: connects through the portal's `fd` to `node`,
/// hands what comes to the share's pipeline and the app's asks to it,
/// until the share is closed. Why the capture ended, if it did.
fn stream(
    fd: OwnedFd,
    node: u32,
    settings: Settings,
    inbox: pw::channel::Receiver<Ask>,
    started: &mpsc::Sender<Result<(), Trouble>>,
) -> Option<Trouble> {
    let fail = |why: String| {
        let _ = started.send(Err(Trouble::failed(why)));
        None
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
    // Made here: the GPU's state stays on this thread.
    let pipeline = Rc::new(RefCell::new(Pipeline::new(settings)));
    let dmabuf = pipeline.borrow().takes_dmabuf();
    let stop = Rc::new(Cell::new(false));
    let _inbox = inbox.attach(main_loop.loop_(), {
        let pipeline = Rc::clone(&pipeline);
        let stop = Rc::clone(&stop);
        move |ask| match ask {
            Ask::Stop => stop.set(true),
            ask => pipeline.borrow_mut().ask(ask),
        }
    });
    // Set once asked to stop: the stream going unconnected then is ours.
    let quitting = Rc::new(Cell::new(false));
    let state_pipeline = Rc::clone(&pipeline);
    let state_quitting = Rc::clone(&quitting);
    let put = Rc::clone(&pipeline);
    let listener = stream
        .add_local_listener_with_user_data(Format::default())
        .state_changed(move |_, _, old, new| {
            eprintln!("noslacking-video: share: PipeWire stream {old:?} → {new:?}");
            if state_quitting.get() {
                return;
            }
            let ended = |why: String| {
                state_pipeline
                    .borrow_mut()
                    .end(Trouble::new(ShareProblem::Ended, why));
            };
            match new {
                pw::stream::StreamState::Error(why) => ended(format!("PipeWire: {why}")),
                pw::stream::StreamState::Unconnected => ended("PipeWire: unconnected".into()),
                _ => {}
            }
        })
        .param_changed(|stream, format, id, param| {
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
            (format.order, format.alpha) = match info.format() {
                VideoFormat::BGRx => (Some(Order::Bgra), false),
                VideoFormat::BGRA => (Some(Order::Bgra), true),
                VideoFormat::RGBx => (Some(Order::Rgba), false),
                VideoFormat::RGBA => (Some(Order::Rgba), true),
                _ => (None, false),
            };
            let size = info.size();
            format.width = size.width;
            format.height = size.height;
            let dmabuf = info.flags().bits() & VIDEO_FLAG_MODIFIER != 0;
            format.dmabuf = dmabuf.then(|| info.modifier());
            eprintln!(
                "noslacking-video: share: PipeWire format {:?} {}x{} at {}/{}, {}",
                info.format(),
                size.width,
                size.height,
                info.framerate().num,
                info.framerate().denom,
                match format.dmabuf {
                    Some(modifier) => format!("dma-bufs (modifier {modifier:#x})"),
                    None => "in memory".to_owned(),
                }
            );
            if let Some(buffers) = buffers(dmabuf) {
                update(stream, &[buffers]);
            }
        })
        .process(move |stream, format| {
            // The newest buffer: any older one waiting goes back at once.
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            while let Some(newer) = stream.dequeue_buffer() {
                buffer = newer;
            }
            let (Some(order), width, height) = (format.order, format.width, format.height) else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            // An empty chunk is a frame with no new picture (the cursor
            // moved, say); a corrupted one is not to be shown.
            if chunk.size() == 0
                || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED)
                || width == 0
                || height == 0
            {
                return;
            }
            let offset = chunk.offset();
            let stride = u32::try_from(chunk.stride()).unwrap_or(0);
            let now = Instant::now();
            if data.type_() == spa::buffer::DataType::DmaBuf {
                let raw = data.fd();
                let Some(modifier) = format.dmabuf else {
                    return;
                };
                if raw < 0 {
                    return;
                }
                // SAFETY: PipeWire lends this buffer, and with it its
                // descriptor, from dequeue until `buffer` is queued
                // back when it drops at the end of this callback; the
                // borrow is used only within it (imported into the GPU
                // or not at all) and never kept.
                let fd = unsafe { BorrowedFd::borrow_raw(raw) };
                let frame = Frame::DmaBuf(DmaBuf {
                    fd,
                    width,
                    height,
                    offset,
                    stride,
                    order,
                    alpha: format.alpha,
                    modifier,
                });
                put.borrow_mut().put(&frame, now);
                return;
            }
            let size = usize::try_from(chunk.size()).unwrap_or(0);
            let offset = usize::try_from(offset).unwrap_or(0);
            let stride = (stride as usize).max(width as usize * 4);
            let Some(bytes) = data.data() else {
                format.unreadable += 1;
                if format.unreadable == 1 {
                    eprintln!("noslacking-video: share: PipeWire gave a buffer not in memory");
                }
                return;
            };
            let Some(pixels) = bytes.get(offset..offset.saturating_add(size).min(bytes.len()))
            else {
                return;
            };
            let frame = Frame::Packed(Packed {
                width,
                height,
                stride,
                order,
                data: pixels,
            });
            put.borrow_mut().put(&frame, now);
        })
        .register();
    let _listener = match listener {
        Ok(listener) => listener,
        Err(error) => return fail(format!("PipeWire: no listener: {error}")),
    };
    let formats = enum_formats(dmabuf);
    let mut params: Vec<&spa::pod::Pod> = formats
        .iter()
        .filter_map(|bytes| spa::pod::Pod::from_bytes(bytes))
        .collect();
    if params.is_empty() {
        return fail("PipeWire: no format to offer".into());
    }
    if let Err(error) = stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ) {
        return fail(format!("PipeWire: could not connect the stream: {error}"));
    }
    let _ = started.send(Ok(()));
    let mut in_memory = !dmabuf;
    while !stop.get() {
        let wait = pipeline
            .borrow()
            .deadline()
            .map_or(Duration::from_millis(100), |at| {
                at.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100))
            });
        main_loop.loop_().iterate(pw::loop_::Timeout::Finite(wait));
        pipeline.borrow_mut().tick(Instant::now());
        if !in_memory && pipeline.borrow().wants_memory() {
            // The GPU stopped taking dma-bufs: memory only, from now on.
            in_memory = true;
            update(&stream, &enum_formats(false));
        }
    }
    quitting.set(true);
    let _ = stream.disconnect();
    pipeline.borrow().ended()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A pretend screen in PipeWire's own graph, as a compositor's: a
    /// video source of 1280×720 BGRx in memory at 15 a second (driven
    /// by the graph), each picture a new grey. Sends its node's id once
    /// it has one; runs until `stop`.
    fn pretend_screen(ready: mpsc::Sender<u32>, stop: Arc<AtomicBool>) {
        use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        use spa::param::video::VideoFormat;
        const SIZE: (u32, u32) = (1280, 720);
        pw::init();
        let main_loop = pw::main_loop::MainLoopBox::new(None).expect("a loop");
        let context = pw::context::ContextBox::new(main_loop.loop_(), None).expect("a context");
        let core = context.connect(None).expect("PipeWire");
        let stream = pw::stream::StreamBox::new(
            &core,
            "noslacking-test-screen",
            pw::properties::properties! {
                *pw::keys::MEDIA_CLASS => "Video/Source",
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Source",
            },
        )
        .expect("a stream");
        let _listener = stream
            .add_local_listener_with_user_data(0u8)
            .param_changed(|stream, _, id, param| {
                if id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                    return;
                }
                let stride = i32::try_from(SIZE.0 * 4).expect("small");
                let buffers = serialize(spa::pod::Object {
                    type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
                    id: spa::param::ParamType::Buffers.as_raw(),
                    properties: vec![
                        spa::pod::Property::new(
                            spa::sys::SPA_PARAM_BUFFERS_buffers,
                            spa::pod::Value::Int(4),
                        ),
                        spa::pod::Property::new(
                            spa::sys::SPA_PARAM_BUFFERS_blocks,
                            spa::pod::Value::Int(1),
                        ),
                        spa::pod::Property::new(
                            spa::sys::SPA_PARAM_BUFFERS_size,
                            spa::pod::Value::Int(stride * i32::try_from(SIZE.1).expect("small")),
                        ),
                        spa::pod::Property::new(
                            spa::sys::SPA_PARAM_BUFFERS_stride,
                            spa::pod::Value::Int(stride),
                        ),
                    ],
                })
                .expect("serialized");
                update(stream, &[buffers]);
            })
            .process(|stream, grey| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                *grey = grey.wrapping_add(9);
                let size = (SIZE.0 * SIZE.1 * 4) as usize;
                if let Some(bytes) = data.data() {
                    let n = size.min(bytes.len());
                    bytes[..n].fill(*grey);
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = i32::try_from(SIZE.0 * 4).expect("small");
                *chunk.size_mut() = u32::try_from(size).expect("small");
            })
            .register()
            .expect("a listener");
        let format = serialize(spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
            spa::pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                spa::utils::Rectangle {
                    width: SIZE.0,
                    height: SIZE.1
                }
            ),
            spa::pod::property!(
                FormatProperties::VideoFramerate,
                Fraction,
                spa::utils::Fraction { num: 15, denom: 1 }
            ),
        ))
        .expect("serialized");
        let mut params = [spa::pod::Pod::from_bytes(&format).expect("a pod")];
        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .expect("connected");
        let mut told = false;
        while !stop.load(Ordering::Relaxed) {
            main_loop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(5)));
            if !told && stream.node_id() != u32::MAX {
                told = ready.send(stream.node_id()).is_ok();
            }
        }
        let _ = stream.disconnect();
    }

    /// The share's PipeWire stream against a pretend screen in this
    /// machine's PipeWire (no portal, no one's screen): the formats
    /// offered (a dma-buf first, which this source cannot give) settle
    /// on memory, frames arrive and encode, and the source going away
    /// ends the share. Needs a running PipeWire:
    /// `cargo test -p noslacking-video --features pipewire -- --ignored pipewire --nocapture`
    #[test]
    #[ignore = "needs a running PipeWire"]
    fn pipewire_hands_a_screen_to_the_share() {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, node) = mpsc::channel();
        let screen = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || pretend_screen(ready, stop))
        };
        let node = node
            .recv_timeout(Duration::from_secs(5))
            .expect("the pretend screen's node");
        // What OpenPipeWireRemote hands over: a connection to PipeWire.
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
        let socket = std::os::unix::net::UnixStream::connect(
            std::path::Path::new(&runtime).join("pipewire-0"),
        )
        .expect("PipeWire's socket");
        let fd = OwnedFd::from(socket);
        let (asks, inbox) = pw::channel::channel::<Ask>();
        let (started, result) = mpsc::channel();
        let share = std::thread::spawn(move || {
            // The GPU if this machine has one that encodes, so dma-bufs
            // are offered first.
            let settings = Settings {
                hardware: true,
                bitrate: 1_000_000,
                gpu: crate::choose_backend().share_gpu(),
            };
            stream(fd, node, settings, inbox, &started)
        });
        result
            .recv_timeout(Duration::from_secs(5))
            .expect("an answer")
            .expect("the stream connected");
        let next = |wait: u64| {
            let (reply, answer) = mpsc::channel();
            assert!(
                asks.send(Ask::Next {
                    force_keyframe: false,
                    repeat: false,
                    wait: Duration::from_millis(wait),
                    reply,
                })
                .is_ok()
            );
            answer
                .recv_timeout(Duration::from_secs(2))
                .expect("answered")
        };
        let mut frames = Vec::new();
        for _ in 0..40 {
            if let Ok(Some(frame)) = next(200) {
                frames.push(frame);
            }
            if frames.len() == 3 {
                break;
            }
        }
        assert_eq!(frames.len(), 3, "frames from PipeWire");
        assert!(frames[0].keyframe);
        assert!(frames.iter().all(|f| (f.width, f.height) == (1280, 720)));
        // The screen goes: the share hears it end.
        stop.store(true, Ordering::Relaxed);
        screen.join().expect("the pretend screen stopped");
        let mut ended = None;
        for _ in 0..20 {
            if let Err(trouble) = next(100) {
                ended = Some(trouble.problem);
                break;
            }
        }
        assert_eq!(ended, Some(ShareProblem::Ended));
        let _ = asks.send(Ask::Stop);
        let trouble = share.join().expect("the share's thread");
        assert_eq!(trouble.map(|t| t.problem), Some(ShareProblem::Ended));
    }
}
