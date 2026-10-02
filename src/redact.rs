//! Keeps Slack's secrets out of the log and the panic log.
//!
//! Tokens carry a recognisable prefix (`xoxp-`, `xoxc-`, the `xoxd-`
//! cookie, …), and real-time socket URLs carry a ticket that works like a
//! password. A whitespace-separated word holding either is replaced. The
//! OAuth client secret has no prefix, so the types holding it print it as
//! `<redacted>` instead (see their `Debug` impls).

use std::borrow::Cow;

/// What a redacted word or field is shown as.
pub const REDACTED: &str = "<redacted>";

/// Every token prefix Slack issues: user, bot, rotating (`xoxe.` and
/// `xoxe-`), app-level, session (`xoxc-`, the `xoxd-` cookie) and the
/// legacy workspace, refresh and session tokens.
const PREFIXES: [&str; 9] = [
    "xoxp-", "xoxb-", "xoxe", "xapp-", "xoxc-", "xoxd-", "xoxa-", "xoxr-", "xoxs-",
];

/// Whether `word` holds a token or a socket URL.
pub fn is_secret(word: &str) -> bool {
    PREFIXES.iter().any(|prefix| word.contains(prefix)) || word.contains("wss://")
}

/// `message` with every secret word replaced, or `None` when it has none
/// (the common case, which then costs no allocation).
pub fn tokens(message: &str) -> Option<String> {
    message.split_whitespace().any(is_secret).then(|| {
        fastframe_log::redact::words(message, is_secret)
            .replace(fastframe_log::redact::LINK, REDACTED)
    })
}

/// The log filter: tokens never reach the log, whatever logs them.
pub fn log_record(_record: &log::Record<'_>, message: &str) -> Option<Cow<'static, str>> {
    tokens(message).map(Cow::Owned)
}

/// A panic message with its links and secrets removed, for the panic log.
pub fn panic_message(text: &str) -> String {
    fastframe_log::redact::words(text, |word| {
        fastframe_log::redact::is_link(word) || is_secret(word)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_shape_is_caught() {
        for secret in [
            "xoxp-1-2-3",
            "xoxb-1-2",
            "xoxe.xoxp-1-abc",
            "xoxe-1-abc",
            "xapp-1-A1-2-abc",
            "xoxc-123",
            "xoxd-abc%2F",
            "xoxa-2-abc",
            "xoxr-abc",
            "xoxs-abc",
            "wss://wss-primary.slack.com/link/?ticket=abc",
        ] {
            let redacted = tokens(&format!("failed with {secret} today")).expect("redacted");
            assert!(!redacted.contains(secret), "{secret}");
            assert_eq!(redacted, "failed with <redacted> today");
        }
    }

    #[test]
    fn secrets_inside_other_text_are_caught() {
        for message in [
            r#"body {"ok":true,"access_token":"xoxp-1-2-3"}"#,
            "Cookie: d=xoxd-abc",
            "Authorization: Bearer xoxb-1-2",
        ] {
            let redacted = tokens(message).expect("redacted");
            assert!(!redacted.contains("xox"), "{redacted}");
        }
    }

    #[test]
    fn plain_messages_pass_untouched() {
        assert_eq!(tokens("loaded 42 messages in C123"), None);
        assert_eq!(tokens(""), None);
    }

    #[test]
    fn panics_lose_links_and_tokens() {
        assert_eq!(
            panic_message("called `Result::unwrap()` on xoxp-1 for https://files.slack.com/x"),
            "called `Result::unwrap()` on <link> for <link>"
        );
    }
}
