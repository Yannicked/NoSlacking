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

/// Translates a sentence with named holes, such as "Signed in to {name}.",
/// and fills each `{key}` from `args`. The whole sentence goes through the
/// catalog, so a translation can put the words in its own order.
pub fn tf(source: &'static str, args: &[(&str, &str)]) -> String {
    fill(&t(source), args)
}

/// Replaces each `{key}` in `pattern` with its value from `args`, in one
/// pass, so braces inside a value (a channel name, an error) stay as they
/// are. Unknown holes are kept, which shows a broken translation instead of
/// hiding it.
pub fn fill(pattern: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let found = after.find('}').and_then(|close| {
            let key = &after[..close];
            args.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value, close))
        });
        match found {
            Some((value, close)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holes_are_filled_once_and_by_name() {
        assert_eq!(
            fill("{who} said {what}", &[("what", "hi"), ("who", "Ann")]),
            "Ann said hi"
        );
        // A value with braces is not filled again.
        assert_eq!(
            fill("Message #{name}", &[("name", "{name}")]),
            "Message #{name}"
        );
        assert_eq!(fill("{unknown} and {", &[]), "{unknown} and {");
    }
}
