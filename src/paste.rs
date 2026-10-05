//! Images and files pasted into the composer.
//!
//! egui hands the app only the clipboard's text, so Ctrl+V on a copied
//! screenshot does nothing in the text field. This reads the image itself
//! (through arboard, which egui-winit already uses for text) and writes it
//! out as a PNG the upload can stream like any other file.
//!
//! Files copied in a file manager arrive as text (`file:///…` or plain
//! paths), which [`pasted_files`] recognises so they upload instead of
//! landing in the message. On Wayland, where dropping files on the window
//! does not reach the app, this is how a file gets in without the picker.

use std::path::{Path, PathBuf};

/// The files pasted text names, when it names nothing but files that
/// exist: one per line, as `file://` addresses (a file manager's copy) or
/// absolute paths. GNOME's form starts with a `copy` or `cut` line, and
/// `text/uri-list` allows `#` comments. Anything else is ordinary text.
pub fn pasted_files(text: &str) -> Option<Vec<PathBuf>> {
    let mut files = Vec::new();
    for (i, line) in text.lines().map(str::trim).enumerate() {
        if line.is_empty() || line.starts_with('#') || (i == 0 && matches!(line, "copy" | "cut")) {
            continue;
        }
        let path = match line.strip_prefix("file://") {
            // `file:///home/…` or `file://localhost/home/…`, percent-encoded.
            Some(rest) => {
                let rest = rest.strip_prefix("localhost").unwrap_or(rest);
                if !rest.starts_with('/') {
                    return None;
                }
                let decoded = urlencoding::decode(rest).ok()?;
                PathBuf::from(windows_drive(&decoded).unwrap_or(&decoded))
            }
            None => PathBuf::from(line),
        };
        if !path.is_absolute() || !path.is_file() {
            return None;
        }
        files.push(path);
    }
    (!files.is_empty()).then_some(files)
}

/// A Windows file URI's path starts with a slash before its drive
/// (`/C:/Users/…`), which is no path there: the path without that slash.
/// Elsewhere the slash belongs to the path.
fn windows_drive(path: &str) -> Option<&str> {
    let rest = path.strip_prefix('/')?;
    let bytes = rest.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    (cfg!(windows) && drive).then_some(rest)
}

/// What the clipboard holds to upload: the files it lists, or its image as
/// a PNG file in `dir`. Empty when it holds text (egui pasted that already,
/// and copied files come as text: see [`pasted_files`]) or nothing to send.
pub fn clipboard_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    if clipboard.get_text().is_ok_and(|text| !text.is_empty()) {
        return Ok(Vec::new());
    }
    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        return Ok(files.into_iter().filter(|path| path.is_file()).collect());
    }
    clipboard_image(&mut clipboard, dir).map(|image| image.into_iter().collect())
}

/// The clipboard's image as a PNG file in `dir`, or `None` when it has no
/// image.
fn clipboard_image(
    clipboard: &mut arboard::Clipboard,
    dir: &Path,
) -> Result<Option<PathBuf>, String> {
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

/// Decodes `bytes` (a PNG, JPEG, GIF or WebP) to the pixels a clipboard
/// takes, refusing what would decode to far more than it weighs.
pub fn clipboard_pixels(bytes: &[u8]) -> Result<arboard::ImageData<'static>, String> {
    crate::images::check_decoded_size(bytes)?;
    let image = image::load_from_memory(bytes)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    Ok(arboard::ImageData {
        width: image.width() as usize,
        height: image.height() as usize,
        bytes: image.into_raw().into(),
    })
}

/// Puts `pixels` on the clipboard. On X11 and Wayland a clipboard lives
/// only as long as the program offering it, so there this keeps offering
/// it until something else is copied: call it on a thread of its own.
pub fn copy_image(pixels: arboard::ImageData<'static>) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ))]
    {
        use arboard::SetExtLinux as _;
        clipboard
            .set()
            .wait()
            .image(pixels)
            .map_err(|e| e.to_string())
    }
    #[cfg(not(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    )))]
    {
        clipboard.set_image(pixels).map_err(|e| e.to_string())
    }
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
    fn copied_files_are_recognised_and_text_is_left_alone() {
        let dir = TestDir::new("paste-files");
        let a = dir.0.join("a b.png");
        let b = dir.0.join("notes.txt");
        std::fs::write(&a, b"x").expect("written");
        std::fs::write(&b, b"x").expect("written");
        // `file:///home/…`, or `file:///C:/Users/…` on Windows.
        let uri = |p: &Path| {
            let path = p.display().to_string().replace('\\', "/");
            let path = path.strip_prefix('/').unwrap_or(&path).replace(' ', "%20");
            format!("file:///{path}")
        };
        assert_eq!(
            pasted_files(&format!("{}\r\n{}\n", uri(&a), uri(&b))),
            Some(vec![a.clone(), b.clone()])
        );
        assert_eq!(
            pasted_files(&format!("copy\n{}", uri(&a))),
            Some(vec![a.clone()])
        );
        assert_eq!(
            pasted_files(&b.display().to_string()),
            Some(vec![b.clone()])
        );
        // Ordinary text, a missing file, a folder or a relative path stays text.
        assert_eq!(pasted_files("hello world"), None);
        assert_eq!(pasted_files(&format!("{} and more", b.display())), None);
        assert_eq!(pasted_files("file:///no/such/file.png"), None);
        assert_eq!(pasted_files(&dir.0.display().to_string()), None);
        assert_eq!(pasted_files("notes.txt"), None);
        assert_eq!(pasted_files(""), None);
    }

    #[test]
    fn pictures_decode_to_clipboard_pixels() {
        let mut png = Vec::new();
        image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255])
            .expect("pixels")
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encoded");
        let pixels = clipboard_pixels(&png).expect("decoded");
        assert_eq!((pixels.width, pixels.height), (2, 1));
        assert_eq!(&pixels.bytes[..4], &[255, 0, 0, 255]);
        assert!(clipboard_pixels(b"not an image").is_err());
    }

    #[test]
    fn pixels_that_do_not_fit_the_size_are_refused() {
        let dir = TestDir::new("paste-bad");
        assert!(save_png(&dir.0, "x.png", 4, 4, vec![0; 3]).is_err());
    }
}
