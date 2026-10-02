//! Images pasted into the composer.
//!
//! egui hands the app only the clipboard's text, so Ctrl+V on a copied
//! screenshot does nothing in the text field. This reads the image itself
//! (through arboard, which egui-winit already uses for text) and writes it
//! out as a PNG the upload can stream like any other file.

use std::path::{Path, PathBuf};

/// The clipboard's image as a PNG file in `dir`, or `None` when the
/// clipboard holds text (egui pasted that already) or no image at all.
pub fn clipboard_image(dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    if clipboard.get_text().is_ok_and(|text| !text.is_empty()) {
        return Ok(None);
    }
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let name = format!("image-{}.png", jiff::Zoned::now().strftime("%Y%m%d-%H%M%S"));
    save_png(
        dir,
        &name,
        image.width,
        image.height,
        image.bytes.into_owned(),
    )
    .map(Some)
}

/// Writes `rgba` pixels, `width` by `height`, as `dir/name`, numbered
/// (`name-2.png`) when a file of that name is already there: two pastes
/// in one second must not overwrite each other mid-upload.
pub fn save_png(
    dir: &Path,
    name: &str,
    width: usize,
    height: usize,
    rgba: Vec<u8>,
) -> Result<PathBuf, String> {
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err("the image is too large".to_owned());
    };
    let buffer = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| "the image's pixels do not match its size".to_owned())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let stem = name.strip_suffix(".png").unwrap_or(name);
    let mut path = dir.join(format!("{stem}.png"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("{stem}-{n}.png"));
    }
    buffer
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TestDir;

    #[test]
    fn pixels_become_a_png_that_reads_back() {
        let dir = TestDir::new("paste-png");
        let pixels: Vec<u8> = (0..2 * 3).flat_map(|i| [i * 40, 0, 0, 255]).collect();
        let path = save_png(&dir.0, "shot.png", 2, 3, pixels.clone()).expect("saved");
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("shot.png"));
        let back = image::open(&path).expect("decodes").to_rgba8();
        assert_eq!((back.width(), back.height()), (2, 3));
        assert_eq!(back.into_raw(), pixels);
        // A second paste of the same name gets a number.
        let again = save_png(&dir.0, "shot.png", 2, 3, pixels).expect("saved");
        assert_eq!(
            again.file_name().and_then(|n| n.to_str()),
            Some("shot-2.png")
        );
    }

    #[test]
    fn pixels_that_do_not_fit_the_size_are_refused() {
        let dir = TestDir::new("paste-bad");
        assert!(save_png(&dir.0, "x.png", 4, 4, vec![0; 3]).is_err());
    }
}
