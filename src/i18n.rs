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

    /// The messages `code` asks `t`, `tf` and `tn` for: the first string
    /// literal of each call. rustfmt moves a long literal to the next line,
    /// so any whitespace may sit between the name, the `(` and the `"`.
    /// A path in front (`crate::i18n::t`) ends in `:`, which is no part of
    /// a name, so those calls count too; `format` or `at` do not.
    fn messages_in(code: &str) -> Vec<String> {
        // A Windows checkout may end the lines with CRLF, which would keep
        // a `\` line continuation's break in the text.
        let code = &code.replace("\r\n", "\n");
        let mut out = Vec::new();
        for name in ["t", "tf", "tn"] {
            for (at, _) in code.match_indices(name) {
                let before = code[..at].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let Some(args) = code[at + name.len()..].trim_start().strip_prefix('(') else {
                    continue;
                };
                // Only a literal: `t(source: &str)` is the definition.
                let args = args.trim_start();
                if args.starts_with('"')
                    && let Some((value, _)) = literal(args)
                {
                    out.push(value);
                }
            }
        }
        out
    }

    #[test]
    fn a_windows_checkout_reads_like_any_other() {
        let lf = "t(\"one \\\n     two\")";
        assert_eq!(messages_in(lf), ["one two"]);
        assert_eq!(messages_in(&lf.replace('\n', "\r\n")), ["one two"]);
    }

    #[test]
    fn the_scan_finds_calls_however_they_are_wrapped() {
        let code = r#"
            ui.label(t("One line"));
            let text = tf(
                "Wrapped {name}",
                &[("name", name)],
            );
            crate::i18n::t(
                "By path",
            );
            i18n::tn("{count} reply", "{count} replies", n);
            format!("not {this}");
            at("nor this");
            t(variable);
            pub fn t(source: &str) -> Cow<str> { gettext("not a message") }
        "#;
        let mut found = messages_in(code);
        found.sort();
        assert_eq!(
            found,
            ["By path", "One line", "Wrapped {name}", "{count} reply"]
        );
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
                out.extend(messages_in(code));
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

    /// Every Rust file under `src/`, as written, with `\` line
    /// continuations joined so a wrapped literal reads as one.
    fn source_text() -> String {
        fn walk(dir: &std::path::Path, out: &mut String) {
            for entry in std::fs::read_dir(dir).expect("read src").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push_str(&std::fs::read_to_string(&path).expect("read file"));
                }
            }
        }
        let mut text = String::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut text,
        );
        let text = text.replace("\r\n", "\n");
        let mut joined = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(at) = rest.find("\\\n") {
            joined.push_str(&rest[..at]);
            rest = rest[at + 2..].trim_start();
        }
        joined.push_str(rest);
        joined
    }

    #[test]
    fn every_dutch_translation_is_still_asked_for() {
        // A message the code no longer asks for lingers in the catalog
        // unnoticed; this finds it. Both escape `"`, `\` and newlines the
        // same way, so the msgid is searched for as written.
        let code = source_text();
        let po = include_str!("../assets/i18n/nl.po").replace("\r\n", "\n");
        let stale: Vec<&str> = po
            .lines()
            .filter_map(|line| {
                line.strip_prefix("msgid ")
                    .or_else(|| line.strip_prefix("msgid_plural "))
            })
            .filter(|quoted| *quoted != "\"\"" && !code.contains(quoted))
            .collect();
        assert!(stale.is_empty(), "no longer in the code: {stale:#?}");
    }

    /// `gettext` wants a `'static` message, as the interface's literals are.
    fn leak(message: &str) -> &'static str {
        Box::leak(message.to_owned().into_boxed_str())
    }

    fn in_catalog(message: &str) -> bool {
        // A Windows checkout may end the lines with CRLF.
        let po = include_str!("../assets/i18n/nl.po").replace("\r\n", "\n");
        let escaped = message
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        po.contains(&format!("msgid \"{escaped}\"\n"))
    }

    /// The check `build.rs` runs on every catalog. Included rather than
    /// declared with `#[path]`, which inside `tests` would look under a
    /// `src/i18n/tests/` folder that does not exist.
    mod po {
        include!("../build/po.rs");
    }

    #[test]
    fn a_comment_glued_to_an_entry_is_caught() {
        let glued = "msgid \"a\"\nmsgstr \"b\"\n# About c.\nmsgid \"c\"\nmsgstr \"d\"\n";
        assert_eq!(po::glued_comments(glued), [3]);
        // A blank line, or a comment under a comment, is the right shape.
        let fine = "# Header\n#, fuzzy\nmsgid \"a\"\nmsgstr \"b\"\n\n# About c.\nmsgid \"c\"\n";
        assert!(po::glued_comments(fine).is_empty());
        // Trailing spaces still count as a blank line, and CRLF as a break.
        assert!(po::glued_comments("msgstr \"b\"\r\n  \r\n# c\r\n").is_empty());
        assert_eq!(po::glued_comments("msgstr \"b\"\r\n# c\r\n"), [2]);
        let real = include_str!("../assets/i18n/nl.po");
        assert_eq!(po::glued_comments(real), Vec::<usize>::new());
    }
}
