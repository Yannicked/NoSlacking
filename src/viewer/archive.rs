//! Zip archives: listed from their central directory, never unpacked.

use std::io::Cursor;

use super::{Archive, Entry, MAX_ENTRIES, Note, unreadable};
use crate::failure::Failure;

/// The most entries an archive may declare before it is refused: the
/// zip reader keeps every one in memory, listed or not.
const MAX_DIRECTORY: u64 = 100_000;
/// An archive unpacking to more than this many times its size gets a
/// warning: fine to list, not to unpack without care.
const WARN_RATIO: u64 = 100;

/// The listing of a zip archive, and what is worth saying about it.
pub(super) fn read(bytes: &[u8]) -> Result<(Archive, Vec<Note>), Failure> {
    let declared = declared_entries(bytes).ok_or_else(|| unreadable("not a zip archive"))?;
    if declared > MAX_DIRECTORY {
        return Err(Failure::ViewTooLarge);
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(unreadable)?;
    let count = zip.len();
    let mut archive = Archive {
        count,
        ..Archive::default()
    };
    for index in 0..count {
        // Raw: the entry's header is read, its contents never are.
        let read = zip.by_index_raw(index).map(|file| Entry {
            path: file.name().to_owned(),
            size: file.size(),
            packed: file.compressed_size(),
            modified: file.last_modified().map(|at| {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    at.year(),
                    at.month(),
                    at.day(),
                    at.hour(),
                    at.minute()
                )
            }),
            folder: file.is_dir(),
            encrypted: file.encrypted(),
        });
        // A damaged entry is still listed by name.
        let entry = read.unwrap_or_else(|_| Entry {
            path: zip.name_for_index(index).unwrap_or_default().to_owned(),
            ..Entry::default()
        });
        archive.unpacked = archive.unpacked.saturating_add(entry.size);
        archive.packed = archive.packed.saturating_add(entry.packed);
        if archive.entries.len() < MAX_ENTRIES {
            archive.entries.push(entry);
        }
    }
    let mut notes = Vec::new();
    if count > MAX_ENTRIES {
        notes.push(Note::Entries {
            shown: MAX_ENTRIES,
            total: count,
        });
    }
    let ratio = archive.unpacked / archive.packed.max(1);
    if ratio > WARN_RATIO && archive.unpacked > 1024 * 1024 {
        notes.push(Note::Packed { ratio });
    }
    Ok((archive, notes))
}

/// How many entries a zip archive says it has, from its end of central
/// directory record (and the zip64 one it points to), or `None` when it
/// has none. Read before the zip reader is, which believes the count.
pub(super) fn declared_entries(bytes: &[u8]) -> Option<u64> {
    const END: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    const LOCATOR: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];
    const END64: [u8; 4] = [0x50, 0x4b, 0x06, 0x06];
    let u16_at = |at: usize| -> Option<u64> {
        let b = bytes.get(at..at + 2)?;
        Some(u64::from(u16::from_le_bytes([b[0], b[1]])))
    };
    let u64_at = |at: usize| -> Option<u64> {
        let b: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
        Some(u64::from_le_bytes(b))
    };
    // The record is 22 bytes and a comment of up to 64 KB, at the end.
    let earliest = bytes.len().saturating_sub(22 + 65_535);
    let end = (earliest..bytes.len().saturating_sub(21))
        .rev()
        .find(|&at| bytes.get(at..at + 4) == Some(&END[..]))?;
    let count = u16_at(end + 10)?;
    if count != 0xFFFF {
        return Some(count);
    }
    // Zip64: a locator just before the record gives where its own end
    // record is, which holds the real count.
    let locator = end.checked_sub(20)?;
    if bytes.get(locator..locator + 4) != Some(&LOCATOR[..]) {
        return Some(count);
    }
    let end64 = usize::try_from(u64_at(locator + 8)?).ok()?;
    if bytes.get(end64..end64 + 4) != Some(&END64[..]) {
        return None;
    }
    u64_at(end64 + 32)
}

#[cfg(test)]
pub(super) mod tests {
    use std::io::Write as _;

    use super::*;

    /// A zip archive of `files` (name and contents), deflated.
    pub(in crate::viewer) fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, contents) in files {
            if name.ends_with('/') {
                zip.add_directory(*name, options).expect("folder");
            } else {
                zip.start_file(*name, options).expect("file");
                zip.write_all(contents).expect("write");
            }
        }
        zip.finish().expect("finish").into_inner()
    }

    #[test]
    fn an_archive_is_listed() {
        let bytes = zip_of(&[
            ("logs/", b""),
            ("logs/today.log", b"all quiet\n"),
            ("readme.md", &b"# Hi\n".repeat(50)),
        ]);
        assert_eq!(declared_entries(&bytes), Some(3));
        let (archive, notes) = read(&bytes).expect("zip");
        assert_eq!(archive.count, 3);
        let paths: Vec<&str> = archive.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["logs/", "logs/today.log", "readme.md"]);
        assert!(archive.entries[0].folder);
        assert_eq!(archive.entries[2].size, 250);
        assert!(archive.entries[2].packed < 250);
        assert!(archive.entries[1].modified.is_some());
        assert_eq!(archive.unpacked, 260);
        assert!(notes.is_empty());
    }

    #[test]
    fn a_bomb_is_listed_with_a_warning_and_never_unpacked() {
        // Eight megabytes of zeros pack into a few kilobytes.
        let zeros = vec![0u8; 8 * 1024 * 1024];
        let bytes = zip_of(&[("zeros.bin", &zeros)]);
        let (archive, notes) = read(&bytes).expect("listing is safe");
        assert_eq!(archive.unpacked, 8 * 1024 * 1024);
        assert!(matches!(notes[..], [Note::Packed { ratio }] if ratio > WARN_RATIO));
    }

    #[test]
    fn a_huge_declared_directory_is_refused() {
        // An end record claiming 0xFFFE entries; no directory follows.
        let mut bytes = vec![0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0];
        bytes.extend(0xFFFEu16.to_le_bytes());
        bytes.extend(0xFFFEu16.to_le_bytes());
        bytes.extend([0; 10]);
        assert_eq!(declared_entries(&bytes), Some(0xFFFE));
        assert!(read(&bytes).is_err());
        // The zip64 record can claim far more.
        let mut big = Vec::new();
        big.extend([0x50, 0x4b, 0x06, 0x06]);
        big.extend([0; 28]);
        big.extend(u64::MAX.to_le_bytes());
        big.extend([0; 16]);
        let locator = big.len();
        big.extend([0x50, 0x4b, 0x06, 0x07, 0, 0, 0, 0]);
        big.extend(0u64.to_le_bytes());
        big.extend([1, 0, 0, 0]);
        assert_eq!(locator, 56);
        big.extend([0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        big.extend([0; 10]);
        assert_eq!(declared_entries(&big), Some(u64::MAX));
        assert_eq!(read(&big), Err(Failure::ViewTooLarge));
    }

    #[test]
    fn junk_is_not_an_archive() {
        for junk in [&b""[..], b"PK", b"hello world", &[0x50, 0x4b, 0x05, 0x06]] {
            assert!(
                matches!(read(junk), Err(Failure::Unreadable(_))),
                "{junk:?}"
            );
        }
        // A real archive cut short loses its directory.
        let bytes = zip_of(&[("a.txt", b"a")]);
        assert!(read(&bytes[..bytes.len() - 10]).is_err());
    }
}
