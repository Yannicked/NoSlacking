//! Slash commands typed in the composer: `/shrug`, `/status`, `/remind`…
//!
//! A message that starts with `/` and a word is a command, as in Slack.
//! The ones with a Web API method of their own run through it, which
//! works with any sign-in; the rest go to `chat.command`, which runs
//! Slack's own command (an app's too) but only takes a browser session's
//! token. `/shrug` needs no Slack at all.

/// A command NoSlacking knows by name, for the suggestions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Known {
    /// Without the slash.
    pub name: &'static str,
    /// What follows the name, as Slack's help writes it.
    pub usage: &'static str,
}

/// The commands suggested while typing a `/` word. Others still run
/// through `chat.command` when typed in full.
pub const KNOWN: &[Known] = &[
    Known {
        name: "me",
        usage: "[text]",
    },
    Known {
        name: "shrug",
        usage: "[message]",
    },
    Known {
        name: "status",
        usage: "[:emoji:] [text] | clear",
    },
    Known {
        name: "away",
        usage: "",
    },
    Known {
        name: "active",
        usage: "",
    },
    Known {
        name: "topic",
        usage: "[text]",
    },
    Known {
        name: "invite",
        usage: "@person",
    },
    Known {
        name: "leave",
        usage: "",
    },
    Known {
        name: "remind",
        usage: "[me|@person] [what] [when]",
    },
];

/// A command and what follows it: `/status :palm_tree: away` is
/// `("status", ":palm_tree: away")`. `None` for a message that is not a
/// command, including `/` alone, `//` and paths like `/usr/bin`.
pub fn parse(text: &str) -> Option<(String, &str)> {
    let rest = text.strip_prefix('/')?;
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let name = &rest[..end];
    let valid = !name.is_empty()
        && name.starts_with(|c: char| c.is_alphabetic())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    valid.then(|| (name.to_lowercase(), rest[end..].trim()))
}

/// The commands whose name starts with what was typed after the `/`.
pub fn matching(typed: &str) -> impl Iterator<Item = &'static Known> + '_ {
    let typed = typed.to_lowercase();
    KNOWN.iter().filter(move |k| k.name.starts_with(&typed))
}

/// `/shrug`'s message: what was typed, then the shrug.
pub fn shrug(text: &str) -> String {
    const SHRUG: &str = r"¯\_(ツ)_/¯";
    if text.is_empty() {
        SHRUG.to_owned()
    } else {
        format!("{text} {SHRUG}")
    }
}

/// `/status`'s emoji and text: `:palm_tree: on holiday` is
/// `(":palm_tree:", "on holiday")`. `clear` (or nothing) clears both.
pub fn status(text: &str) -> (String, String) {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("clear") {
        return (String::new(), String::new());
    }
    if let Some(rest) = text.strip_prefix(':')
        && let Some((emoji, after)) = rest.split_once(':')
        && !emoji.is_empty()
        && !emoji.contains(char::is_whitespace)
    {
        return (format!(":{emoji}:"), after.trim().to_owned());
    }
    (String::new(), text.to_owned())
}

/// The people mentioned in a command's wire text (`<@U1> <@U2|ann>`).
pub fn mentioned(wire: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = wire;
    while let Some(at) = rest.find("<@") {
        rest = &rest[at + 2..];
        let end = rest.find('>').unwrap_or(rest.len());
        let id = rest[..end].split('|').next().unwrap_or_default();
        if !id.is_empty() && !found.iter().any(|f| f == id) {
            found.push(id.to_owned());
        }
        rest = &rest[end..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_slash_and_a_word_is_a_command() {
        assert_eq!(parse("/shrug ok then"), Some(("shrug".into(), "ok then")));
        assert_eq!(parse("/AWAY"), Some(("away".into(), "")));
        assert_eq!(parse("/giphy  cats \n"), Some(("giphy".into(), "cats")));
        for not in [
            "/",
            "// comment",
            "/ x",
            "/9lives",
            "/usr/bin is here",
            "a /me",
            "",
        ] {
            assert_eq!(parse(not), None, "{not:?}");
        }
    }

    #[test]
    fn suggestions_follow_the_typed_prefix() {
        let names: Vec<&str> = matching("").map(|k| k.name).collect();
        assert_eq!(names.len(), KNOWN.len());
        let names: Vec<&str> = matching("A").map(|k| k.name).collect();
        assert_eq!(names, ["away", "active"]);
        assert_eq!(matching("zzz").count(), 0);
    }

    #[test]
    fn shrugs_and_statuses() {
        assert_eq!(shrug(""), r"¯\_(ツ)_/¯");
        assert_eq!(shrug("fine"), r"fine ¯\_(ツ)_/¯");
        assert_eq!(
            status(":palm_tree: on holiday"),
            (":palm_tree:".into(), "on holiday".into())
        );
        assert_eq!(
            status("in a meeting"),
            (String::new(), "in a meeting".into())
        );
        assert_eq!(status(" Clear "), (String::new(), String::new()));
        assert_eq!(status(":: odd"), (String::new(), ":: odd".into()));
        assert_eq!(status(":a b: x"), (String::new(), ":a b: x".into()));
    }

    #[test]
    fn mentioned_people_come_from_the_wire_text() {
        assert_eq!(
            mentioned("<@U1> and <@U2|ann>, <@U1> again <#C1>"),
            ["U1", "U2"]
        );
        assert!(mentioned("nobody <@").is_empty());
    }
}
