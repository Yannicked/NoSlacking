//! The file viewer's reading side: which files it opens, and turning their
//! bytes into what it shows: a grid of cells, an archive's listing or the
//! lines of a text file.
//!
//! The worker downloads a file and calls [`read`] on a blocking thread;
//! the interface (`ui::viewer`) only draws the [`Document`]. Nothing here
//! touches the network or egui, so every part is tested on small files.
//!
//! A file is never trusted. Every count and size it declares is checked
//! against a cap before anything is allocated for it, and compressed parts
//! are measured, not believed, before a parser reads them (as
//! `images::check_decoded_size` does for pictures). Where the part that
//! fits still means something (the first rows, the first lines) it is
//! shown with a [`Note`] saying what was left out; otherwise the file is
//! refused with a [`Failure`].
//!
//! Old binary workbooks (`.xls`, `.xlsb`) are not opened: calamine's
//! readers for them slice records and reserve memory by what the file
//! claims, and a release build aborts on a panic, so one damaged file
//! could close the app. Slack's PDF of them still opens from the card.

mod archive;
mod delimited;
mod sheet;
mod text;

use crate::failure::Failure;

#[cfg(any(test, feature = "demo"))]
pub use sheet::{SampleCell, sample_workbook};

/// The most of a file downloaded to view it. A spreadsheet or an archive
/// larger than this is refused (its index is at the end); the first part
/// of a text or CSV file is shown.
pub const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
/// The most rows of a sheet or CSV file shown.
pub const MAX_ROWS: usize = 50_000;
/// The most columns shown.
pub const MAX_COLUMNS: usize = 500;
/// The most sheets of a workbook shown.
pub const MAX_SHEETS: usize = 64;
/// The most filled cells kept, over every sheet: about the memory of the
/// file itself again, whatever its shape.
pub const MAX_CELLS: usize = 2_000_000;
/// The most characters of one cell shown; a grid cell shows one line.
pub const MAX_CELL_CHARS: usize = 1_000;
/// The most lines of a text file shown.
pub const MAX_LINES: usize = 200_000;
/// The most characters of one line shown: a minified file is one line, and
/// laying out megabytes of it would stall every frame.
pub const MAX_LINE_CHARS: usize = 5_000;
/// The most entries of an archive listed.
pub const MAX_ENTRIES: usize = 10_000;

/// What kind of viewer a file opens in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An Excel (`.xlsx`, `.xlsm`) or OpenDocument (`.ods`) workbook.
    Sheet,
    /// Comma-separated values; the separator is guessed (`;` is common).
    Csv,
    /// Tab-separated values.
    Tsv,
    /// A zip archive, listed but never unpacked.
    Zip,
    /// Plain text, code, a log, Markdown, JSON and the like.
    Text,
}

impl Kind {
    /// Whether only the whole file can be read: a zip's index is at its
    /// end, so the start of one is no use.
    pub fn needs_whole_file(self) -> bool {
        matches!(self, Self::Sheet | Self::Zip)
    }
}

/// Slack's kinds (`filetype`) and extensions read as text.
const TEXT_TYPES: &[&str] = &[
    "text",
    "txt",
    "log",
    "markdown",
    "md",
    "json",
    "jsonl",
    "ndjson",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "xml",
    "html",
    "htm",
    "css",
    "scss",
    "javascript",
    "js",
    "mjs",
    "jsx",
    "typescript",
    "ts",
    "tsx",
    "python",
    "py",
    "rust",
    "rs",
    "go",
    "java",
    "kotlin",
    "kt",
    "swift",
    "c",
    "h",
    "cpp",
    "cc",
    "hpp",
    "csharp",
    "cs",
    "ruby",
    "rb",
    "php",
    "perl",
    "pl",
    "lua",
    "shell",
    "sh",
    "bash",
    "zsh",
    "fish",
    "powershell",
    "ps1",
    "sql",
    "diff",
    "patch",
    "dockerfile",
    "makefile",
    "r",
    "scala",
    "haskell",
    "hs",
    "elixir",
    "ex",
    "exs",
    "erlang",
    "erl",
    "clojure",
    "clj",
    "dart",
    "groovy",
    "matlab",
    "ocaml",
    "ml",
    "vb",
    "vbscript",
    "properties",
    "env",
    "gitignore",
    "srt",
    "vtt",
    "tex",
    "rst",
    "org",
    "adoc",
    "graphql",
    "proto",
    "nix",
    "zig",
    "svelte",
    "vue",
    "applescript",
    "coffeescript",
    "cypher",
    "d",
    "fortran",
    "julia",
    "lisp",
    "pascal",
    "puppet",
    "scheme",
    "smalltalk",
    "verilog",
    "vhdl",
    "xquery",
    "apex",
    "basic",
    "cobol",
    "objc",
    "sass",
    "less",
    "csv_text",
];

/// Which viewer opens a file, from its name, Slack's kind for it and its
/// MIME type, or `None` when none does. `previewed` says Slack sent the
/// file's first lines, which it does only for text.
pub fn kind(name: &str, filetype: &str, mimetype: &str, previewed: bool) -> Option<Kind> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let filetype = filetype.trim().to_ascii_lowercase();
    let mimetype = mimetype.to_ascii_lowercase();
    let is = |names: &[&str]| names.contains(&filetype.as_str()) || names.contains(&ext.as_str());
    // Slack's posts and canvases are rich documents, not their text.
    if is(&["post", "space", "quip", "canvas", "gdoc", "gsheet", "gpres"]) {
        return None;
    }
    if is(&["xlsx", "xlsm", "ods"]) {
        return Some(Kind::Sheet);
    }
    if is(&["xls", "xlsb"]) {
        return None;
    }
    if is(&["tsv", "tab"]) || mimetype == "text/tab-separated-values" {
        return Some(Kind::Tsv);
    }
    if is(&["csv"]) || mimetype == "text/csv" {
        return Some(Kind::Csv);
    }
    if is(&["zip"])
        || matches!(
            mimetype.as_str(),
            "application/zip" | "application/x-zip-compressed"
        )
    {
        return Some(Kind::Zip);
    }
    let text_mime = mimetype.starts_with("text/")
        || matches!(
            mimetype.as_str(),
            "application/json" | "application/xml" | "application/x-yaml" | "application/toml"
        );
    if previewed || text_mime || is(TEXT_TYPES) {
        return Some(Kind::Text);
    }
    None
}

/// A file as the viewer shows it.
#[derive(Clone, PartialEq)]
pub struct Document {
    pub body: Body,
    /// What was left out of the whole file (its download was cut short).
    pub notes: Vec<Note>,
}

/// File contents are no one's business in a log: only their shape.
impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shape = match &self.body {
            Body::Sheets(sheets) => format!("{} sheets", sheets.len()),
            Body::Archive(archive) => format!("{} entries", archive.entries.len()),
            Body::Text(text) => format!("{} lines", text.lines.len()),
        };
        f.debug_struct("Document")
            .field("body", &shape)
            .field("notes", &self.notes)
            .finish()
    }
}

/// What a file holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// A workbook's sheets, or the one table of a CSV file.
    Sheets(Vec<Sheet>),
    Archive(Archive),
    Text(Text),
}

/// One table of cells.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sheet {
    /// The sheet's name; empty for a CSV file.
    pub name: String,
    /// The cells by row and column from A1, as text. A row ends at its last
    /// filled cell, so an empty row holds nothing.
    pub rows: Vec<Vec<String>>,
    /// How many columns the widest row has.
    pub columns: usize,
    /// What of this sheet was left out.
    pub notes: Vec<Note>,
}

/// An archive's listing, from its central directory alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Archive {
    /// The entries listed, in the archive's order.
    pub entries: Vec<Entry>,
    /// How many entries the archive has, listed or not.
    pub count: usize,
    /// The size of everything in it, unpacked, as the archive declares.
    pub unpacked: u64,
    /// The size of everything in it, packed.
    pub packed: u64,
}

/// One file or folder in an archive.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub path: String,
    /// Its size unpacked, as declared.
    pub size: u64,
    /// Its size in the archive.
    pub packed: u64,
    /// When it was last changed, as the archive says (local time, no zone):
    /// "2026-09-30 14:05".
    pub modified: Option<String>,
    pub folder: bool,
    /// Locked with a password.
    pub encrypted: bool,
}

/// A text file, line by line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Text {
    /// The lines, without their line ends, tabs as spaces.
    pub lines: Vec<String>,
}

/// Something left out of what is shown, or worth knowing about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// Only the first `shown` bytes of the file were downloaded.
    Download { shown: u64 },
    /// Only the first `shown` rows are shown.
    Rows { shown: usize },
    /// Only the first `shown` columns are shown.
    Columns { shown: usize },
    /// Only `shown` of the workbook's `total` sheets are shown.
    Sheets { shown: usize, total: usize },
    /// This many cells were cut short.
    LongCells { count: usize },
    /// Only the first `shown` lines are shown.
    Lines { shown: usize },
    /// This many lines were cut short.
    LongLines { count: usize },
    /// Only `shown` of the archive's `total` entries are listed.
    Entries { shown: usize, total: usize },
    /// Parts of the text were not UTF-8 and show as �.
    NotUtf8,
    /// The archive would unpack to more than this many times its size:
    /// listing it is safe, unpacking it may not be.
    Packed { ratio: u64 },
}

/// Reads a downloaded file for the viewer. `cut` says the download
/// stopped at [`MAX_DOWNLOAD`], so `bytes` is only the file's start.
pub fn read(kind: Kind, bytes: &[u8], cut: bool) -> Result<Document, Failure> {
    if cut && kind.needs_whole_file() {
        return Err(Failure::ViewTooLarge);
    }
    let bytes = if cut { whole_lines(bytes) } else { bytes };
    let mut notes = Vec::new();
    if cut {
        notes.push(Note::Download {
            shown: bytes.len() as u64,
        });
    }
    let body = match kind {
        Kind::Sheet => {
            let (sheets, mut book_notes) = sheet::read(bytes)?;
            notes.append(&mut book_notes);
            Body::Sheets(sheets)
        }
        Kind::Csv => Body::Sheets(vec![delimited::read(bytes, None)?]),
        Kind::Tsv => Body::Sheets(vec![delimited::read(bytes, Some(b'\t'))?]),
        Kind::Zip => {
            let (archive, mut archive_notes) = archive::read(bytes)?;
            notes.append(&mut archive_notes);
            Body::Archive(archive)
        }
        Kind::Text => {
            let (text, mut text_notes) = text::read(bytes)?;
            notes.append(&mut text_notes);
            Body::Text(text)
        }
    };
    Ok(Document { body, notes })
}

/// The start of a cut-short file up to its last line end, so neither a
/// half line nor half a character is shown.
fn whole_lines(bytes: &[u8]) -> &[u8] {
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(end) => &bytes[..=end],
        None => bytes,
    }
}

/// The letters naming column `index` (from 0): A … Z, AA, AB ….
pub fn column_name(index: usize) -> String {
    let mut name = Vec::new();
    let mut n = index + 1;
    while n > 0 {
        let rem = (n - 1) % 26;
        name.push(b'A' + rem as u8);
        n = (n - 1) / 26;
    }
    name.reverse();
    String::from_utf8_lossy(&name).into_owned()
}

/// `text` as one line of at most [`MAX_CELL_CHARS`] characters: line ends
/// become spaces. True when it was cut.
fn cell_text(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len().min(MAX_CELL_CHARS));
    let mut cut = false;
    for (count, c) in text.chars().enumerate() {
        if count == MAX_CELL_CHARS {
            cut = true;
            out.push('…');
            break;
        }
        out.push(if c == '\n' || c == '\r' || c == '\t' {
            ' '
        } else {
            c
        });
    }
    (out, cut)
}

/// A table filled cell by cell, within the caps: what is past them is
/// dropped and noted.
#[derive(Default)]
struct Grid {
    rows: Vec<Vec<String>>,
    columns: usize,
    cells: usize,
    long: usize,
    rows_cut: bool,
    columns_cut: bool,
}

impl Grid {
    /// Puts `text` at `row` and `column` (from 0). False when the grid is
    /// full and nothing more should be read.
    fn put(&mut self, row: usize, column: usize, text: &str, budget: &mut usize) -> bool {
        if row >= MAX_ROWS || *budget == 0 {
            self.rows_cut = true;
            return false;
        }
        if column >= MAX_COLUMNS {
            self.columns_cut = true;
            return true;
        }
        if text.is_empty() {
            return true;
        }
        let (text, cut) = cell_text(text);
        if cut {
            self.long += 1;
        }
        if self.rows.len() <= row {
            self.rows.resize_with(row + 1, Vec::new);
        }
        let cells = &mut self.rows[row];
        if cells.len() <= column {
            cells.resize_with(column + 1, String::new);
        }
        cells[column] = text;
        self.columns = self.columns.max(column + 1);
        self.cells += 1;
        *budget -= 1;
        true
    }

    /// The finished sheet, called `name`, with its notes.
    fn finish(self, name: String) -> Sheet {
        let mut notes = Vec::new();
        if self.rows_cut {
            notes.push(Note::Rows {
                shown: self.rows.len(),
            });
        }
        if self.columns_cut {
            notes.push(Note::Columns { shown: MAX_COLUMNS });
        }
        if self.long > 0 {
            notes.push(Note::LongCells { count: self.long });
        }
        Sheet {
            name,
            rows: self.rows,
            columns: self.columns,
            notes,
        }
    }
}

/// An unreadable file's failure, with the reader's own (technical) words.
fn unreadable(error: impl std::fmt::Display) -> Failure {
    Failure::Unreadable(error.to_string())
}

/// The viewer as the interface holds it while open.
#[derive(Clone, Debug)]
pub struct Viewer {
    /// The request this answers; a late answer to another is dropped.
    pub id: u64,
    pub name: String,
    /// Slack's kind for the file (`rust`, `json`), which names the
    /// language a text file is coloured as.
    pub filetype: String,
    pub kind: Kind,
    /// Where to download the file from, for the bar's button.
    pub download: Option<String>,
    pub state: State,
    /// The sheet shown, by index.
    pub sheet: usize,
    /// The first row stays in place while scrolling, as a header.
    pub freeze: bool,
    /// What is being looked for in a text file.
    pub find: String,
    /// What [`Self::matches`] were found for, so they are found again
    /// only when the query changes.
    pub searched: String,
    /// The lines holding the query, and which of them is shown.
    pub matches: Vec<usize>,
    pub found: usize,
    /// Scroll to the match shown on the next frame.
    pub jump: bool,
}

impl Viewer {
    /// A viewer waiting for request `id`'s file.
    pub fn loading(
        id: u64,
        name: String,
        filetype: String,
        kind: Kind,
        download: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            filetype,
            kind,
            download,
            state: State::Loading,
            sheet: 0,
            freeze: false,
            find: String::new(),
            searched: String::new(),
            matches: Vec::new(),
            found: 0,
            jump: false,
        }
    }

    /// Finds the query in `lines` again if it changed since last time,
    /// and moves to its first match.
    pub fn search(&mut self, lines: &[String]) {
        if self.find == self.searched {
            return;
        }
        self.searched.clone_from(&self.find);
        self.matches = find(lines, &self.find);
        self.found = 0;
        self.jump = !self.matches.is_empty();
    }

    /// Moves `by` matches on, round to the start or the end.
    pub fn step(&mut self, by: isize) {
        let count = self.matches.len();
        if count == 0 {
            return;
        }
        self.found = (self.found as isize + by).rem_euclid(count as isize) as usize;
        self.jump = true;
    }
}

/// How far the file has come.
#[derive(Clone, Debug)]
pub enum State {
    Loading,
    Ready(Box<Document>),
    Failed(Failure),
}

/// The lines of `lines` that hold `query`, ignoring case, in order.
pub fn find(lines: &[String], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_open_in_the_right_viewer() {
        let k = |name, filetype, mime| kind(name, filetype, mime, false);
        assert_eq!(k("Q4 budget.xlsx", "xlsx", ""), Some(Kind::Sheet));
        assert_eq!(k("plan.ods", "", ""), Some(Kind::Sheet));
        assert_eq!(k("macro.XLSM", "", ""), Some(Kind::Sheet));
        assert_eq!(k("old.xls", "xls", ""), None);
        assert_eq!(k("big.xlsb", "", ""), None);
        assert_eq!(k("export.csv", "csv", "text/csv"), Some(Kind::Csv));
        assert_eq!(k("export.tsv", "", ""), Some(Kind::Tsv));
        assert_eq!(k("logs.zip", "zip", "application/zip"), Some(Kind::Zip));
        assert_eq!(k("main.rs", "rust", "text/plain"), Some(Kind::Text));
        assert_eq!(k("notes", "", "text/plain"), Some(Kind::Text));
        assert_eq!(k("data.json", "json", "application/json"), Some(Kind::Text));
        assert_eq!(k("photo.png", "png", "image/png"), None);
        assert_eq!(k("clip.mp4", "mp4", "video/mp4"), None);
        assert_eq!(k("Plan", "post", "application/vnd.slack-docs"), None);
        assert_eq!(kind("snippet", "", "", true), Some(Kind::Text));
    }

    #[test]
    fn columns_are_named_like_a_spreadsheet() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
        assert_eq!(column_name(16_383), "XFD");
    }

    #[test]
    fn a_huge_cell_is_cut_to_one_line() {
        let (text, cut) = cell_text("one\ntwo");
        assert_eq!((text.as_str(), cut), ("one two", false));
        let huge = "x".repeat(MAX_CELL_CHARS * 3);
        let (text, cut) = cell_text(&huge);
        assert!(cut);
        assert_eq!(text.chars().count(), MAX_CELL_CHARS + 1);
    }

    #[test]
    fn the_grid_stops_at_its_caps() {
        let mut grid = Grid::default();
        let mut budget = MAX_CELLS;
        assert!(grid.put(0, MAX_COLUMNS, "far right", &mut budget));
        assert!(grid.put(2, 1, "b3", &mut budget));
        assert!(!grid.put(MAX_ROWS, 0, "far down", &mut budget));
        let sheet = grid.finish("S".into());
        assert_eq!(sheet.rows.len(), 3);
        assert!(sheet.rows[0].is_empty() && sheet.rows[1].is_empty());
        assert_eq!(sheet.rows[2], vec![String::new(), "b3".into()]);
        assert_eq!(sheet.columns, 2);
        assert_eq!(
            sheet.notes,
            vec![
                Note::Rows { shown: 3 },
                Note::Columns { shown: MAX_COLUMNS }
            ]
        );
        let mut grid = Grid::default();
        let mut budget = 1;
        assert!(grid.put(0, 0, "a", &mut budget));
        assert!(!grid.put(0, 1, "b", &mut budget));
    }

    #[test]
    fn a_cut_download_keeps_whole_lines_and_says_so() {
        let document = read(Kind::Text, b"one\ntwo\nthr", true).expect("text reads");
        let Body::Text(text) = &document.body else {
            panic!("not text");
        };
        assert_eq!(text.lines, vec!["one", "two"]);
        assert_eq!(document.notes, vec![Note::Download { shown: 8 }]);
        // A cut workbook or archive is no use: its index is at the end.
        assert_eq!(read(Kind::Zip, b"PK", true), Err(Failure::ViewTooLarge));
        assert_eq!(read(Kind::Sheet, b"PK", true), Err(Failure::ViewTooLarge));
    }

    #[test]
    fn finding_ignores_case() {
        let lines: Vec<String> = ["Alpha", "beta", "ALPHABET"].map(String::from).into();
        assert_eq!(find(&lines, "alpha"), vec![0, 2]);
        assert!(find(&lines, "  ").is_empty());
    }

    #[test]
    fn the_viewer_steps_through_matches() {
        let mut viewer = Viewer::loading(1, "a.txt".into(), "text".into(), Kind::Text, None);
        let lines: Vec<String> = ["one", "two", "one more"].map(String::from).into();
        viewer.find = "one".into();
        viewer.search(&lines);
        assert_eq!(viewer.matches, vec![0, 2]);
        assert!(viewer.jump);
        viewer.step(1);
        assert_eq!(viewer.found, 1);
        viewer.step(1);
        assert_eq!(viewer.found, 0);
        viewer.step(-1);
        assert_eq!(viewer.found, 1);
        // The same query is not searched again, so the place is kept.
        viewer.search(&lines);
        assert_eq!(viewer.found, 1);
    }

    #[test]
    fn documents_never_print_their_contents() {
        let document = Document {
            body: Body::Text(Text {
                lines: vec!["password=hunter2".into()],
            }),
            notes: Vec::new(),
        };
        let printed = format!("{document:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("1 lines"), "{printed}");
    }
}
