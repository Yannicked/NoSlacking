//! A camera's frames as I420, whatever it gives: YUYV (most webcams'
//! raw format), NV12, I420 itself, MJPEG (decoded by zune-jpeg, pure
//! Rust, which fills in the Huffman tables webcams leave out), packed
//! RGB and grey. An odd size loses its last row or column (4:2:0 wants
//! even sizes); a buffer shorter than its size says is refused, never
//! read past.

use noslacking_video_ipc::Planes;

/// A black I420 picture of `width`×`height` (even), to write into.
fn black(width: usize, height: usize) -> Option<Planes> {
    Some(Planes {
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        y: vec![16; width * height],
        u: vec![128; width / 2 * (height / 2)],
        v: vec![128; width / 2 * (height / 2)],
    })
}

/// The even size of a `width`×`height` frame, none if empty or larger
/// than a picture may be.
fn even(width: u32, height: u32) -> Option<(usize, usize)> {
    let (w, h) = (width & !1, height & !1);
    if w == 0 || h == 0 || w > noslacking_video_ipc::MAX_SIDE || h > noslacking_video_ipc::MAX_SIDE
    {
        return None;
    }
    Some((usize::try_from(w).ok()?, usize::try_from(h).ok()?))
}

/// Whether `data` holds `rows` rows of `row` bytes each `stride` apart.
fn holds(data: &[u8], stride: usize, row: usize, rows: usize) -> bool {
    stride >= row
        && rows > 0
        && stride
            .checked_mul(rows - 1)
            .and_then(|n| n.checked_add(row))
            .is_some_and(|n| data.len() >= n)
}

/// A YUYV (YUY2) frame, rows `stride` bytes apart, as I420: luma as is,
/// chroma from each pair of rows averaged.
pub fn from_yuyv(data: &[u8], width: u32, height: u32, stride: usize) -> Option<Planes> {
    let (w, h) = even(width, height)?;
    if !holds(data, stride, w * 2, h) {
        return None;
    }
    let mut out = black(w, h)?;
    for row in 0..h {
        let line = &data[row * stride..row * stride + w * 2];
        for (x, px) in line.as_chunks::<2>().0.iter().enumerate() {
            out.y[row * w + x] = px[0];
        }
    }
    for row in 0..h / 2 {
        let a = &data[2 * row * stride..2 * row * stride + w * 2];
        let b = &data[(2 * row + 1) * stride..(2 * row + 1) * stride + w * 2];
        for (x, (pa, pb)) in a
            .as_chunks::<4>()
            .0
            .iter()
            .zip(b.as_chunks::<4>().0)
            .enumerate()
        {
            let at = row * (w / 2) + x;
            out.u[at] = avg2(pa[1], pb[1]);
            out.v[at] = avg2(pa[3], pb[3]);
        }
    }
    Some(out)
}

/// An NV12 frame (luma, then interleaved blue and red at half height),
/// rows `stride` bytes apart in both planes, as I420.
pub fn from_nv12(data: &[u8], width: u32, height: u32, stride: usize) -> Option<Planes> {
    let (w, h) = even(width, height)?;
    let full_height = usize::try_from(height).ok()?;
    let chroma_at = stride.checked_mul(full_height)?;
    if !holds(data, stride, w, h) || !holds(data.get(chroma_at..)?, stride, w, h / 2) {
        return None;
    }
    let mut out = black(w, h)?;
    for row in 0..h {
        out.y[row * w..(row + 1) * w].copy_from_slice(&data[row * stride..row * stride + w]);
    }
    let chroma = &data[chroma_at..];
    for row in 0..h / 2 {
        let line = &chroma[row * stride..row * stride + w];
        for (x, pair) in line.as_chunks::<2>().0.iter().enumerate() {
            out.u[row * (w / 2) + x] = pair[0];
            out.v[row * (w / 2) + x] = pair[1];
        }
    }
    Some(out)
}

/// An I420 (YU12) frame, luma rows `stride` bytes apart and chroma rows
/// half that, as tightly packed I420.
pub fn from_i420(data: &[u8], width: u32, height: u32, stride: usize) -> Option<Planes> {
    let (w, h) = even(width, height)?;
    let full_height = usize::try_from(height).ok()?;
    let chroma_stride = stride.div_ceil(2);
    let chroma_rows = full_height.div_ceil(2);
    let u_at = stride.checked_mul(full_height)?;
    let v_at = u_at.checked_add(chroma_stride.checked_mul(chroma_rows)?)?;
    if !holds(data, stride, w, h)
        || !holds(data.get(u_at..)?, chroma_stride, w / 2, h / 2)
        || !holds(data.get(v_at..)?, chroma_stride, w / 2, h / 2)
    {
        return None;
    }
    let mut out = black(w, h)?;
    for row in 0..h {
        out.y[row * w..(row + 1) * w].copy_from_slice(&data[row * stride..row * stride + w]);
    }
    let (cw, ch) = (w / 2, h / 2);
    for (plane, at) in [(&mut out.u, u_at), (&mut out.v, v_at)] {
        for row in 0..ch {
            let from = at + row * chroma_stride;
            plane[row * cw..(row + 1) * cw].copy_from_slice(&data[from..from + cw]);
        }
    }
    Some(out)
}

/// Packed RGB (3 bytes a pixel), rows `stride` bytes apart, as I420,
/// BT.601 studio range, as WebRTC senders send it.
pub fn from_rgb(data: &[u8], width: u32, height: u32, stride: usize) -> Option<Planes> {
    let (w, h) = even(width, height)?;
    if !holds(data, stride, w * 3, h) {
        return None;
    }
    // The even part, tightly packed, for the converter.
    let rgb: Vec<u8> = (0..h)
        .flat_map(|row| &data[row * stride..row * stride + w * 3])
        .copied()
        .collect();
    let mut out = black(w, h)?;
    let size = |n: usize| u32::try_from(n).ok();
    let mut planar = yuv::YuvPlanarImageMut {
        y_plane: yuv::BufferStoreMut::Borrowed(&mut out.y),
        y_stride: size(w)?,
        u_plane: yuv::BufferStoreMut::Borrowed(&mut out.u),
        u_stride: size(w / 2)?,
        v_plane: yuv::BufferStoreMut::Borrowed(&mut out.v),
        v_stride: size(w / 2)?,
        width: size(w)?,
        height: size(h)?,
    };
    yuv::rgb_to_yuv420(
        &mut planar,
        &rgb,
        size(w * 3)?,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
        yuv::YuvConversionMode::Balanced,
    )
    .ok()?;
    drop(planar);
    Some(out)
}

/// A grey frame (one byte a pixel, full range), rows `stride` bytes
/// apart, as I420 without colour.
pub fn from_gray(data: &[u8], width: u32, height: u32, stride: usize) -> Option<Planes> {
    let (w, h) = even(width, height)?;
    if !holds(data, stride, w, h) {
        return None;
    }
    let mut out = black(w, h)?;
    for row in 0..h {
        for x in 0..w {
            // Full range to studio range.
            let p = u32::from(data[row * stride + x]);
            out.y[row * w + x] = u8::try_from(16 + p * 219 / 255).unwrap_or(235);
        }
    }
    Some(out)
}

/// An MJPEG frame (one JPEG) as I420, no larger than `MAX_SIDE` a side.
pub fn from_mjpeg(data: &[u8]) -> Option<Planes> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let side = noslacking_video_ipc::MAX_SIDE as usize;
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(side)
        .set_max_height(side);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    decoder.decode_headers().ok()?;
    let (width, height) = decoder.dimensions()?;
    let mut rgb = vec![0u8; decoder.output_buffer_size()?];
    decoder.decode_into(&mut rgb).ok()?;
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    from_rgb(&rgb, width, height, usize::try_from(width).ok()? * 3)
}

fn avg2(a: u8, b: u8) -> u8 {
    u8::try_from((u16::from(a) + u16::from(b)).div_ceil(2)).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_formats_become_i420() {
        // YUYV: Y0 U Y1 V for a 4x2 picture.
        let yuyv = [
            10, 100, 20, 200, 30, 110, 40, 210, //
            50, 102, 60, 202, 70, 112, 80, 212,
        ];
        let picture = from_yuyv(&yuyv, 4, 2, 8).expect("YUYV");
        assert_eq!(picture.y, vec![10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(picture.u, vec![101, 111]);
        assert_eq!(picture.v, vec![201, 211]);
        assert!(from_yuyv(&yuyv[..10], 4, 2, 8).is_none(), "too short");
        // Rows padded to 12 bytes.
        let mut padded = yuyv[..8].to_vec();
        padded.extend_from_slice(&[0; 4]);
        padded.extend_from_slice(&yuyv[8..]);
        assert_eq!(from_yuyv(&padded, 4, 2, 12), Some(picture));

        // NV12: 4x2 luma, then one row of U V U V.
        let nv12 = [1, 2, 3, 4, 5, 6, 7, 8, 100, 200, 110, 210];
        let picture = from_nv12(&nv12, 4, 2, 4).expect("NV12");
        assert_eq!(picture.y, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(picture.u, vec![100, 110]);
        assert_eq!(picture.v, vec![200, 210]);
        assert!(from_nv12(&nv12[..9], 4, 2, 4).is_none());

        // I420 with its rows padded to 6 (chroma to 3).
        let i420 = [
            1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0, //
            100, 110, 0, //
            200, 210, 0,
        ];
        let picture = from_i420(&i420, 4, 2, 6).expect("I420");
        assert_eq!(picture.y, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(picture.u, vec![100, 110]);
        assert_eq!(picture.v, vec![200, 210]);
        assert!(from_i420(&i420[..16], 4, 2, 6).is_none());

        // RGB: white and black, in studio range.
        let mut rgb = vec![255u8; 4 * 2 * 3];
        rgb[..6].fill(0);
        let picture = from_rgb(&rgb, 4, 2, 12).expect("RGB");
        assert!(picture.check().is_ok());
        assert!(picture.y[0] <= 17 && picture.y[3] >= 234, "{:?}", picture.y);

        // Odd sizes lose their last row and column, never panic.
        let odd = from_rgb(&[128u8; 5 * 3 * 3], 5, 3, 15).expect("RGB");
        assert_eq!((odd.width, odd.height), (4, 2));
        assert!(from_gray(&[0u8; 9], 3, 3, 3).is_some_and(|p| p.check().is_ok()));
        assert!(from_yuyv(&[], 0, 0, 0).is_none());
        assert!(from_mjpeg(b"not a jpeg").is_none());
        // A stride shorter than a row is refused.
        assert!(from_yuyv(&yuyv, 4, 2, 4).is_none());
        // A size past what a picture may be is refused before reading.
        assert!(from_gray(&[0u8; 16], 8192, 2, 8192).is_none());
    }

    /// A JPEG frame decodes, as a webcam's MJPEG would.
    #[test]
    fn a_jpeg_frame_decodes() {
        // A 16×8 red JPEG (baseline, 4:2:0), made once with ImageMagick.
        const RED: &[u8] = include_bytes!("fixtures/red-16x8.jpg");
        let picture = from_mjpeg(RED).expect("decoded");
        assert_eq!((picture.width, picture.height), (16, 8));
        // Red: Cr high, Cb low.
        assert!(picture.v[0] > 200, "{}", picture.v[0]);
        assert!(picture.u[0] < 120, "{}", picture.u[0]);
    }
}
