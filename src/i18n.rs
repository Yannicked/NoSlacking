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

    /// Reads the Rust string literal starting at `source[0] == '"'`,
    /// returning its value and the rest after it.
    fn literal(source: &str) -> Option<(String, &str)> {
        let mut value = String::new();
        let mut chars = source.char_indices().skip(1);
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => return Some((value, &source[at + 1..])),
                '\\' => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    // A line continuation: the break and the indent go.
                    '\n' => {
                        let mut rest = chars.clone();
                        while rest.clone().next().is_some_and(|(_, c)| c.is_whitespace()) {
                            rest.next();
                        }
                        chars = rest;
                    }
                    other => value.push(other),
                },
                c => value.push(c),
            }
        }
        None
    }

    /// Every message the interface asks `t`, `tf` and `tn` for, outside
    /// tests.
    fn interface_messages() -> Vec<String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read src").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read file");
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                for call in ["t(\"", "tf(\"", "tn(\""] {
                    for (at, _) in code.match_indices(call) {
                        let before = code[..at].chars().next_back();
                        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                            continue;
                        }
                        let start = at + call.len() - 1;
                        if let Some((value, _)) = literal(&code[start..]) {
                            out.push(value);
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out.sort();
        out.dedup();
        out
    }

    /// The holes a message has, such as `{name}`.
    fn holes(text: &str) -> std::collections::BTreeSet<String> {
        text.split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(hole, _)| hole.to_owned()))
            .collect()
    }

    #[test]
    fn every_interface_message_has_a_dutch_translation() {
        let messages = interface_messages();
        assert!(messages.len() > 100, "the scan found {}", messages.len());
        let missing: Vec<&String> = messages
            .iter()
            .filter(|m| {
                let dutch = fastframe_i18n::gettext(Locale::Dutch, leak(m));
                // A translation that is the English word itself (Live, App)
                // still has to be in the catalog.
                dutch == m.as_str() && !in_catalog(m)
            })
            .collect();
        assert!(missing.is_empty(), "not in nl.po: {missing:#?}");
        let broken: Vec<&String> = messages
            .iter()
            .filter(|m| holes(&fastframe_i18n::gettext(Locale::Dutch, leak(m))) != holes(m))
            .collect();
        assert!(
            broken.is_empty(),
            "placeholders differ in nl.po: {broken:#?}"
        );
    }

    /// `gettext` wants a `'static` message, as the interface's literals are.
    fn leak(message: &str) -> &'static str {
        Box::leak(message.to_owned().into_boxed_str())
    }

    fn in_catalog(message: &str) -> bool {
        let po = include_str!("../assets/i18n/nl.po");
        let escaped = message
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        po.contains(&format!("msgid \"{escaped}\"\n"))
    }
}
