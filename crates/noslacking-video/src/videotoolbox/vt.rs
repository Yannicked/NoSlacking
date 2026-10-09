//! The part of VideoToolbox, CoreMedia and CoreVideo the back end uses,
//! behind safe types: a decompression session that turns one AVCC frame
//! into one pixel buffer, a compression session that turns one pixel
//! buffer into one AVCC frame, a pixel transfer session that converts
//! and scales one buffer into another, and pixel buffers read and
//! written through [`super::planes`]. The calls are objc2's bindings;
//! everything `unsafe` in the back end is here.
//!
//! Both sessions are used synchronously: a frame goes in and its output
//! is waited for before the call returns, as the [`crate::backend`]
//! traits want. Their callbacks leave the output in a slot the session
//! owns, boxed so its address stays put, and the session is invalidated
//! (no more callbacks) before the slot goes.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Mutex, PoisonError};

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, Type,
};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMBlockBufferAssureMemoryNowFlag,
    kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVAttachmentMode, CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate,
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_32RGBA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTDecodeFrameFlags, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSession, VTEncodeInfoFlags,
    VTPixelTransferSession, VTSessionCopyProperty, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxFrameDelayCount, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTDecompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTPixelTransferPropertyKey_RealTime, kVTProfileLevel_H264_Baseline_AutoLevel,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use super::planes::{Nv12, Nv12Mut};

/// An `OSStatus` or `CVReturn` other than success, from any of the
/// frameworks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub i32);

impl Status {
    /// Not the system's: a call succeeded but gave nothing back.
    pub const MISSING: Self = Self(i32::MIN);
    /// `kVTInvalidSessionErr`: the session is gone (the GPU was reset,
    /// the machine slept); a new one may work.
    pub const INVALID_SESSION: Self = Self(-12903);
    /// `kVTVideoDecoderBadDataErr`: the frame is broken.
    pub const BAD_DATA: Self = Self(-12909);
    /// `kVTVideoDecoderUnsupportedDataFormatErr`: the stream is not one
    /// the decoder takes.
    pub const UNSUPPORTED_FORMAT: Self = Self(-12910);
    /// `kVTCouldNotFindVideoDecoderErr`: no decoder for the stream (with
    /// hardware required, none in hardware).
    pub const NO_DECODER: Self = Self(-12906);
    /// `kVTCouldNotFindVideoEncoderErr`: no encoder (in hardware).
    pub const NO_ENCODER: Self = Self(-12908);
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match *self {
            Self::MISSING => return f.write_str("no output"),
            Self::INVALID_SESSION => "invalid session",
            Self::BAD_DATA => "bad data",
            Self::UNSUPPORTED_FORMAT => "unsupported format",
            Self::NO_DECODER => "no decoder",
            Self::NO_ENCODER => "no encoder",
            Self(-12911) => "decoder malfunction",
            Self(-12912) => "encoder malfunction",
            Self(-12913) => "decoder not available now",
            Self(-12915) => "encoder not available now",
            Self(-12902) => "a parameter",
            Self(-12900) => "a property not supported",
            _ => "",
        };
        if name.is_empty() {
            write!(f, "OSStatus {}", self.0)
        } else {
            write!(f, "{name} (OSStatus {})", self.0)
        }
    }
}

/// `status` as a result.
fn check(status: i32) -> Result<(), Status> {
    if status == 0 {
        Ok(())
    } else {
        Err(Status(status))
    }
}

/// Locks a slot, a poisoned one too: what it holds is plain data.
fn lock<T>(slot: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A dictionary of options.
fn dictionary(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<&CFString> = pairs.iter().map(|(key, _)| *key).collect();
    let values: Vec<&CFType> = pairs.iter().map(|(_, value)| *value).collect();
    CFDictionary::from_slices(&keys, &values)
}

/// A number as an option's value.
fn number(value: i32) -> CFRetained<CFNumber> {
    CFNumber::new_i32(value)
}

/// A four-character pixel format as a number, as CoreVideo takes it.
fn format_number(format: u32) -> CFRetained<CFNumber> {
    number(i32::from_ne_bytes(format.to_ne_bytes()))
}

/// Sets one property of a session.
fn set_property(session: &CFType, key: &CFString, value: &CFType) -> Result<(), Status> {
    // SAFETY: `session` is a live VideoToolbox session and `key` and
    // `value` live CF objects; the session retains what it keeps.
    check(unsafe { VTSessionSetProperty(session, key, Some(value)) })
}

/// A session's boolean property; false if it has none.
fn bool_property(session: &CFType, key: &CFString) -> bool {
    let mut value: *const CFType = ptr::null();
    // SAFETY: the out pointer is a CFTypeRef the call fills with a
    // retained object (or leaves null), which is taken over below.
    let status = unsafe {
        VTSessionCopyProperty(
            session,
            key,
            None,
            ptr::from_mut(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 {
        return false;
    }
    let Some(value) = NonNull::new(value.cast_mut()) else {
        return false;
    };
    // SAFETY: a Copy call's result is ours to release.
    let value = unsafe { CFRetained::from_raw(value) };
    value
        .downcast_ref::<CFBoolean>()
        .is_some_and(CFBoolean::as_bool)
}

/// A format description for an H.264 stream with these parameter sets
/// (each without its start code), its frames' NAL units behind
/// four-byte lengths.
fn format_description(sps: &[u8], pps: &[u8]) -> Result<CFRetained<CMFormatDescription>, Status> {
    if sps.is_empty() || pps.is_empty() {
        return Err(Status::UNSUPPORTED_FORMAT);
    }
    let pointers = [
        NonNull::from(sps).cast::<u8>(),
        NonNull::from(pps).cast::<u8>(),
    ];
    let sizes = [sps.len(), pps.len()];
    let mut out: *const CMFormatDescription = ptr::null();
    // SAFETY: two pointers and two sizes, each pair a live slice, read
    // during the call only (the description copies them); `out` gets a
    // retained description.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            NonNull::from(&pointers).cast(),
            NonNull::from(&sizes).cast(),
            4,
            NonNull::from(&mut out),
        )
    };
    check(status)?;
    let out = NonNull::new(out.cast_mut()).ok_or(Status::MISSING)?;
    // SAFETY: a Create call's result is ours to release.
    Ok(unsafe { CFRetained::from_raw(out) })
}

/// The SPS and PPS of a format description, and how many bytes its
/// NAL units' lengths take.
fn parameter_sets(format: &CMFormatDescription) -> Result<(Vec<Vec<u8>>, usize), Status> {
    let mut sets = Vec::new();
    let mut count = 0usize;
    let mut length_size: i32 = 4;
    let mut index = 0;
    loop {
        let mut pointer: *const u8 = ptr::null();
        let mut size = 0usize;
        // SAFETY: the out pointers are live locals; the set's bytes stay
        // the description's, copied out at once.
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                &mut pointer,
                &mut size,
                &mut count,
                &mut length_size,
            )
        };
        check(status)?;
        if !pointer.is_null() && size > 0 {
            // SAFETY: the description holds `size` bytes at `pointer`
            // while it lives, and it outlives this copy.
            sets.push(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec());
        }
        index += 1;
        if index >= count {
            break;
        }
    }
    let length_size = usize::try_from(length_size).map_err(|_| Status::MISSING)?;
    Ok((sets, length_size))
}

/// A pixel buffer: a decoded picture, or one to encode.
pub struct PixelBuffer(CFRetained<CVPixelBuffer>);

impl std::fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (width, height) = self.size();
        write!(f, "PixelBuffer({width}x{height} {:#x})", self.format())
    }
}

/// A pixel buffer's base address locked, unlocked when this goes (a
/// panic in between too).
struct Locked<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
}

impl<'a> Locked<'a> {
    fn new(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags) -> Result<Self, Status> {
        // SAFETY: a live pixel buffer; unlocked with the same flags in
        // `drop`.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        if status != kCVReturnSuccess {
            return Err(Status(status));
        }
        Ok(Self { buffer, flags })
    }

    /// Plane `plane` (of a planar buffer) as bytes: its base address
    /// and every row it has.
    fn plane(&self, plane: usize) -> Option<(*mut u8, usize, usize)> {
        let base = CVPixelBufferGetBaseAddressOfPlane(self.buffer, plane).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(self.buffer, plane);
        let rows = CVPixelBufferGetHeightOfPlane(self.buffer, plane);
        (!base.is_null()).then_some((base, stride, stride.checked_mul(rows)?))
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: locked in `new` with these flags.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

/// Whether `format` is NV12, of either range.
pub fn is_nv12(format: u32) -> bool {
    format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        || format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
}

impl PixelBuffer {
    /// Its width and height in pixels.
    pub fn size(&self) -> (usize, usize) {
        (
            CVPixelBufferGetWidth(&self.0),
            CVPixelBufferGetHeight(&self.0),
        )
    }

    /// Its four-character pixel format.
    pub fn format(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(&self.0)
    }

    /// Reads its top-left `width`×`height` as NV12 with `read`; none if
    /// it is not NV12 or not that large.
    pub fn read_nv12<R>(
        &self,
        width: u32,
        height: u32,
        read: impl FnOnce(&Nv12<'_>) -> Option<R>,
    ) -> Result<Option<R>, Status> {
        if !is_nv12(self.format()) || CVPixelBufferGetPlaneCount(&self.0) != 2 {
            return Ok(None);
        }
        let locked = Locked::new(&self.0, CVPixelBufferLockFlags::ReadOnly)?;
        let (Some((y, y_stride, y_len)), Some((uv, uv_stride, uv_len))) =
            (locked.plane(0), locked.plane(1))
        else {
            return Err(Status::MISSING);
        };
        // SAFETY: while the buffer is locked each plane is `stride` ×
        // its rows bytes at its base address, and nothing writes them
        // (locked read-only, and no one else holds this buffer
        // writable); the slices go with `locked`, at the end of this
        // call.
        let (y, uv) = unsafe {
            (
                std::slice::from_raw_parts(y.cast_const(), y_len),
                std::slice::from_raw_parts(uv.cast_const(), uv_len),
            )
        };
        Ok(read(&Nv12 {
            width,
            height,
            y,
            y_stride,
            uv,
            uv_stride,
        }))
    }

    /// Writes it as NV12 with `write`; false if it is not NV12 or
    /// `write` wrote nothing.
    pub fn write_nv12(
        &mut self,
        write: impl FnOnce(&mut Nv12Mut<'_>) -> bool,
    ) -> Result<bool, Status> {
        if !is_nv12(self.format()) || CVPixelBufferGetPlaneCount(&self.0) != 2 {
            return Ok(false);
        }
        let (width, height) = self.size();
        let locked = Locked::new(&self.0, CVPixelBufferLockFlags::empty())?;
        let (Some((y, y_stride, y_len)), Some((uv, uv_stride, uv_len))) =
            (locked.plane(0), locked.plane(1))
        else {
            return Err(Status::MISSING);
        };
        // SAFETY: as in `read_nv12`, locked for writing: this buffer is
        // ours alone (fresh from the pool, not yet encoded), and the two
        // planes do not overlap.
        let (y, uv) = unsafe {
            (
                std::slice::from_raw_parts_mut(y, y_len),
                std::slice::from_raw_parts_mut(uv, uv_len),
            )
        };
        Ok(write(&mut Nv12Mut {
            width: u32::try_from(width).unwrap_or(0),
            height: u32::try_from(height).unwrap_or(0),
            y,
            y_stride,
            uv,
            uv_stride,
        }))
    }

    /// Writes it as one packed plane with `write` (its bytes and their
    /// stride); false if `write` wrote nothing.
    pub fn write_packed(
        &mut self,
        write: impl FnOnce(&mut [u8], usize) -> bool,
    ) -> Result<bool, Status> {
        let locked = Locked::new(&self.0, CVPixelBufferLockFlags::empty())?;
        let base = CVPixelBufferGetBaseAddress(&self.0).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRow(&self.0);
        let length = stride
            .checked_mul(CVPixelBufferGetHeight(&self.0))
            .ok_or(Status::MISSING)?;
        if base.is_null() {
            return Err(Status::MISSING);
        }
        // SAFETY: locked for writing, a packed buffer is `stride` × its
        // height bytes at its base address, and it is ours alone (made
        // here, not in a transfer while this runs).
        let bytes = unsafe { std::slice::from_raw_parts_mut(base, length) };
        let wrote = write(bytes, stride);
        drop(locked);
        Ok(wrote)
    }

    /// Marks it as BT.601, studio range: what a conversion into it from
    /// RGB should make, as the app's software path and WebRTC senders
    /// make it, and what the stream's colour information will say.
    pub fn mark_bt601(&self) {
        // SAFETY: a live pixel buffer and two constant CF strings.
        unsafe {
            self.0.set_attachment(
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                CVAttachmentMode::ShouldPropagate,
            );
        }
    }

    /// A packed pixel buffer of `width`×`height`, BGRA or RGBA, in
    /// IOSurface memory the GPU reads.
    pub fn packed(width: usize, height: usize, bgra: bool) -> Result<Self, Status> {
        let format = if bgra {
            kCVPixelFormatType_32BGRA
        } else {
            kCVPixelFormatType_32RGBA
        };
        let surface = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: a constant CF string.
        let surface_key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
        let attributes = dictionary(&[(surface_key, surface.as_ref())]);
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: `out` gets a retained buffer; the attributes are read
        // during the call.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                format,
                Some(attributes.as_ref()),
                NonNull::from(&mut out),
            )
        };
        if status != kCVReturnSuccess {
            return Err(Status(status));
        }
        let out = NonNull::new(out).ok_or(Status::MISSING)?;
        // SAFETY: a Create call's result is ours to release.
        Ok(Self(unsafe { CFRetained::from_raw(out) }))
    }
}

/// What the decoder's callback leaves: the last picture, or why not.
type DecodedSlot = Mutex<Option<Result<CFRetained<CVPixelBuffer>, Status>>>;

/// Where VideoToolbox hands each decoded picture.
///
/// # Safety
///
/// Called by VideoToolbox with the `decompressionOutputRefCon` given at
/// creation (a live [`DecodedSlot`]) and, on success, a live image
/// buffer it lends for the call.
unsafe extern "C-unwind" fn decoded(
    slot: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _shown_at: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the slot is the session's, which is invalidated (no more
    // callbacks) before the slot is dropped.
    let slot = unsafe { &*slot.cast::<DecodedSlot>() };
    let result = match (status, NonNull::new(image)) {
        (0, Some(image)) => {
            // SAFETY: a live buffer, retained to keep past the call.
            Ok(unsafe { CFRetained::retain(image) })
        }
        // Decoded, but dropped: nothing to show.
        (0, None) => return,
        (status, _) => Err(Status(status)),
    };
    *lock(slot) = Some(result);
}

/// A decompression session for one H.264 stream's parameter sets, giving
/// NV12.
pub struct Decompression {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    /// The callback's slot; boxed, so its address stays put.
    slot: Box<DecodedSlot>,
}

impl std::fmt::Debug for Decompression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decompression").finish_non_exhaustive()
    }
}

impl Decompression {
    /// A session for the stream with this SPS and PPS (without start
    /// codes), on the GPU (only, if `require_hardware`).
    pub fn new(sps: &[u8], pps: &[u8], require_hardware: bool) -> Result<Self, Status> {
        let format = format_description(sps, pps)?;
        // SAFETY: constant CF strings.
        let (enable, require, pixel_format) = unsafe {
            (
                kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
                kCVPixelBufferPixelFormatTypeKey,
            )
        };
        let specification = dictionary(&[
            (enable, CFBoolean::new(true).as_ref()),
            (require, CFBoolean::new(require_hardware).as_ref()),
        ]);
        let nv12 = format_number(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        let attributes = dictionary(&[(pixel_format, nv12.as_ref())]);
        let slot: Box<DecodedSlot> = Box::new(Mutex::new(None));
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decoded),
            decompressionOutputRefCon: ptr::from_ref::<DecodedSlot>(&slot).cast_mut().cast(),
        };
        let mut session: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: the description and dictionaries are live; the record
        // is copied by the call, and its refcon (the boxed slot) lives
        // as long as the session, which is invalidated first.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                Some(specification.as_ref()),
                Some(attributes.as_ref()),
                &record,
                NonNull::from(&mut session),
            )
        };
        check(status)?;
        let session = NonNull::new(session).ok_or(Status::MISSING)?;
        // SAFETY: a Create call's result is ours to release.
        let session = unsafe { CFRetained::from_raw(session) };
        // SAFETY: a constant CF string.
        let real_time = unsafe { kVTDecompressionPropertyKey_RealTime };
        // A hint only (decode as fast as frames come); some decoders
        // have no such property.
        let _ = set_property(&session, real_time, CFBoolean::new(true));
        Ok(Self {
            session,
            format,
            slot,
        })
    }

    /// Decodes one frame (its NAL units behind four-byte lengths): its
    /// picture, or none if the decoder dropped it.
    pub fn decode(&mut self, avcc: &[u8]) -> Result<Option<PixelBuffer>, Status> {
        if avcc.is_empty() {
            return Ok(None);
        }
        let sample = sample_buffer(avcc, &self.format)?;
        *lock(&self.slot) = None;
        let mut info = VTDecodeInfoFlags(0);
        // SAFETY: a live session and sample; synchronous (no
        // asynchronous flag), so the callback has run when this returns,
        // and the wait below makes sure of it.
        let status = unsafe {
            self.session.decode_frame(
                &sample,
                VTDecodeFrameFlags::empty(),
                ptr::null_mut(),
                &mut info,
            )
        };
        // SAFETY: a live session.
        unsafe { self.session.wait_for_asynchronous_frames() };
        check(status)?;
        lock(&self.slot)
            .take()
            .transpose()
            .map(|buffer| buffer.map(PixelBuffer))
    }
}

impl Drop for Decompression {
    fn drop(&mut self) {
        // SAFETY: a live session; no callback comes after this, so the
        // slot can go.
        unsafe { self.session.invalidate() };
    }
}

/// One frame's bytes (AVCC) as a sample buffer of `format`, the bytes
/// copied into memory of its own.
fn sample_buffer(
    avcc: &[u8],
    format: &CMFormatDescription,
) -> Result<CFRetained<CMSampleBuffer>, Status> {
    let length = avcc.len();
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: no memory given (null), so the block allocates `length`
    // bytes of its own now (kCMBlockBufferAssureMemoryNowFlag); `block`
    // gets a retained buffer.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            length,
            None,
            ptr::null(),
            0,
            length,
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    check(status)?;
    let block = NonNull::new(block).ok_or(Status::MISSING)?;
    // SAFETY: a Create call's result is ours to release.
    let block = unsafe { CFRetained::from_raw(block) };
    // SAFETY: `avcc` is `length` live bytes, copied into the block's
    // own `length` bytes.
    let status =
        unsafe { CMBlockBuffer::replace_data_bytes(NonNull::from(avcc).cast(), &block, 0, length) };
    check(status)?;
    let mut sample: *mut CMSampleBuffer = ptr::null_mut();
    // SAFETY: one sample of `length` bytes, no timing; the size array is
    // read during the call; `sample` gets a retained buffer.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            0,
            ptr::null(),
            1,
            &length,
            NonNull::from(&mut sample),
        )
    };
    check(status)?;
    let sample = NonNull::new(sample).ok_or(Status::MISSING)?;
    // SAFETY: a Create call's result is ours to release.
    Ok(unsafe { CFRetained::from_raw(sample) })
}

/// One encoded frame as VideoToolbox gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Its NAL units, each behind its length.
    pub avcc: Vec<u8>,
    /// How many bytes each length takes.
    pub length_size: usize,
    /// The stream's SPS and PPS, without start codes.
    pub parameter_sets: Vec<Vec<u8>>,
}

/// What the encoder's callback leaves: the last frame, or why not.
type EncodedSlot = Mutex<Option<Result<Sample, Status>>>;

/// An encoded sample's bytes and parameter sets, copied out.
fn read_sample(sample: &CMSampleBuffer) -> Result<Sample, Status> {
    // SAFETY: a live sample buffer, lent for the callback.
    let (data, format) = unsafe { (sample.data_buffer(), sample.format_description()) };
    let (data, format) = (data.ok_or(Status::MISSING)?, format.ok_or(Status::MISSING)?);
    // SAFETY: a live block buffer.
    let length = unsafe { data.data_length() };
    let mut avcc = vec![0u8; length];
    if length > 0 {
        // SAFETY: `avcc` has room for the `length` bytes copied.
        let status =
            unsafe { data.copy_data_bytes(0, length, NonNull::from(avcc.as_mut_slice()).cast()) };
        check(status)?;
    }
    let (parameter_sets, length_size) = parameter_sets(&format)?;
    Ok(Sample {
        avcc,
        length_size,
        parameter_sets,
    })
}

/// Where VideoToolbox hands each encoded frame.
///
/// # Safety
///
/// Called by VideoToolbox with the `outputCallbackRefCon` given at
/// creation (a live [`EncodedSlot`]) and, on success, a live sample
/// buffer it lends for the call.
unsafe extern "C-unwind" fn encoded(
    slot: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the slot is the session's, which is invalidated (no more
    // callbacks) before the slot is dropped.
    let slot = unsafe { &*slot.cast::<EncodedSlot>() };
    let result = match (status, NonNull::new(sample)) {
        // SAFETY: a live sample buffer, lent for this call.
        (0, Some(sample)) => read_sample(unsafe { sample.as_ref() }),
        // Dropped by the encoder: nothing to send.
        (0, None) => return,
        (status, _) => Err(Status(status)),
    };
    *lock(slot) = Some(result);
}

/// A compression session property, with its value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Property {
    /// Encode as frames come, not ahead of them.
    RealTime(bool),
    /// Whether B-frames (frames out of order) may be made.
    AllowFrameReordering(bool),
    /// Constrained baseline, any level (macOS 12 and later).
    ConstrainedBaseline,
    /// Baseline, any level.
    Baseline,
    /// The average bit rate, bit/s.
    AverageBitRate(i32),
    /// At most this many bytes in a second.
    BytesPerSecond(i32),
    /// Pictures a second, as a hint.
    ExpectedFrameRate(i32),
    /// A keyframe at least every this many pictures.
    MaxKeyFrameInterval(i32),
    /// A keyframe at least every this many seconds.
    MaxKeyFrameIntervalDuration(f64),
    /// How many pictures the encoder may hold before it hands one out.
    MaxFrameDelayCount(i32),
}

/// A compression session for one size, giving H.264 in AVCC.
pub struct Compression {
    session: CFRetained<VTCompressionSession>,
    pool: CFRetained<CVPixelBufferPool>,
    /// The callback's slot; boxed, so its address stays put.
    slot: Box<EncodedSlot>,
}

impl std::fmt::Debug for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compression").finish_non_exhaustive()
    }
}

impl Compression {
    /// A session encoding `width`×`height` NV12 pictures as H.264, on
    /// the GPU (only, if `require_hardware`).
    pub fn new(width: u32, height: u32, require_hardware: bool) -> Result<Self, Status> {
        let side = |v: u32| i32::try_from(v).map_err(|_| Status::UNSUPPORTED_FORMAT);
        let (w, h) = (side(width)?, side(height)?);
        // SAFETY: constant CF strings.
        let (enable, require, pixel_format, width_key, height_key, surface_key) = unsafe {
            (
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            )
        };
        let specification = dictionary(&[
            (enable, CFBoolean::new(true).as_ref()),
            (require, CFBoolean::new(require_hardware).as_ref()),
        ]);
        let nv12 = format_number(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        let (wide, tall) = (number(w), number(h));
        let surface = CFDictionary::<CFString, CFType>::empty();
        let attributes = dictionary(&[
            (pixel_format, nv12.as_ref()),
            (width_key, wide.as_ref()),
            (height_key, tall.as_ref()),
            (surface_key, surface.as_ref()),
        ]);
        let slot: Box<EncodedSlot> = Box::new(Mutex::new(None));
        let mut session: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: the dictionaries are live during the call; the refcon
        // (the boxed slot) lives as long as the session, which is
        // invalidated first; `session` gets a retained session.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                w,
                h,
                kCMVideoCodecType_H264,
                Some(specification.as_ref()),
                Some(attributes.as_ref()),
                None,
                Some(encoded),
                ptr::from_ref::<EncodedSlot>(&slot).cast_mut().cast(),
                NonNull::from(&mut session),
            )
        };
        check(status)?;
        let session = NonNull::new(session).ok_or(Status::MISSING)?;
        // SAFETY: a Create call's result is ours to release.
        let session = unsafe { CFRetained::from_raw(session) };
        // Invalidated by `Drop` from here on, even if the pool is not
        // there.
        // SAFETY: a live session; the pool it returns is retained.
        let pool = unsafe { session.pixel_buffer_pool() };
        let Some(pool) = pool else {
            // SAFETY: a live session, dropped next.
            unsafe { session.invalidate() };
            return Err(Status::MISSING);
        };
        Ok(Self {
            session,
            pool,
            slot,
        })
    }

    /// Sets one property.
    pub fn set(&self, property: Property) -> Result<(), Status> {
        // SAFETY: constant CF strings, read once each.
        let (key, value): (&CFString, CFRetained<CFType>) = unsafe {
            match property {
                Property::RealTime(on) => (
                    kVTCompressionPropertyKey_RealTime,
                    CFBoolean::new(on).retain().into(),
                ),
                Property::AllowFrameReordering(on) => (
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    CFBoolean::new(on).retain().into(),
                ),
                Property::ConstrainedBaseline => (
                    kVTCompressionPropertyKey_ProfileLevel,
                    // kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel's
                    // value, spelled out: the symbol is macOS 12's, and
                    // linking it would keep the helper from starting on
                    // older systems, where setting this fails instead.
                    CFString::from_static_str("H264_ConstrainedBaseline_AutoLevel").into(),
                ),
                Property::Baseline => (
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_Baseline_AutoLevel.retain().into(),
                ),
                Property::AverageBitRate(rate) => (
                    kVTCompressionPropertyKey_AverageBitRate,
                    number(rate).into(),
                ),
                Property::BytesPerSecond(bytes) => {
                    let (bytes, second) = (number(bytes), CFNumber::new_f64(1.0));
                    let limits = CFArray::<CFNumber>::from_objects(&[&*bytes, &*second]);
                    (
                        kVTCompressionPropertyKey_DataRateLimits,
                        // An array is a CF object like any other (its
                        // element type is only this side's).
                        CFRetained::cast_unchecked::<CFType>(limits),
                    )
                }
                Property::ExpectedFrameRate(fps) => (
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    number(fps).into(),
                ),
                Property::MaxKeyFrameInterval(frames) => (
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    number(frames).into(),
                ),
                Property::MaxKeyFrameIntervalDuration(seconds) => (
                    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                    CFNumber::new_f64(seconds).into(),
                ),
                Property::MaxFrameDelayCount(frames) => (
                    kVTCompressionPropertyKey_MaxFrameDelayCount,
                    number(frames).into(),
                ),
            }
        };
        set_property(&self.session, key, &value)
    }

    /// Whether it encodes on the GPU.
    pub fn hardware(&self) -> bool {
        // SAFETY: a constant CF string.
        let key = unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder };
        bool_property(&self.session, key)
    }

    /// A pixel buffer from its pool, of its size and format, to write a
    /// picture into.
    pub fn buffer(&self) -> Result<PixelBuffer, Status> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: a live pool; `out` gets a retained buffer.
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out))
        };
        if status != kCVReturnSuccess {
            return Err(Status(status));
        }
        let out = NonNull::new(out).ok_or(Status::MISSING)?;
        // SAFETY: a Create call's result is ours to release.
        Ok(PixelBuffer(unsafe { CFRetained::from_raw(out) }))
    }

    /// Encodes `buffer`, shown at `value`/`timescale` seconds, an IDR if
    /// `force_keyframe`, and waits for it: the frame, or none if the
    /// encoder dropped it.
    pub fn encode(
        &mut self,
        buffer: &PixelBuffer,
        at: (i64, i32),
        force_keyframe: bool,
    ) -> Result<Option<Sample>, Status> {
        // SAFETY: a constant CF string.
        let force = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let options = force_keyframe.then(|| dictionary(&[(force, CFBoolean::new(true).as_ref())]));
        let shown_at = CMTime {
            value: at.0,
            timescale: at.1,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        *lock(&self.slot) = None;
        // SAFETY: a live session, buffer and options; the session
        // retains the buffer while it needs it. A constant time.
        let status = unsafe {
            self.session.encode_frame(
                &buffer.0,
                shown_at,
                kCMTimeInvalid,
                options.as_deref().map(AsRef::as_ref),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        check(status)?;
        // Every frame up to this one out now: the caller waits for it.
        // SAFETY: a live session.
        check(unsafe { self.session.complete_frames(shown_at) })?;
        lock(&self.slot).take().transpose()
    }
}

impl Drop for Compression {
    fn drop(&mut self) {
        // SAFETY: a live session; no callback comes after this, so the
        // slot can go.
        unsafe { self.session.invalidate() };
    }
}

/// A pixel transfer session: converts and scales one pixel buffer into
/// another on the GPU.
pub struct Transfer(CFRetained<VTPixelTransferSession>);

impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transfer")
    }
}

impl Transfer {
    /// A new session.
    pub fn new() -> Result<Self, Status> {
        let mut session: *mut VTPixelTransferSession = ptr::null_mut();
        // SAFETY: `session` gets a retained session.
        check(unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) })?;
        let session = NonNull::new(session).ok_or(Status::MISSING)?;
        // SAFETY: a Create call's result is ours to release.
        let session = unsafe { CFRetained::from_raw(session) };
        // SAFETY: a constant CF string.
        let real_time = unsafe { kVTPixelTransferPropertyKey_RealTime };
        // A hint only: older systems have no such property.
        let _ = set_property(&session, real_time, CFBoolean::new(true));
        Ok(Self(session))
    }

    /// `from` converted (and scaled, to fill it) into `to`.
    pub fn transfer(&self, from: &PixelBuffer, to: &PixelBuffer) -> Result<(), Status> {
        // SAFETY: a live session and two live buffers, `to` written only.
        check(unsafe { self.0.transfer_image(&from.0, &to.0) })
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        // SAFETY: a live session, released next.
        unsafe { self.0.invalidate() };
    }
}
