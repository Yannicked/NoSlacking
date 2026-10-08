//! Small text helpers shared across the app.

/// `text` safe inside an XML or HTML element or a quoted attribute: `&`,
/// `<`, `>` and both quotes become entities, so nothing in it can close
/// the element or attribute it sits in.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_covers_markup_and_quotes() {
        assert_eq!(
            xml_escape(r#"<a href="x">Tom & Jerry's</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&#39;s&lt;/a&gt;"
        );
        assert_eq!(xml_escape("plain 👍"), "plain 👍");
    }
}
