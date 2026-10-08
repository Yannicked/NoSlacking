//! Workbooks: Excel's `.xlsx` and `.xlsm`, and OpenDocument's `.ods`.
//!
//! Both are zip archives of XML. Before calamine opens one, every part is
//! checked: the declared sizes and compression ratios, then the real
//! unpacked size of each part, measured by unpacking it into nothing (a
//! zip may lie about sizes, and the unpacker believes the data, not the
//! header). Then the counts calamine reserves memory for are checked:
//! Excel's shared string count, and the cells an OpenDocument table's
//! repeats spell out. Only then is it read, cell by cell where calamine
//! allows, into a [`Grid`] that stops at the caps.

use std::io::{Cursor, Read};

use calamine::{Data, DataRef, ExcelDateTime, Reader as _, SheetType, SheetVisible};
use quick_xml::events::{BytesStart, Event};

use super::archive::declared_entries;
use super::{Grid, MAX_CELLS, MAX_SHEETS, Note, Sheet, unreadable};
use crate::failure::Failure;

/// The most parts a workbook may have.
const MAX_PARTS: u64 = 10_000;
/// The most a workbook's parts may unpack to, together.
const MAX_UNPACKED: u64 = 200 * 1024 * 1024;
/// A part larger than [`RATIO_FROM`] may unpack to at most this many times
/// its packed size. XML packs well, often 20 times, rarely 100.
const MAX_RATIO: u64 = 200;
const RATIO_FROM: u64 = 1024 * 1024;
/// The most shared strings an Excel workbook may declare: calamine
/// reserves room for all of them at once.
const MAX_SHARED_STRINGS: u64 = 2_000_000;
/// The most cells calamine may build for an OpenDocument workbook,
/// repeats spelled out, over every table.
const MAX_ODS_CELLS: u64 = 4_000_000;
/// The furthest row and column an OpenDocument table has, as calamine
/// caps repeats.
const ODS_ROWS: u64 = 1_048_576;
const ODS_COLUMNS: u64 = 16_384;
/// The latest date Excel has (9999-12-31), as a day number.
const LAST_EXCEL_DAY: f64 = 2_958_466.0;

/// Which kind of workbook a zip holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Book {
    Excel,
    OpenDocument,
}

/// The sheets of a workbook, and what was left out of it.
pub(super) fn read(bytes: &[u8]) -> Result<(Vec<Sheet>, Vec<Note>), Failure> {
    match check(bytes)? {
        Book::Excel => excel(bytes),
        Book::OpenDocument => open_document(bytes),
    }
}

/// Makes sure a workbook is safe to hand to calamine, and says which
/// kind it is.
fn check(bytes: &[u8]) -> Result<Book, Failure> {
    let parts = declared_entries(bytes).ok_or_else(|| unreadable("not a workbook"))?;
    if parts > MAX_PARTS {
        return Err(Failure::ViewTooLarge);
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(unreadable)?;
    let names: Vec<String> = zip.file_names().map(str::to_ascii_lowercase).collect();
    let has = |name: &str| names.iter().any(|n| n == name);
    let book = if has("xl/workbook.xml") {
        Book::Excel
    } else if has("content.xml") {
        Book::OpenDocument
    } else if has("xl/workbook.bin") {
        return Err(unreadable("binary workbooks (.xlsb) are not read"));
    } else {
        return Err(unreadable("not a workbook"));
    };
    // What the parts claim, first: cheap, and it catches the plain bombs.
    let mut declared = 0u64;
    for index in 0..zip.len() {
        let part = zip.by_index_raw(index).map_err(unreadable)?;
        declared = declared.saturating_add(part.size());
        if part.size() > RATIO_FROM && part.size() / part.compressed_size().max(1) > MAX_RATIO {
            return Err(Failure::Bomb);
        }
    }
    if declared > MAX_UNPACKED {
        return Err(Failure::Bomb);
    }
    // Then what they really unpack to: never more than they claim.
    for index in 0..zip.len() {
        let part = zip.by_index(index).map_err(unreadable)?;
        let claimed = part.size();
        let unpacked =
            std::io::copy(&mut part.take(claimed + 1), &mut std::io::sink()).map_err(unreadable)?;
        if unpacked > claimed {
            return Err(Failure::Bomb);
        }
    }
    match book {
        Book::Excel => {
            for (index, name) in names.iter().enumerate() {
                if name.ends_with("sharedstrings.xml") {
                    let part = zip.by_index(index).map_err(unreadable)?;
                    if shared_strings(part)? > MAX_SHARED_STRINGS {
                        return Err(Failure::ViewTooLarge);
                    }
                }
            }
        }
        Book::OpenDocument => {
            let content = zip.by_name("content.xml").map_err(unreadable)?;
            if ods_cells(std::io::BufReader::new(content))? > MAX_ODS_CELLS {
                return Err(Failure::ViewTooLarge);
            }
        }
    }
    Ok(book)
}

/// How many strings an Excel shared string table declares (its
/// `uniqueCount`), which calamine reserves room for. The table's opening
/// tag comes first in the part; one not found near the start is refused.
fn shared_strings(part: impl Read) -> Result<u64, Failure> {
    let mut xml = quick_xml::Reader::from_reader(std::io::BufReader::new(part.take(64 * 1024)));
    let mut buf = Vec::new();
    loop {
        match xml.read_event_into(&mut buf) {
            Ok(Event::Start(tag) | Event::Empty(tag)) if tag.local_name().as_ref() == b"sst" => {
                let count = tag
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == b"uniqueCount")
                    .and_then(|a| std::str::from_utf8(&a.value).ok()?.trim().parse().ok());
                return Ok(count.unwrap_or(0));
            }
            Ok(Event::Eof) | Err(_) => return Err(unreadable("no shared string table")),
            Ok(_) => {}
        }
        buf.clear();
    }
}

/// How many cells calamine would build for an OpenDocument workbook's
/// `content.xml`: every cell a row holds with its repeats spelled out,
/// and each table's filled area with its repeated rows. Never less than
/// calamine's own count, as anything that may hold a value counts as
/// filled. Values calamine's number parser could choke on are refused.
fn ods_cells(content: impl std::io::BufRead) -> Result<u64, Failure> {
    let mut xml = quick_xml::Reader::from_reader(content);
    let mut buf = Vec::new();
    let mut total = 0u64;
    let mut table = OdsTable::default();
    let mut row: Option<OdsRow> = None;
    loop {
        let event = xml.read_event_into(&mut buf).map_err(unreadable)?;
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                let closed = matches!(event, Event::Empty(_));
                match tag.name().as_ref() {
                    b"table:table" => table = OdsTable::default(),
                    b"table:table-row" => {
                        let repeats = repeat(tag, b"table:number-rows-repeated")?.min(ODS_ROWS);
                        let started = OdsRow {
                            repeats,
                            ..OdsRow::default()
                        };
                        if closed {
                            table.end_row(started);
                        } else {
                            row = Some(started);
                        }
                    }
                    b"table:table-cell" | b"table:covered-table-cell" => {
                        let repeats = repeat(tag, b"table:number-columns-repeated")?;
                        let filled = !closed || may_hold_a_value(tag)?;
                        if let Some(row) = row.as_mut() {
                            row.cell(repeats, filled);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(tag) => match tag.name().as_ref() {
                b"table:table-row" => {
                    if let Some(row) = row.take() {
                        table.end_row(row);
                    }
                }
                b"table:table" => {
                    total = total.saturating_add(table.cells());
                    table = OdsTable::default();
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        if total > MAX_ODS_CELLS {
            break;
        }
        buf.clear();
    }
    Ok(total)
}

/// A repeat count attribute, 1 when absent; an unreadable one is refused,
/// as calamine refuses it.
fn repeat(tag: &BytesStart<'_>, name: &[u8]) -> Result<u64, Failure> {
    for attribute in tag.attributes() {
        let attribute = attribute.map_err(unreadable)?;
        if attribute.key.as_ref() == name {
            return std::str::from_utf8(&attribute.value)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .ok_or_else(|| unreadable("bad repeat count"));
        }
    }
    Ok(1)
}

/// Whether a self-closed cell may hold a value: it has a value, a type or
/// a formula. Its number, if any, must be a plain one.
fn may_hold_a_value(tag: &BytesStart<'_>) -> Result<bool, Failure> {
    let mut value = false;
    for attribute in tag.attributes() {
        let attribute = attribute.map_err(unreadable)?;
        let key = attribute.key.as_ref();
        if key == b"office:value" && !plain_number(&attribute.value) {
            return Err(unreadable("bad number"));
        }
        value |= key.starts_with(b"office:") || key == b"table:formula";
    }
    Ok(value)
}

/// Whether `text` is a plain decimal number, such as `-12.5e3`: what any
/// float parser reads alike.
fn plain_number(text: &[u8]) -> bool {
    let text = text.strip_prefix(b"-").unwrap_or(text);
    let text = text.strip_prefix(b"+").unwrap_or(text);
    let (mantissa, exponent) = match text.iter().position(|&b| b == b'e' || b == b'E') {
        Some(at) => (&text[..at], Some(&text[at + 1..])),
        None => (text, None),
    };
    let mut digits = 0;
    let mut dots = 0;
    for &b in mantissa {
        match b {
            b'0'..=b'9' => digits += 1,
            b'.' => dots += 1,
            _ => return false,
        }
    }
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e
            .strip_prefix(b"-")
            .or_else(|| e.strip_prefix(b"+"))
            .unwrap_or(e);
        !e.is_empty() && e.iter().all(u8::is_ascii_digit)
    });
    digits > 0 && dots <= 1 && exponent_ok
}

/// One OpenDocument row being counted.
#[derive(Default)]
struct OdsRow {
    repeats: u64,
    /// Cells calamine pushes for it: filled ones with their repeats, and
    /// the empty ones before a filled one.
    pushed: u64,
    pending: u64,
}

impl OdsRow {
    fn cell(&mut self, repeats: u64, filled: bool) {
        if filled {
            self.pushed = (self.pushed + self.pending)
                .saturating_add(repeats)
                .min(ODS_COLUMNS);
            self.pending = 0;
        } else {
            self.pending = self.pending.saturating_add(repeats).min(ODS_COLUMNS);
        }
    }
}

/// One OpenDocument table being counted.
#[derive(Default)]
struct OdsTable {
    pushed: u64,
    rows: u64,
    first: Option<u64>,
    last: u64,
    width: u64,
}

impl OdsTable {
    fn end_row(&mut self, row: OdsRow) {
        let start = self.rows;
        self.rows = (self.rows + row.repeats).min(ODS_ROWS);
        self.pushed = self.pushed.saturating_add(row.pushed);
        if row.pushed > 0 {
            self.first.get_or_insert(start);
            self.last = self.rows;
            self.width = self.width.max(row.pushed);
        }
    }

    /// The cells pushed while reading, and the filled area spelled out.
    fn cells(&self) -> u64 {
        let height = self.first.map_or(0, |first| self.last - first);
        self.pushed
            .saturating_add(height.saturating_mul(self.width))
    }
}

/// The worksheets calamine lists that a reader would see: no charts, no
/// hidden sheets.
fn shown_sheets(sheets: &[calamine::Sheet]) -> Vec<String> {
    sheets
        .iter()
        .filter(|s| s.typ == SheetType::WorkSheet && s.visible == SheetVisible::Visible)
        .map(|s| s.name.clone())
        .collect()
}

/// The note for a workbook with more sheets than are shown.
fn sheets_note(total: usize) -> Vec<Note> {
    if total > MAX_SHEETS {
        vec![Note::Sheets {
            shown: MAX_SHEETS,
            total,
        }]
    } else {
        Vec::new()
    }
}

/// An Excel workbook, read a cell at a time: calamine never builds a
/// sheet's whole grid, so one cell in a far corner costs nothing.
fn excel(bytes: &[u8]) -> Result<(Vec<Sheet>, Vec<Note>), Failure> {
    let mut book: calamine::Xlsx<_> =
        calamine::Xlsx::new(Cursor::new(bytes)).map_err(unreadable)?;
    let names = shown_sheets(book.sheets_metadata());
    let notes = sheets_note(names.len());
    let mut budget = MAX_CELLS;
    let mut sheets = Vec::new();
    for name in names.into_iter().take(MAX_SHEETS) {
        let mut grid = Grid::default();
        let mut cells = book.worksheet_cells_reader(&name).map_err(unreadable)?;
        loop {
            match cells.next_cell() {
                Ok(Some(cell)) => {
                    let (row, column) = cell.get_position();
                    let text = excel_text(cell.get_value());
                    if !grid.put(row as usize, column as usize, &text, &mut budget) {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) if grid.cells == 0 && sheets.is_empty() => {
                    return Err(unreadable(error));
                }
                // What was read before the damage still shows.
                Err(_) => break,
            }
        }
        sheets.push(grid.finish(name));
    }
    Ok((sheets, notes))
}

/// An OpenDocument workbook. calamine reads its tables whole, which
/// [`check`] has made sure stays small.
fn open_document(bytes: &[u8]) -> Result<(Vec<Sheet>, Vec<Note>), Failure> {
    let mut book: calamine::Ods<_> = calamine::Ods::new(Cursor::new(bytes)).map_err(unreadable)?;
    let names = shown_sheets(book.sheets_metadata());
    let notes = sheets_note(names.len());
    let mut budget = MAX_CELLS;
    let mut sheets = Vec::new();
    for name in names.into_iter().take(MAX_SHEETS) {
        let range = book.worksheet_range(&name).map_err(unreadable)?;
        let (top, left) = range.start().unwrap_or((0, 0));
        let mut grid = Grid::default();
        for (row, column, value) in range.used_cells() {
            let row = row + top as usize;
            let column = column + left as usize;
            if !grid.put(row, column, &data_text(value), &mut budget) {
                break;
            }
        }
        sheets.push(grid.finish(name));
    }
    Ok((sheets, notes))
}

/// A cell of an Excel sheet as text.
fn excel_text(value: &DataRef<'_>) -> String {
    match value {
        DataRef::Int(n) => n.to_string(),
        DataRef::Float(n) => number(*n),
        DataRef::String(s) => s.clone(),
        DataRef::SharedString(s) => (*s).to_owned(),
        DataRef::Bool(b) => boolean(*b),
        DataRef::DateTime(at) => date(at),
        DataRef::DateTimeIso(s) => s.replacen('T', " ", 1),
        DataRef::DurationIso(s) => s.clone(),
        DataRef::Error(e) => e.to_string(),
        DataRef::Empty => String::new(),
    }
}

/// A cell of an OpenDocument sheet as text.
fn data_text(value: &Data) -> String {
    match value {
        Data::Int(n) => n.to_string(),
        Data::Float(n) => number(*n),
        Data::String(s) => s.clone(),
        Data::Bool(b) => boolean(*b),
        Data::DateTime(at) => date(at),
        Data::DateTimeIso(s) => s.replacen('T', " ", 1),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => e.to_string(),
        Data::Empty => String::new(),
    }
}

/// TRUE or FALSE, as spreadsheets write them.
fn boolean(value: bool) -> String {
    if value { "TRUE" } else { "FALSE" }.to_owned()
}

/// A number as a spreadsheet's General format shows it: whole numbers
/// without a point, others to 15 significant digits, the very large and
/// very small in exponent form.
pub(super) fn number(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let size = value.abs();
    if value.fract() == 0.0 && size < 1e15 {
        return format!("{value:.0}");
    }
    if size >= 1e15 || size < 1e-9 {
        return format!("{value:e}");
    }
    let decimals = (15 - (size.log10().floor() as i32 + 1)).clamp(0, 17) as usize;
    let text = format!("{value:.decimals$}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" { "0" } else { text }.to_owned()
}

/// An Excel date, time or duration as text: "2026-09-30", "14:05",
/// "2026-09-30 14:05", or "36:15:00" for a duration. A day number
/// outside Excel's calendar shows as the number.
fn date(at: &ExcelDateTime) -> String {
    let value = at.as_f64();
    if at.is_duration() {
        if !value.is_finite() || value.abs() > LAST_EXCEL_DAY {
            return number(value);
        }
        let seconds = (value * 86_400.0).round() as i64;
        let sign = if seconds < 0 { "-" } else { "" };
        let seconds = seconds.abs();
        return format!(
            "{sign}{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
    }
    if !(0.0..LAST_EXCEL_DAY).contains(&value) {
        return number(value);
    }
    let (year, month, day, hour, minute, second, _) = at.to_ymd_hms_milli();
    let time = if second == 0 {
        format!("{hour:02}:{minute:02}")
    } else {
        format!("{hour:02}:{minute:02}:{second:02}")
    };
    if value < 1.0 {
        time
    } else if value.fract() == 0.0 {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02} {time}")
    }
}

/// A cell of a sample workbook.
#[cfg(any(test, feature = "demo"))]
#[derive(Clone, Copy, Debug)]
pub enum SampleCell<'a> {
    Text(&'a str),
    Number(f64),
    /// An Excel day number, shown as a date.
    Date(f64),
    Empty,
}

/// A small Excel workbook of `sheets` (a name and rows of cells), for the
/// tests and the demo: the least of the format calamine reads.
#[cfg(any(test, feature = "demo"))]
pub fn sample_workbook(sheets: &[(&str, &[&[SampleCell<'_>]])]) -> Vec<u8> {
    use std::fmt::Write as _;
    use std::io::Write as _;

    let escape = crate::text::xml_escape;
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut types = String::new();
    let mut listed = String::new();
    let mut links = String::new();
    for (index, (name, rows)) in sheets.iter().enumerate() {
        let n = index + 1;
        let mut data = String::new();
        for (r, row) in rows.iter().enumerate() {
            let _ = write!(data, r#"<row r="{}">"#, r + 1);
            for (c, cell) in row.iter().enumerate() {
                let at = format!("{}{}", super::column_name(c), r + 1);
                let _ = match cell {
                    SampleCell::Text(text) => write!(
                        data,
                        r#"<c r="{at}" t="inlineStr"><is><t>{}</t></is></c>"#,
                        escape(text)
                    ),
                    SampleCell::Number(v) => write!(data, r#"<c r="{at}"><v>{v}</v></c>"#),
                    SampleCell::Date(v) => write!(data, r#"<c r="{at}" s="1"><v>{v}</v></c>"#),
                    SampleCell::Empty => Ok(()),
                };
            }
            data.push_str("</row>");
        }
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData></worksheet>"#
            ),
        ));
        let _ = write!(
            types,
            r#"<Override PartName="/xl/worksheets/sheet{n}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        );
        let _ = write!(
            listed,
            r#"<sheet name="{}" sheetId="{n}" r:id="rId{n}"/>"#,
            escape(name)
        );
        let _ = write!(
            links,
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
        );
    }
    let styles = r#"<?xml version="1.0" encoding="UTF-8"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14" applyNumberFormat="1"/></cellXfs></styleSheet>"#;
    let style_link = r#"<Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#;
    parts.extend([
        (
            "[Content_Types].xml".to_owned(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>{types}</Types>"#
            ),
        ),
        (
            "_rels/.rels".to_owned(),
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_owned(),
        ),
        (
            "xl/workbook.xml".to_owned(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{listed}</sheets></workbook>"#
            ),
        ),
        (
            "xl/_rels/workbook.xml.rels".to_owned(),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{links}{style_link}</Relationships>"#
            ),
        ),
        ("xl/styles.xml".to_owned(), styles.to_owned()),
    ]);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, contents) in &parts {
        // Writing to memory cannot fail; a broken sample shows as an
        // unreadable file, never a crash.
        if zip.start_file(name.as_str(), options).is_err()
            || zip.write_all(contents.as_bytes()).is_err()
        {
            return Vec::new();
        }
    }
    zip.finish().map(Cursor::into_inner).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::MAX_COLUMNS;
    use super::super::archive::tests::zip_of;
    use super::SampleCell::{Date, Empty, Number, Text};
    use super::*;

    #[test]
    fn a_workbook_reads_with_its_sheets_and_formats() {
        let bytes = sample_workbook(&[
            (
                "Budget",
                &[
                    &[Text("Item"), Text("Cost"), Text("Due")],
                    &[Text("Build machines"), Number(4200.0), Date(46_295.0)],
                    &[Text("Coffee & tea"), Number(12.5), Date(46_295.5)],
                    &[Empty, Number(0.1 + 0.2)],
                ],
            ),
            ("Notes", &[&[Empty, Text("far right")]]),
        ]);
        let (sheets, notes) = read(&bytes).expect("workbook");
        assert!(notes.is_empty());
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].name, "Budget");
        assert_eq!(sheets[0].rows[0], vec!["Item", "Cost", "Due"]);
        assert_eq!(
            sheets[0].rows[1],
            vec!["Build machines", "4200", "2026-09-30"]
        );
        assert_eq!(
            sheets[0].rows[2],
            vec!["Coffee & tea", "12.5", "2026-09-30 12:00"]
        );
        assert_eq!(sheets[0].rows[3], vec!["", "0.3"]);
        assert_eq!(sheets[0].columns, 3);
        assert_eq!(sheets[1].rows[0], vec!["", "far right"]);
    }

    #[test]
    fn numbers_read_as_a_spreadsheet_shows_them() {
        assert_eq!(number(42.0), "42");
        assert_eq!(number(-3.25), "-3.25");
        assert_eq!(number(1.0 / 3.0), "0.333333333333333");
        assert_eq!(number(0.1 + 0.2), "0.3");
        assert_eq!(number(1e20), "1e20");
        assert_eq!(number(f64::NAN), "NaN");
        let at = |v, kind| ExcelDateTime::new(v, kind, false);
        use calamine::ExcelDateTimeType::{DateTime, TimeDelta};
        assert_eq!(date(&at(0.5, DateTime)), "12:00");
        assert_eq!(date(&at(1.5, TimeDelta)), "36:00:00");
        assert_eq!(date(&at(1e300, DateTime)), "1e300");
        assert_eq!(date(&at(-5.0, DateTime)), "-5");
    }

    #[test]
    fn far_cells_stay_within_the_caps() {
        // One sheet whose few cells reach far past the caps: only what fits
        // is kept, and nothing is allocated for the gap.
        let mut wide = vec![Empty; MAX_COLUMNS + 3];
        wide[0] = Text("a1");
        wide[MAX_COLUMNS + 2] = Text("too far right");
        let row: &[SampleCell<'_>] = &wide;
        let bytes = sample_workbook(&[("Wide", &[row])]);
        let (sheets, _) = read(&bytes).expect("wide");
        assert_eq!(sheets[0].columns, 1);
        assert_eq!(sheets[0].notes, vec![Note::Columns { shown: MAX_COLUMNS }]);

        let sheet = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="1048576"><c r="XFD1048576"><v>2</v></c></row></sheetData></worksheet>"#;
        let mut bytes = sample_workbook(&[("Far", &[&[Number(1.0)]])]);
        bytes = replace_part(&bytes, "xl/worksheets/sheet1.xml", sheet.as_bytes());
        let (sheets, _) = read(&bytes).expect("far corner");
        assert_eq!(sheets[0].rows.len(), 1);
        assert_eq!(sheets[0].notes, vec![Note::Rows { shown: 1 }]);
    }

    #[test]
    fn a_huge_cell_is_cut() {
        let huge = "word ".repeat(10_000);
        let bytes = sample_workbook(&[("S", &[&[Text(&huge)]])]);
        let (sheets, _) = read(&bytes).expect("huge cell");
        assert_eq!(
            sheets[0].rows[0][0].chars().count(),
            super::super::MAX_CELL_CHARS + 1
        );
        assert_eq!(sheets[0].notes, vec![Note::LongCells { count: 1 }]);
    }

    /// `book` with its part `name` swapped for `contents`.
    fn replace_part(book: &[u8], name: &str, contents: &[u8]) -> Vec<u8> {
        let mut zip = zip::ZipArchive::new(Cursor::new(book)).expect("zip");
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        for index in 0..zip.len() {
            let mut part = zip.by_index(index).expect("part");
            let mut data = Vec::new();
            part.read_to_end(&mut data).expect("read");
            let part_name = part.name().to_owned();
            if part_name == name {
                data = contents.to_vec();
            }
            parts.push((part_name, data));
        }
        let parts: Vec<(&str, &[u8])> = parts
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        zip_of(&parts)
    }

    #[test]
    fn a_bomb_is_refused_before_it_is_read() {
        // A sheet of 30 MB of blanks packs into a few kilobytes: far past the
        // ratio a real workbook has.
        let bytes = sample_workbook(&[("S", &[&[Number(1.0)]])]);
        let blanks = vec![b' '; 30 * 1024 * 1024];
        let bomb = replace_part(&bytes, "xl/worksheets/sheet1.xml", &blanks);
        assert_eq!(read(&bomb), Err(Failure::Bomb));
    }

    #[test]
    fn a_part_unpacking_to_more_than_it_claims_is_refused() {
        let bytes = sample_workbook(&[("S", &[&[Number(1.0)]])]);
        let mut lying = replace_part(&bytes, "xl/styles.xml", &[b'x'; 5000]);
        // Shrink the declared size of the part in its local header and in
        // the central directory: both follow the name's signature.
        let name = b"xl/styles.xml";
        let mut at = 0;
        let mut patched = 0;
        while let Some(found) = lying[at..]
            .windows(name.len())
            .position(|w| w == name)
            .map(|p| p + at)
        {
            // The uncompressed size sits 4 bytes before the name length's
            // field in a local header (at name - 30 + 22), and at name - 46
            // + 24 in the directory.
            let signature = |back: usize| found.checked_sub(back);
            for (back, size_at) in [(30, 22), (46, 24)] {
                if let Some(start) = signature(back)
                    && lying.get(start..start + 2) == Some(&[0x50, 0x4b][..])
                {
                    lying[start + size_at..start + size_at + 4]
                        .copy_from_slice(&100u32.to_le_bytes());
                    patched += 1;
                }
            }
            at = found + 1;
        }
        assert_eq!(patched, 2);
        assert_eq!(read(&lying), Err(Failure::Bomb));
    }

    #[test]
    fn a_huge_shared_string_count_is_refused() {
        let bytes = sample_workbook(&[("S", &[&[Number(1.0)]])]);
        let sst = br#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="99999999999999"><si><t>a</t></si></sst>"#;
        let mut zip = zip::ZipArchive::new(Cursor::new(&bytes[..])).expect("zip");
        let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
        for index in 0..zip.len() {
            let mut part = zip.by_index(index).expect("part");
            let mut data = Vec::new();
            part.read_to_end(&mut data).expect("read");
            parts.push((part.name().to_owned(), data));
        }
        parts.push(("xl/sharedStrings.xml".into(), sst.to_vec()));
        let parts: Vec<(&str, &[u8])> = parts
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        assert_eq!(read(&zip_of(&parts)), Err(Failure::ViewTooLarge));
    }

    /// An OpenDocument spreadsheet whose `content.xml` body is `table`.
    fn ods(table: &str) -> Vec<u8> {
        let content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet>{table}</office:spreadsheet></office:body></office:document-content>"#
        );
        zip_of(&[
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.spreadsheet",
            ),
            ("content.xml", content.as_bytes()),
            (
                "META-INF/manifest.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/></manifest:manifest>"#,
            ),
        ])
    }

    #[test]
    fn an_open_document_sheet_reads() {
        let bytes = ods(
            r#"<table:table table:name="Plan"><table:table-row><table:table-cell office:value-type="string"><text:p>Week</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="3"><text:p>3</text:p></table:table-cell></table:table-row><table:table-row table:number-rows-repeated="2"><table:table-cell/></table:table-row><table:table-row><table:table-cell/><table:table-cell office:value-type="float" office:value="2.5"/></table:table-row></table:table>"#,
        );
        let (sheets, notes) = read(&bytes).expect("ods");
        assert!(notes.is_empty());
        assert_eq!(sheets[0].name, "Plan");
        assert_eq!(sheets[0].rows[0], vec!["Week", "3"]);
        assert_eq!(sheets[0].rows[3], vec!["", "2.5"]);
    }

    #[test]
    fn open_document_repeats_cannot_blow_up() {
        // A few hundred bytes that spell out a million rows of 16,384
        // filled cells each.
        let bytes = ods(
            r#"<table:table table:name="Bomb"><table:table-row table:number-rows-repeated="1048576"><table:table-cell table:number-columns-repeated="16384" office:value-type="float" office:value="1"/></table:table-row></table:table>"#,
        );
        assert_eq!(read(&bytes), Err(Failure::ViewTooLarge));
        let odd = ods(
            r#"<table:table table:name="S"><table:table-row><table:table-cell office:value-type="float" office:value="infinity"/></table:table-row></table:table>"#,
        );
        assert!(matches!(read(&odd), Err(Failure::Unreadable(_))));
    }

    #[test]
    fn plain_numbers_are_told_apart() {
        for good in ["1", "-2.5", "+3e10", ".5", "7.", "1E-3"] {
            assert!(plain_number(good.as_bytes()), "{good}");
        }
        for bad in ["", "-", "inf", "NaN", "1e", "1.2.3", "0x10", "1e+"] {
            assert!(!plain_number(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn damaged_workbooks_are_refused_without_panicking() {
        let good = sample_workbook(&[("S", &[&[Number(1.0)]])]);
        assert!(matches!(read(b"not a zip"), Err(Failure::Unreadable(_))));
        assert!(matches!(
            read(&zip_of(&[("hello.txt", b"hi")])),
            Err(Failure::Unreadable(_))
        ));
        assert!(matches!(
            read(&zip_of(&[("xl/workbook.bin", b"\0")])),
            Err(Failure::Unreadable(_))
        ));
        // Cut anywhere, flipped anywhere: an error or a sheet, never a panic.
        for end in (0..good.len()).step_by(97) {
            let _ = read(&good[..end]);
        }
        for at in (0..good.len()).step_by(13) {
            let mut flipped = good.clone();
            flipped[at] ^= 0x5a;
            let _ = read(&flipped);
        }
        let broken = replace_part(
            &good,
            "xl/worksheets/sheet1.xml",
            b"<worksheet><sheetData><row",
        );
        let _ = read(&broken);
        let broken = replace_part(&good, "xl/workbook.xml", b"<<<");
        assert!(read(&broken).is_err());
    }
}
