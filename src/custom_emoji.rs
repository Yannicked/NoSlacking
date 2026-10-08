//! Adding a custom emoji to a workspace: the "Add emoji" dialog's state
//! and the checks Slack would make, so a name or picture it would refuse
//! is caught before anything is sent.
//!
//! Slack has no public method for this. Its web client posts `emoji.add`
//! with the browser session's token and cookie, so only browser-session
//! sign-ins offer it (see `slack::client::Client::add_emoji`).

use std::sync::Arc;

use crate::emoji::EmojiSet;

/// The largest picture Slack takes for an emoji: "Square images under
/// 128KB […] work best" (Slack's help, "Add custom emoji and aliases to
/// your workspace"). Larger ones are refused with `error_too_big`.
pub const MAX_BYTES: usize = 128 * 1024;

/// The longest name taken here. Slack does not document a limit; this is
/// well past any name anyone types, and keeps a pasted paragraph out.
pub const MAX_NAME: usize = 100;

/// Why a name cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameProblem {
    Empty,
    TooLong,
    /// Something other than lowercase letters, digits, `-` and `_`.
    Characters,
    /// A standard emoji, or one of the workspace's own, has it.
    Taken,
}

impl NameProblem {
    /// The problem in words, in the interface's language.
    pub fn message(self) -> std::borrow::Cow<'static, str> {
        use crate::i18n::t;
        match self {
            Self::Empty => t("Give the emoji a name."),
            Self::TooLong => t("That name is too long."),
            Self::Characters => t("Use lowercase letters, numbers, dashes and underscores only."),
            Self::Taken => t("An emoji by that name exists already."),
        }
    }
}

/// The name as it would be sent: what was typed, without the colons
/// around it that people type out of habit, or spaces.
pub fn clean_name(typed: &str) -> &str {
    let name = typed.trim();
    let name = name.strip_prefix(':').unwrap_or(name);
    name.strip_suffix(':').unwrap_or(name)
}

/// Checks a name as Slack would: lowercase letters, digits, `-` and `_`
/// only, and not a name a standard emoji or one of the workspace's custom
/// emoji (or aliases) already has.
pub fn check_name(typed: &str, custom: &EmojiSet) -> Result<String, NameProblem> {
    let name = clean_name(typed);
    if name.is_empty() {
        return Err(NameProblem::Empty);
    }
    if name.chars().count() > MAX_NAME {
        return Err(NameProblem::TooLong);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
    {
        return Err(NameProblem::Characters);
    }
    if custom.contains(name) || crate::emoji::unicode(name, None).is_some() {
        return Err(NameProblem::Taken);
    }
    Ok(name.to_owned())
}

/// The kinds of picture Slack takes for an emoji.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
}

impl Format {
    /// Its media type, for the upload.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
        }
    }
}

/// What a picked picture is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: Format,
    pub width: u32,
    pub height: u32,
}

impl ImageInfo {
    /// Whether it is square, which Slack advises; others are stretched
    /// or padded into a square by Slack.
    pub fn square(&self) -> bool {
        self.width == self.height
    }
}

/// Why a picture cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProblem {
    /// Over [`MAX_BYTES`].
    TooLarge,
    /// Not a PNG, JPEG or GIF, or not readable as one.
    NotAnImage,
}

impl ImageProblem {
    /// The problem in words, in the interface's language.
    pub fn message(self) -> std::borrow::Cow<'static, str> {
        use crate::i18n::t;
        match self {
            Self::TooLarge => t("The picture is over 128 KB, which Slack does not take."),
            Self::NotAnImage => t("Pick a PNG, JPEG or GIF picture."),
        }
    }
}

/// Checks a picture's size and kind by its bytes (not its file name), and
/// reads its dimensions.
fn check_image(bytes: &[u8]) -> Result<ImageInfo, ImageProblem> {
    if bytes.len() > MAX_BYTES {
        return Err(ImageProblem::TooLarge);
    }
    let format = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => Format::Png,
        Ok(image::ImageFormat::Jpeg) => Format::Jpeg,
        Ok(image::ImageFormat::Gif) => Format::Gif,
        _ => return Err(ImageProblem::NotAnImage),
    };
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ImageProblem::NotAnImage)?
        .into_dimensions()
        .map_err(|_| ImageProblem::NotAnImage)?;
    Ok(ImageInfo {
        format,
        width,
        height,
    })
}

/// A picture picked for a new emoji.
#[derive(Clone, Debug)]
pub struct Picked {
    pub file_name: String,
    pub bytes: Arc<[u8]>,
    /// The image loader's URI for its preview, unique to this pick.
    pub uri: String,
    pub check: Result<ImageInfo, ImageProblem>,
}

impl Picked {
    /// A picked file's bytes, checked; `pick` numbers the picks so each
    /// preview has a URI of its own.
    pub fn new(file_name: String, bytes: Vec<u8>, pick: u64) -> Self {
        let check = check_image(&bytes);
        Self {
            uri: format!("bytes://noslacking-new-emoji/{pick}/{file_name}"),
            file_name,
            bytes: bytes.into(),
            check,
        }
    }
}

/// The "Add emoji" dialog.
#[derive(Clone, Debug, Default)]
pub struct Dialog {
    /// The workspace the emoji goes to.
    pub team: String,
    pub name: String,
    pub picked: Option<Picked>,
    /// Waiting for a file picker or for Slack.
    pub busy: bool,
    /// Why Slack refused the last try, worded.
    pub error: Option<String>,
}

impl Dialog {
    /// The name and picture to send, if both pass the checks.
    pub fn ready(&self, custom: &EmojiSet) -> Option<(String, &Picked, ImageInfo)> {
        let name = check_name(&self.name, custom).ok()?;
        let picked = self.picked.as_ref()?;
        let info = picked.check.ok()?;
        Some((name, picked, info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom() -> EmojiSet {
        EmojiSet::new(
            [
                ("partyparrot".to_owned(), "https://x.y/p.gif".to_owned()),
                ("parrot".to_owned(), "alias:partyparrot".to_owned()),
            ]
            .into(),
        )
    }

    #[test]
    fn emoji_names_follow_slacks_rules() {
        let set = custom();
        assert_eq!(check_name("shipit", &set).as_deref(), Ok("shipit"));
        assert_eq!(
            check_name(" :ship-it_2: ", &set).as_deref(),
            Ok("ship-it_2")
        );
        assert_eq!(check_name("", &set), Err(NameProblem::Empty));
        assert_eq!(check_name("::", &set), Err(NameProblem::Empty));
        assert_eq!(check_name("ShipIt", &set), Err(NameProblem::Characters));
        assert_eq!(check_name("ship it", &set), Err(NameProblem::Characters));
        assert_eq!(check_name("ship.it", &set), Err(NameProblem::Characters));
        assert_eq!(check_name("café", &set), Err(NameProblem::Characters));
        assert_eq!(
            check_name(&"a".repeat(MAX_NAME + 1), &set),
            Err(NameProblem::TooLong)
        );
        assert!(check_name(&"a".repeat(MAX_NAME), &set).is_ok());
    }

    #[test]
    fn a_taken_emoji_name_is_refused() {
        let set = custom();
        assert_eq!(check_name("partyparrot", &set), Err(NameProblem::Taken));
        assert_eq!(
            check_name("parrot", &set),
            Err(NameProblem::Taken),
            "an alias"
        );
        assert_eq!(
            check_name("tada", &set),
            Err(NameProblem::Taken),
            "standard"
        );
        assert_eq!(
            check_name("thumbsup_all", &set),
            Err(NameProblem::Taken),
            "Slack's own alias"
        );
        assert_eq!(
            check_name("large_green_circle", &set),
            Err(NameProblem::Taken)
        );
    }

    /// A PNG of `w` by `h` pixels.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::new(w, h)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encodes");
        out
    }

    #[test]
    fn pictures_are_checked_by_their_bytes() {
        let info = check_image(&png(64, 64)).expect("a small PNG");
        assert_eq!(info.format, Format::Png);
        assert_eq!(info.format.mime(), "image/png");
        assert!(info.square());
        let wide = check_image(&png(128, 64)).expect("wide is allowed");
        assert!(!wide.square());
        assert_eq!(
            check_image(b"%PDF-1.7 not a picture"),
            Err(ImageProblem::NotAnImage)
        );
        assert_eq!(check_image(&[]), Err(ImageProblem::NotAnImage));
        // The size counts before anything is decoded.
        let mut big = png(8, 8);
        big.resize(MAX_BYTES + 1, 0);
        assert_eq!(check_image(&big), Err(ImageProblem::TooLarge));
        // A truncated file says it is a PNG but cannot be read.
        assert_eq!(check_image(&png(8, 8)[..20]), Err(ImageProblem::NotAnImage));
    }

    #[test]
    fn the_dialog_is_ready_only_when_both_pass() {
        let set = custom();
        let mut dialog = Dialog {
            name: "shipit".into(),
            ..Dialog::default()
        };
        assert!(dialog.ready(&set).is_none(), "no picture yet");
        dialog.picked = Some(Picked::new("ship.png".into(), png(32, 32), 1));
        assert!(dialog.ready(&set).is_some());
        dialog.name = "tada".into();
        assert!(dialog.ready(&set).is_none(), "a taken name");
        dialog.name = "shipit".into();
        dialog.picked = Some(Picked::new("notes.txt".into(), b"hello".to_vec(), 2));
        assert!(dialog.ready(&set).is_none(), "not a picture");
    }
}
