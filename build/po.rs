// Shape checks for the `.po` catalogs, shared by `build.rs` (which stops
// the build on a bad catalog) and the tests in `src/i18n.rs` (which try it
// on samples). Plain comments, not `//!`: the tests `include!` this file,
// where an inner doc comment is not allowed.

/// The lines (counted from 1) where a `#` comment follows an entry with no
/// blank line between them. The catalog compiler takes such a comment as
/// part of the entry above, so the comment's own entry merges into it and
/// one translation silently goes missing.
pub fn glued_comments(po: &str) -> Vec<usize> {
    let mut glued = Vec::new();
    let mut previous: Option<&str> = None;
    for (index, line) in po.lines().enumerate() {
        let line = line.trim_end();
        if line.starts_with('#')
            && previous.is_some_and(|above| !above.is_empty() && !above.starts_with('#'))
        {
            glued.push(index + 1);
        }
        previous = Some(line);
    }
    glued
}
