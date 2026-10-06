//! CSV and TSV files, read into one table.

use super::{Grid, MAX_CELLS, Sheet, unreadable};
use crate::failure::Failure;

/// The separators a CSV file may use, most likely first.
const SEPARATORS: [u8; 4] = *b",;\t|";
/// How many lines are looked at to guess the separator.
const SNIFF_LINES: usize = 20;

/// The table in a CSV or TSV file, separated by `separator`, or by the one
/// [`sniff`] guesses.
pub(super) fn read(bytes: &[u8], separator: Option<u8>) -> Result<Sheet, Failure> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if bytes[..bytes.len().min(8 * 1024)].contains(&0) {
        return Err(Failure::NotText);
    }
    let separator = separator.unwrap_or_else(|| sniff(bytes));
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(separator)
        .from_reader(bytes);
    let mut grid = Grid::default();
    let mut budget = MAX_CELLS;
    let mut record = csv::ByteRecord::new();
    let mut row = 0;
    'rows: loop {
        match reader.read_byte_record(&mut record) {
            Ok(true) => {}
            Ok(false) => break,
            // The rows before a damaged one are still worth seeing.
            Err(error) if row == 0 => return Err(unreadable(error)),
            Err(_) => break,
        }
        for (column, field) in record.iter().enumerate() {
            if !grid.put(row, column, &String::from_utf8_lossy(field), &mut budget) {
                break 'rows;
            }
        }
        row += 1;
    }
    Ok(grid.finish(String::new()))
}

/// The separator of a CSV file: of comma, semicolon, tab and bar, the one
/// found the same number of times (outside quotes) on most of its first
/// lines, more than once if it can. Comma when nothing stands out.
pub(super) fn sniff(bytes: &[u8]) -> u8 {
    let lines: Vec<&[u8]> = bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .take(SNIFF_LINES)
        .collect();
    let mut best = (b',', 0usize, 0usize);
    for separator in SEPARATORS {
        let counts: Vec<usize> = lines.iter().map(|line| count(line, separator)).collect();
        let Some(&first) = counts.first() else {
            continue;
        };
        if first == 0 {
            continue;
        }
        let agreeing = counts.iter().filter(|&&c| c == first).count();
        if (agreeing, first) > (best.1, best.2) {
            best = (separator, agreeing, first);
        }
    }
    best.0
}

/// How many times `separator` is in `line`, outside quoted fields.
fn count(line: &[u8], separator: u8) -> usize {
    let mut quoted = false;
    let mut found = 0;
    for &b in line {
        if b == b'"' {
            quoted = !quoted;
        } else if b == separator && !quoted {
            found += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_COLUMNS, MAX_ROWS, Note};
    use super::*;

    #[test]
    fn the_separator_is_sniffed() {
        assert_eq!(sniff(b"a,b,c\n1,2,3\n"), b',');
        assert_eq!(
            sniff(b"naam;bedrag\n\"Jansen, P.\";12,50\nVries;3,00\n"),
            b';'
        );
        assert_eq!(sniff(b"a\tb\n1\t2\n"), b'\t');
        assert_eq!(sniff(b"a|b|c\n1|2|3\n"), b'|');
        assert_eq!(sniff(b"just one column\nhere\n"), b',');
        assert_eq!(sniff(b""), b',');
    }

    #[test]
    fn csv_reads_into_a_grid() {
        let sheet = read(
            b"\xEF\xBB\xBFname,amount,note\n\"Lima, Ana\",12.5,\"two\nlines\"\nBo,3\n",
            None,
        )
        .expect("csv");
        assert_eq!(sheet.columns, 3);
        assert_eq!(sheet.rows[0], vec!["name", "amount", "note"]);
        assert_eq!(sheet.rows[1], vec!["Lima, Ana", "12.5", "two lines"]);
        assert_eq!(sheet.rows[2], vec!["Bo", "3"]);
        assert!(sheet.notes.is_empty());

        let tsv = read(b"a\tb,c\n", Some(b'\t')).expect("tsv");
        assert_eq!(tsv.rows[0], vec!["a", "b,c"]);
    }

    #[test]
    fn too_many_rows_or_columns_are_cut() {
        let tall = "x\n".repeat(MAX_ROWS + 5);
        let sheet = read(tall.as_bytes(), None).expect("tall");
        assert_eq!(sheet.rows.len(), MAX_ROWS);
        assert_eq!(sheet.notes, vec![Note::Rows { shown: MAX_ROWS }]);

        let wide = vec!["c"; MAX_COLUMNS + 20].join(",");
        let sheet = read(wide.as_bytes(), Some(b',')).expect("wide");
        assert_eq!(sheet.columns, MAX_COLUMNS);
        assert_eq!(sheet.notes, vec![Note::Columns { shown: MAX_COLUMNS }]);
    }

    #[test]
    fn a_binary_file_is_not_csv() {
        assert_eq!(read(b"PK\x03\x04\0\0", None), Err(Failure::NotText));
    }

    #[test]
    fn odd_csv_never_panics() {
        for input in [
            &b"\"unclosed,quote\n1,2"[..],
            b"\"\"\"",
            b",,,\n,,",
            b"\r\r\r",
            b"\n\n",
        ] {
            let _ = read(input, None);
        }
    }
}
