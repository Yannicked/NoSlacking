//! The command line: what `noslacking` accepts, its `--help`, and the
//! parsing, as a pure function over the arguments so it can be tested.
//!
//! Each flag is one line in [`FLAGS`]: its name, the names of the values
//! it takes, its line in `--help` and what it sets on [`Cli`]. Parsing
//! follows clap's rules, which this replaced: `--flag value` or
//! `--flag=value`, each flag at most once, a value never taken from
//! something that looks like a flag, one positional link, and `--` ending
//! the flags.

use std::ffi::OsString;
use std::path::PathBuf;

/// What the command line asked for.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Cli {
    /// A noslacking:// link the desktop passed (`LINK`).
    pub link: Option<String>,
    /// `--hidden`: start in the tray without a window.
    pub hidden: bool,
    /// `--verbose`: log more.
    pub verbose: bool,
    /// `--release-slack-links`: give slack:// links back and quit.
    pub release_slack_links: bool,
    /// `--huddle-probe TEAM CHANNEL`: test huddle audio and quit.
    #[cfg(feature = "huddle-audio")]
    pub huddle_probe: Option<[String; 2]>,
    /// `--seconds N`: how long the huddle probe listens.
    #[cfg(feature = "huddle-audio")]
    pub seconds: u64,
    /// `--huddle-region REGION`: the media region the probe asks for.
    #[cfg(feature = "huddle-audio")]
    pub huddle_region: Option<String>,
    /// `--demo`: run against a pretend Slack.
    #[cfg(feature = "demo")]
    pub demo: bool,
    /// `--demo-shot PATH`: where to save the demo's screenshot.
    #[cfg(feature = "demo")]
    pub demo_shot: Option<PathBuf>,
    /// `--demo-size WxH`: the demo window's size.
    #[cfg(feature = "demo")]
    pub demo_size: Option<String>,
    /// `--demo-hover X,Y`: where the pretend pointer rests.
    #[cfg(feature = "demo")]
    pub demo_hover: Option<String>,
    /// `--demo-view VIEW`: the view to open before the screenshot.
    #[cfg(feature = "demo")]
    pub demo_view: Option<String>,
    /// `--demo-light`: the light palette.
    #[cfg(feature = "demo")]
    pub demo_light: bool,
    /// `--demo-right-click X,Y`: where to right-click.
    #[cfg(feature = "demo")]
    pub demo_right_click: Option<String>,
    /// `--demo-click X,Y`: where to click.
    #[cfg(feature = "demo")]
    pub demo_click: Option<String>,
    /// `--demo-keys KEYS`: keys to press.
    #[cfg(feature = "demo")]
    pub demo_keys: Option<String>,
    /// `--demo-wheel LINES`: how far to scroll up.
    #[cfg(feature = "demo")]
    pub demo_wheel: Option<f32>,
    /// `--demo-type TEXT`: text to type.
    #[cfg(feature = "demo")]
    pub demo_type: Option<String>,
    /// `--demo-frames DIR`: where to save every frame.
    #[cfg(feature = "demo")]
    pub demo_frames: Option<PathBuf>,
    /// `--demo-shot-delay MS`: the wait before the screenshot.
    #[cfg(feature = "demo")]
    pub demo_shot_delay: u64,
}

impl Cli {
    /// Nothing given: every flag off, the defaults [`FLAGS`] names in
    /// `--help`.
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "huddle-audio")]
            seconds: 30,
            #[cfg(feature = "demo")]
            demo_shot_delay: 1500,
            ..Self::default()
        }
    }

    /// Whether to run against the pretend Slack; never without the demo
    /// feature.
    pub fn demo(&self) -> bool {
        #[cfg(feature = "demo")]
        {
            self.demo
        }
        #[cfg(not(feature = "demo"))]
        {
            false
        }
    }
}

/// What a flag takes, and what it does with it.
pub enum Takes {
    /// A switch: no value.
    Nothing(fn(&mut Cli)),
    /// One value, named in `--help`; the setter says why a value is wrong.
    #[cfg_attr(
        not(any(feature = "demo", feature = "huddle-audio")),
        expect(dead_code, reason = "only the demo and huddle flags take one value")
    )]
    One(&'static str, fn(&mut Cli, OsString) -> Result<(), String>),
    /// Two values in a row, named in `--help`.
    #[cfg_attr(
        not(feature = "huddle-audio"),
        expect(dead_code, reason = "only the huddle probe takes two values")
    )]
    Two([&'static str; 2], fn(&mut Cli, [String; 2])),
}

/// One `--flag`: its name without the dashes, what it takes, and its
/// line in `--help`.
pub struct Flag {
    /// The name, without the leading `--`.
    pub name: &'static str,
    /// What it takes and sets.
    pub takes: Takes,
    /// One line for `--help`.
    pub help: &'static str,
}

/// Every flag, in `--help`'s order. To add one: a field on [`Cli`] (and
/// its default in [`Cli::new`] if not empty) and a line here.
pub const FLAGS: &[Flag] = &[
    Flag {
        name: "hidden",
        takes: Takes::Nothing(|cli| cli.hidden = true),
        help: "Start in the tray without a window, as at login. Only with the tray item and \"Keep running in the tray\" on; otherwise the window opens",
    },
    Flag {
        name: "verbose",
        takes: Takes::Nothing(|cli| cli.verbose = true),
        help: "Log more (also NOSLACKING_LOG=debug)",
    },
    Flag {
        name: "release-slack-links",
        takes: Takes::Nothing(|cli| cli.release_slack_links = true),
        help: "Give slack:// links back to whatever had them before, if a browser sign-in left them with NoSlacking, and quit. Every start does this too",
    },
    #[cfg(feature = "huddle-audio")]
    Flag {
        name: "huddle-probe",
        takes: Takes::Two(["TEAM", "CHANNEL"], |cli, ids| cli.huddle_probe = Some(ids)),
        help: "Join the huddle in a conversation, listen and leave, logging each step, and quit: a test of huddle audio against real Slack. Takes the workspace's team id and the channel id, and the browser sign-in saved for that workspace",
    },
    #[cfg(feature = "huddle-audio")]
    Flag {
        name: "seconds",
        takes: Takes::One("N", |cli, v| number(v).map(|v| cli.seconds = v)),
        help: "How long the huddle probe listens, in seconds [default: 30]",
    },
    #[cfg(feature = "huddle-audio")]
    Flag {
        name: "huddle-region",
        takes: Takes::One("REGION", |cli, v| {
            text(v).map(|v| cli.huddle_region = Some(v))
        }),
        help: "The media region the huddle probe asks Slack for. Without it, the nearest is asked of AWS, falling back to us-east-1",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo",
        takes: Takes::Nothing(|cli| cli.demo = true),
        help: "Run against a pretend Slack, offline, with sample data",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-shot",
        takes: Takes::One("PATH", |cli, v| path(v).map(|v| cli.demo_shot = Some(v))),
        help: "Save a screenshot of the demo to PATH and quit",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-size",
        takes: Takes::One("WxH", |cli, v| text(v).map(|v| cli.demo_size = Some(v))),
        help: "The demo window's size as WxH logical points",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-hover",
        takes: Takes::One("X,Y", |cli, v| text(v).map(|v| cli.demo_hover = Some(v))),
        help: "Hold a pretend pointer at X,Y (logical points), to capture hover states",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-view",
        takes: Takes::One("VIEW", |cli, v| text(v).map(|v| cli.demo_view = Some(v))),
        help: "Open a view before the screenshot: thread, settings, sign-in, switcher, palette, picker, profile, share, upload, drafts, lightbox, media, previews, viewer-sheet, viewer-csv, viewer-zip, viewer-text, compact, held-media, shortcuts, delete-file, add-emoji or (with huddle-audio) listening",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-light",
        takes: Takes::Nothing(|cli| cli.demo_light = true),
        help: "Use the light palette in the demo",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-right-click",
        takes: Takes::One("X,Y", |cli, v| {
            text(v).map(|v| cli.demo_right_click = Some(v))
        }),
        help: "Right-click at X,Y (logical points) 2 s in, to show a context menu",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-click",
        takes: Takes::One("X,Y", |cli, v| text(v).map(|v| cli.demo_click = Some(v))),
        help: "Click at X,Y (logical points) 2.5 s in, after any right-click",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-keys",
        takes: Takes::One("KEYS", |cli, v| text(v).map(|v| cli.demo_keys = Some(v))),
        help: "Press keys one per frame from 2.5 s in, such as \"Shift+ArrowUp,ArrowUp,R\" (egui key names), to capture keyboard states",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-wheel",
        takes: Takes::One("LINES", |cli, v| {
            number(v).map(|v| cli.demo_wheel = Some(v))
        }),
        help: "Scroll the message list up by this many lines, 2.5 s in, as a reader would",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-type",
        takes: Takes::One("TEXT", |cli, v| text(v).map(|v| cli.demo_type = Some(v))),
        help: "Type TEXT into the focused field from 2.5 s in, a character every other frame, as a person typing would",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-frames",
        takes: Takes::One("DIR", |cli, v| path(v).map(|v| cli.demo_frames = Some(v))),
        help: "Save every frame from 2.5 s in to DIR (frame-000.png, …) until the typing is done, then quit: for catching a frame that draws wrong",
    },
    #[cfg(feature = "demo")]
    Flag {
        name: "demo-shot-delay",
        takes: Takes::One("MS", |cli, v| number(v).map(|v| cli.demo_shot_delay = v)),
        help: "Wait this long before the demo screenshot (default 1500 ms), for catching animations at different moments [default: 1500]",
    },
];

/// A value as text; paths take the [`OsString`] as it is instead.
fn text(value: OsString) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| "invalid UTF-8 was detected".to_owned())
}

/// A value as a path, which may be any bytes the system allows.
#[cfg_attr(
    not(feature = "demo"),
    expect(dead_code, reason = "only the demo flags take paths")
)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "every value's reader answers the same way"
)]
fn path(value: OsString) -> Result<PathBuf, String> {
    Ok(value.into())
}

/// A value as a number of any kind.
#[cfg_attr(
    not(any(feature = "demo", feature = "huddle-audio")),
    expect(dead_code, reason = "only the demo and huddle flags take values")
)]
fn number<T>(value: OsString) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    text(value)?.parse().map_err(|e: T::Err| e.to_string())
}

/// What the command line comes to.
#[derive(Debug, PartialEq)]
pub enum Parsed {
    /// Start, as asked.
    Run(Box<Cli>),
    /// Print [`help`] and quit.
    Help,
    /// Print [`version`] and quit.
    Version,
}

/// A command line that could not be read: the reason, worded as clap
/// worded it, for `error: …`.
#[derive(Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "error: {}\n\n{USAGE}\n\nFor more information, try '--help'.",
            self.0
        )
    }
}

impl std::error::Error for Error {}

/// The usage line, in `--help` and under every error.
const USAGE: &str = "Usage: noslacking [OPTIONS] [LINK]";

/// Reads the arguments after the program's name.
///
/// `--help`/`-h` and `--version`/`-V` win as soon as they are reached;
/// anything wrong before them is an error.
pub fn parse<I>(args: I) -> Result<Parsed, Error>
where
    I: IntoIterator,
    I::Item: Into<OsString>,
{
    let mut cli = Cli::new();
    let mut seen: Vec<&'static str> = Vec::new();
    let mut args = args.into_iter().map(Into::into).peekable();
    let mut flags_over = false;
    while let Some(arg) = args.next() {
        let Some(word) = arg.to_str() else {
            // Not UTF-8: a flag needs a readable name, the link must be
            // text, so this can only be an error.
            return Err(Error(format!(
                "invalid UTF-8 was detected in '{}'",
                arg.to_string_lossy()
            )));
        };
        if flags_over || word == "-" || !word.starts_with('-') {
            if cli.link.is_some() {
                return Err(Error(format!("unexpected argument '{word}' found")));
            }
            cli.link = Some(word.to_owned());
            continue;
        }
        match word {
            "--" => {
                flags_over = true;
                continue;
            }
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" | "-V" => return Ok(Parsed::Version),
            _ => {}
        }
        let (name, inline) = match word.strip_prefix("--").map(|w| w.split_once('=')) {
            Some(Some((name, value))) => (name, Some(OsString::from(value))),
            Some(None) => (&word[2..], None),
            None => ("", None),
        };
        let Some(flag) = FLAGS.iter().find(|f| !name.is_empty() && f.name == name) else {
            return Err(Error(format!(
                "unexpected argument '{word}' found\n\n  tip: to pass '{word}' as a value, use '-- {word}'"
            )));
        };
        let shown = shown(flag);
        if seen.contains(&flag.name) {
            return Err(Error(format!(
                "the argument '{shown}' cannot be used multiple times"
            )));
        }
        seen.push(flag.name);
        // The next argument, unless it looks like a flag.
        let mut next_value = || match args.peek().and_then(|a| a.to_str()) {
            Some(next) if next.starts_with('-') && next != "-" => None,
            _ => args.next(),
        };
        match &flag.takes {
            Takes::Nothing(set) => {
                if let Some(value) = inline {
                    return Err(Error(format!(
                        "unexpected value '{}' for '{shown}' found; no more were expected",
                        value.to_string_lossy()
                    )));
                }
                set(&mut cli);
            }
            Takes::One(_, set) => {
                let Some(value) = inline.or_else(&mut next_value) else {
                    return Err(Error(format!(
                        "a value is required for '{shown}' but none was supplied"
                    )));
                };
                let lossy = value.to_string_lossy().into_owned();
                set(&mut cli, value).map_err(|why| {
                    Error(format!("invalid value '{lossy}' for '{shown}': {why}"))
                })?;
            }
            Takes::Two(_, set) => {
                let missing = |given: usize| {
                    Error(if given == 0 {
                        format!("a value is required for '{shown}' but none was supplied")
                    } else {
                        format!("2 values required for '{shown}' but {given} was provided")
                    })
                };
                // As with clap, both values follow the flag.
                if inline.is_some() {
                    return Err(missing(1));
                }
                let mut two = [String::new(), String::new()];
                for (given, slot) in two.iter_mut().enumerate() {
                    let value = next_value().ok_or_else(|| missing(given))?;
                    *slot = text(value)
                        .map_err(|why| Error(format!("invalid value for '{shown}': {why}")))?;
                }
                set(&mut cli, two);
            }
        }
    }
    Ok(Parsed::Run(Box::new(cli)))
}

/// A flag as `--help` and errors show it: `--seconds <N>`.
fn shown(flag: &Flag) -> String {
    let values: &[&str] = match &flag.takes {
        Takes::Nothing(_) => &[],
        Takes::One(value, _) => std::slice::from_ref(value),
        Takes::Two(values, _) => values,
    };
    let mut shown = format!("--{}", flag.name);
    for value in values {
        shown.push_str(&format!(" <{value}>"));
    }
    shown
}

/// `--version`'s answer.
pub fn version() -> String {
    format!("noslacking {}", env!("CARGO_PKG_VERSION"))
}

/// `--help`'s answer: every flag this build has, one line each.
pub fn help() -> String {
    let mut rows: Vec<(String, &str)> = FLAGS.iter().map(|f| (shown(f), f.help)).collect();
    rows.push(("-h, --help".to_owned(), "Print help"));
    rows.push(("-V, --version".to_owned(), "Print version"));
    let width = rows.iter().map(|(left, _)| left.len()).max().unwrap_or(0);
    let mut help = format!(
        "{}\n\n{USAGE}\n\nArguments:\n  [LINK]  A noslacking:// link; the desktop passes sign-in redirects this way\n\nOptions:\n",
        env!("CARGO_PKG_DESCRIPTION")
    );
    for (left, text) in rows {
        // Long flags line up under the short ones, as clap had them.
        let indent = if left.starts_with("--") {
            "      "
        } else {
            "  "
        };
        let pad = width + 6 - indent.len() - left.len() + 2;
        help.push_str(&format!("{indent}{left}{:pad$}{text}\n", ""));
    }
    help
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Cli {
        match parse(args.iter().copied()) {
            Ok(Parsed::Run(cli)) => *cli,
            other => panic!("{args:?} did not parse to a run: {other:?}"),
        }
    }

    fn error(args: &[&str]) -> String {
        match parse(args.iter().copied()) {
            Err(Error(why)) => why,
            other => panic!("{args:?} did not fail: {other:?}"),
        }
    }

    #[test]
    fn nothing_given_is_the_defaults() {
        assert_eq!(run(&[]), Cli::new());
        assert!(!Cli::new().demo());
    }

    #[test]
    fn the_switches_turn_on() {
        let cli = run(&["--hidden", "--verbose", "--release-slack-links"]);
        assert!(cli.hidden && cli.verbose && cli.release_slack_links);
    }

    #[test]
    fn a_switch_takes_no_value() {
        assert_eq!(
            error(&["--verbose=1"]),
            "unexpected value '1' for '--verbose' found; no more were expected"
        );
    }

    #[test]
    fn the_link_is_the_one_positional() {
        let link = "noslacking://oauth?code=1&state=2";
        assert_eq!(run(&[link]).link.as_deref(), Some(link));
        assert_eq!(run(&["--hidden", link]).link.as_deref(), Some(link));
        assert_eq!(run(&[link, "--verbose"]).link.as_deref(), Some(link));
        assert_eq!(
            error(&[link, "slack://other"]),
            "unexpected argument 'slack://other' found"
        );
    }

    #[test]
    fn a_double_dash_ends_the_flags() {
        assert_eq!(run(&["--", "--verbose"]).link.as_deref(), Some("--verbose"));
        assert!(!run(&["--", "--verbose"]).verbose);
    }

    #[test]
    fn help_and_version_win_once_reached() {
        for args in [
            &["--help"][..],
            &["-h"],
            &["--verbose", "--help", "--bogus"],
        ] {
            assert_eq!(parse(args.iter().copied()), Ok(Parsed::Help));
        }
        for args in [&["--version"][..], &["-V"], &["--version", "--help"]] {
            assert_eq!(parse(args.iter().copied()), Ok(Parsed::Version));
        }
        assert!(parse(["--bogus", "--help"]).is_err());
        assert_eq!(
            version(),
            format!("noslacking {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn help_lists_every_flag_once() {
        let help = help();
        assert!(help.starts_with("A native Slack client\n\nUsage: noslacking [OPTIONS] [LINK]"));
        for flag in FLAGS {
            let line = format!("{}  ", shown(flag));
            assert_eq!(help.matches(&line).count(), 1, "{line}");
            assert!(help.contains(flag.help));
        }
        assert!(help.contains("      --release-slack-links  "));
        assert!(help.contains("  -h, --help  "));
        assert!(help.contains("  -V, --version  "));
    }

    #[test]
    fn an_unknown_flag_is_an_error() {
        for bogus in ["--bogus", "-x", "-hV", "---verbose"] {
            assert!(
                error(&[bogus]).starts_with(&format!("unexpected argument '{bogus}' found")),
                "{bogus}"
            );
        }
        assert!(error(&["--=1"]).starts_with("unexpected argument '--=1' found"));
        let shown = Error("unexpected argument '--bogus' found".to_owned()).to_string();
        assert!(shown.starts_with("error: unexpected argument '--bogus' found"));
        assert!(shown.ends_with("For more information, try '--help'."));
    }

    #[test]
    fn a_flag_given_twice_is_an_error() {
        assert_eq!(
            error(&["--verbose", "--verbose"]),
            "the argument '--verbose' cannot be used multiple times"
        );
    }

    #[test]
    fn the_huddle_and_demo_flags_exist_only_with_their_features() {
        let names: Vec<&str> = FLAGS.iter().map(|f| f.name).collect();
        assert_eq!(
            names.contains(&"huddle-probe"),
            cfg!(feature = "huddle-audio")
        );
        assert_eq!(names.contains(&"seconds"), cfg!(feature = "huddle-audio"));
        assert_eq!(
            names.contains(&"huddle-region"),
            cfg!(feature = "huddle-audio")
        );
        assert_eq!(names.contains(&"demo"), cfg!(feature = "demo"));
        assert_eq!(names.contains(&"demo-shot"), cfg!(feature = "demo"));
        assert_eq!(
            names.iter().filter(|n| n.starts_with("demo")).count(),
            if cfg!(feature = "demo") { 13 } else { 0 }
        );
        let help = help();
        assert_eq!(
            help.contains("--huddle-probe"),
            cfg!(feature = "huddle-audio")
        );
        assert_eq!(help.contains("--demo"), cfg!(feature = "demo"));
    }

    #[cfg(not(feature = "demo"))]
    #[test]
    fn without_the_demo_feature_demo_flags_are_unknown() {
        assert!(error(&["--demo"]).starts_with("unexpected argument '--demo' found"));
    }

    #[cfg(not(feature = "huddle-audio"))]
    #[test]
    fn without_huddle_audio_the_probe_is_unknown() {
        assert!(
            error(&["--huddle-probe", "T1", "C1"])
                .starts_with("unexpected argument '--huddle-probe' found")
        );
        assert!(error(&["--seconds", "5"]).starts_with("unexpected argument '--seconds' found"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_is_not_utf8_is_an_error_not_a_panic() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(b"noslacking://\xff".to_vec());
        let Err(Error(why)) = parse([bad]) else {
            panic!("a non-UTF-8 link parsed");
        };
        assert!(why.starts_with("invalid UTF-8 was detected"), "{why}");
    }

    #[cfg(windows)]
    #[test]
    fn a_link_that_is_not_utf16_is_an_error_not_a_panic() {
        use std::os::windows::ffi::OsStringExt;
        let bad = OsString::from_wide(&[0x6e, 0xd800]);
        assert!(parse([bad]).is_err());
    }

    #[cfg(feature = "huddle-audio")]
    mod huddle {
        use super::*;

        #[test]
        fn the_probe_takes_team_and_channel() {
            let cli = run(&["--huddle-probe", "T1", "C1"]);
            assert_eq!(cli.huddle_probe, Some(["T1".to_owned(), "C1".to_owned()]));
            assert_eq!(cli.seconds, 30);
            assert_eq!(cli.huddle_region, None);
        }

        #[test]
        fn the_probe_needs_both_ids() {
            let missing =
                "2 values required for '--huddle-probe <TEAM> <CHANNEL>' but 1 was provided";
            assert_eq!(error(&["--huddle-probe", "T1"]), missing);
            assert_eq!(error(&["--huddle-probe", "T1", "--verbose"]), missing);
            assert_eq!(error(&["--huddle-probe=T1", "C1"]), missing);
            assert_eq!(
                error(&["--huddle-probe"]),
                "a value is required for '--huddle-probe <TEAM> <CHANNEL>' but none was supplied"
            );
        }

        #[test]
        fn seconds_and_region_in_both_forms() {
            for args in [
                &["--seconds", "5", "--huddle-region", "eu-west-1"][..],
                &["--seconds=5", "--huddle-region=eu-west-1"],
            ] {
                let cli = run(args);
                assert_eq!(cli.seconds, 5);
                assert_eq!(cli.huddle_region.as_deref(), Some("eu-west-1"));
            }
        }

        #[test]
        fn a_bad_number_says_why() {
            assert_eq!(
                error(&["--seconds", "x"]),
                "invalid value 'x' for '--seconds <N>': invalid digit found in string"
            );
            assert_eq!(
                error(&["--seconds="]),
                "invalid value '' for '--seconds <N>': cannot parse integer from empty string"
            );
            // As with clap, a negative number looks like a flag.
            assert_eq!(
                error(&["--seconds", "-1"]),
                "a value is required for '--seconds <N>' but none was supplied"
            );
        }

        #[test]
        fn help_shows_the_default_seconds() {
            assert!(help().contains(&format!("[default: {}]", Cli::new().seconds)));
        }
    }

    #[cfg(feature = "demo")]
    mod demo {
        use super::*;

        #[test]
        fn every_demo_flag_in_both_forms() {
            let pairs = [
                ("demo-shot", "out.png"),
                ("demo-size", "800x600"),
                ("demo-hover", "10,20"),
                ("demo-view", "thread"),
                ("demo-right-click", "30,40"),
                ("demo-click", "50,60"),
                ("demo-keys", "Shift+ArrowUp,R"),
                ("demo-wheel", "2.5"),
                ("demo-type", "hello there"),
                ("demo-frames", "frames"),
                ("demo-shot-delay", "250"),
            ];
            let spaced: Vec<String> = pairs
                .iter()
                .flat_map(|(name, value)| [format!("--{name}"), (*value).to_owned()])
                .chain(["--demo".to_owned(), "--demo-light".to_owned()])
                .collect();
            let joined: Vec<String> = pairs
                .iter()
                .map(|(name, value)| format!("--{name}={value}"))
                .chain(["--demo".to_owned(), "--demo-light".to_owned()])
                .collect();
            for args in [spaced, joined] {
                let Ok(Parsed::Run(cli)) = parse(args.clone()) else {
                    panic!("{args:?} did not parse");
                };
                assert!(cli.demo() && cli.demo_light);
                assert_eq!(cli.demo_shot, Some(PathBuf::from("out.png")));
                assert_eq!(cli.demo_size.as_deref(), Some("800x600"));
                assert_eq!(cli.demo_hover.as_deref(), Some("10,20"));
                assert_eq!(cli.demo_view.as_deref(), Some("thread"));
                assert_eq!(cli.demo_right_click.as_deref(), Some("30,40"));
                assert_eq!(cli.demo_click.as_deref(), Some("50,60"));
                assert_eq!(cli.demo_keys.as_deref(), Some("Shift+ArrowUp,R"));
                assert_eq!(cli.demo_wheel, Some(2.5));
                assert_eq!(cli.demo_type.as_deref(), Some("hello there"));
                assert_eq!(cli.demo_frames, Some(PathBuf::from("frames")));
                assert_eq!(cli.demo_shot_delay, 250);
            }
        }

        #[test]
        fn the_shot_delay_defaults_to_1500() {
            assert_eq!(run(&["--demo"]).demo_shot_delay, 1500);
            assert!(help().contains("[default: 1500]"));
        }

        #[test]
        fn a_value_is_never_a_flag() {
            assert_eq!(
                error(&["--demo-type", "--verbose"]),
                "a value is required for '--demo-type <TEXT>' but none was supplied"
            );
            assert_eq!(
                error(&["--demo-type"]),
                "a value is required for '--demo-type <TEXT>' but none was supplied"
            );
            // The joined form takes anything, empty or dashed.
            assert_eq!(run(&["--demo-type="]).demo_type.as_deref(), Some(""));
            assert_eq!(run(&["--demo-type=-x"]).demo_type.as_deref(), Some("-x"));
        }

        #[test]
        fn a_bad_wheel_says_why() {
            assert_eq!(
                error(&["--demo-wheel", "lots"]),
                "invalid value 'lots' for '--demo-wheel <LINES>': invalid float literal"
            );
        }

        #[cfg(unix)]
        #[test]
        fn a_path_may_be_any_bytes() {
            use std::os::unix::ffi::OsStringExt;
            let path = OsString::from_vec(b"shot-\xff.png".to_vec());
            let Ok(Parsed::Run(cli)) = parse([OsString::from("--demo-shot"), path.clone()]) else {
                panic!("a non-UTF-8 path did not parse");
            };
            assert_eq!(cli.demo_shot, Some(PathBuf::from(path)));
        }
    }
}
