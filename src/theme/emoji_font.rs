//! Which colour emoji font NoSlacking carries on each platform, behind the
//! platform's own.
//!
//! - macOS: none. Apple Color Emoji has every emoji the system knows.
//! - Windows: only the flags, a subset of Noto Color Emoji
//!   (`NotoColorEmoji-Flags.ttf`, under a megabyte). Segoe UI Emoji draws
//!   every other emoji but has no country or subdivision flags.
//! - Linux and the other Unixes: all of Noto Color Emoji, about ten
//!   megabytes, with the `bundled-emoji` feature (on by default). Some
//!   desktops (Fedora among them) ship it only as a COLRv1 vector font,
//!   which fastframe-emoji cannot draw. A package that turns the feature
//!   off can install the font as `share/noslacking/NotoColorEmoji.ttf`
//!   instead, which is read at start-up; fastframe-emoji already finds a
//!   colour bitmap font in the system's font directories by itself.

/// The emoji font compiled in for this platform, or found next to the
/// install when nothing is compiled in.
pub fn bundled() -> Option<&'static [u8]> {
    compiled().or_else(installed)
}

#[cfg(target_os = "macos")]
fn compiled() -> Option<&'static [u8]> {
    None
}

#[cfg(windows)]
fn compiled() -> Option<&'static [u8]> {
    Some(include_bytes!(
        "../../assets/fonts/NotoColorEmoji-Flags.ttf"
    ))
}

#[cfg(all(not(any(target_os = "macos", windows)), feature = "bundled-emoji"))]
fn compiled() -> Option<&'static [u8]> {
    Some(include_bytes!("../../assets/fonts/NotoColorEmoji.ttf"))
}

#[cfg(all(not(any(target_os = "macos", windows)), not(feature = "bundled-emoji")))]
fn compiled() -> Option<&'static [u8]> {
    None
}

/// The file name a package installs under `share/noslacking/`.
#[cfg(not(any(target_os = "macos", windows)))]
const INSTALLED_NAME: &str = "NotoColorEmoji.ttf";

/// A colour bitmap emoji font installed with NoSlacking, read whole and
/// kept for the life of the process (fastframe-emoji takes a `'static`
/// font, and the font is needed until the app quits anyway).
#[cfg(not(any(target_os = "macos", windows)))]
fn installed() -> Option<&'static [u8]> {
    let exe = std::env::current_exe().ok();
    let candidates = candidates(
        exe.as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
    );
    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if is_bitmap_font(&bytes) {
            log::info!("colour emoji font from {}", path.display());
            return Some(Box::leak(bytes.into_boxed_slice()));
        }
        log::warn!(
            "{} is not a colour bitmap (CBDT or sbix) font; skipping it",
            path.display()
        );
    }
    None
}

/// The other platforms compile in what they need.
#[cfg(any(target_os = "macos", windows))]
fn installed() -> Option<&'static [u8]> {
    None
}

/// Where a package may have put the font, most specific first: the
/// `share/` next to the binary's `bin/` (the release tarball's layout,
/// wherever it was unpacked), then the XDG data directories.
#[cfg(not(any(target_os = "macos", windows)))]
fn candidates(
    exe: Option<&std::path::Path>,
    data_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    data_dirs: Option<&std::ffi::OsStr>,
) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(prefix) = exe
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::parent)
    {
        roots.push(prefix.join("share"));
    }
    match data_home.filter(|dir| !dir.is_empty()) {
        Some(dir) => roots.push(PathBuf::from(dir)),
        None => roots.extend(home.map(|home| PathBuf::from(home).join(".local/share"))),
    }
    let data_dirs = data_dirs
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("/usr/local/share:/usr/share"));
    roots.extend(std::env::split_paths(data_dirs).filter(|dir| dir.is_absolute()));
    let mut paths: Vec<PathBuf> = Vec::new();
    for root in roots {
        let path = root.join("noslacking").join(INSTALLED_NAME);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Whether an OpenType file has colour bitmaps fastframe-emoji can draw:
/// a `CBDT` or `sbix` table in its table directory. Reading the directory
/// is enough to turn away a COLRv1 or outline font before keeping ten
/// megabytes of it.
#[cfg_attr(
    any(target_os = "macos", windows),
    allow(dead_code, reason = "these platforms read no font file at start-up")
)]
pub fn is_bitmap_font(bytes: &[u8]) -> bool {
    let count = match bytes.get(4..6) {
        Some(&[high, low]) => usize::from(u16::from_be_bytes([high, low])),
        _ => return false,
    };
    (0..count).any(|table| {
        let start = 12 + table * 16;
        matches!(bytes.get(start..start + 4), Some(b"CBDT" | b"sbix"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table directory naming the given tables, without their data.
    fn font_with(tables: &[&[u8; 4]]) -> Vec<u8> {
        let mut bytes = vec![0, 1, 0, 0];
        bytes.extend((tables.len() as u16).to_be_bytes());
        bytes.extend([0; 6]);
        for tag in tables {
            bytes.extend(*tag);
            bytes.extend([0; 12]);
        }
        bytes
    }

    #[test]
    fn only_colour_bitmap_fonts_are_kept() {
        assert!(is_bitmap_font(&font_with(&[b"CBDT", b"CBLC", b"cmap"])));
        assert!(is_bitmap_font(&font_with(&[b"cmap", b"sbix"])));
        assert!(!is_bitmap_font(&font_with(&[b"COLR", b"CPAL", b"glyf"])));
        assert!(!is_bitmap_font(b"not a font"));
        assert!(!is_bitmap_font(&[]));
        // A directory that claims more tables than the file holds.
        let mut short = font_with(&[b"glyf"]);
        short[5] = 200;
        assert!(!is_bitmap_font(&short));
    }

    #[test]
    fn the_flags_subset_is_a_colour_font_with_flags_only() {
        let flags = include_bytes!("../../assets/fonts/NotoColorEmoji-Flags.ttf");
        assert!(is_bitmap_font(flags));
        assert!(flags.len() < 1_000_000, "{} bytes", flags.len());
    }

    #[test]
    fn every_platform_has_what_its_own_font_lacks() {
        let font = compiled();
        if cfg!(target_os = "macos") {
            assert!(font.is_none());
        } else if cfg!(any(windows, feature = "bundled-emoji")) {
            assert!(font.is_some_and(is_bitmap_font));
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn the_font_is_looked_for_next_to_the_install_first() {
        use std::ffi::OsStr;
        use std::path::{Path, PathBuf};
        let found = candidates(
            Some(Path::new("/opt/noslacking/bin/noslacking")),
            None,
            Some(OsStr::new("/home/me")),
            None,
        );
        assert_eq!(
            found,
            [
                "/opt/noslacking/share/noslacking/NotoColorEmoji.ttf",
                "/home/me/.local/share/noslacking/NotoColorEmoji.ttf",
                "/usr/local/share/noslacking/NotoColorEmoji.ttf",
                "/usr/share/noslacking/NotoColorEmoji.ttf",
            ]
            .map(PathBuf::from)
        );
        // XDG_DATA_HOME wins over HOME, relative entries are ignored, and a
        // binary in /usr/bin does not list /usr/share twice.
        let found = candidates(
            Some(Path::new("/usr/bin/noslacking")),
            Some(OsStr::new("/data")),
            Some(OsStr::new("/home/me")),
            Some(OsStr::new("relative:/usr/share")),
        );
        assert_eq!(
            found,
            [
                "/usr/share/noslacking/NotoColorEmoji.ttf",
                "/data/noslacking/NotoColorEmoji.ttf",
            ]
            .map(PathBuf::from)
        );
    }
}
