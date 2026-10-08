//! Generates src/emoji_table.rs, Slack's emoji names, from iamcal's
//! emoji-data, the table Slack names its emoji after:
//! `emoji-table-gen [emoji.json] > src/emoji_table.rs`. Without a file it
//! downloads emoji.json at the pinned commit.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The emoji-data commit the checked-in table comes from (Unicode 17).
const COMMIT: &str = "13ee711e222ea17fe537bfea953c687866f16411";

/// One emoji of emoji.json, the fields the table needs.
#[derive(serde::Deserialize)]
struct Emoji {
    /// Its code points in hex, fully qualified: `1F170-FE0F`.
    unified: String,
    /// Its names, the one Slack writes first.
    short_names: Vec<String>,
}

fn main() {
    let json = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(&path).unwrap_or_else(|error| {
            fail(&format!("{path}: {error}"));
        }),
        None => download(),
    };
    let emoji: Vec<Emoji> =
        serde_json::from_str(&json).unwrap_or_else(|error| fail(&format!("emoji.json: {error}")));
    match table(&emoji) {
        Ok(table) => print!("{table}"),
        Err(why) => fail(&why),
    }
}

/// emoji.json at [`COMMIT`].
fn download() -> String {
    let url = format!("https://raw.githubusercontent.com/iamcal/emoji-data/{COMMIT}/emoji.json");
    eprintln!("downloading {url}");
    ureq::get(&url)
        .call()
        .and_then(|mut response| response.body_mut().read_to_string())
        .unwrap_or_else(|error| fail(&format!("{url}: {error}")))
}

fn fail(why: &str) -> ! {
    eprintln!("emoji-table-gen: {why}");
    std::process::exit(1);
}

/// `1F170-FE0F` as the text it stands for.
fn text(unified: &str) -> Result<String, String> {
    unified
        .split('-')
        .map(|hex| {
            u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| format!("not a code point: {hex} in {unified}"))
        })
        .collect()
}

/// The Rust source of the table: every name to its emoji, and every emoji
/// without variation selectors to its names.
fn table(emoji: &[Emoji]) -> Result<String, String> {
    let mut names = BTreeMap::new();
    let mut by_emoji = BTreeMap::new();
    for one in emoji {
        let text = text(&one.unified)?;
        for name in &one.short_names {
            if names.insert(name.as_str(), text.clone()).is_some() {
                return Err(format!("two emoji are called {name}"));
            }
        }
        let bare: String = text.chars().filter(|&c| c != '\u{FE0F}').collect();
        if by_emoji.insert(bare, &one.short_names).is_some() {
            return Err(format!("{} is in the table twice", one.unified));
        }
    }
    let mut out = format!(
        "// Generated from iamcal/emoji-data (emoji.json, MIT licensed: see\n\
         // assets/emoji-data-LICENSE.txt) at commit\n\
         // {COMMIT}\n\
         // by tools/emoji-table-gen. Slack names its emoji after this table.\n\
         // Do not edit; regenerate from the repository root with:\n\
         //\n\
         //   cargo run --manifest-path tools/emoji-table-gen/Cargo.toml > src/emoji_table.rs\n\
         \n\
         /// Every Slack shortcode and its emoji, sorted by name.\n\
         const NAMES: &[(&str, &str)] = &[\n"
    );
    for (name, text) in &names {
        let _ = writeln!(out, "    (\"{name}\", \"{text}\"),");
    }
    out.push_str(
        "];\n\
         \n\
         /// Each emoji (without variation selectors) and its Slack names, the\n\
         /// first being the one Slack writes, sorted by emoji.\n\
         const BY_EMOJI: &[(&str, &[&str])] = &[\n",
    );
    for (text, names) in &by_emoji {
        let names: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
        let _ = writeln!(out, "    (\"{text}\", &[{}]),", names.join(", "));
    }
    out.push_str("];\n");
    Ok(out)
}
