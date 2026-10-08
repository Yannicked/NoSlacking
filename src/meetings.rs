//! Microsoft Teams meetings, as the interface knows them: which meeting to
//! join (a meeting link, `…/meet/{code}?p={token}`, or a meeting ID with
//! its passcode, as Teams' "Join with an ID" takes them), and the link
//! that invites others to one started here. The joining itself is
//! `teams::calling::call::meeting` (see `docs/research/teams-calls.md`
//! §H).
//!
//! An ID and a link come to the same thing: the web client joins by ID
//! with the link it would have had, the typed passcode in place of the
//! link's token (recorded), and the meeting service answers the
//! meeting's own code, passcode and link, which the join then sends back.

use crate::redact::REDACTED;

/// Where a meeting typed by ID is, for a personal (Teams free) account:
/// the only kind recorded.
const PERSONAL_HOST: &str = "teams.live.com";

/// A meeting to join. Its code and passcode let anyone in, so `Debug`
/// shows neither.
#[derive(Clone, PartialEq, Eq)]
pub struct Meeting {
    /// The link's host: `teams.live.com`, or a work account's.
    host: String,
    /// The meeting ID, digits only.
    code: String,
    /// The link's token or the typed passcode.
    passcode: String,
}

impl std::fmt::Debug for Meeting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meeting")
            .field("host", &self.host)
            .field("code", &REDACTED)
            .field("passcode", &REDACTED)
            .finish()
    }
}

/// Why what was typed is not a meeting to join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotAMeeting {
    /// Neither a meeting link nor a meeting ID.
    Unreadable,
    /// A meeting ID without its passcode.
    NoPasscode,
    /// A link to a work account's meeting in the old form
    /// (`/l/meetup-join/…`), which is joined another way.
    OldWorkLink,
}

impl Meeting {
    /// Reads what was typed: a meeting link, whose passcode it carries,
    /// or a meeting ID (spaces allowed, as Teams writes it) with
    /// `passcode`.
    pub fn parse(link_or_id: &str, passcode: &str) -> Result<Self, NotAMeeting> {
        let text = link_or_id.trim();
        if let Some(link) = Self::from_link(text) {
            return Ok(link);
        }
        if text.contains("/l/meetup-join/") {
            return Err(NotAMeeting::OldWorkLink);
        }
        let code: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        if !looks_like_code(&code) {
            return Err(NotAMeeting::Unreadable);
        }
        let passcode = passcode.trim();
        if passcode.is_empty() {
            return Err(NotAMeeting::NoPasscode);
        }
        Ok(Self {
            host: PERSONAL_HOST.to_owned(),
            code,
            passcode: passcode.to_owned(),
        })
    }

    /// Reads a meeting link: `https://{host}/meet/{code}?p={token}`.
    fn from_link(text: &str) -> Option<Self> {
        let url = reqwest::Url::parse(text).ok()?;
        let host = url.host_str()?.to_ascii_lowercase();
        if !(host == PERSONAL_HOST || host.ends_with(".microsoft.com")) {
            return None;
        }
        let mut segments = url.path_segments()?;
        if segments.next()? != "meet" {
            return None;
        }
        let code = segments.next()?.to_owned();
        if !looks_like_code(&code) {
            return None;
        }
        let passcode = url
            .query_pairs()
            .find(|(key, _)| key == "p")
            .map(|(_, value)| value.into_owned())
            .filter(|p| !p.is_empty())?;
        Some(Self {
            host,
            code,
            passcode,
        })
    }

    /// The meeting ID.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The link's token, or the passcode typed.
    pub fn passcode(&self) -> &str {
        &self.passcode
    }

    /// The link the meeting service is given, as the web client builds
    /// it for an ID too.
    pub fn url(&self) -> String {
        let base = format!("https://{}/meet/{}", self.host, self.code);
        match reqwest::Url::parse(&base) {
            Ok(mut url) => {
                url.query_pairs_mut().append_pair("p", &self.passcode);
                url.into()
            }
            // The host came from a URL that parsed, or is ours.
            Err(_) => base,
        }
    }
}

/// The conversation a meeting's call is shown in while it has none of
/// its own here: its chat appears only once you are let in.
pub const MEETING_CHANNEL: &str = "teams-meeting";

/// A meeting's join link, to invite others with. Anyone holding it can
/// ask to join, so `Debug` does not show it.
#[derive(Clone, PartialEq, Eq)]
pub struct MeetingLink(pub String);

impl std::fmt::Debug for MeetingLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// The meetings dialog: "Meet now", or join by link or by ID and
/// passcode, for the workspace `team`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dialog {
    pub team: String,
    /// A meeting link, or a meeting ID.
    pub link: String,
    /// The passcode, for an ID.
    pub passcode: String,
    /// Why the last Join did not, until something is typed again.
    pub problem: Option<NotAMeeting>,
}

impl Dialog {
    /// The dialog for `team`, with `link` already in it (a meeting link
    /// that was opened), or empty.
    pub fn new(team: &str, link: &str) -> Self {
        Self {
            team: team.to_owned(),
            link: link.to_owned(),
            ..Self::default()
        }
    }

    /// The meeting to join from what is typed; `None`, saying why in
    /// `problem`, when it is not one.
    pub fn read(&mut self) -> Option<Meeting> {
        match Meeting::parse(&self.link, &self.passcode) {
            Ok(meeting) => {
                self.problem = None;
                Some(meeting)
            }
            Err(problem) => {
                self.problem = Some(problem);
                None
            }
        }
    }
}

/// Whether `code` is a meeting ID: digits only, as long as Teams makes
/// them (13 in a recording; some services print 10 to 15).
fn looks_like_code(code: &str) -> bool {
    (9..=16).contains(&code.len()) && code.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meeting_link_carries_its_code_and_passcode() {
        let meeting = Meeting::parse(
            " https://teams.live.com/meet/9312345678901?p=AbCdEf123 ",
            "",
        )
        .expect("a link");
        assert_eq!(meeting.code, "9312345678901");
        assert_eq!(meeting.passcode, "AbCdEf123");
        assert_eq!(
            meeting.url(),
            "https://teams.live.com/meet/9312345678901?p=AbCdEf123"
        );
        // A work account's meeting in the new form reads the same.
        let work = Meeting::parse("https://teams.microsoft.com/meet/2223334445556?p=Xy9", "")
            .expect("a work link");
        assert_eq!(work.host, "teams.microsoft.com");
    }

    #[test]
    fn an_id_is_joined_with_the_link_it_would_have_had() {
        let meeting = Meeting::parse("931 234 567 890 1", "a1B2c3").expect("an id");
        assert_eq!(meeting.code, "9312345678901");
        assert_eq!(
            meeting.url(),
            "https://teams.live.com/meet/9312345678901?p=a1B2c3"
        );
        assert_eq!(
            Meeting::parse("9312345678901", "  "),
            Err(NotAMeeting::NoPasscode)
        );
    }

    #[test]
    fn what_is_not_a_meeting_says_why() {
        for text in [
            "",
            "hello",
            "12345",
            "https://example.com/meet/9312345678901?p=x",
        ] {
            assert_eq!(
                Meeting::parse(text, "abc"),
                Err(NotAMeeting::Unreadable),
                "{text}"
            );
        }
        // A link without its passcode is not one to join.
        assert_eq!(
            Meeting::parse("https://teams.live.com/meet/9312345678901", "abc").map(|m| m.code),
            Err(NotAMeeting::Unreadable)
        );
        assert_eq!(
            Meeting::parse(
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0",
                ""
            ),
            Err(NotAMeeting::OldWorkLink)
        );
    }

    #[test]
    fn neither_code_nor_passcode_reaches_the_log() {
        let meeting = Meeting::parse("9312345678901", "secret1").expect("an id");
        let shown = format!("{meeting:?}");
        assert!(!shown.contains("9312345678901") && !shown.contains("secret1"));
    }

    #[test]
    fn the_dialog_joins_what_reads_and_says_why_not() {
        let mut dialog = Dialog::new("teams_1", "");
        dialog.link = "931 234 567 890 1".into();
        assert_eq!(dialog.read(), None);
        assert_eq!(dialog.problem, Some(NotAMeeting::NoPasscode));
        dialog.passcode = "a1B2c3".into();
        assert_eq!(dialog.read().map(|m| m.code), Some("9312345678901".into()));
        assert_eq!(dialog.problem, None);
        let opened = Dialog::new("teams_1", "https://teams.live.com/meet/9312345678901?p=x");
        assert!(opened.clone().read().is_some());
    }
}
