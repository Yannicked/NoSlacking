//! A small syntax highlighter for code blocks.
//!
//! Slack has no notion of a code block's language, but people write one
//! the way they would on GitHub: ```` ```rust ```` on the fence's first
//! line. [`split_language`] finds such a line, and [`highlight`] cuts the
//! code into runs of keywords, strings, numbers and comments.
//!
//! It is a lexer, not a parser: one pass, no grammar, a list of words per
//! language. That is all a chat message needs, and it costs a few kilobytes
//! where a grammar-based highlighter brings megabytes of syntax definitions.
//! Anything it does not recognise stays plain text, so a wrong guess only
//! loses colour.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// What a run of code is, which decides its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Plain,
    Keyword,
    /// Type names, and other names that stand for a kind of thing: a
    /// TOML table, a shell variable.
    Type,
    String,
    /// Numbers and literal constants (`true`, `null`).
    Number,
    Comment,
    /// A diff's added lines.
    Added,
    /// A diff's removed lines.
    Removed,
}

/// Highlighted code: byte ranges of the text and what each is.
pub type Highlighted = Arc<[(Range<usize>, Kind)]>;

/// How a language is lexed. Word lists are one string, words apart by
/// spaces, so the table stays readable.
#[derive(Debug, PartialEq, Eq)]
pub struct Language {
    /// The name shown on the block.
    pub name: &'static str,
    /// What people write after the fence, lower-case.
    aliases: &'static [&'static str],
    keywords: &'static str,
    types: &'static str,
    constants: &'static str,
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// Characters that open and close a string.
    quotes: &'static [char],
    /// `"""` and `'''` strings that span lines.
    triple_quotes: bool,
    /// Keywords match in any case (SQL).
    any_case: bool,
    /// A capitalised word is a type (Rust, Go, Java and friends).
    capital_types: bool,
    /// `'` opens a string only when it closes right after one character
    /// (or an escape): otherwise it is a Rust lifetime.
    short_char_quotes: bool,
    /// `$NAME` is a variable (shells, PHP).
    dollar_variables: bool,
    /// The first word on a line, followed by `:` or `=`, is a key
    /// (YAML, TOML, CSS properties).
    keys: Option<char>,
    mode: Mode,
}

/// Languages that need more than words and quotes.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Code,
    Diff,
    Markup,
}

/// A language with nothing but a name, to fill in from.
const fn code(name: &'static str, aliases: &'static [&'static str]) -> Language {
    Language {
        name,
        aliases,
        keywords: "",
        types: "",
        constants: "",
        line_comments: &[],
        block_comment: None,
        quotes: &['"', '\''],
        triple_quotes: false,
        any_case: false,
        capital_types: false,
        short_char_quotes: false,
        dollar_variables: false,
        keys: None,
        mode: Mode::Code,
    }
}

const C_COMMENT: Option<(&str, &str)> = Some(("/*", "*/"));
const C_TYPES: &str = "bool char double float int long short signed unsigned void size_t \
    int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t";
const C_KEYWORDS: &str = "auto break case const continue default do else enum extern for \
    goto if inline register return sizeof static struct switch typedef union volatile while \
    #include #define #if #ifdef #ifndef #endif #else #pragma";
const CPP_KEYWORDS: &str = "auto break case catch class const constexpr continue default \
    delete do else enum explicit extern for friend goto if inline namespace new noexcept \
    operator override private protected public return sizeof static struct switch template \
    this throw try typedef typename union using virtual volatile while #include #define #if \
    #ifdef #ifndef #endif #else #pragma";
const JS_KEYWORDS: &str = "async await break case catch class const continue debugger default \
    delete do else export extends finally for from function if import in instanceof let new \
    of return static super switch this throw try typeof var void while with yield";
const TS_KEYWORDS: &str = "abstract as async await break case catch class const continue \
    declare default delete do else enum export extends finally for from function if \
    implements import in instanceof interface keyof let namespace new of private protected \
    public readonly return static super switch this throw try type typeof var void while yield";
const JS_CONSTANTS: &str = "true false null undefined NaN Infinity";

/// Every language the highlighter knows.
static LANGUAGES: &[Language] = &[
    Language {
        keywords: "as async await break const continue crate dyn else enum extern fn for if \
            impl in let loop match mod move mut pub ref return self Self static struct super \
            trait type unsafe use where while",
        types: "bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64",
        constants: "true false",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        capital_types: true,
        short_char_quotes: true,
        ..code("Rust", &["rust", "rs"])
    },
    Language {
        keywords: "and as assert async await break case class continue def del elif else \
            except finally for from global if import in is lambda match nonlocal not or pass \
            raise return self try while with yield",
        types: "int float str bool list dict set tuple bytes object",
        constants: "True False None",
        line_comments: &["#"],
        triple_quotes: true,
        ..code("Python", &["python", "py", "python3"])
    },
    Language {
        keywords: JS_KEYWORDS,
        constants: JS_CONSTANTS,
        line_comments: &["//"],
        block_comment: C_COMMENT,
        quotes: &['"', '\'', '`'],
        capital_types: true,
        ..code(
            "JavaScript",
            &["javascript", "js", "jsx", "mjs", "cjs", "node"],
        )
    },
    Language {
        keywords: TS_KEYWORDS,
        types: "any boolean never number object string symbol unknown void bigint",
        constants: JS_CONSTANTS,
        line_comments: &["//"],
        block_comment: C_COMMENT,
        quotes: &['"', '\'', '`'],
        capital_types: true,
        ..code("TypeScript", &["typescript", "ts", "tsx"])
    },
    Language {
        keywords: "break case chan const continue default defer else fallthrough for func go \
            goto if import interface map package range return select struct switch type var",
        types: "bool byte complex64 complex128 error float32 float64 int int8 int16 int32 \
            int64 rune string uint uint8 uint16 uint32 uint64 uintptr any",
        constants: "true false nil iota",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        quotes: &['"', '\'', '`'],
        capital_types: true,
        ..code("Go", &["go", "golang"])
    },
    Language {
        keywords: "abstract assert break case catch class continue default do else enum \
            extends final finally for if implements import instanceof interface native new \
            package private protected public record return static super switch synchronized \
            this throw throws try var volatile while",
        types: "boolean byte char double float int long short void",
        constants: "true false null",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        capital_types: true,
        ..code("Java", &["java"])
    },
    Language {
        keywords: "abstract as break by class companion continue data do else enum for fun if \
            import in interface internal is lateinit object open override package private \
            protected public return sealed suspend this throw try val var when while",
        constants: "true false null",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        capital_types: true,
        ..code("Kotlin", &["kotlin", "kt", "kts"])
    },
    Language {
        keywords: "as async await break case catch class continue default defer do else enum \
            extension fileprivate for func guard if import in init inout internal let private \
            protocol public repeat return self static struct switch throw throws try var where \
            while",
        constants: "true false nil",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        capital_types: true,
        ..code("Swift", &["swift"])
    },
    Language {
        keywords: C_KEYWORDS,
        types: C_TYPES,
        constants: "NULL true false",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        ..code("C", &["c", "h"])
    },
    Language {
        keywords: CPP_KEYWORDS,
        types: C_TYPES,
        constants: "nullptr true false NULL",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        capital_types: true,
        ..code("C++", &["cpp", "c++", "cc", "cxx", "hpp", "hh"])
    },
    Language {
        keywords: "abstract as async await base break case catch class const continue default \
            delegate do else enum event explicit extern finally for foreach if in interface \
            internal is namespace new operator out override private protected public readonly \
            record ref return sealed static struct switch this throw try using var virtual void \
            while",
        types: "bool byte char decimal double float int long object sbyte short string uint \
            ulong ushort",
        constants: "true false null",
        line_comments: &["//"],
        block_comment: C_COMMENT,
        capital_types: true,
        ..code("C#", &["csharp", "cs", "c#"])
    },
    Language {
        keywords: "alias and begin break case class def do else elsif end ensure for if in \
            module next not or redo require rescue retry return self super then unless until \
            when while yield",
        constants: "true false nil",
        line_comments: &["#"],
        capital_types: true,
        ..code("Ruby", &["ruby", "rb"])
    },
    Language {
        keywords: "abstract as break case catch class const continue default do echo else \
            elseif extends final finally fn for foreach function if implements interface match \
            namespace new private protected public require return static switch throw trait \
            try use while",
        constants: "true false null TRUE FALSE NULL",
        line_comments: &["//", "#"],
        block_comment: C_COMMENT,
        capital_types: true,
        dollar_variables: true,
        ..code("PHP", &["php"])
    },
    Language {
        keywords: "case do done elif else esac export fi for function if in local return \
            select then until while echo cd exit set unset source sudo",
        constants: "true false",
        line_comments: &["#"],
        dollar_variables: true,
        ..code(
            "Shell",
            &[
                "sh",
                "bash",
                "zsh",
                "shell",
                "console",
                "fish",
                "shellsession",
            ],
        )
    },
    Language {
        keywords: "select from where and or not insert into values update set delete create \
            table drop alter add index join left right inner outer full on group by order \
            having limit offset as distinct union all case when then else end in is like \
            between exists primary key foreign references default with returning begin commit \
            rollback view asc desc",
        types: "int integer bigint smallint text varchar char boolean date timestamp \
            timestamptz numeric decimal real float serial uuid json jsonb blob",
        constants: "null true false",
        line_comments: &["--"],
        block_comment: C_COMMENT,
        any_case: true,
        ..code(
            "SQL",
            &["sql", "postgres", "postgresql", "mysql", "sqlite", "psql"],
        )
    },
    Language {
        constants: "true false null",
        quotes: &['"'],
        ..code("JSON", &["json", "jsonc", "json5"])
    },
    Language {
        constants: "true false null yes no on off",
        line_comments: &["#"],
        keys: Some(':'),
        ..code("YAML", &["yaml", "yml"])
    },
    Language {
        constants: "true false",
        line_comments: &["#", ";"],
        keys: Some('='),
        triple_quotes: true,
        ..code("TOML", &["toml", "ini", "cfg", "conf", "properties", "env"])
    },
    Language {
        block_comment: C_COMMENT,
        keys: Some(':'),
        ..code("CSS", &["css", "scss", "less"])
    },
    Language {
        keywords: "FROM RUN CMD LABEL EXPOSE ENV ADD COPY ENTRYPOINT VOLUME USER WORKDIR ARG \
            ONBUILD STOPSIGNAL HEALTHCHECK SHELL AS",
        line_comments: &["#"],
        dollar_variables: true,
        ..code("Dockerfile", &["dockerfile", "docker", "containerfile"])
    },
    Language {
        block_comment: Some(("<!--", "-->")),
        mode: Mode::Markup,
        ..code("HTML", &["html", "xml", "svg", "xhtml", "vue", "plist"])
    },
    Language {
        quotes: &[],
        mode: Mode::Diff,
        ..code("Diff", &["diff", "patch"])
    },
];

/// The language a fence names: `rust`, `py`, `c++`.
pub fn language(name: &str) -> Option<&'static Language> {
    let name = name.trim().to_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.aliases.contains(&name.as_str()))
}

/// A code block's language, from its first line, and the code without that
/// line. Only a known language name counts, and only with code after it: a
/// one-line block saying `json` is just the word.
pub fn split_language(code: &str) -> (Option<&'static Language>, &str) {
    let Some((first, rest)) = code.split_once('\n') else {
        return (None, code);
    };
    match language(first) {
        Some(found) if !rest.trim().is_empty() => (Some(found), rest),
        _ => (None, code),
    }
}

/// Cuts `code` into runs, in order and covering all of it.
pub fn highlight(language: &Language, code: &str) -> Vec<(Range<usize>, Kind)> {
    let mut runs = Runs::default();
    match language.mode {
        Mode::Diff => diff(code, &mut runs),
        Mode::Markup => markup(language, code, &mut runs),
        Mode::Code => lex(language, code, &mut runs),
    }
    runs.finish(code.len())
}

/// Runs being collected; neighbours of one kind merge, and gaps are plain.
#[derive(Default)]
struct Runs {
    runs: Vec<(Range<usize>, Kind)>,
}

impl Runs {
    fn push(&mut self, range: Range<usize>, kind: Kind) {
        if range.is_empty() {
            return;
        }
        let start = self.runs.last().map_or(0, |(r, _)| r.end);
        if range.start > start {
            self.push_merged(start..range.start, Kind::Plain);
        }
        self.push_merged(range, kind);
    }

    fn push_merged(&mut self, range: Range<usize>, kind: Kind) {
        if let Some((last, last_kind)) = self.runs.last_mut()
            && *last_kind == kind
            && last.end == range.start
        {
            last.end = range.end;
            return;
        }
        self.runs.push((range, kind));
    }

    fn finish(mut self, len: usize) -> Vec<(Range<usize>, Kind)> {
        let end = self.runs.last().map_or(0, |(r, _)| r.end);
        if end < len {
            self.push_merged(end..len, Kind::Plain);
        }
        self.runs
    }
}

/// Lines starting with `+` are added and `-` removed; the `+++` and `---`
/// file headers and `@@` hunks are headings.
fn diff(code: &str, runs: &mut Runs) {
    let mut at = 0;
    for line in code.split_inclusive('\n') {
        let kind = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            Kind::Type
        } else if line.starts_with('+') {
            Kind::Added
        } else if line.starts_with('-') {
            Kind::Removed
        } else {
            Kind::Plain
        };
        runs.push(at..at + line.len(), kind);
        at += line.len();
    }
}

/// Tags, their names and attribute values, and comments.
fn markup(language: &Language, code: &str, runs: &mut Runs) {
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some((open, close)) = language.block_comment
            && code[i..].starts_with(open)
        {
            let end = code[i + open.len()..]
                .find(close)
                .map_or(code.len(), |at| i + open.len() + at + close.len());
            runs.push(i..end, Kind::Comment);
            i = end;
            continue;
        }
        if bytes[i] == b'<' {
            // The tag's name: `<div`, `</div`, `<?xml`, `<!DOCTYPE`.
            let mut j = i + 1;
            while j < bytes.len() && matches!(bytes[j], b'/' | b'?' | b'!') {
                j += 1;
            }
            let name_end = j + ident_len(&code[j..], |c| c == '-' || c == ':' || c == '.');
            if name_end == j {
                i += 1;
                continue;
            }
            runs.push(j..name_end, Kind::Keyword);
            // Inside the tag: attribute names stay plain, values are
            // strings, up to the closing `>`.
            let mut k = name_end;
            while k < bytes.len() && bytes[k] != b'>' {
                let c = bytes[k];
                if c == b'"' || c == b'\'' {
                    let end = code[k + 1..]
                        .find(char::from(c))
                        .map_or(code.len(), |at| k + 1 + at + 1);
                    runs.push(k..end, Kind::String);
                    k = end;
                } else if c == b'<' {
                    break;
                } else {
                    k += 1;
                }
            }
            i = k;
            continue;
        }
        if bytes[i] == b'&'
            && let Some(len) = code[i..].find(';').filter(|&len| (2..12).contains(&len))
            && code[i + 1..i + len]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '#')
        {
            runs.push(i..i + len + 1, Kind::Number);
            i += len + 1;
            continue;
        }
        i += code[i..].chars().next().map_or(1, char::len_utf8);
    }
}

/// The length in bytes of the identifier at the start of `text`: letters,
/// digits, `_`, and whatever `extra` allows.
fn ident_len(text: &str, extra: impl Fn(char) -> bool) -> usize {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_alphabetic() || c == '_' => {}
        _ => return 0,
    }
    chars
        .find(|&(_, c)| !(c.is_alphanumeric() || c == '_' || extra(c)))
        .map_or(text.len(), |(at, _)| at)
}

/// Whether `c` can be part of a word, so a number or keyword next to it is
/// not one.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The general lexer: comments, strings, numbers and words.
fn lex(language: &Language, code: &str, runs: &mut Runs) {
    let bytes = code.as_bytes();
    let mut i = 0;
    // Whether only spaces came before `i` on its line, for keys.
    let mut line_start = true;
    while i < bytes.len() {
        let rest = &code[i..];
        let c = rest.chars().next().unwrap_or('\0');
        if c == '\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if c == ' ' || c == '\t' {
            i += 1;
            continue;
        }
        let at_line_start = std::mem::replace(&mut line_start, false);
        if let Some(marker) = language
            .line_comments
            .iter()
            .find(|m| rest.starts_with(**m))
        {
            // `#` in a shell word (`a#b`) or a URL's `//` is no comment.
            let glued = code[..i].chars().next_back().is_some_and(|p| {
                !p.is_whitespace() && (*marker == "#" || p == ':') && !"([{;,".contains(p)
            });
            if !glued {
                let end = rest.find('\n').map_or(code.len(), |at| i + at);
                runs.push(i..end, Kind::Comment);
                i = end;
                continue;
            }
        }
        if let Some((open, close)) = language.block_comment
            && rest.starts_with(open)
        {
            let end = rest[open.len()..]
                .find(close)
                .map_or(code.len(), |at| i + open.len() + at + close.len());
            runs.push(i..end, Kind::Comment);
            i = end;
            continue;
        }
        if language.quotes.contains(&c) {
            if let Some(len) = string_len(language, rest, c) {
                runs.push(i..i + len, Kind::String);
                i += len;
                continue;
            }
            i += c.len_utf8();
            continue;
        }
        let before = code[..i].chars().next_back();
        if c.is_ascii_digit() && !before.is_some_and(is_word) {
            let len = rest
                .char_indices()
                .find(|&(at, d)| {
                    !(d.is_ascii_alphanumeric()
                        || d == '_'
                        // A decimal point, not a method call or a range.
                        || (d == '.'
                            && rest[at + 1..].starts_with(|n: char| n.is_ascii_digit())))
                })
                .map_or(rest.len(), |(at, _)| at);
            runs.push(i..i + len, Kind::Number);
            i += len;
            continue;
        }
        if language.dollar_variables && c == '$' {
            let name = &rest[1..];
            let len = if name.starts_with('{') {
                name.find('}').map_or(0, |at| at + 1)
            } else {
                ident_len(name, |_| false).max(
                    // `$1`, `$?`, `$@`
                    usize::from(
                        name.starts_with(|n: char| n.is_ascii_digit() || "?@#*!$".contains(n)),
                    ),
                )
            };
            if len > 0 {
                runs.push(i..i + 1 + len, Kind::Type);
                i += 1 + len;
                continue;
            }
        }
        // Preprocessor words (`#include`) and decorators keep their sigil.
        let sigil = usize::from((c == '#' || c == '@') && rest.len() > 1);
        let word_len = ident_len(&rest[sigil..], |c| {
            language.keys.is_some() && (c == '-' || c == '.')
        });
        if word_len > 0 && !before.is_some_and(is_word) {
            let len = sigil + word_len;
            let word = &rest[..len];
            if let Some(kind) = classify(language, word, &rest[len..], at_line_start) {
                runs.push(i..i + len, kind);
            }
            i += len;
            continue;
        }
        if c == '[' && at_line_start && language.keys == Some('=') {
            // A TOML table or INI section: `[server]`, `[[bin]]`.
            let end = rest.find('\n').unwrap_or(rest.len());
            if rest[..end].trim_end().ends_with(']') {
                runs.push(i..i + end, Kind::Type);
                i += end;
                continue;
            }
        }
        i += c.len_utf8();
    }
}

/// What a word is, or `None` for plain.
fn classify(language: &Language, word: &str, after: &str, at_line_start: bool) -> Option<Kind> {
    let matches = |list: &str| {
        let mut words = list.split_ascii_whitespace();
        if language.any_case {
            words.any(|w| w.eq_ignore_ascii_case(word))
        } else {
            words.any(|w| w == word)
        }
    };
    if let Some(separator) = language.keys
        && at_line_start
        && after.trim_start_matches([' ', '\t']).starts_with(separator)
    {
        return Some(Kind::Keyword);
    }
    if matches(language.constants) {
        Some(Kind::Number)
    } else if matches(language.keywords) {
        Some(Kind::Keyword)
    } else if matches(language.types)
        || (language.capital_types
            && word.starts_with(|c: char| c.is_uppercase())
            && word.chars().any(char::is_lowercase))
    {
        Some(Kind::Type)
    } else if word.starts_with('@') {
        // A decorator or annotation.
        Some(Kind::Type)
    } else {
        None
    }
}

/// The length of the string that opens at the start of `text` with
/// `quote`, or `None` when the quote opens nothing.
fn string_len(language: &Language, text: &str, quote: char) -> Option<usize> {
    if language.triple_quotes && (quote == '"' || quote == '\'') {
        let triple: String = std::iter::repeat_n(quote, 3).collect();
        if let Some(inner) = text.strip_prefix(triple.as_str()) {
            let end = inner
                .find(triple.as_str())
                .map_or(text.len(), |at| 3 + at + 3);
            return Some(end);
        }
    }
    let inner = &text[quote.len_utf8()..];
    if quote == '\'' && language.short_char_quotes {
        // `'a'` and `'\n'` are characters; `'a` alone is a lifetime.
        let mut chars = inner.char_indices();
        let close = match chars.next() {
            Some((_, '\\')) => inner.get(2..).and_then(|s| s.find('\'')).map(|at| at + 2),
            Some((_, c)) => inner[c.len_utf8()..]
                .starts_with('\'')
                .then_some(c.len_utf8()),
            None => None,
        };
        return close.filter(|&at| at <= 10).map(|at| 1 + at + 1);
    }
    // Backtick strings span lines; others end at the line's end.
    let multiline = quote == '`';
    let mut escaped = false;
    for (at, c) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '\n' if !multiline => return Some(quote.len_utf8() + at),
            c if c == quote => return Some(quote.len_utf8() + at + c.len_utf8()),
            _ => {}
        }
    }
    Some(text.len())
}

/// Highlighted code kept between frames, like [`crate::mrkdwn::ParseCache`]:
/// keyed by language and text, and swept of what was not drawn since the
/// last sweep.
#[derive(Debug, Default)]
pub struct HighlightCache {
    /// By language name, then by text: looking up a `&str` then needs no
    /// copy of the text on every frame.
    entries: HashMap<&'static str, HashMap<String, (Highlighted, bool)>>,
}

impl HighlightCache {
    /// The runs of `code`, highlighted now or on an earlier frame.
    pub fn get(&mut self, language: &'static Language, code: &str) -> Highlighted {
        let entries = self.entries.entry(language.name).or_default();
        if let Some((runs, used)) = entries.get_mut(code) {
            *used = true;
            return runs.clone();
        }
        let runs: Highlighted = highlight(language, code).into();
        entries.insert(code.to_owned(), (runs.clone(), true));
        runs
    }

    /// Forgets what was not asked for since the last sweep.
    pub fn sweep(&mut self) {
        for entries in self.entries.values_mut() {
            entries.retain(|_, (_, used)| std::mem::take(used));
        }
        self.entries.retain(|_, entries| !entries.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runs as (text, kind), without the plain ones.
    fn marked(name: &str, code: &str) -> Vec<(String, Kind)> {
        let language = language(name).expect("known language");
        let runs = highlight(language, code);
        // The runs cover the code exactly, in order.
        let mut at = 0;
        for (range, _) in &runs {
            assert_eq!(range.start, at, "{code:?}: {runs:?}");
            at = range.end;
        }
        assert_eq!(at, code.len());
        runs.into_iter()
            .filter(|(_, kind)| *kind != Kind::Plain)
            .map(|(range, kind)| (code[range].to_owned(), kind))
            .collect()
    }

    fn owned(list: &[(&str, Kind)]) -> Vec<(String, Kind)> {
        list.iter().map(|(t, k)| ((*t).to_owned(), *k)).collect()
    }

    #[test]
    fn the_first_line_names_the_language() {
        let (found, rest) = split_language("rust\nfn main() {}");
        assert_eq!(found.map(|l| l.name), Some("Rust"));
        assert_eq!(rest, "fn main() {}");
        assert_eq!(
            split_language("Py \nx = 1").0.map(|l| l.name),
            Some("Python")
        );
        // A lone word is the code itself, and so is an unknown first line.
        assert!(split_language("json").0.is_none());
        assert!(split_language("json\n").0.is_none());
        assert_eq!(split_language("hello\nworld"), (None, "hello\nworld"));
        assert!(split_language("let x = 1;\nlet y = 2;").0.is_none());
    }

    #[test]
    fn rust_keywords_types_strings_and_comments() {
        use Kind::*;
        assert_eq!(
            marked("rust", "fn main() { let s: &str = \"hi\"; // done\n}"),
            owned(&[
                ("fn", Keyword),
                ("let", Keyword),
                ("str", Type),
                ("\"hi\"", String),
                ("// done", Comment),
            ])
        );
        // A lifetime is no string; a character is.
        assert_eq!(
            marked("rs", "fn f<'a>(c: char) { 'x' }"),
            owned(&[("fn", Keyword), ("char", Type), ("'x'", String)])
        );
        assert_eq!(
            marked("rust", "Vec::<u8>::new(); 0x1F + 2.5"),
            owned(&[
                ("Vec", Type),
                ("u8", Type),
                ("0x1F", Number),
                ("2.5", Number)
            ])
        );
    }

    #[test]
    fn python_triple_quotes_span_lines() {
        use Kind::*;
        assert_eq!(
            marked(
                "python",
                "def f():\n    \"\"\"Doc\n    more\"\"\"\n    return None # x"
            ),
            owned(&[
                ("def", Keyword),
                ("\"\"\"Doc\n    more\"\"\"", String),
                ("return", Keyword),
                ("None", Number),
                ("# x", Comment),
            ])
        );
    }

    #[test]
    fn words_inside_words_are_not_keywords() {
        assert!(marked("js", "format iffy x1").is_empty());
        assert_eq!(marked("js", "a2b"), []);
    }

    #[test]
    fn shell_variables_and_glued_hashes() {
        use Kind::*;
        assert_eq!(
            marked("bash", "echo $HOME ${X} a#b # note"),
            owned(&[
                ("echo", Keyword),
                ("$HOME", Type),
                ("${X}", Type),
                ("# note", Comment),
            ])
        );
    }

    #[test]
    fn sql_matches_any_case() {
        use Kind::*;
        assert_eq!(
            marked("sql", "SELECT id FROM t -- why\nwhere x = 'a'"),
            owned(&[
                ("SELECT", Keyword),
                ("FROM", Keyword),
                ("-- why", Comment),
                ("where", Keyword),
                ("'a'", String),
            ])
        );
    }

    #[test]
    fn keys_and_tables() {
        use Kind::*;
        assert_eq!(
            marked("toml", "[package]\nname = \"x\"\nlto = true"),
            owned(&[
                ("[package]", Type),
                ("name", Keyword),
                ("\"x\"", String),
                ("lto", Keyword),
                ("true", Number),
            ])
        );
        assert_eq!(
            marked("yaml", "on-push:\n  - run: make # build"),
            owned(&[("on-push", Keyword), ("# build", Comment)])
        );
    }

    #[test]
    fn diffs_colour_whole_lines() {
        use Kind::*;
        assert_eq!(
            marked("diff", "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n same"),
            owned(&[
                ("--- a\n+++ b\n@@ -1 +1 @@\n", Type),
                ("-old\n", Removed),
                ("+new\n", Added),
            ])
        );
    }

    #[test]
    fn markup_tags_attributes_and_comments() {
        use Kind::*;
        assert_eq!(
            marked("html", "<!-- c --><a href=\"/x\">&amp; go</a>"),
            owned(&[
                ("<!-- c -->", Comment),
                ("a", Keyword),
                ("\"/x\"", String),
                ("&amp;", Number),
                ("a", Keyword),
            ])
        );
    }

    #[test]
    fn unclosed_strings_and_comments_run_to_their_end() {
        use Kind::*;
        assert_eq!(marked("js", "x = \"open\ny"), owned(&[("\"open", String)]));
        assert_eq!(marked("c", "/* open"), owned(&[("/* open", Comment)]));
        assert_eq!(marked("js", "`a\nb"), owned(&[("`a\nb", String)]));
    }

    #[test]
    fn every_language_survives_arbitrary_text() {
        let samples = [
            "",
            "'",
            "\"",
            "`",
            "\\",
            "$",
            "${",
            "#",
            "@",
            "<",
            "</",
            "<!--",
            "&",
            "&;",
            "0x",
            "1.",
            "é'x'",
            "日本 \"語",
            "'\\",
            "'\\x",
            "[",
            "[[a]]",
            "a:\n",
            "\n\n",
            "/*",
            "--",
        ];
        for language in LANGUAGES {
            for a in samples {
                for b in samples {
                    let code = format!("{a}{b} {a}");
                    let runs = highlight(language, &code);
                    let covered: usize = runs.iter().map(|(r, _)| r.len()).sum();
                    assert_eq!(covered, code.len(), "{} {code:?}", language.name);
                    assert!(runs.iter().all(|(r, _)| code.is_char_boundary(r.start)));
                }
            }
        }
    }

    #[test]
    fn aliases_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for language in LANGUAGES {
            for alias in language.aliases {
                assert!(seen.insert(*alias), "{alias} is listed twice");
            }
        }
    }

    #[test]
    fn the_cache_keeps_what_is_drawn() {
        let rust = language("rust").expect("rust");
        let mut cache = HighlightCache::default();
        let first = cache.get(rust, "fn x() {}");
        assert!(Arc::ptr_eq(&first, &cache.get(rust, "fn x() {}")));
        cache.sweep();
        cache.sweep();
        assert!(cache.entries.is_empty());
    }
}
