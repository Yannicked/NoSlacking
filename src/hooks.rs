//! Scripting hooks: run your own program on new messages that matter.
//!
//! In the spirit of wee-slack's hooks, and kept small and safe. Off by
//! default. Each hook names a program and what it fires on: mentions of
//! you, direct messages, or keywords. A new message that matches runs the
//! program with the message as JSON on its standard input (see
//! [`payload`]; the shape is documented in the README).
//!
//! - The program runs directly, never through a shell: its command line is
//!   split on spaces, with `"…"` or `'…'` around a part that has spaces.
//! - The payload never carries a token, a cookie or anything else secret.
//! - A run that takes longer than [`TIMEOUT`] is stopped, and at most
//!   [`MAX_RUNNING`] run at once; anything beyond that is skipped.
//! - Only failures are logged, never the message.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{ConversationKind, Message};

/// How long a hook may run before it is stopped.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// The most hooks running at once. A burst of messages must not start a
/// burst of processes.
pub const MAX_RUNNING: usize = 4;
/// The payload's format, raised when it changes in a way scripts notice.
pub const VERSION: u32 = 1;

/// The hooks, as saved in the settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hooks {
    /// Nothing runs unless this is on.
    pub enabled: bool,
    pub list: Vec<Hook>,
}

/// One program, and the messages it runs for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hook {
    /// The program and its arguments, as typed (see [`split`]).
    pub command: String,
    /// Runs for messages that mention you by name.
    pub mentions: bool,
    /// Runs for every message in a direct or group message.
    pub direct: bool,
    /// Runs for messages with one of these words, comma separated.
    pub keywords: String,
}

impl Hook {
    /// The keywords, one by one.
    pub fn keyword_list(&self) -> Vec<String> {
        self.keywords
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// Why a hook runs for a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    Mention,
    Direct,
    /// The keyword that matched.
    Keyword(String),
}

impl Reason {
    fn word(&self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Direct => "direct",
            Self::Keyword(_) => "keyword",
        }
    }
}

/// Splits a command line into the program and its arguments: on
/// whitespace, with `"…"` or `'…'` keeping spaces in one part. There are
/// no escapes, variables or globs; nothing here is a shell.
pub fn split(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => part.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    parts.push(std::mem::take(&mut part));
                    started = false;
                }
            }
            None => {
                part.push(c);
                started = true;
            }
        }
    }
    if started {
        parts.push(part);
    }
    parts
}

/// Why `hook` runs for `message`, in a conversation of `kind`, for you
/// (`me`); `plain` is the text without markup, for keywords. `None` when
/// it does not.
pub fn reason(
    hook: &Hook,
    kind: ConversationKind,
    message: &Message,
    plain: &str,
    me: &str,
) -> Option<Reason> {
    if hook.mentions && crate::notify::names_me(&message.text, me) {
        return Some(Reason::Mention);
    }
    if hook.direct && kind.is_dm() {
        return Some(Reason::Direct);
    }
    hook.keyword_list()
        .into_iter()
        .find(|k| crate::notify::has_keyword(plain, std::slice::from_ref(k)))
        .map(Reason::Keyword)
}

/// Where a message was posted, for [`payload`].
#[derive(Clone, Copy, Debug)]
pub struct Place<'a> {
    pub team: &'a str,
    pub team_name: &'a str,
    pub domain: &'a str,
    pub channel: &'a str,
    pub channel_name: &'a str,
    pub kind: ConversationKind,
}

/// The JSON a hook reads on its standard input. Only what the message
/// says and where it was posted: never a token, a cookie or a file URL
/// that would need one.
pub fn payload(
    place: &Place<'_>,
    message: &Message,
    author: &str,
    plain: &str,
    reason: &Reason,
) -> Value {
    let kind = match place.kind {
        ConversationKind::Channel => "channel",
        ConversationKind::Private => "private",
        ConversationKind::Direct => "direct",
        ConversationKind::Group => "group",
    };
    let thread = message.thread_ts.as_ref().filter(|t| **t != message.ts);
    let mut value = json!({
        "version": VERSION,
        "event": "message",
        "reason": reason.word(),
        "workspace": {
            "id": place.team,
            "name": place.team_name,
            "domain": place.domain,
        },
        "conversation": {
            "id": place.channel,
            "name": place.channel_name,
            "kind": kind,
        },
        "message": {
            "ts": message.ts.as_str(),
            "thread_ts": thread.map(crate::model::Ts::as_str),
            "user": message.user,
            "author": author,
            "text": message.text,
            "plain": plain,
        },
        "permalink": crate::links::permalink(place.domain, place.channel, &message.ts, thread),
    });
    if let Reason::Keyword(keyword) = reason {
        value["keyword"] = keyword.as_str().into();
    }
    value
}

/// How many hooks are running now, across the app.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// Runs `argv` with `input` on its standard input, on a thread of its own,
/// and stops it after [`TIMEOUT`]. Skipped when [`MAX_RUNNING`] already
/// run. Only failures are logged.
pub fn run(argv: Vec<String>, input: String) {
    let Some(program) = argv.first().cloned() else {
        return;
    };
    if RUNNING.fetch_add(1, Ordering::SeqCst) >= MAX_RUNNING {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        log::warn!("hook {program}: skipped, {MAX_RUNNING} hooks are still running");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("noslacking-hook".into())
        .spawn(move || {
            if let Err(error) = run_now(&argv, input) {
                log::warn!("hook {program}: {error}");
            }
            RUNNING.fetch_sub(1, Ordering::SeqCst);
        });
    if let Err(error) = spawned {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        log::warn!("could not start a thread for a hook: {error}");
    }
}

/// Runs one hook and waits for it, for at most [`TIMEOUT`].
fn run_now(argv: &[String], input: String) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("no program")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    // Written from a thread of its own: a program that never reads its
    // input must not keep the timeout from stopping it.
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            // A program may close its input early; that is its business.
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("ended with {status}")),
            Ok(None) if started.elapsed() >= TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("stopped after {} s", TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Delivery, Ts};

    fn message(text: &str) -> Message {
        Message {
            ts: Ts::new("1700000000.000100"),
            user: Some("U2".into()),
            username: None,
            bot_icon: None,
            bot_id: None,
            text: text.into(),
            thread_ts: None,
            reply_count: 0,
            replies_known: false,
            reply_users: Vec::new(),
            latest_reply: None,
            reactions: Vec::new(),
            files: Vec::new(),
            attachments: Vec::new(),
            blocks: Vec::new(),
            edited: false,
            subtype: None,
            delivery: Delivery::Sent,
            broadcast: false,
            pinned: false,
        }
    }

    #[test]
    fn command_lines_split_without_a_shell() {
        assert_eq!(split("notify-send  -u low"), ["notify-send", "-u", "low"]);
        assert_eq!(
            split(r#""/Applications/My Tool.app/bin/tool" --flag 'two words'"#),
            ["/Applications/My Tool.app/bin/tool", "--flag", "two words"]
        );
        assert_eq!(split(r#"say "" x"#), ["say", "", "x"]);
        // Shell syntax is just text.
        assert_eq!(
            split("a; rm -rf $HOME | b"),
            ["a;", "rm", "-rf", "$HOME", "|", "b"]
        );
        assert!(split("   ").is_empty());
    }

    #[test]
    fn hooks_fire_for_what_they_ask() {
        let hook = Hook {
            command: "x".into(),
            mentions: true,
            direct: false,
            keywords: "deploy, outage ,".into(),
        };
        let channel = ConversationKind::Channel;
        let mention = message("<@U1> look");
        assert_eq!(
            reason(&hook, channel, &mention, "@me look", "U1"),
            Some(Reason::Mention)
        );
        let deploy = message("Deploy done");
        assert_eq!(
            reason(&hook, channel, &deploy, "Deploy done", "U1"),
            Some(Reason::Keyword("deploy".into()))
        );
        let other = message("deployed");
        assert_eq!(reason(&hook, channel, &other, "deployed", "U1"), None);
        assert_eq!(
            reason(&hook, ConversationKind::Direct, &other, "deployed", "U1"),
            None,
            "direct messages only when asked"
        );
        let direct = Hook {
            direct: true,
            ..Hook::default()
        };
        assert_eq!(
            reason(&direct, ConversationKind::Group, &other, "deployed", "U1"),
            Some(Reason::Direct)
        );
        assert_eq!(reason(&Hook::default(), channel, &mention, "", "U1"), None);
    }

    #[test]
    fn the_payload_says_what_and_where_and_nothing_secret() {
        let mut reply = message("<@U1> deploy?");
        reply.thread_ts = Some(Ts::new("1699999999.000100"));
        reply.files.push(crate::model::File {
            id: "F1".into(),
            name: "a.png".into(),
            title: String::new(),
            mimetype: "image/png".into(),
            size: 1,
            url_private: Some("https://files.slack.com/secret".into()),
            download_url: None,
            thumb: None,
            thumb_size: None,
            permalink: None,
            ..Default::default()
        });
        let place = Place {
            team: "T1",
            team_name: "Acme",
            domain: "acme",
            channel: "C1",
            channel_name: "general",
            kind: ConversationKind::Channel,
        };
        let value = payload(
            &place,
            &reply,
            "Ana",
            "@me deploy?",
            &Reason::Keyword("deploy".into()),
        );
        assert_eq!(value["version"], VERSION);
        assert_eq!(value["reason"], "keyword");
        assert_eq!(value["keyword"], "deploy");
        assert_eq!(value["workspace"]["domain"], "acme");
        assert_eq!(value["conversation"]["kind"], "channel");
        assert_eq!(value["message"]["author"], "Ana");
        assert_eq!(value["message"]["thread_ts"], "1699999999.000100");
        assert_eq!(value["message"]["text"], "<@U1> deploy?");
        assert_eq!(
            value["permalink"],
            "https://acme.slack.com/archives/C1/p1700000000000100\
             ?thread_ts=1699999999.000100&cid=C1"
        );
        let text = value.to_string();
        for secret in ["xox", "files.slack.com", "token", "cookie"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[test]
    fn saved_hooks_read_back_and_default_to_off() {
        let hooks: Hooks =
            serde_json::from_str(r#"{"list":[{"command":"say hi"}]}"#).expect("hooks");
        assert!(!hooks.enabled);
        assert_eq!(hooks.list[0].command, "say hi");
        assert!(!hooks.list[0].mentions);
        assert_eq!(Hooks::default().list.len(), 0);
    }
}
