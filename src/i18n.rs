//! Interface languages.
//!
//! Catalogs are compiled from `assets/i18n/*.po` by `build.rs`. English is
//! the source language and the fallback for every message a catalog has not
//! translated. The interface asks [`t`] for each string; the language is
//! process-wide, so views need not carry it around.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU8, Ordering};

include!(concat!(env!("OUT_DIR"), "/catalogs.rs"));

/// The languages NoSlacking ships.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Locale {
    #[default]
    English,
    Dutch,
}

impl Locale {
    pub const ALL: [Self; 2] = [Self::English, Self::Dutch];

    /// The language's own name, for the picker.
    pub fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Dutch => "Nederlands",
        }
    }

    /// The system's preferred language, when NoSlacking ships it.
    pub fn detect() -> Self {
        fastframe_i18n::detect(|tag| match tag.language.as_str() {
            "nl" => Some(Self::Dutch),
            "en" => Some(Self::English),
            _ => None,
        })
        .unwrap_or_default()
    }

    fn index(self) -> u8 {
        match self {
            Self::English => 0,
            Self::Dutch => 1,
        }
    }
}

impl fastframe_i18n::Locale for Locale {
    fn catalog(self) -> Option<&'static dyn fastframe_i18n::Translator> {
        match self {
            Self::English => None,
            Self::Dutch => Some(&nl::Translator),
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Switches the interface language.
pub fn set_locale(locale: Locale) {
    CURRENT.store(locale.index(), Ordering::Relaxed);
}

/// The interface language.
pub fn locale() -> Locale {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Locale::Dutch,
        _ => Locale::English,
    }
}

/// Translates an interface string.
pub fn t(source: &'static str) -> Cow<'static, str> {
    fastframe_i18n::gettext(locale(), source)
}

/// Translates a phrase that depends on a count, such as "{count} replies".
/// The caller replaces `{count}`.
pub fn tn(singular: &'static str, plural: &'static str, count: u32) -> String {
    fastframe_i18n::ngettext(locale(), singular, plural, count)
        .replace("{count}", &count.to_string())
}
