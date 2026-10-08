//! Linux cameras through V4L2, the kernel's video API, spoken directly:
//! `ioctl`s on the device file and its buffers mapped into memory (the
//! "streaming I/O, memory mapping" way every webcam driver offers).
//!
//! Why not `nokhwa` (which the helper uses on macOS and Windows): on Linux
//! it goes through the `v4l` crate, whose bindings bindgen generates at
//! build time, so every build of the helper would need libclang. The part
//! of V4L2 a camera needs is small and has been stable for twenty years:
//! a dozen structures (`linux/videodev2.h`, copied by hand like libva's,
//! their sizes checked in the tests against the kernel's ioctl numbers)
//! and a dozen calls. This module is where they are, with every `unsafe`
//! block saying why it holds.
//!
//! What it does: lists the capture devices under `/dev/video*` that
//! stream in a format the helper reads ([`list`]); opens one, choosing
//! the format and size nearest 640×480 (YUYV, NV12 or I420 at that size
//! first, MJPEG otherwise: a raw format at a large size runs slowly over
//! USB 2), asks for 30 a second, maps four buffers and streams
//! ([`V4l2Camera::open`]); and hands each frame on as I420
//! ([`super::Device::next`]). Dropping it stops the stream, unmaps the
//! buffers and closes the device, which puts the camera's light out.
//!
//! Only 64-bit Linux on the usual architectures, whose `ioctl` numbers
//! share one layout and whose `struct v4l2_buffer` is the size checked
//! here; elsewhere the helper says it has no camera.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use noslacking_video_ipc::{CaptureProblem, Planes, Source, SourceKind};

use super::convert;
use crate::capture::Trouble;

/// `_IOC_WRITE`.
const IOC_WRITE: u64 = 1;
/// `_IOC_READ`.
const IOC_READ: u64 = 2;

/// An `ioctl` number of V4L2's (type `'V'`), as `_IOC` makes it on the
/// architectures this builds for.
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}

const VIDIOC_QUERYCAP: u64 = ioc(IOC_READ, 0, size_of::<Capability>());
const VIDIOC_ENUM_FMT: u64 = ioc(IOC_READ | IOC_WRITE, 2, size_of::<FmtDesc>());
const VIDIOC_G_FMT: u64 = ioc(IOC_READ | IOC_WRITE, 4, size_of::<Format>());
const VIDIOC_S_FMT: u64 = ioc(IOC_READ | IOC_WRITE, 5, size_of::<Format>());
const VIDIOC_REQBUFS: u64 = ioc(IOC_READ | IOC_WRITE, 8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: u64 = ioc(IOC_READ | IOC_WRITE, 9, size_of::<Buffer>());
const VIDIOC_QBUF: u64 = ioc(IOC_READ | IOC_WRITE, 15, size_of::<Buffer>());
const VIDIOC_DQBUF: u64 = ioc(IOC_READ | IOC_WRITE, 17, size_of::<Buffer>());
const VIDIOC_STREAMON: u64 = ioc(IOC_WRITE, 18, size_of::<i32>());
const VIDIOC_STREAMOFF: u64 = ioc(IOC_WRITE, 19, size_of::<i32>());
const VIDIOC_S_PARM: u64 = ioc(IOC_READ | IOC_WRITE, 22, size_of::<StreamParm>());
const VIDIOC_ENUM_FRAMESIZES: u64 = ioc(IOC_READ | IOC_WRITE, 74, size_of::<FrmSizeEnum>());

/// `V4L2_CAP_VIDEO_CAPTURE`.
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
/// `V4L2_CAP_STREAMING`.
const CAP_STREAMING: u32 = 0x0400_0000;
/// `V4L2_CAP_DEVICE_CAPS`: `device_caps` says what this node does.
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
/// `V4L2_CAP_TIMEPERFRAME`, in a stream parameter's `capability`.
const CAP_TIMEPERFRAME: u32 = 0x1000;
/// `V4L2_BUF_TYPE_VIDEO_CAPTURE`.
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
/// `V4L2_MEMORY_MMAP`.
const MEMORY_MMAP: u32 = 1;
/// `V4L2_FIELD_NONE`: whole frames, not interlaced fields.
const FIELD_NONE: u32 = 1;
/// `V4L2_FRMSIZE_TYPE_DISCRETE`.
const FRMSIZE_DISCRETE: u32 = 1;
/// `V4L2_BUF_FLAG_ERROR`: the frame came out damaged.
const BUF_FLAG_ERROR: u32 = 0x40;

/// A V4L2 pixel format, its four characters as a little-endian word.
const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// The formats read, best first: raw formats cost nothing to decode.
const FORMATS: [u32; 5] = [
    fourcc(b"YUYV"),
    fourcc(b"NV12"),
    fourcc(b"YU12"),
    fourcc(b"MJPG"),
    fourcc(b"JPEG"),
];

/// The size asked for: what a call sends.
const WANTED: (u32, u32) = (640, 480);
/// Frames a second asked for.
const FPS: u32 = 30;
/// Buffers mapped: one being filled, one being read, two to spare.
const BUFFERS: u32 = 4;
/// How long a read waits for a frame before letting its thread look
/// whether to stop, in milliseconds.
const WAIT_MS: i32 = 200;

/// `struct v4l2_capability`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_fmtdesc`.
#[repr(C)]
#[derive(Clone, Copy)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_pix_format`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    encoding: u32,
    quantization: u32,
    xfer_func: u32,
}

/// The union in `struct v4l2_format`: 200 bytes, aligned as a pointer
/// (some of its members hold one).
#[repr(C)]
#[derive(Clone, Copy)]
union FormatUnion {
    pix: PixFormat,
    raw: [u8; 200],
    align: *mut c_void,
}

/// `struct v4l2_format`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Format {
    kind: u32,
    fmt: FormatUnion,
}

/// `struct v4l2_requestbuffers`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_timecode`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timecode {
    kind: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

/// The union `m` in `struct v4l2_buffer`: where a buffer is.
#[repr(C)]
#[derive(Clone, Copy)]
union BufferM {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut c_void,
    fd: i32,
}

/// `struct v4l2_buffer`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Buffer {
    index: u32,
    kind: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    m: BufferM,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

impl Buffer {
    /// A capture buffer of mapped memory, `index`.
    fn mapped(index: u32) -> Self {
        Self {
            index,
            kind: BUF_TYPE_VIDEO_CAPTURE,
            bytesused: 0,
            flags: 0,
            field: 0,
            timestamp: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            timecode: Timecode::default(),
            sequence: 0,
            memory: MEMORY_MMAP,
            m: BufferM { userptr: 0 },
            length: 0,
            reserved2: 0,
            request_fd: 0,
        }
    }
}

/// `struct v4l2_fract`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Fract {
    numerator: u32,
    denominator: u32,
}

/// `struct v4l2_captureparm`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

/// The union in `struct v4l2_streamparm`.
#[repr(C)]
#[derive(Clone, Copy)]
union StreamParmUnion {
    capture: CaptureParm,
    raw: [u8; 200],
}

/// `struct v4l2_streamparm`.
#[repr(C)]
#[derive(Clone, Copy)]
struct StreamParm {
    kind: u32,
    parm: StreamParmUnion,
}

/// `struct v4l2_frmsizeenum`, its union as six words: a discrete size's
/// width and height, or a stepwise range's minimum, maximum and step for
/// the width, then the same for the height.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    kind: u32,
    sizes: [u32; 6],
    reserved: [u32; 2],
}

/// One `ioctl` on `file`, again if a signal interrupted it.
///
/// # Safety
///
/// `request` must be an ioctl whose argument is a `T` the kernel reads
/// and writes no more than `size_of::<T>()` of: every request above is
/// made from its structure's size, and each caller passes that
/// structure.
unsafe fn ioctl<T>(file: &File, request: u64, argument: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: the caller promises `argument` is the structure
        // `request` names, so the kernel stays inside it; the descriptor
        // is open for as long as `file` is borrowed.
        let result = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                request as libc::Ioctl,
                std::ptr::from_mut(argument).cast::<c_void>(),
            )
        };
        if result != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The device's capabilities.
fn query(file: &File) -> io::Result<Capability> {
    let mut capability = Capability {
        driver: [0; 16],
        card: [0; 32],
        bus_info: [0; 32],
        version: 0,
        capabilities: 0,
        device_caps: 0,
        reserved: [0; 3],
    };
    // SAFETY: VIDIOC_QUERYCAP takes a `struct v4l2_capability`.
    unsafe { ioctl(file, VIDIOC_QUERYCAP, &mut capability) }?;
    Ok(capability)
}

/// Whether a node with `capability` captures video by streaming (a
/// camera, not its metadata node or an output).
fn captures(capability: &Capability) -> bool {
    let caps = if capability.capabilities & CAP_DEVICE_CAPS != 0 {
        capability.device_caps
    } else {
        capability.capabilities
    };
    caps & CAP_VIDEO_CAPTURE != 0 && caps & CAP_STREAMING != 0
}

/// A C string in a fixed field, up to its first nul.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).trim().to_owned()
}

/// The pixel formats the device captures in, in its order.
fn formats(file: &File) -> Vec<u32> {
    let mut found = Vec::new();
    for index in 0..64 {
        let mut desc = FmtDesc {
            index,
            kind: BUF_TYPE_VIDEO_CAPTURE,
            flags: 0,
            description: [0; 32],
            pixelformat: 0,
            mbus_code: 0,
            reserved: [0; 3],
        };
        // SAFETY: VIDIOC_ENUM_FMT takes a `struct v4l2_fmtdesc`.
        if unsafe { ioctl(file, VIDIOC_ENUM_FMT, &mut desc) }.is_err() {
            break;
        }
        found.push(desc.pixelformat);
    }
    found
}

/// The sizes the device captures `format` at: discrete ones, or for a
/// range the size wanted brought within it.
fn sizes(file: &File, format: u32) -> Vec<(u32, u32)> {
    let mut found = Vec::new();
    for index in 0..256 {
        let mut size = FrmSizeEnum {
            index,
            pixel_format: format,
            ..FrmSizeEnum::default()
        };
        // SAFETY: VIDIOC_ENUM_FRAMESIZES takes a `struct v4l2_frmsizeenum`.
        if unsafe { ioctl(file, VIDIOC_ENUM_FRAMESIZES, &mut size) }.is_err() {
            break;
        }
        if size.kind == FRMSIZE_DISCRETE {
            found.push((size.sizes[0], size.sizes[1]));
        } else {
            // Stepwise or continuous: one entry says the range.
            let [min_w, max_w, _, min_h, max_h, _] = size.sizes;
            found.push((
                WANTED.0.clamp(min_w, max_w.max(min_w)),
                WANTED.1.clamp(min_h, max_h.max(min_h)),
            ));
            break;
        }
    }
    found
}

/// How far `size` is from what is wanted, smaller better: one at least
/// as large (shrunk on the way out) before one smaller, and of those the
/// fewest pixels to throw away or miss.
fn distance(size: (u32, u32)) -> (u8, u64) {
    let area = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
    if size.0 >= WANTED.0 && size.1 >= WANTED.1 {
        (0, area(size) - area(WANTED))
    } else {
        (1, area(WANTED).saturating_sub(area(size)))
    }
}

/// The format and size to capture in, of `offered` (each format's
/// sizes): the size nearest 640×480, raw formats before MJPEG at that
/// distance. None when no format is one the helper reads.
fn choose(offered: &[(u32, Vec<(u32, u32)>)]) -> Option<(u32, (u32, u32))> {
    offered
        .iter()
        .filter_map(|(format, sizes)| {
            let rank = FORMATS.iter().position(|f| f == format)?;
            let size = if sizes.is_empty() {
                // A driver that lists no sizes: ask, and take its answer.
                WANTED
            } else {
                *sizes.iter().min_by_key(|size| distance(**size))?
            };
            Some((distance(size), rank, *format, size))
        })
        .min_by_key(|(distance, rank, _, _)| (*distance, *rank))
        .map(|(_, _, format, size)| (format, size))
}

/// Opens `path` to read and write, without waiting.
fn open_device(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// What an error opening or starting the camera tells the app.
fn trouble(path: &Path, what: &str, error: &io::Error) -> Trouble {
    let detail = format!("{}: {what}: {error}", path.display());
    let problem = match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => CaptureProblem::Denied,
        Some(libc::EBUSY) => CaptureProblem::Busy,
        Some(libc::ENOENT | libc::ENODEV | libc::ENXIO) => CaptureProblem::Gone,
        _ => CaptureProblem::Failed,
    };
    Trouble::new(problem, detail)
}

/// The `/dev/video*` nodes, by number.
fn nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<(u32, PathBuf)> = std::fs::read_dir("/dev")
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name();
                    let number = name.to_str()?.strip_prefix("video")?.parse().ok()?;
                    Some((number, entry.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes.into_iter().map(|(_, path)| path).collect()
}

/// The cameras there are: capture nodes that stream in a format the
/// helper reads, by number, named by their driver's card name. Opening
/// one to ask does not start it (its light stays off). A node this user
/// may not open is left out; if that is all of them, the system did not
/// allow the camera.
pub fn list() -> Result<Vec<Source>, Trouble> {
    let mut cameras = Vec::new();
    let mut denied = None;
    for path in nodes() {
        let file = match open_device(&path) {
            Ok(file) => file,
            Err(error) => {
                if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM)) {
                    denied = Some(trouble(&path, "opening", &error));
                }
                continue;
            }
        };
        let Ok(capability) = query(&file) else {
            continue;
        };
        if !captures(&capability) || !formats(&file).iter().any(|f| FORMATS.contains(f)) {
            continue;
        }
        let card = text(&capability.card);
        cameras.push(Source {
            id: format!("v4l2:{}", path.display()),
            name: if card.is_empty() {
                path.display().to_string()
            } else {
                card
            },
            kind: SourceKind::Camera,
        });
    }
    match denied {
        Some(trouble) if cameras.is_empty() => Err(trouble),
        _ => Ok(cameras),
    }
}

/// The device a listed camera's id names.
pub fn path_of(id: &str) -> Option<PathBuf> {
    id.strip_prefix("v4l2:").map(PathBuf::from)
}

/// One mapped buffer.
struct Mapped {
    at: *mut c_void,
    length: usize,
}

/// An open camera, streaming into its mapped buffers.
pub struct V4l2Camera {
    name: String,
    path: PathBuf,
    /// Unmapped before the file closes (fields drop in order).
    buffers: Vec<Mapped>,
    file: File,
    format: u32,
    width: u32,
    height: u32,
    stride: usize,
    streaming: bool,
    /// Frames that came out damaged, or did not read.
    unreadable: u64,
    taken: u64,
}

impl std::fmt::Debug for V4l2Camera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2Camera")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("size", &(self.width, self.height))
            .finish_non_exhaustive()
    }
}

impl V4l2Camera {
    /// Opens the camera at `path` and starts it streaming, in the format
    /// and size nearest 640×480 at 30 a second.
    pub fn open(path: &Path) -> Result<Self, Trouble> {
        let file = open_device(path).map_err(|e| trouble(path, "opening", &e))?;
        let capability = query(&file).map_err(|e| trouble(path, "asking what it is", &e))?;
        if !captures(&capability) {
            return Err(Trouble::new(
                CaptureProblem::Unavailable,
                format!("{} does not capture video", path.display()),
            ));
        }
        let name = text(&capability.card);
        let offered: Vec<(u32, Vec<(u32, u32)>)> = formats(&file)
            .into_iter()
            .map(|format| (format, sizes(&file, format)))
            .collect();
        let Some((format, size)) = choose(&offered) else {
            return Err(Trouble::new(
                CaptureProblem::Failed,
                format!(
                    "{} ({name}) offers no format the helper reads: {}",
                    path.display(),
                    offered
                        .iter()
                        .map(|(f, _)| fourcc_name(*f))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        };
        let mut camera = Self {
            name,
            path: path.to_owned(),
            buffers: Vec::new(),
            file,
            format,
            width: size.0,
            height: size.1,
            stride: 0,
            streaming: false,
            unreadable: 0,
            taken: 0,
        };
        camera.set_format(format, size)?;
        camera.set_rate();
        camera.map()?;
        camera.start()?;
        eprintln!(
            "noslacking-video: camera: {} ({}): {} {}x{}",
            camera.name,
            camera.path.display(),
            fourcc_name(camera.format),
            camera.width,
            camera.height
        );
        Ok(camera)
    }

    /// Its name, for the log.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn fail(&self, what: &str, error: &io::Error) -> Trouble {
        trouble(&self.path, what, error)
    }

    /// Sets the capture format, and reads back what the driver made of
    /// it.
    fn set_format(&mut self, format: u32, size: (u32, u32)) -> Result<(), Trouble> {
        let mut asked = Format {
            kind: BUF_TYPE_VIDEO_CAPTURE,
            fmt: FormatUnion { raw: [0; 200] },
        };
        asked.fmt.pix = PixFormat {
            width: size.0,
            height: size.1,
            pixelformat: format,
            field: FIELD_NONE,
            ..PixFormat::default()
        };
        // SAFETY: VIDIOC_S_FMT takes a `struct v4l2_format`.
        unsafe { ioctl(&self.file, VIDIOC_S_FMT, &mut asked) }
            .map_err(|e| self.fail("setting the format", &e))?;
        let mut got = Format {
            kind: BUF_TYPE_VIDEO_CAPTURE,
            fmt: FormatUnion { raw: [0; 200] },
        };
        // SAFETY: VIDIOC_G_FMT takes a `struct v4l2_format`.
        unsafe { ioctl(&self.file, VIDIOC_G_FMT, &mut got) }
            .map_err(|e| self.fail("reading the format", &e))?;
        // SAFETY: a capture format's union holds a `v4l2_pix_format`
        // (the kernel wrote one for this buffer type), and every bit
        // pattern is a valid one: it is all integers.
        let pix = unsafe { got.fmt.pix };
        if !FORMATS.contains(&pix.pixelformat) || pix.width == 0 || pix.height == 0 {
            return Err(Trouble::failed(format!(
                "{}: the driver chose {} {}x{}",
                self.path.display(),
                fourcc_name(pix.pixelformat),
                pix.width,
                pix.height
            )));
        }
        let packed = match fourcc_name(pix.pixelformat).as_str() {
            "YUYV" => pix.width as usize * 2,
            "MJPG" | "JPEG" => 0,
            _ => pix.width as usize,
        };
        self.format = pix.pixelformat;
        self.width = pix.width;
        self.height = pix.height;
        self.stride = (pix.bytesperline as usize).max(packed);
        Ok(())
    }

    /// Asks for 30 frames a second, where the driver lets it be asked.
    /// A camera that cannot is used at its own rate.
    fn set_rate(&mut self) {
        let mut parm = StreamParm {
            kind: BUF_TYPE_VIDEO_CAPTURE,
            parm: StreamParmUnion { raw: [0; 200] },
        };
        parm.parm.capture = CaptureParm {
            capability: CAP_TIMEPERFRAME,
            timeperframe: Fract {
                numerator: 1,
                denominator: FPS,
            },
            ..CaptureParm::default()
        };
        // SAFETY: VIDIOC_S_PARM takes a `struct v4l2_streamparm`.
        if let Err(error) = unsafe { ioctl(&self.file, VIDIOC_S_PARM, &mut parm) } {
            eprintln!("noslacking-video: camera: no frame rate set ({error})");
            return;
        }
        // SAFETY: a capture parameter's union holds a
        // `v4l2_captureparm`, all integers.
        let rate = unsafe { parm.parm.capture }.timeperframe;
        if rate.numerator > 0 {
            eprintln!(
                "noslacking-video: camera: {:.1} frames a second",
                f64::from(rate.denominator) / f64::from(rate.numerator)
            );
        }
    }

    /// Asks for the buffers and maps them.
    fn map(&mut self) -> Result<(), Trouble> {
        let mut request = RequestBuffers {
            count: BUFFERS,
            kind: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            ..RequestBuffers::default()
        };
        // SAFETY: VIDIOC_REQBUFS takes a `struct v4l2_requestbuffers`.
        unsafe { ioctl(&self.file, VIDIOC_REQBUFS, &mut request) }
            .map_err(|e| self.fail("asking for buffers", &e))?;
        if request.count < 2 {
            return Err(Trouble::failed(format!(
                "{}: only {} buffers",
                self.path.display(),
                request.count
            )));
        }
        for index in 0..request.count.min(16) {
            let mut buffer = Buffer::mapped(index);
            // SAFETY: VIDIOC_QUERYBUF takes a `struct v4l2_buffer`.
            unsafe { ioctl(&self.file, VIDIOC_QUERYBUF, &mut buffer) }
                .map_err(|e| self.fail("asking where a buffer is", &e))?;
            // SAFETY: for a mapped buffer the kernel set `m.offset`.
            let offset = unsafe { buffer.m.offset };
            let length = buffer.length as usize;
            // SAFETY: maps `length` bytes of the device at the offset
            // the driver gave for this buffer, shared and readable; a
            // failure is MAP_FAILED, checked below, and nothing is
            // overlaid on memory of ours (the address is the kernel's
            // choice).
            let at = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    self.file.as_raw_fd(),
                    libc::off_t::from(offset),
                )
            };
            if at == libc::MAP_FAILED {
                return Err(self.fail("mapping a buffer", &io::Error::last_os_error()));
            }
            self.buffers.push(Mapped { at, length });
        }
        Ok(())
    }

    /// Queues every buffer and starts the stream: the camera's light
    /// goes on.
    fn start(&mut self) -> Result<(), Trouble> {
        for index in 0..self.buffers.len() {
            self.queue(u32::try_from(index).unwrap_or(0))?;
        }
        let mut kind = BUF_TYPE_VIDEO_CAPTURE as i32;
        // SAFETY: VIDIOC_STREAMON takes the buffer type as an `int`.
        unsafe { ioctl(&self.file, VIDIOC_STREAMON, &mut kind) }
            .map_err(|e| self.fail("starting", &e))?;
        self.streaming = true;
        Ok(())
    }

    /// Gives buffer `index` back to the driver to fill.
    fn queue(&self, index: u32) -> Result<(), Trouble> {
        let mut buffer = Buffer::mapped(index);
        // SAFETY: VIDIOC_QBUF takes a `struct v4l2_buffer`.
        unsafe { ioctl(&self.file, VIDIOC_QBUF, &mut buffer) }
            .map_err(|e| self.fail("queueing a buffer", &e))
    }

    /// The next frame as I420 and when it was taken; none if none came
    /// within a fifth of a second or it did not read. An error if the
    /// camera went away.
    pub fn frame(&mut self) -> Result<Option<(Planes, Instant)>, Trouble> {
        let mut poll = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one `pollfd`, ours, for a descriptor open while `self`
        // is.
        let ready = unsafe { libc::poll(&raw mut poll, 1, WAIT_MS) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(self.fail("waiting for a frame", &error));
        }
        if ready == 0 {
            return Ok(None);
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
            && poll.revents & libc::POLLIN == 0
        {
            return Err(Trouble::new(
                CaptureProblem::Ended,
                format!("{}: the camera went away", self.path.display()),
            ));
        }
        let mut buffer = Buffer::mapped(0);
        // SAFETY: VIDIOC_DQBUF takes a `struct v4l2_buffer`.
        match unsafe { ioctl(&self.file, VIDIOC_DQBUF, &mut buffer) } {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) if error.raw_os_error() == Some(libc::ENODEV) => {
                return Err(Trouble::new(
                    CaptureProblem::Ended,
                    format!("{}: the camera went away", self.path.display()),
                ));
            }
            Err(error) => return Err(self.fail("taking a frame", &error)),
        }
        let at = Instant::now();
        let index = buffer.index as usize;
        let Some(mapped) = self.buffers.get(index) else {
            return Err(Trouble::failed(format!(
                "{}: the driver gave back buffer {index} of {}",
                self.path.display(),
                self.buffers.len()
            )));
        };
        let used = (buffer.bytesused as usize).min(mapped.length);
        let picture = if buffer.flags & BUF_FLAG_ERROR != 0 || used == 0 {
            None
        } else {
            // SAFETY: the buffer is mapped for `mapped.length` bytes
            // (`used` is no more) for as long as `self` lives, and the
            // driver does not write to it until it is queued again,
            // below, after the slice's last use.
            let data = unsafe { std::slice::from_raw_parts(mapped.at.cast::<u8>(), used) };
            self.convert(data)
        };
        self.queue(buffer.index)?;
        match picture {
            Some(picture) => {
                self.taken += 1;
                Ok(Some((picture, at)))
            }
            None => {
                self.unreadable += 1;
                if self.unreadable == 1 {
                    eprintln!(
                        "noslacking-video: camera: a {} frame of {used} bytes did not read",
                        fourcc_name(self.format)
                    );
                }
                Ok(None)
            }
        }
    }

    /// One frame of the chosen format as I420.
    fn convert(&self, data: &[u8]) -> Option<Planes> {
        let (width, height, stride) = (self.width, self.height, self.stride);
        match fourcc_name(self.format).as_str() {
            "YUYV" => convert::from_yuyv(data, width, height, stride),
            "NV12" => convert::from_nv12(data, width, height, stride),
            "YU12" => convert::from_i420(data, width, height, stride),
            "MJPG" | "JPEG" => convert::from_mjpeg(data),
            _ => None,
        }
    }
}

impl Drop for V4l2Camera {
    fn drop(&mut self) {
        if self.streaming {
            let mut kind = BUF_TYPE_VIDEO_CAPTURE as i32;
            // SAFETY: VIDIOC_STREAMOFF takes the buffer type as an `int`.
            if let Err(error) = unsafe { ioctl(&self.file, VIDIOC_STREAMOFF, &mut kind) } {
                eprintln!("noslacking-video: camera: stopping: {error}");
            }
        }
        for mapped in self.buffers.drain(..) {
            // SAFETY: each was mapped by `map` with this length, and no
            // slice of it outlives `next`.
            unsafe { libc::munmap(mapped.at, mapped.length) };
        }
        let mut release = RequestBuffers {
            count: 0,
            kind: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            ..RequestBuffers::default()
        };
        // SAFETY: VIDIOC_REQBUFS takes a `struct v4l2_requestbuffers`;
        // none of them is mapped any more.
        let _ = unsafe { ioctl(&self.file, VIDIOC_REQBUFS, &mut release) };
        eprintln!(
            "noslacking-video: camera: {} closed; {} frames taken, {} unreadable",
            self.name, self.taken, self.unreadable
        );
    }
}

/// A pixel format's four characters, for the log.
fn fourcc_name(format: u32) -> String {
    format
        .to_le_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structures are the kernel's sizes on 64-bit Linux, and the
    /// requests made from them are the numbers `linux/videodev2.h` gives.
    #[test]
    fn structures_and_requests_match_the_kernel() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<FmtDesc>(), 64);
        assert_eq!(size_of::<PixFormat>(), 48);
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Timecode>(), 16);
        assert_eq!(size_of::<Buffer>(), 88);
        assert_eq!(std::mem::offset_of!(Buffer, timestamp), 24);
        assert_eq!(std::mem::offset_of!(Buffer, m), 64);
        assert_eq!(std::mem::offset_of!(Buffer, length), 72);
        assert_eq!(size_of::<StreamParm>(), 204);
        assert_eq!(size_of::<FrmSizeEnum>(), 44);
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(fourcc(b"YUYV"), 0x5659_5559);
        assert_eq!(fourcc_name(fourcc(b"MJPG")), "MJPG");
    }

    #[test]
    fn the_format_nearest_640x480_is_chosen_raw_before_mjpeg() {
        let yuyv = fourcc(b"YUYV");
        let mjpg = fourcc(b"MJPG");
        let grey = fourcc(b"GREY");
        // The common webcam: both at 640×480; raw wins.
        let both = [
            (yuyv, vec![(1280, 720), (640, 480), (320, 240)]),
            (mjpg, vec![(1920, 1080), (1280, 720), (640, 480)]),
        ];
        assert_eq!(choose(&both), Some((yuyv, (640, 480))));
        // Raw only large (slow over USB 2): MJPEG at 640×480 instead.
        let large = [
            (yuyv, vec![(1920, 1080), (1280, 720)]),
            (mjpg, vec![(1280, 720), (640, 480)]),
        ];
        assert_eq!(choose(&large), Some((mjpg, (640, 480))));
        // Nothing at 640×480: the smallest that covers it, else the
        // largest under it.
        assert_eq!(
            choose(&[(yuyv, vec![(1280, 960), (800, 600), (320, 240)])]),
            Some((yuyv, (800, 600)))
        );
        assert_eq!(
            choose(&[(yuyv, vec![(160, 120), (352, 288)])]),
            Some((yuyv, (352, 288)))
        );
        // A format the helper does not read, alone: nothing.
        assert_eq!(choose(&[(grey, vec![(640, 480)])]), None);
        // Sizes not listed: 640×480 is asked for.
        assert_eq!(choose(&[(mjpg, vec![])]), Some((mjpg, (640, 480))));
    }

    #[test]
    fn errors_say_what_the_app_tells_the_user() {
        let path = Path::new("/dev/video9");
        let problem =
            |errno| trouble(path, "opening", &io::Error::from_raw_os_error(errno)).problem;
        assert_eq!(problem(libc::EACCES), CaptureProblem::Denied);
        assert_eq!(problem(libc::EBUSY), CaptureProblem::Busy);
        assert_eq!(problem(libc::ENOENT), CaptureProblem::Gone);
        assert_eq!(problem(libc::EIO), CaptureProblem::Failed);
        assert_eq!(
            path_of("v4l2:/dev/video2"),
            Some(PathBuf::from("/dev/video2"))
        );
        assert_eq!(path_of("native:0"), None);
        assert_eq!(text(b"Integrated Camera\0\0\0junk"), "Integrated Camera");
    }

    /// The first camera of this machine, opened for a moment: a few
    /// frames come as 640×480-ish I420, and it closes. Opens a real
    /// camera (its light goes on for under a second), so it is ignored:
    /// `cargo test -p noslacking-video -- --ignored v4l2 --nocapture`.
    #[test]
    #[ignore = "opens this machine's camera"]
    #[allow(
        clippy::print_stdout,
        reason = "what the camera gave is for the reader"
    )]
    fn v4l2_opens_the_first_camera_and_reads_frames() {
        let cameras = list().expect("listed");
        println!("{cameras:?}");
        let first = cameras.first().expect("a camera");
        let path = path_of(&first.id).expect("a V4L2 camera");
        let mut camera = V4l2Camera::open(&path).expect("opened");
        let started = Instant::now();
        let mut frames = 0;
        while frames < 10 && started.elapsed() < std::time::Duration::from_secs(5) {
            if let Some((picture, _)) = camera.frame().expect("no trouble") {
                assert!(picture.check().is_ok());
                frames += 1;
                if frames == 1 {
                    println!(
                        "first frame after {:?}: {}x{}",
                        started.elapsed(),
                        picture.width,
                        picture.height
                    );
                }
            }
        }
        println!("{frames} frames in {:?}", started.elapsed());
        assert_eq!(frames, 10);
    }
}
