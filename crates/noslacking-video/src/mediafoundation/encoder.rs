//! A capture (a shared screen, the camera) encoded by the GPU vendor's
//! H.264 encoder through Media Foundation: the first hardware encoder
//! transform the system lists that takes NV12 and the settings below.
//!
//! Pictures go in from memory as NV12 (converted from what the capture
//! gives on the processor). Most hardware encoders are asynchronous:
//! they send an event when they want a picture and another when one is
//! encoded, which [`MfGpu::encode`] waits for in turn, so each picture
//! in gives its frame out before the call returns, as the pipeline
//! expects. An encoder that holds pictures back fails here after a
//! while, and the pipeline goes on in software.
//!
//! Set up as the VA-API encoder: constrained baseline (baseline where
//! the encoder refuses that name), no B-frames, CBR, low latency, a new
//! bit rate in place, IDRs when asked and every
//! [`super::IDR_EVERY_SECONDS`], each with the SPS and PPS in front.

use std::time::{Duration, Instant};

use noslacking_video_ipc::Planes;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, METransformHaveOutput,
    METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR,
    eAVEncH264VProfile_Base, eAVEncH264VProfile_ConstrainedBase,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface, PWSTR};

use super::mf::{self, Output, Platform, failure, variant_bool, variant_u32};
use crate::backend::{Encoded, Failure};
use crate::capture::{Frame, convert};
use crate::pipeline::Gpu;

/// How long one picture may take, from waiting for the encoder to want
/// it to its frame coming out: generous, as the first one sets the
/// encoder up.
const ENCODE_WAIT: Duration = Duration::from_secs(2);
/// How long to sleep between looks at an asynchronous encoder's events.
const POLL: Duration = Duration::from_micros(500);

/// Whether a hardware encoder opens for a 720p stream here: its name.
/// Run on the server's thread at start, for the capabilities.
pub fn probe() -> Result<String, String> {
    let _platform = Platform::start()?;
    let session = Session::open((1280, 720), 30, 1_000_000)?;
    Ok(session.name.clone())
}

/// A capture's encoder on the GPU, made on the capture's thread.
pub struct MfGpu {
    session: Option<Session>,
    /// The picture last put in, at the session's size.
    picture: Option<Planes>,
    // Last: the session goes before Media Foundation stops.
    _platform: Platform,
}

impl std::fmt::Debug for MfGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfGpu")
            .field("encoder", &self.session.as_ref().map(|s| &s.name))
            .finish_non_exhaustive()
    }
}

impl MfGpu {
    /// Starts Media Foundation on this thread; the encoder itself opens
    /// with the first picture's size.
    pub fn open() -> Result<Self, String> {
        Ok(Self {
            session: None,
            picture: None,
            _platform: Platform::start()?,
        })
    }
}

impl Gpu for MfGpu {
    fn name(&self) -> String {
        match &self.session {
            Some(session) => format!("the GPU (mediafoundation: {})", session.name),
            None => "the GPU (mediafoundation)".to_owned(),
        }
    }

    fn takes_dmabuf(&self) -> bool {
        false
    }

    fn max_size(&self) -> (u32, u32) {
        super::ENCODE_MAX
    }

    fn open(&mut self, size: (u32, u32), fps: u32, bitrate: u32) -> Result<(), Failure> {
        // The old one goes first: a GPU has only so many encoders.
        self.session = None;
        self.picture = None;
        self.session = Some(Session::open(size, fps, bitrate).map_err(Failure::unsupported)?);
        Ok(())
    }

    fn load(&mut self, frame: &Frame<'_>) -> Result<(), Failure> {
        let Some(session) = &self.session else {
            return Err(Failure::device("not opened"));
        };
        let (width, height) = session.size;
        let picture = match frame {
            Frame::Packed(packed) => {
                convert::to_i420(packed).ok_or_else(|| Failure::broken("an unusable picture"))?
            }
            Frame::I420(planes) => (*planes).clone(),
        };
        self.picture = Some(convert::scale(picture, width, height));
        Ok(())
    }

    fn encode(&mut self, force_keyframe: bool) -> Result<Encoded, Failure> {
        let (Some(session), Some(picture)) = (self.session.as_mut(), self.picture.as_ref()) else {
            return Err(Failure::device("nothing to encode"));
        };
        session.encode(picture, force_keyframe)
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Failure> {
        match self.session.as_mut() {
            Some(session) => {
                session.set_bitrate(bitrate);
                Ok(())
            }
            None => Ok(()),
        }
    }
}

/// One encoder transform, set up for one size.
struct Session {
    transform: IMFTransform,
    codec: ICodecAPI,
    /// Its events, if it is asynchronous.
    events: Option<IMFMediaEventGenerator>,
    activate: IMFActivate,
    /// Its name, for the log.
    name: String,
    streams: (u32, u32),
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    /// The bytes of a sample to give it, if it does not make its own.
    allocate: Option<u32>,
    /// Pictures it asked for and has not been given.
    wanted: u32,
    /// Frames it said are ready and that were not taken.
    ready: u32,
    frames: u64,
    since_idr: u32,
    /// A keyframe was asked for and not given yet: asked for again.
    owed_keyframe: bool,
    /// Setting a new bit rate failed once: not tried again.
    fixed_rate: bool,
    /// The SPS and PPS last seen, for IDRs that come without.
    sets: Vec<u8>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a plain call: hardware transforms ask to be shut down
        // through what made them.
        let _ = unsafe { self.activate.ShutdownObject() };
    }
}

impl Session {
    /// The first hardware encoder that takes `size` at `fps` and
    /// `bitrate` as set up here; why none did.
    fn open(size: (u32, u32), fps: u32, bitrate: u32) -> Result<Self, String> {
        let mut why = Vec::new();
        for activate in hardware_encoders()? {
            match Self::configure(activate, size, fps, bitrate) {
                Ok(session) => return Ok(session),
                Err(reason) => why.push(reason),
            }
        }
        if why.is_empty() {
            Err("no hardware H.264 encoder".to_owned())
        } else {
            Err(why.join("; "))
        }
    }

    /// Makes `activate`'s encoder and sets it up.
    fn configure(
        activate: IMFActivate,
        size: (u32, u32),
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, String> {
        let name = friendly_name(&activate).unwrap_or_else(|| "a hardware encoder".to_owned());
        let at = |what: &str| {
            let name = name.clone();
            let what = what.to_owned();
            move |e: windows::core::Error| format!("{name}: {what}: {e}")
        };
        let bitrate = bitrate.clamp(super::BITRATE_RANGE.0, super::BITRATE_RANGE.1);
        let fps = fps.max(1);
        // SAFETY: COM and Media Foundation are started on this thread;
        // every value handed over outlives its call.
        let made = unsafe {
            (|| {
                let transform: IMFTransform =
                    activate.ActivateObject().map_err(at("activating"))?;
                let attributes = transform.GetAttributes().map_err(at("its attributes"))?;
                let asynchronous = attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
                if asynchronous {
                    attributes
                        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .map_err(at("unlocking"))?;
                }
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
                let codec: ICodecAPI = transform.cast().map_err(at("no ICodecAPI"))?;
                let streams = mf::stream_ids(&transform);
                // Some encoders take the rate control only before the
                // output type: so first, and the rate again after.
                set_up(&codec, bitrate, fps).map_err(at("CBR"))?;
                let output = output_type(size, fps, bitrate, eAVEncH264VProfile_ConstrainedBase.0)
                    .map_err(at("an output type"))?;
                if transform.SetOutputType(streams.1, &output, 0).is_err() {
                    let output = output_type(size, fps, bitrate, eAVEncH264VProfile_Base.0)
                        .map_err(at("an output type"))?;
                    transform
                        .SetOutputType(streams.1, &output, 0)
                        .map_err(at("refused baseline H.264"))?;
                }
                let input = input_type(&transform, streams.0, size, fps)
                    .map_err(at("an NV12 input type"))?;
                transform
                    .SetInputType(streams.0, &input, 0)
                    .map_err(at("refused NV12"))?;
                let _ = set(
                    &codec,
                    &CODECAPI_AVEncCommonMeanBitRate,
                    variant_u32(bitrate),
                );
                let info = transform
                    .GetOutputStreamInfo(streams.1)
                    .map_err(at("its output"))?;
                let events = if asynchronous {
                    Some(transform.cast().map_err(at("no events"))?)
                } else {
                    None
                };
                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .map_err(at("starting"))?;
                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    .map_err(at("starting"))?;
                Ok((
                    transform,
                    codec,
                    events,
                    streams,
                    allocate(info.dwFlags, info.cbSize, size),
                ))
            })()
        };
        let (transform, codec, events, streams, allocate) = match made {
            Ok(made) => made,
            Err(why) => {
                // SAFETY: as in Drop: what was made goes.
                let _ = unsafe { activate.ShutdownObject() };
                return Err(why);
            }
        };
        Ok(Self {
            transform,
            codec,
            events,
            activate,
            name,
            streams,
            size,
            fps,
            bitrate,
            allocate,
            wanted: 0,
            ready: 0,
            frames: 0,
            since_idr: 0,
            owed_keyframe: false,
            fixed_rate: false,
            sets: Vec::new(),
        })
    }

    /// Encodes `picture` (at the session's size), an IDR if
    /// `force_keyframe` or one is due.
    fn encode(&mut self, picture: &Planes, force_keyframe: bool) -> Result<Encoded, Failure> {
        let force = force_keyframe
            || self.owed_keyframe
            || self.frames == 0
            || self.since_idr >= super::idr_interval(self.fps);
        if force && self.frames > 0 {
            set(
                &self.codec,
                &CODECAPI_AVEncVideoForceKeyFrame,
                variant_u32(1),
            )
            .map_err(|e| Failure::unsupported(format!("no keyframes on request: {e}")))?;
        }
        let len = super::nv12_len(self.size.0 as usize, self.size.1)
            .ok_or_else(|| Failure::broken("a picture too large"))?;
        let second = 10_000_000_i64;
        let duration = second / i64::from(self.fps);
        let time = i64::try_from(self.frames).unwrap_or(i64::MAX / 2) * duration;
        let sample =
            mf::memory_sample(len, time, duration, |out| super::i420_to_nv12(picture, out))?;
        let data = if self.events.is_some() {
            self.run_async(&sample)?
        } else {
            self.run_sync(&sample)?
        };
        self.frames += 1;
        if self.sets.is_empty() {
            self.sets = self.sequence_header();
        }
        let encoded = super::finish_frame(&data, &mut self.sets)?;
        if encoded.keyframe {
            self.since_idr = 1;
            self.owed_keyframe = false;
        } else {
            self.since_idr = self.since_idr.saturating_add(1);
            // Some encoders apply a forced keyframe a picture late.
            self.owed_keyframe = force;
        }
        Ok(encoded)
    }

    /// `sample` through an asynchronous encoder: wait until it wants a
    /// picture, give it, wait until a frame is ready, take it.
    fn run_async(&mut self, sample: &IMFSample) -> Result<Vec<u8>, Failure> {
        let deadline = Instant::now() + ENCODE_WAIT;
        while self.wanted == 0 {
            self.next_event(deadline)?;
        }
        self.wanted -= 1;
        // SAFETY: a live sample, given to the encoder that asked.
        unsafe { self.transform.ProcessInput(self.streams.0, sample, 0) }
            .map_err(|e| failure("encoding", &e))?;
        loop {
            if self.ready > 0 {
                self.ready -= 1;
                if let Some(data) = self.take_output()? {
                    return Ok(data);
                }
                continue;
            }
            self.next_event(deadline)?;
        }
    }

    /// `sample` through a synchronous encoder: in, and its frame out.
    fn run_sync(&mut self, sample: &IMFSample) -> Result<Vec<u8>, Failure> {
        // SAFETY: a live sample.
        unsafe { self.transform.ProcessInput(self.streams.0, sample, 0) }
            .map_err(|e| failure("encoding", &e))?;
        for _ in 0..4 {
            if let Some(data) = self.take_output()? {
                return Ok(data);
            }
        }
        Err(Failure::device("the encoder held the picture back"))
    }

    /// One frame out, if the encoder has one: none if it wants more (or
    /// its output's format changed, which is set again).
    fn take_output(&mut self) -> Result<Option<Vec<u8>>, Failure> {
        match mf::process_output(&self.transform, self.streams.1, self.allocate) {
            Ok(Output::Sample(sample)) => mf::sample_bytes(&sample)
                .map(Some)
                .map_err(|e| failure("reading a frame", &e)),
            Ok(Output::NeedMoreInput) => Ok(None),
            Ok(Output::StreamChange) => {
                self.renegotiate()?;
                Ok(None)
            }
            Err(e) => Err(failure("encoding", &e)),
        }
    }

    /// Sets the output type again after the encoder changed it.
    fn renegotiate(&mut self) -> Result<(), Failure> {
        // SAFETY: plain calls on the live transform.
        unsafe {
            let offered = self
                .transform
                .GetOutputAvailableType(self.streams.1, 0)
                .map_err(|e| failure("the encoder's new output", &e))?;
            self.transform
                .SetOutputType(self.streams.1, &offered, 0)
                .map_err(|e| failure("the encoder's new output", &e))?;
            let info = self
                .transform
                .GetOutputStreamInfo(self.streams.1)
                .map_err(|e| failure("the encoder's new output", &e))?;
            self.allocate = allocate(info.dwFlags, info.cbSize, self.size);
        }
        Ok(())
    }

    /// Waits for the asynchronous encoder's next event and counts it;
    /// fails on an error event or at `deadline`.
    fn next_event(&mut self, deadline: Instant) -> Result<(), Failure> {
        let Some(events) = &self.events else {
            return Err(Failure::device("not an asynchronous encoder"));
        };
        loop {
            // SAFETY: a plain call; the event is released when dropped.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    // SAFETY: plain calls on the live event.
                    let (kind, status) = unsafe { (event.GetType(), event.GetStatus()) };
                    if let Ok(status) = status
                        && status.is_err()
                    {
                        return Err(failure("the encoder failed", &status.into()));
                    }
                    match kind {
                        Ok(kind) if kind == METransformNeedInput.0 as u32 => self.wanted += 1,
                        Ok(kind) if kind == METransformHaveOutput.0 as u32 => self.ready += 1,
                        _ => {}
                    }
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Err(Failure::device(format!(
                            "{} took over {} ms for a picture",
                            self.name,
                            ENCODE_WAIT.as_millis()
                        )));
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => return Err(failure("the encoder's events", &e)),
            }
        }
    }

    /// The SPS and PPS the encoder's output type holds, if it has them.
    fn sequence_header(&self) -> Vec<u8> {
        // SAFETY: the blob is Media Foundation's allocation, read as long
        // as it says and freed here.
        unsafe {
            let Ok(current) = self.transform.GetOutputCurrentType(self.streams.1) else {
                return Vec::new();
            };
            let mut data = std::ptr::null_mut();
            let mut len = 0;
            if current
                .GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut data, &mut len)
                .is_err()
                || data.is_null()
            {
                return Vec::new();
            }
            let sets = super::parameter_sets(std::slice::from_raw_parts(data, len as usize));
            CoTaskMemFree(Some(data.cast_const().cast()));
            sets
        }
    }

    /// A new target bit rate, from the next picture on; kept as it was
    /// if the encoder will not change it while it runs.
    fn set_bitrate(&mut self, bitrate: u32) {
        let bitrate = bitrate.clamp(super::BITRATE_RANGE.0, super::BITRATE_RANGE.1);
        if bitrate == self.bitrate || self.fixed_rate {
            return;
        }
        match set(
            &self.codec,
            &CODECAPI_AVEncCommonMeanBitRate,
            variant_u32(bitrate),
        ) {
            Ok(()) => self.bitrate = bitrate,
            Err(e) => {
                eprintln!(
                    "noslacking-video: {}: the bit rate stays at {} ({e})",
                    self.name, self.bitrate
                );
                self.fixed_rate = true;
            }
        }
    }
}

/// The hardware H.264 encoders that take NV12, best first.
fn hardware_encoders() -> Result<Vec<IMFActivate>, String> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the type infos, `list` and `count` outlive the call; the
    // array it allocates holds `count` activation objects, each taken
    // (moved out) once, and is then freed.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&raw const input),
            Some(&raw const output),
            &mut list,
            &mut count,
        )
        .map_err(|e| format!("listing encoders: {e}"))?;
        let mut found = Vec::new();
        if !list.is_null() {
            for i in 0..count as usize {
                found.extend(list.add(i).read());
            }
            CoTaskMemFree(Some(list.cast_const().cast()));
        }
        Ok(found)
    }
}

/// The encoder's name as its maker registered it.
fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut name = PWSTR::null();
    let mut len = 0;
    // SAFETY: the string is Media Foundation's allocation, copied and
    // freed here.
    unsafe {
        activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
            .ok()?;
        let text = name.to_string().ok();
        CoTaskMemFree(Some(name.0.cast_const().cast()));
        text
    }
}

/// The bytes of an output sample to make for an encoder whose output
/// stream says `flags` and `size`: none if it makes its own.
fn allocate(flags: u32, size: u32, picture: (u32, u32)) -> Option<u32> {
    let own = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32;
    // An encoder that does not say how much: a raw picture's worth is
    // more than any frame it makes of it.
    (flags & own == 0).then(|| {
        if size > 0 {
            size
        } else {
            picture.0.saturating_mul(picture.1).saturating_mul(3) / 2
        }
    })
}

/// Sets one of the encoder's codec settings.
fn set(
    codec: &ICodecAPI,
    api: &GUID,
    value: windows::Win32::System::Variant::VARIANT,
) -> windows::core::Result<()> {
    // SAFETY: both point at values that outlive the call.
    unsafe { codec.SetValue(api, &value) }
}

/// CBR at `bitrate`, no B-frames, a GOP of the IDR interval at `fps`,
/// and low latency; only CBR is required.
fn set_up(codec: &ICodecAPI, bitrate: u32, fps: u32) -> windows::core::Result<()> {
    set(
        codec,
        &CODECAPI_AVEncCommonRateControlMode,
        variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
    )?;
    let _ = set(
        codec,
        &CODECAPI_AVEncCommonMeanBitRate,
        variant_u32(bitrate),
    );
    let _ = set(
        codec,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        variant_u32(0),
    );
    let _ = set(
        codec,
        &CODECAPI_AVEncMPVGOPSize,
        variant_u32(super::idr_interval(fps)),
    );
    let _ = set(codec, &CODECAPI_AVLowLatencyMode, variant_bool(true));
    Ok(())
}

/// The H.264 output type: `size` at `fps`, `bitrate`, progressive,
/// square pixels, of `profile` (`eAVEncH264VProfile`).
fn output_type(
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
    profile: i32,
) -> windows::core::Result<IMFMediaType> {
    let media = mf::media_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
    // SAFETY: plain calls on the live media type.
    unsafe {
        media.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        media.SetUINT64(&MF_MT_FRAME_SIZE, super::pack(size.0, size.1))?;
        media.SetUINT64(&MF_MT_FRAME_RATE, super::pack(fps, 1))?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, super::pack(1, 1))?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)?;
    }
    Ok(media)
}

/// The NV12 input type the encoder offers, with `size` and `fps` set on
/// it; one made here if it offers none.
fn input_type(
    transform: &IMFTransform,
    stream: u32,
    size: (u32, u32),
    fps: u32,
) -> windows::core::Result<IMFMediaType> {
    // SAFETY: plain calls on the live transform and media types.
    unsafe {
        let mut chosen = None;
        for index in 0.. {
            let Ok(offered) = transform.GetInputAvailableType(stream, index) else {
                break;
            };
            if offered.GetGUID(&MF_MT_SUBTYPE) == Ok(MFVideoFormat_NV12) {
                chosen = Some(offered);
                break;
            }
        }
        let media = match chosen {
            Some(media) => media,
            None => mf::media_type(&MFMediaType_Video, &MFVideoFormat_NV12)?,
        };
        media.SetUINT64(&MF_MT_FRAME_SIZE, super::pack(size.0, size.1))?;
        media.SetUINT64(&MF_MT_FRAME_RATE, super::pack(fps, 1))?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(media)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pattern::pattern;

    /// Mean absolute difference of two planes.
    fn difference(a: &[u8], b: &[u8]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| f64::from(x.abs_diff(*y)))
            .sum::<f64>()
            / a.len() as f64
    }

    /// The test screen encoded on this machine's GPU and decoded back by
    /// rusty_h264, close to what went in, with keyframes when asked.
    /// Needs a hardware H.264 encoder.
    /// `cargo test -p noslacking-video -- --ignored mediafoundation --nocapture`
    #[test]
    #[ignore = "needs a GPU with an H.264 encoder"]
    #[allow(clippy::print_stdout, reason = "the timings are for the reader")]
    fn mediafoundation_encodes_what_rusty_h264_reads_back() {
        let mut gpu = MfGpu::open().expect("Media Foundation");
        let size = (1280, 720);
        gpu.open(size, 30, 2_500_000).expect("a hardware encoder");
        println!("{}", gpu.name());
        let mut decoder = rusty_h264_decoder::Decoder::new();
        for n in 0..10u64 {
            let picture = pattern(size.0, size.1, n, Duration::from_millis(n * 33));
            let started = Instant::now();
            gpu.load(&Frame::I420(&picture)).expect("loaded");
            let encoded = gpu.encode(n == 5).expect("encoded");
            let took = started.elapsed();
            assert_eq!(encoded.keyframe, n == 0 || n == 5, "frame {n}");
            if n == 3 {
                gpu.set_bitrate(1_000_000).expect("a new rate");
            }
            let back = decoder
                .decode(&encoded.data)
                .expect("decodes")
                .expect("a picture");
            assert_eq!((back.width, back.height), (1280, 720));
            let luma = difference(&back.y, &picture.y);
            println!(
                "frame {n}: {} bytes in {:.2} ms, luma off by {luma:.2}",
                encoded.data.len(),
                took.as_secs_f64() * 1000.0
            );
            assert!(luma < 6.0, "frame {n}: {luma}");
        }
    }
}
