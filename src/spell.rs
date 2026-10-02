//! Spell checking for the composer, with the Hunspell dictionaries already
//! on the computer and spellbook, a pure-Rust reader of them.
//!
//! Dictionaries are `<tag>.aff` and `<tag>.dic` pairs (`en_US`, `nl_NL`)
//! found where desktops keep them: `/usr/share/hunspell` and friends on
//! Linux, `~/Library/Spelling` on macOS, and on every platform the
//! `dictionaries` folder in NoSlacking's config directory, which is the
//! only place on Windows. Loading one takes a moment, so it happens on a
//! thread of its own; until then nothing is marked.
//!
//! Only plain words are checked: not links, code, mentions, channels,
//! emoji codes, words with digits, ALL-CAPS abbreviations or camelCase
//! names, none of which a dictionary knows.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// The spell-checking setting.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpellSettings {
    /// Whether misspelt words are marked.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// The dictionary, by tag (`en_US`); `None` picks one for the
    /// desktop's language.
    #[serde(default)]
    pub language: Option<String>,
}

fn enabled() -> bool {
    true
}

impl Default for SpellSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            language: None,
        }
    }
}

/// A dictionary on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Its tag, the file name without `.dic`: `en_US`.
    pub tag: String,
    /// The affix rules.
    pub aff: PathBuf,
    /// The word list.
    pub dic: PathBuf,
}

/// The folders dictionaries are looked for in, most specific first.
fn folders(config_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    if let Some(config) = config_dir {
        folders.push(config.join("dictionaries"));
    }
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    if cfg!(target_os = "macos") {
        if let Some(home) = &home {
            folders.push(home.join("Library/Spelling"));
        }
        folders.push(PathBuf::from("/Library/Spelling"));
    } else if cfg!(not(windows)) {
        if let Some(home) = &home {
            folders.push(home.join(".local/share/hunspell"));
        }
        for dir in [
            "/app/share/hunspell",
            "/usr/local/share/hunspell",
            "/usr/share/hunspell",
            "/usr/share/myspell",
            "/usr/share/myspell/dicts",
        ] {
            folders.push(PathBuf::from(dir));
        }
    }
    folders
}

/// Every dictionary in `folders`, by tag; the first folder with a tag
/// wins.
fn scan(folders: &[PathBuf]) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(folder) else {
            continue;
        };
        let mut here: Vec<Found> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let dic = entry.path();
                if dic.extension()? != "dic" {
                    return None;
                }
                let tag = dic.file_stem()?.to_str()?.to_owned();
                // Hyphenation and thesaurus files share the folder.
                if tag.starts_with("hyph_") || tag.starts_with("th_") {
                    return None;
                }
                let aff = dic.with_extension("aff");
                aff.is_file().then_some(Found { tag, aff, dic })
            })
            .filter(|new| found.iter().all(|old| old.tag != new.tag))
            .collect();
        found.append(&mut here);
    }
    found.sort_by(|a, b| a.tag.cmp(&b.tag));
    found
}

/// The dictionaries on this computer, found once.
pub fn available(config_dir: &Path) -> &'static [Found] {
    static FOUND: OnceLock<Vec<Found>> = OnceLock::new();
    FOUND.get_or_init(|| scan(&folders(Some(config_dir))))
}

/// The dictionary to use for `wanted`, or for the desktop's language.
fn choose<'a>(found: &'a [Found], wanted: Option<&str>, desktop: &str) -> Option<&'a Found> {
    if let Some(wanted) = wanted {
        return found.iter().find(|f| f.tag == wanted);
    }
    // `en_US.UTF-8` → `en_US`, then `en`.
    let desktop = desktop
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .replace('-', "_");
    let language = desktop.split('_').next().unwrap_or("");
    found
        .iter()
        .find(|f| !desktop.is_empty() && f.tag == desktop)
        .or_else(|| {
            found
                .iter()
                .find(|f| !language.is_empty() && f.tag.split(['_', '-']).next() == Some(language))
        })
        .or_else(|| found.iter().find(|f| f.tag == "en_US"))
}

/// The desktop's language, as the environment says it.
fn desktop_language() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty() && value != "C" && value != "POSIX")
        .unwrap_or_default()
}

/// A dictionary file's text: UTF-8, or else Latin-1, which older
/// dictionaries use (their `SET ISO8859-1`).
fn decode(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| error.into_bytes().into_iter().map(char::from).collect())
}

struct Loaded {
    tag: String,
    dictionary: spellbook::Dictionary,
    /// Words checked already: typing re-checks the whole draft every frame.
    known: Mutex<HashMap<String, bool>>,
}

/// The dictionary in use, if any has loaded.
static CURRENT: RwLock<Option<Arc<Loaded>>> = RwLock::new(None);
/// What [`configure`] was last asked for, so a load that finishes after a
/// newer choice is dropped.
static WANTED: Mutex<Option<String>> = Mutex::new(None);

/// Uses `settings` from now on: loads the chosen dictionary on a thread of
/// its own, or stops checking.
pub fn configure(settings: &SpellSettings, config_dir: &Path) {
    let found = if settings.enabled {
        choose(
            available(config_dir),
            settings.language.as_deref(),
            &desktop_language(),
        )
        .cloned()
    } else {
        None
    };
    let tag = found.as_ref().map(|f| f.tag.clone());
    *lock(&WANTED) = tag.clone();
    let current = read_current().map(|loaded| loaded.tag.clone());
    if current == tag {
        return;
    }
    *CURRENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    let Some(found) = found else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("noslacking-spelling".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let (Ok(aff), Ok(dic)) = (std::fs::read(&found.aff), std::fs::read(&found.dic)) else {
                log::warn!("could not read the {} dictionary", found.tag);
                return;
            };
            match spellbook::Dictionary::new(&decode(aff), &decode(dic)) {
                Ok(dictionary) => {
                    if lock(&WANTED).as_deref() != Some(found.tag.as_str()) {
                        return;
                    }
                    log::info!(
                        "spell checking in {} ({:.0} ms)",
                        found.tag,
                        started.elapsed().as_secs_f32() * 1e3
                    );
                    *CURRENT
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(Arc::new(Loaded {
                            tag: found.tag,
                            dictionary,
                            known: Mutex::new(HashMap::new()),
                        }));
                }
                Err(error) => log::warn!("the {} dictionary is unusable: {error}", found.tag),
            }
        });
    if let Err(error) = spawned {
        log::warn!("could not start the spelling thread: {error}");
    }
}

fn read_current() -> Option<Arc<Loaded>> {
    CURRENT.read().ok().and_then(|current| current.clone())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How many checked words are remembered before the memory starts over.
const KNOWN_MAX: usize = 20_000;

/// The byte ranges of the misspelt words in `text`; none while no
/// dictionary is loaded.
pub fn misspelt(text: &str) -> Vec<Range<usize>> {
    let Some(loaded) = read_current() else {
        return Vec::new();
    };
    let mut known = lock(&loaded.known);
    if known.len() > KNOWN_MAX {
        known.clear();
    }
    words(text)
        .into_iter()
        .filter(|range| {
            let word = &text[range.clone()];
            !*known
                .entry(word.to_owned())
                .or_insert_with(|| loaded.dictionary.check(word))
        })
        .collect()
}

/// Spellings to offer for `word`, best first.
pub fn suggestions(word: &str) -> Vec<String> {
    let Some(loaded) = read_current() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    loaded.dictionary.suggest(word, &mut out);
    out.truncate(6);
    out
}

/// The byte ranges of the words in `text` worth checking.
pub fn words(text: &str) -> Vec<Range<usize>> {
    let skipped = skipped_spans(text);
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        // An apostrophe inside a word (don't, l'homme) belongs to it.
        let inner_apostrophe = (c == '\'' || c == '\u{2019}')
            && start.is_some()
            && chars.peek().is_some_and(|(_, next)| next.is_alphabetic());
        if c.is_alphabetic() || inner_apostrophe {
            start.get_or_insert(index);
            continue;
        }
        if let Some(begin) = start.take() {
            push_word(text, begin..index, c, &skipped, &mut out);
        }
    }
    if let Some(begin) = start {
        push_word(text, begin..text.len(), ' ', &skipped, &mut out);
    }
    out
}

fn push_word(
    text: &str,
    range: Range<usize>,
    after: char,
    skipped: &[Range<usize>],
    out: &mut Vec<Range<usize>>,
) {
    let word = &text[range.clone()];
    let before = text[..range.start].chars().next_back();
    let joined = |c: Option<char>| {
        c.is_some_and(|c| {
            c.is_ascii_digit() || matches!(c, '_' | '@' | '#' | ':' | '/' | '\\' | '.' | '-' | '=')
        })
    };
    let letters = word.chars().filter(|c| c.is_alphabetic()).count();
    let capitals = word.chars().filter(|c| c.is_uppercase()).count();
    let camel = word.chars().skip(1).any(char::is_uppercase) && capitals < letters;
    // `.` and `-` after a word are punctuation, but before one they join
    // it to something (a file name, a flag) that is not a word.
    let tied_after = after.is_ascii_digit() || matches!(after, '_' | '@' | '/' | '\\' | '=');
    if letters < 2
        || capitals == letters
        || camel
        || joined(before)
        || tied_after
        || skipped
            .iter()
            .any(|span| span.start < range.end && range.start < span.end)
    {
        return;
    }
    out.push(range);
}

/// Stretches never checked: code (`` `x` `` and ``` blocks), links and
/// anything else with a scheme, and Slack's `<…>` references.
fn skipped_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut fenced = 0;
    while let Some(open) = text[fenced..].find("```") {
        let begin = fenced + open;
        let end = text[begin + 3..]
            .find("```")
            .map_or(text.len(), |close| begin + 3 + close + 3);
        spans.push(begin..end);
        fenced = end;
        if fenced >= text.len() {
            break;
        }
    }
    let mut index = 0;
    while let Some(offset) = text[index..].find('`') {
        let begin = index + offset;
        if spans.iter().any(|span| span.contains(&begin)) {
            index = begin + 1;
            continue;
        }
        let end = text[begin + 1..]
            .find(['`', '\n'])
            .map_or(text.len(), |close| begin + 1 + close + 1);
        spans.push(begin..end.min(text.len()));
        index = end.min(text.len());
        if index >= text.len() {
            break;
        }
    }
    let mut start = 0;
    for token in text.split_inclusive(char::is_whitespace) {
        let trimmed = token.trim_end();
        if trimmed.contains("://") || trimmed.starts_with("www.") || trimmed.starts_with('<') {
            spans.push(start..start + trimmed.len());
        }
        start += token.len();
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked(text: &str) -> Vec<&str> {
        words(text).into_iter().map(|range| &text[range]).collect()
    }

    #[test]
    fn only_plain_words_are_checked() {
        assert_eq!(
            checked("Thsi is a tset, don't worry."),
            ["Thsi", "is", "tset", "don't", "worry"]
        );
        assert_eq!(
            checked("see https://exmaple.com/pth and www.foo.bar now"),
            ["see", "and", "now"]
        );
        assert_eq!(checked("ping @alcie in #genral :tada:"), ["ping", "in"]);
        assert_eq!(
            checked("run `cargo biuld` then\n```\nfn mian()\n```\ndone"),
            ["run", "then", "done"]
        );
        assert_eq!(
            checked("NASA and HTTPS, iPhone and parseJson"),
            ["and", "and"]
        );
        assert_eq!(checked("v2 abc123 file.rs x"), ["file"]);
        assert_eq!(checked("<@U123> hi there"), ["hi", "there"]);
        assert_eq!(checked("naïve café"), ["naïve", "café"]);
    }

    #[test]
    fn dictionaries_are_found_by_tag_and_language() {
        let found = |tag: &str| Found {
            tag: tag.into(),
            aff: PathBuf::from(format!("{tag}.aff")),
            dic: PathBuf::from(format!("{tag}.dic")),
        };
        let all = [found("de_DE"), found("en_GB"), found("en_US"), found("nl")];
        let pick = |wanted: Option<&str>, desktop: &str| {
            choose(&all, wanted, desktop).map(|f| f.tag.as_str())
        };
        assert_eq!(pick(Some("en_GB"), "nl_NL.UTF-8"), Some("en_GB"));
        assert_eq!(pick(Some("fr_FR"), "nl_NL.UTF-8"), None);
        assert_eq!(pick(None, "nl_NL.UTF-8"), Some("nl"));
        assert_eq!(pick(None, "en_GB.UTF-8"), Some("en_GB"));
        assert_eq!(pick(None, "de-AT"), Some("de_DE"));
        assert_eq!(pick(None, ""), Some("en_US"));
        assert_eq!(pick(None, "ja_JP.UTF-8"), Some("en_US"));
    }

    #[test]
    fn old_dictionaries_in_latin_1_still_read() {
        assert_eq!(decode(b"caf\xe9".to_vec()), "café");
        assert_eq!(decode("café".as_bytes().to_vec()), "café");
    }

    #[test]
    fn a_tiny_dictionary_checks_and_suggests() {
        let aff = "SET UTF-8\nTRY esianrtolcdugmphbyfvkwz\nSFX S Y 1\nSFX S 0 s .\n";
        let dic = "3\nhello\nworld/S\nthere\n";
        let dictionary = spellbook::Dictionary::new(aff, dic).expect("parses");
        assert!(dictionary.check("hello") && dictionary.check("worlds"));
        assert!(!dictionary.check("helo"));
        let mut out = Vec::new();
        dictionary.suggest("helo", &mut out);
        assert!(out.iter().any(|s| s == "hello"), "{out:?}");
    }

    #[test]
    fn settings_default_to_on_and_automatic() {
        let settings: SpellSettings = serde_json::from_str("{}").expect("parses");
        assert_eq!(settings, SpellSettings::default());
        assert!(settings.enabled && settings.language.is_none());
    }
}
