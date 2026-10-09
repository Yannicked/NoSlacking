//! What decoding and encoding share of Media Foundation and Direct3D
//! 11: starting both on a thread, the GPU's device, media types,
//! samples, and taking a transform's output.
//!
//! Every call here is into Windows' COM interfaces through the `windows`
//! crate, which marks them all `unsafe`; the pointers handed over are to
//! values that outlive the call, and what comes back is owned by the
//! crate's interface types, which release it when dropped.

use std::mem::ManuallyDrop;
use std::rc::Rc;

use windows::Win32::Foundation::{E_UNEXPECTED, HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_DECODER_PROFILE_H264_VLD_NOFGT, D3D11_SDK_VERSION,
    D3D11_VIDEO_DECODER_DESC, D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
    ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform,
    MF_E_HW_MFT_FAILED_START_STREAMING, MF_E_INVALIDMEDIATYPE, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_E_TRANSFORM_TYPE_NOT_SET,
    MF_E_UNSUPPORTED_D3D_TYPE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_VERSION,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFSTARTUP_NOSOCKET, MFShutdown, MFStartup, MFT_OUTPUT_DATA_BUFFER,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface};

use super::hresult;
use crate::backend::Failure;

// The codes the arithmetic tells apart are Windows' own.
const _: () = {
    assert!(hresult::NEED_MORE_INPUT == MF_E_TRANSFORM_NEED_MORE_INPUT.0);
    assert!(hresult::STREAM_CHANGE == MF_E_TRANSFORM_STREAM_CHANGE.0);
    assert!(hresult::NOT_ACCEPTING == MF_E_NOTACCEPTING.0);
    assert!(hresult::INVALID_MEDIA_TYPE == MF_E_INVALIDMEDIATYPE.0);
    assert!(hresult::TYPE_NOT_SET == MF_E_TRANSFORM_TYPE_NOT_SET.0);
    assert!(hresult::UNSUPPORTED_D3D_TYPE == MF_E_UNSUPPORTED_D3D_TYPE.0);
    assert!(hresult::HW_FAILED_START == MF_E_HW_MFT_FAILED_START_STREAMING.0);
};

/// A failure of `what`, its kind from the error's code.
pub fn failure(what: &str, error: &windows::core::Error) -> Failure {
    Failure::new(
        super::failure_kind(error.code().0),
        format!("{what}: {error}"),
    )
}

/// COM and Media Foundation started on this thread, until dropped: each
/// thread that makes or uses a transform holds one (the server's for
/// decoding, each capture's for its encoder).
#[derive(Debug)]
pub struct Platform {
    /// Whether COM was started here (and so is stopped here): a thread
    /// that has it already in another mode keeps it.
    com: bool,
}

impl Platform {
    /// Starts COM (multithreaded, unless the thread already has it) and
    /// Media Foundation.
    pub fn start() -> Result<Self, String> {
        // SAFETY: no reserved pointer; a success is undone in Drop.
        let started = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if started.is_err() && started != RPC_E_CHANGED_MODE {
            return Err(format!("COM did not start: {started:?}"));
        }
        let com = started.is_ok();
        // SAFETY: a plain call; undone in Drop.
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) } {
            if com {
                // SAFETY: balances the CoInitializeEx above.
                unsafe { CoUninitialize() };
            }
            return Err(format!("Media Foundation did not start: {error}"));
        }
        Ok(Self { com })
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        // SAFETY: balances MFStartup in `start`; everything made through
        // Media Foundation on this thread is gone, as it holds this.
        let _ = unsafe { MFShutdown() };
        if self.com {
            // SAFETY: balances CoInitializeEx in `start`.
            unsafe { CoUninitialize() };
        }
    }
}

/// The GPU as Direct3D 11 has it, with the device manager that hands it
/// to a transform.
pub struct Device {
    /// The device, made for video.
    pub device: ID3D11Device,
    /// What a transform is given to reach the device.
    pub manager: IMFDXGIDeviceManager,
    /// The GPU's name, for the log.
    pub adapter: String,
    // Last: everything above is released before Media Foundation stops.
    _platform: Rc<Platform>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

impl Device {
    /// The default GPU's device, ready to be shared with transforms.
    pub fn open(platform: Rc<Platform>) -> Result<Self, String> {
        let mut device = None;
        // SAFETY: `device` outlives the call; no adapter, module or
        // feature levels (the defaults), and no context wanted.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .map_err(|e| format!("no Direct3D 11 device for video: {e}"))?;
        let device = device.ok_or("no Direct3D 11 device")?;
        // The transform uses the device from threads of its own.
        let multithread: ID3D11Multithread = device.cast().map_err(|e| e.to_string())?;
        // SAFETY: a plain call on a live device.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
        let mut token = 0;
        let mut manager = None;
        // SAFETY: both outlive the call.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| format!("no DXGI device manager: {e}"))?;
        let manager = manager.ok_or("no DXGI device manager")?;
        // SAFETY: the device and the token the manager gave.
        unsafe { manager.ResetDevice(&device, token) }
            .map_err(|e| format!("the device manager refused the device: {e}"))?;
        let adapter = adapter_name(&device).unwrap_or_else(|| "a GPU".to_owned());
        Ok(Self {
            device,
            manager,
            adapter,
            _platform: platform,
        })
    }

    /// The largest of [`super::DECODE_SIZES`] the GPU decodes H.264
    /// (DXVA's H.264 profile into NV12) at; none if it does not.
    pub fn decode_max(&self) -> Option<(u32, u32)> {
        let video: ID3D11VideoDevice = self.device.cast().ok()?;
        let profile = D3D11_DECODER_PROFILE_H264_VLD_NOFGT;
        // SAFETY: plain queries on a live device; `profile` and the
        // descriptions outlive the calls.
        unsafe {
            let count = video.GetVideoDecoderProfileCount();
            let listed = (0..count).any(|i| video.GetVideoDecoderProfile(i) == Ok(profile));
            if !listed
                || !video
                    .CheckVideoDecoderFormat(&profile, DXGI_FORMAT_NV12)
                    .is_ok_and(|ok| ok.as_bool())
            {
                return None;
            }
            super::DECODE_SIZES.into_iter().find(|&(width, height)| {
                let description = D3D11_VIDEO_DECODER_DESC {
                    Guid: profile,
                    SampleWidth: width,
                    SampleHeight: height,
                    OutputFormat: DXGI_FORMAT_NV12,
                };
                video
                    .GetVideoDecoderConfigCount(&description)
                    .is_ok_and(|n| n > 0)
            })
        }
    }
}

/// The name of the adapter `device` is on.
fn adapter_name(device: &ID3D11Device) -> Option<String> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    // SAFETY: plain queries on live objects.
    let description = unsafe { dxgi.GetAdapter().ok()?.GetDesc().ok()? };
    let name = &description.Description;
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..end]))
}

/// A new media type of `major` and `subtype`.
pub fn media_type(major: &GUID, subtype: &GUID) -> windows::core::Result<IMFMediaType> {
    // SAFETY: a plain call; the GUIDs outlive the calls that copy them.
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, major)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        Ok(media)
    }
}

/// A `VT_UI4` value for `ICodecAPI`.
pub fn variant_u32(value: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: value },
            }),
        },
    }
}

/// A `VT_BOOL` value for `ICodecAPI`.
pub fn variant_bool(value: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: if value {
                        windows::Win32::Foundation::VARIANT_TRUE
                    } else {
                        windows::Win32::Foundation::VARIANT_FALSE
                    },
                },
            }),
        },
    }
}

/// A sample in memory of `len` bytes, filled by `fill`, at `time` for
/// `duration` (both in 100 ns units).
pub fn memory_sample(
    len: usize,
    time: i64,
    duration: i64,
    fill: impl FnOnce(&mut [u8]) -> bool,
) -> Result<IMFSample, Failure> {
    let size = u32::try_from(len).map_err(|_| Failure::broken("a frame too large"))?;
    // SAFETY: the buffer is locked while its bytes are written, `len` of
    // them as it was made with, and unlocked before it is handed on.
    unsafe {
        let buffer = MFCreateMemoryBuffer(size).map_err(|e| failure("a buffer", &e))?;
        let mut data = std::ptr::null_mut();
        buffer
            .Lock(&mut data, None, None)
            .map_err(|e| failure("locking a buffer", &e))?;
        let filled = !data.is_null() && fill(std::slice::from_raw_parts_mut(data, len));
        buffer
            .Unlock()
            .map_err(|e| failure("unlocking a buffer", &e))?;
        if !filled {
            return Err(Failure::broken("a picture that did not fit its buffer"));
        }
        buffer
            .SetCurrentLength(size)
            .map_err(|e| failure("a buffer's length", &e))?;
        let sample = MFCreateSample().map_err(|e| failure("a sample", &e))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| failure("a sample's buffer", &e))?;
        sample
            .SetSampleTime(time)
            .map_err(|e| failure("a sample's time", &e))?;
        sample
            .SetSampleDuration(duration)
            .map_err(|e| failure("a sample's duration", &e))?;
        Ok(sample)
    }
}

/// An empty sample of `len` bytes for a transform that does not make its
/// own.
fn output_sample(len: u32) -> windows::core::Result<IMFSample> {
    // SAFETY: plain calls; the sample holds the buffer.
    unsafe {
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(len)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

/// The bytes of `sample`, as one piece.
pub fn sample_bytes(sample: &IMFSample) -> windows::core::Result<Vec<u8>> {
    // SAFETY: the buffer is read while locked, as long as it says, and
    // unlocked before it goes.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut len = 0;
        buffer.Lock(&mut data, None, Some(&mut len))?;
        let bytes = if data.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, len as usize).to_vec()
        };
        buffer.Unlock()?;
        Ok(bytes)
    }
}

/// What a transform's output stream gave.
pub enum Output {
    /// A sample.
    Sample(IMFSample),
    /// Nothing until more input.
    NeedMoreInput,
    /// Its format changed and must be set again.
    StreamChange,
}

/// Takes one output from `transform`'s `stream`: into a sample of
/// `allocate` bytes made here, or (none) one the transform makes.
pub fn process_output(
    transform: &IMFTransform,
    stream: u32,
    allocate: Option<u32>,
) -> windows::core::Result<Output> {
    let sample = allocate.map(output_sample).transpose()?;
    let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: stream,
        pSample: ManuallyDrop::new(sample),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    }];
    let mut status = 0;
    // SAFETY: `buffers` and `status` outlive the call; the sample and
    // events it leaves in `buffers` are ours to release, which taking
    // them out of their ManuallyDrop below does.
    let result = unsafe { transform.ProcessOutput(0, &mut buffers, &mut status) };
    let [buffer] = buffers;
    let sample = ManuallyDrop::into_inner(buffer.pSample);
    drop(ManuallyDrop::into_inner(buffer.pEvents));
    match result {
        Ok(()) => sample
            .map(Output::Sample)
            .ok_or_else(|| windows::core::Error::from(E_UNEXPECTED)),
        Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Output::NeedMoreInput),
        Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Output::StreamChange),
        Err(error) => Err(error),
    }
}

/// The first stream IDs of `transform` (input, output): most number
/// them from 0, which a transform that does not say means.
pub fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let (mut input, mut output) = ([0u32], [0u32]);
    // SAFETY: both slices outlive the call, one ID each.
    match unsafe { transform.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => (input[0], output[0]),
        Err(_) => (0, 0),
    }
}
