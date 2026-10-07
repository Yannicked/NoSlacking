//! What a shared screen needs of libva besides encoding: packed RGB
//! surfaces (written from memory, or imported from a dma-buf PipeWire
//! lends, so the compositor's picture reaches the encoder without the
//! processor touching it), and exporting a surface as a dma-buf, which
//! the benchmark's pretend screen hands over as PipeWire would.
//!
//! The structures are `va/va.h`'s and `va/va_drmcommon.h`'s from libva
//! 2.23, copied by hand like the rest (sizes and offsets checked against
//! clang's in the tests, padding written out).

use std::ffi::{c_int, c_uint, c_void};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::rc::Rc;

use super::{Display, INVALID_ID, Image, ImageFormat, ImageHandle, Surfaces};

/// `VA_RT_FORMAT_RGB32`.
const RT_FORMAT_RGB32: u32 = 0x0002_0000;
/// `VASurfaceAttribPixelFormat`.
const ATTRIB_PIXEL_FORMAT: c_int = 1;
/// `VASurfaceAttribMemoryType`.
const ATTRIB_MEMORY_TYPE: c_int = 6;
/// `VASurfaceAttribExternalBufferDescriptor`.
const ATTRIB_EXTERNAL_BUFFER: c_int = 7;
/// `VASurfaceAttribDRMFormatModifiers`.
const ATTRIB_DRM_FORMAT_MODIFIERS: c_int = 9;
/// `VA_SURFACE_ATTRIB_SETTABLE`.
const ATTRIB_SETTABLE: u32 = 0x2;
/// `VAGenericValueTypeInteger`.
const GENERIC_INTEGER: c_int = 1;
/// `VAGenericValueTypePointer`.
const GENERIC_POINTER: c_int = 3;
/// `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`.
const MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
/// `VA_EXPORT_SURFACE_READ_ONLY`.
const EXPORT_READ_ONLY: u32 = 0x1;
/// `VA_EXPORT_SURFACE_COMPOSED_LAYERS`.
const EXPORT_COMPOSED_LAYERS: u32 = 0x8;
/// `VA_LSB_FIRST`.
const LSB_FIRST: u32 = 1;
/// `DRM_FORMAT_MOD_LINEAR`.
pub const MOD_LINEAR: u64 = 0;

// The generic value's integer is written into the low half of its
// union, which is where C reads it only on a little-endian machine:
// every target this builds for.
const _: () = assert!(cfg!(target_endian = "little"));

/// A packed RGB layout: VA's fourcc and DRM's for the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    /// `VA_FOURCC_*`: the bytes in memory order.
    pub va: u32,
    /// `DRM_FORMAT_*`: the same as a little-endian word.
    pub drm: u32,
}

impl Rgb {
    /// Blue, green, red, padding: `VA_FOURCC_BGRX`, `DRM_FORMAT_XRGB8888`.
    pub const BGRX: Self = Self::new(*b"BGRX", *b"XR24");
    /// Blue, green, red, alpha: `VA_FOURCC_BGRA`, `DRM_FORMAT_ARGB8888`.
    pub const BGRA: Self = Self::new(*b"BGRA", *b"AR24");
    /// Red, green, blue, padding: `VA_FOURCC_RGBX`, `DRM_FORMAT_XBGR8888`.
    pub const RGBX: Self = Self::new(*b"RGBX", *b"XB24");
    /// Red, green, blue, alpha: `VA_FOURCC_RGBA`, `DRM_FORMAT_ABGR8888`.
    pub const RGBA: Self = Self::new(*b"RGBA", *b"AB24");

    const fn new(va: [u8; 4], drm: [u8; 4]) -> Self {
        Self {
            va: u32::from_le_bytes(va),
            drm: u32::from_le_bytes(drm),
        }
    }
}

/// `VASurfaceAttrib`, its `VAGenericValue` laid out flat: the type, the
/// padding C puts before the union, and the union's eight bytes.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct SurfaceAttrib {
    kind: c_int,
    flags: u32,
    value_type: c_int,
    pad: u32,
    value: u64,
}

impl SurfaceAttrib {
    fn integer(kind: c_int, value: u32) -> Self {
        Self {
            kind,
            flags: ATTRIB_SETTABLE,
            value_type: GENERIC_INTEGER,
            pad: 0,
            value: u64::from(value),
        }
    }

    fn pointer<T>(kind: c_int, value: *mut T) -> Self {
        Self {
            kind,
            flags: ATTRIB_SETTABLE,
            value_type: GENERIC_POINTER,
            pad: 0,
            value: value as usize as u64,
        }
    }
}

/// One object (buffer) of `VADRMPRIMESurfaceDescriptor`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
struct PrimeObject {
    fd: c_int,
    size: u32,
    drm_format_modifier: u64,
}

/// One layer of `VADRMPRIMESurfaceDescriptor`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
struct PrimeLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

/// `VADRMPRIMESurfaceDescriptor`, with the four bytes of padding C puts
/// at its end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
struct PrimeDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [PrimeObject; 4],
    num_layers: u32,
    layers: [PrimeLayer; 4],
    pad: u32,
}

/// `VADRMFormatModifierList`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct ModifierList {
    num_modifiers: u32,
    pad: u32,
    modifiers: *mut u64,
}

/// A dma-buf to import: one plane of packed RGB.
#[derive(Debug)]
pub struct DmaBufIn<'a> {
    /// Its file descriptor, open for the import.
    pub fd: BorrowedFd<'a>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the picture starts in the buffer.
    pub offset: u32,
    /// Bytes from one row to the next.
    pub pitch: u32,
    /// The pixels' layout.
    pub format: Rgb,
    /// The buffer's DRM format modifier.
    pub modifier: u64,
}

/// A surface exported as a dma-buf: its descriptor (ours to close) and
/// layout.
#[derive(Debug)]
pub struct Exported {
    /// The buffer.
    pub fd: OwnedFd,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the picture starts in it.
    pub offset: u32,
    /// Bytes from one row to the next.
    pub pitch: u32,
    /// The DRM format modifier the driver laid it out with.
    pub modifier: u64,
    /// `DRM_FORMAT_*`.
    pub drm_format: u32,
}

impl Display {
    /// One surface of `rt_format` at `width`×`height`, made with
    /// `attributes`.
    fn surface_with(
        self: &Rc<Self>,
        rt_format: u32,
        width: u32,
        height: u32,
        attributes: &mut [SurfaceAttrib],
    ) -> Result<Surfaces, String> {
        let mut ids = vec![INVALID_ID; 1];
        let count = c_uint::try_from(attributes.len()).map_err(|e| e.to_string())?;
        // SAFETY: `ids` has room for the one surface asked for; the
        // attributes are `count` live `VASurfaceAttrib`s whose pointers
        // (a descriptor, a modifier list) point at the caller's locals,
        // alive for this call, which is all libva reads them in.
        let status = unsafe {
            (self.libva.functions.create_surfaces)(
                self.raw,
                rt_format,
                width,
                height,
                ids.as_mut_ptr(),
                1,
                attributes.as_mut_ptr().cast::<c_void>(),
                count,
            )
        };
        self.check(status, "vaCreateSurfaces")?;
        Ok(Surfaces {
            display: Rc::clone(self),
            ids,
        })
    }

    /// A packed RGB surface of `format` to write pictures into; laid out
    /// linearly if `linear` and the driver can (else as it likes).
    pub fn rgb_surface(
        self: &Rc<Self>,
        width: u32,
        height: u32,
        format: Rgb,
        linear: bool,
    ) -> Result<Surfaces, String> {
        let mut modifier = MOD_LINEAR;
        let mut list = ModifierList {
            num_modifiers: 1,
            pad: 0,
            modifiers: &mut modifier,
        };
        let mut attributes = vec![SurfaceAttrib::integer(ATTRIB_PIXEL_FORMAT, format.va)];
        if linear {
            attributes.push(SurfaceAttrib::pointer(
                ATTRIB_DRM_FORMAT_MODIFIERS,
                &mut list,
            ));
            if let Ok(surfaces) = self.surface_with(RT_FORMAT_RGB32, width, height, &mut attributes)
            {
                return Ok(surfaces);
            }
            attributes.pop();
        }
        self.surface_with(RT_FORMAT_RGB32, width, height, &mut attributes)
    }

    /// A surface over `buffer`'s memory, for as long as it lives.
    pub fn import(self: &Rc<Self>, buffer: &DmaBufIn<'_>) -> Result<Surfaces, String> {
        let size = u64::from(buffer.offset)
            + u64::from(buffer.pitch) * u64::from(buffer.height.saturating_sub(1))
            + u64::from(buffer.width) * 4;
        let mut descriptor = PrimeDescriptor {
            fourcc: buffer.format.va,
            width: buffer.width,
            height: buffer.height,
            num_objects: 1,
            num_layers: 1,
            ..PrimeDescriptor::default()
        };
        descriptor.objects[0] = PrimeObject {
            fd: buffer.fd.as_raw_fd(),
            size: u32::try_from(size).map_err(|e| e.to_string())?,
            drm_format_modifier: buffer.modifier,
        };
        descriptor.layers[0] = PrimeLayer {
            drm_format: buffer.format.drm,
            num_planes: 1,
            offset: [buffer.offset, 0, 0, 0],
            pitch: [buffer.pitch, 0, 0, 0],
            ..PrimeLayer::default()
        };
        let mut attributes = [
            SurfaceAttrib::integer(ATTRIB_MEMORY_TYPE, MEM_TYPE_DRM_PRIME_2),
            SurfaceAttrib::pointer(ATTRIB_EXTERNAL_BUFFER, &mut descriptor),
        ];
        self.surface_with(
            RT_FORMAT_RGB32,
            buffer.width,
            buffer.height,
            &mut attributes,
        )
    }

    /// Whether the driver exports surfaces (libva 2.1 and on).
    pub fn exports(&self) -> bool {
        self.libva.functions.export_surface_handle.is_some()
    }

    /// `surface`, a packed RGB surface of one plane, as a dma-buf to
    /// read.
    pub fn export(&self, surface: u32) -> Result<Exported, String> {
        let export = self
            .libva
            .functions
            .export_surface_handle
            .ok_or("this libva does not export surfaces")?;
        let mut descriptor = PrimeDescriptor::default();
        // SAFETY: a surface of this display; libva fills the descriptor,
        // a live local of the type it writes.
        let status = unsafe {
            export(
                self.raw,
                surface,
                MEM_TYPE_DRM_PRIME_2,
                EXPORT_READ_ONLY | EXPORT_COMPOSED_LAYERS,
                (&raw mut descriptor).cast::<c_void>(),
            )
        };
        self.check(status, "vaExportSurfaceHandle")?;
        let objects = usize::try_from(descriptor.num_objects)
            .unwrap_or(0)
            .min(descriptor.objects.len());
        // Every descriptor libva handed over is ours to close, whatever
        // else is wrong.
        let mut fds: Vec<OwnedFd> = descriptor.objects[..objects]
            .iter()
            .filter(|object| object.fd >= 0)
            // SAFETY: vaExportSurfaceHandle opened these descriptors for
            // the caller, who owns and must close them; each is taken
            // once.
            .map(|object| unsafe { OwnedFd::from_raw_fd(object.fd) })
            .collect();
        let layer = descriptor.layers[0];
        if objects != 1 || fds.len() != 1 || descriptor.num_layers != 1 || layer.num_planes != 1 {
            return Err(format!(
                "an export of {objects} objects and {} layers, not one",
                descriptor.num_layers
            ));
        }
        let fd = fds.remove(0);
        Ok(Exported {
            fd,
            width: descriptor.width,
            height: descriptor.height,
            offset: layer.offset[0],
            pitch: layer.pitch[0],
            modifier: descriptor.objects[0].drm_format_modifier,
            drm_format: layer.drm_format,
        })
    }

    /// Writes packed RGB `data` (rows `stride` apart, `size` pixels) into
    /// RGB surface `surface` of the same size and layout `format`.
    pub fn write_packed(
        &self,
        surface: u32,
        size: (u32, u32),
        format: Rgb,
        data: &[u8],
        stride: usize,
    ) -> Result<(), String> {
        let (width, height) = (size.0 as usize, size.1 as usize);
        let row = width * 4;
        if width == 0 || height == 0 || stride < row {
            return Err("an empty picture or a short stride".into());
        }
        let needed = stride
            .checked_mul(height - 1)
            .and_then(|n| n.checked_add(row))
            .ok_or("the picture overflows")?;
        if data.len() < needed {
            return Err("the picture is shorter than its size".into());
        }
        if let Ok(image) = self.derived_rgb(surface, format) {
            return self.fill_rgb(&image, size, data, stride);
        }
        let image = self.new_rgb_image(size, format)?;
        self.fill_rgb(&image, size, data, stride)?;
        // SAFETY: an image and a surface of this display, both `size`
        // large.
        let status = unsafe {
            (self.libva.functions.put_image)(
                self.raw,
                surface,
                image.image.image_id,
                0,
                0,
                size.0,
                size.1,
                0,
                0,
                size.0,
                size.1,
            )
        };
        self.check(status, "vaPutImage")
    }

    /// The surface's own memory as an image, if the driver derives one in
    /// `format`.
    fn derived_rgb(&self, surface: u32, format: Rgb) -> Result<ImageHandle<'_>, String> {
        let mut image = Image::default();
        // SAFETY: a surface of this display; the out pointer is to a
        // live local.
        let status = unsafe { (self.libva.functions.derive_image)(self.raw, surface, &mut image) };
        self.check(status, "vaDeriveImage")?;
        let handle = ImageHandle {
            display: self,
            image,
        };
        if handle.image.format.fourcc != format.va || handle.image.num_planes != 1 {
            return Err("the derived image is not that RGB".into());
        }
        Ok(handle)
    }

    /// A packed RGB image of `size` to put into a surface.
    fn new_rgb_image(&self, size: (u32, u32), format: Rgb) -> Result<ImageHandle<'_>, String> {
        let mut description = ImageFormat {
            fourcc: format.va,
            byte_order: LSB_FIRST,
            bits_per_pixel: 32,
            ..ImageFormat::default()
        };
        let int = |n: u32| c_int::try_from(n).map_err(|e| e.to_string());
        let mut image = Image::default();
        // SAFETY: a format description and an out pointer to live
        // locals.
        let status = unsafe {
            (self.libva.functions.create_image)(
                self.raw,
                &mut description,
                int(size.0)?,
                int(size.1)?,
                &mut image,
            )
        };
        self.check(status, "vaCreateImage")?;
        let handle = ImageHandle {
            display: self,
            image,
        };
        if handle.image.format.fourcc != format.va || handle.image.num_planes != 1 {
            return Err("the image is not that RGB".into());
        }
        Ok(handle)
    }

    /// Maps `handle`'s buffer and copies `data` into it row by row,
    /// after checking every row fits.
    fn fill_rgb(
        &self,
        handle: &ImageHandle<'_>,
        size: (u32, u32),
        data: &[u8],
        stride: usize,
    ) -> Result<(), String> {
        let image = &handle.image;
        let (width, height) = (size.0 as usize, size.1 as usize);
        let row = width * 4;
        let pitch = image.pitches[0] as usize;
        let offset = image.offsets[0] as usize;
        let end = pitch
            .checked_mul(height - 1)
            .and_then(|n| n.checked_add(offset))
            .and_then(|n| n.checked_add(row));
        if u32::from(image.width) < size.0
            || u32::from(image.height) < size.1
            || pitch < row
            || end.is_none_or(|end| end > image.data_size as usize)
        {
            return Err("the image does not hold the picture".into());
        }
        let mut mapped: *mut c_void = std::ptr::null_mut();
        // SAFETY: the image's buffer, mapped for writing; unmapped below
        // before the image is destroyed.
        let status = unsafe { (self.libva.functions.map_buffer)(self.raw, image.buf, &mut mapped) };
        self.check(status, "vaMapBuffer")?;
        if mapped.is_null() {
            return Err("vaMapBuffer gave nothing".into());
        }
        let length = image.data_size as usize;
        // SAFETY: libva mapped `data_size` bytes at `mapped`, writable
        // and ours alone until vaUnmapBuffer, after the last use of
        // `out`.
        let out = unsafe { std::slice::from_raw_parts_mut(mapped.cast::<u8>(), length) };
        for (n, line) in data.chunks(stride).take(height).enumerate() {
            let start = offset + n * pitch;
            out[start..start + row].copy_from_slice(&line[..row]);
        }
        // SAFETY: mapped above; `out` is not used past here.
        let status = unsafe { (self.libva.functions.unmap_buffer)(self.raw, image.buf) };
        self.check(status, "vaUnmapBuffer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Sizes and offsets as clang computed them from libva 2.23's va.h
    /// and va_drmcommon.h on x86_64.
    #[test]
    fn prime_structures_match_libva() {
        assert_eq!(size_of::<SurfaceAttrib>(), 24);
        assert_eq!(offset_of!(SurfaceAttrib, flags), 4);
        assert_eq!(offset_of!(SurfaceAttrib, value_type), 8);
        assert_eq!(offset_of!(SurfaceAttrib, value), 16);
        assert_eq!(size_of::<PrimeObject>(), 16);
        assert_eq!(offset_of!(PrimeObject, size), 4);
        assert_eq!(offset_of!(PrimeObject, drm_format_modifier), 8);
        assert_eq!(size_of::<PrimeLayer>(), 56);
        assert_eq!(offset_of!(PrimeLayer, num_planes), 4);
        assert_eq!(offset_of!(PrimeLayer, object_index), 8);
        assert_eq!(offset_of!(PrimeLayer, offset), 24);
        assert_eq!(offset_of!(PrimeLayer, pitch), 40);
        assert_eq!(size_of::<PrimeDescriptor>(), 312);
        assert_eq!(offset_of!(PrimeDescriptor, objects), 16);
        assert_eq!(offset_of!(PrimeDescriptor, num_layers), 80);
        assert_eq!(offset_of!(PrimeDescriptor, layers), 84);
        assert_eq!(size_of::<ModifierList>(), 16);
        assert_eq!(offset_of!(ModifierList, modifiers), 8);
    }

    #[test]
    fn fourccs_spell_their_bytes() {
        // VA_FOURCC('B','G','R','X') as libva's header defines it.
        assert_eq!(Rgb::BGRX.va, 0x5852_4742);
        // DRM_FORMAT_XRGB8888: fourcc_code('X', 'R', '2', '4').
        assert_eq!(Rgb::BGRX.drm, 0x3432_5258);
    }
}
