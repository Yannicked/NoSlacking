//! Sending a message later: the times the composer offers, the dialog for
//! a time of your own (also used to change a scheduled message), and what
//! the Scheduled view lists.

use std::fmt;

use jiff::Zoned;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;

use crate::model::Ts;

/// How far ahead Slack schedules a message: 120 days.
pub const MAX_AHEAD: i64 = 120 * 86_400;
/// How soon a scheduled message may go out, so it is not already past
/// when Slack gets it.
pub const MIN_AHEAD: i64 = 60;

/// When the composer's "Send later" menu sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    /// Half an hour from now.
    HalfHour,
    /// Tomorrow at nine in the morning.
    TomorrowMorning,
}

impl When {
    /// The moment, in seconds since the epoch, as seen from `now`.
    pub fn post_at(self, now: &Zoned) -> Option<i64> {
        match self {
            Self::HalfHour => Some(now.timestamp().as_second() + 30 * 60),
            Self::TomorrowMorning => {
                let tomorrow = now.date().tomorrow().ok()?;
                let nine = tomorrow.at(9, 0, 0, 0).to_zoned(now.time_zone().clone());
                Some(nine.ok()?.timestamp().as_second())
            }
        }
    }
}

/// A message waiting in Slack to be sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Scheduled {
    pub id: String,
    pub channel: String,
    /// When it goes out, in seconds since the epoch.
    pub post_at: i64,
    /// In wire form, mentions as `<@U1>`.
    pub text: String,
    /// The thread it answers, when Slack says.
    pub thread: Option<Ts>,
}

/// What the "Send at" dialog is for.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// The draft of the composer under `key` ([`crate::app::App::draft_key`]),
    /// in `channel` and maybe a thread.
    Draft {
        key: String,
        channel: String,
        thread: Option<Ts>,
    },
    /// A scheduled message, sent again at a new time with new text.
    Edit(Scheduled),
    /// Not a message to send: a reminder about message `ts` of `channel`
    /// (a reply in `thread`), set at the time chosen.
    Remind {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
}

impl Target {
    /// How far ahead Slack takes the time: a reminder may be set years
    /// ahead, a message only months.
    pub fn max_ahead(&self) -> i64 {
        match self {
            Self::Remind { .. } => super::remind::MAX_AHEAD,
            Self::Draft { .. } | Self::Edit(_) => MAX_AHEAD,
        }
    }
}

/// The dialog that asks when to send, and for a scheduled message what.
#[derive(Clone, Debug, PartialEq)]
pub struct Dialog {
    pub target: Target,
    /// The text, for a scheduled message being changed, as typed, with
    /// the mentions in it (shown name, wire form) as in a draft.
    pub text: String,
    pub mentions: Vec<(String, String)>,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// `HH:MM`, 24-hour.
    pub time: String,
    /// What was wrong with the last try, if anything.
    pub problem: Option<Problem>,
    /// Waiting for Slack.
    pub busy: bool,
}

impl Dialog {
    /// A dialog for `target`, filled in with `at` (seconds since the
    /// epoch) as seen in `zone`, or tomorrow morning.
    pub fn new(target: Target, text: String, at: Option<i64>, now: &Zoned) -> Self {
        let at = at
            .and_then(|s| jiff::Timestamp::from_second(s).ok())
            .map(|t| t.to_zoned(now.time_zone().clone()))
            .or_else(|| {
                let seconds = When::TomorrowMorning.post_at(now)?;
                Some(
                    jiff::Timestamp::from_second(seconds)
                        .ok()?
                        .to_zoned(now.time_zone().clone()),
                )
            })
            .unwrap_or_else(|| now.clone());
        Self {
            target,
            text,
            mentions: Vec::new(),
            date: at.strftime("%Y-%m-%d").to_string(),
            time: at.strftime("%H:%M").to_string(),
            problem: None,
            busy: false,
        }
    }
}

/// Why the dialog's time cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Not a date as `YYYY-MM-DD`.
    Date,
    /// Not a time as `HH:MM`.
    Time,
    /// Now or before.
    Past,
    /// Further ahead than Slack schedules.
    TooFar,
    /// Further ahead than Slack sets a reminder.
    TooFarToRemind,
    /// A changed message with no text.
    Empty,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::i18n::t;
        f.write_str(&match self {
            Self::Date => t("Write the date as 2026-03-31."),
            Self::Time => t("Write the time as 14:30."),
            Self::Past => t("That time has passed."),
            Self::TooFar => t("Slack schedules at most 120 days ahead."),
            Self::TooFarToRemind => t("Slack sets reminders at most five years ahead."),
            Self::Empty => t("The message is empty."),
        })
    }
}

/// The moment `date` and `time` name in `zone`, in seconds since the
/// epoch, if it is one Slack can send at, seen from `now` (seconds), at
/// most `max_ahead` seconds from `now`: the limit of a message
/// ([`MAX_AHEAD`]), or of a reminder ([`super::remind::MAX_AHEAD`]).
pub fn moment_within(
    date: &str,
    time: &str,
    zone: &TimeZone,
    now: i64,
    max_ahead: i64,
) -> Result<i64, Problem> {
    let date: Date = date.trim().parse().map_err(|_| Problem::Date)?;
    let time = parse_time(time).ok_or(Problem::Time)?;
    let zoned = date
        .to_datetime(time)
        .to_zoned(zone.clone())
        .map_err(|_| Problem::Time)?;
    let seconds = zoned.timestamp().as_second();
    if seconds < now + MIN_AHEAD {
        return Err(Problem::Past);
    }
    if seconds > now + max_ahead {
        return Err(if max_ahead > MAX_AHEAD {
            Problem::TooFarToRemind
        } else {
            Problem::TooFar
        });
    }
    Ok(seconds)
}

/// `14:30`, `9:05` or `0930`.
fn parse_time(text: &str) -> Option<Time> {
    let text = text.trim();
    let (hours, minutes) = match text.split_once(':') {
        Some(parts) => parts,
        // Checked, because four bytes may be two characters like `1€`.
        None if text.len() == 4 => text.split_at_checked(2)?,
        None => return None,
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(hours) || !digits(minutes) || minutes.len() != 2 {
        return None;
    }
    Time::new(hours.parse().ok()?, minutes.parse().ok()?, 0, 0).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amsterdam(at: &str) -> Zoned {
        let zone = TimeZone::get("Europe/Amsterdam").unwrap_or(TimeZone::UTC);
        at.parse::<jiff::civil::DateTime>()
            .expect("a date and time")
            .to_zoned(zone)
            .expect("in the zone")
    }

    #[test]
    fn the_quick_choices_land_where_they_say() {
        let now = amsterdam("2026-03-28T22:15");
        let half = When::HalfHour.post_at(&now).expect("a time");
        assert_eq!(half, now.timestamp().as_second() + 1800);
        let morning = When::TomorrowMorning.post_at(&now).expect("a time");
        let local = jiff::Timestamp::from_second(morning)
            .expect("a timestamp")
            .to_zoned(now.time_zone().clone());
        assert_eq!(
            local.strftime("%Y-%m-%d %H:%M").to_string(),
            "2026-03-29 09:00"
        );
    }

    #[test]
    fn typed_times_are_checked() {
        let now = amsterdam("2026-03-28T22:15");
        let zone = now.time_zone().clone();
        let seconds = now.timestamp().as_second();
        let tomorrow =
            moment_within("2026-03-29", "9:30", &zone, seconds, MAX_AHEAD).expect("tomorrow");
        assert_eq!(
            moment_within("2026-03-29", "0930", &zone, seconds, MAX_AHEAD),
            Ok(tomorrow)
        );
        assert_eq!(
            moment_within("29-03-2026", "09:30", &zone, seconds, MAX_AHEAD),
            Err(Problem::Date)
        );
        assert_eq!(
            moment_within("2026-03-29", "9.30", &zone, seconds, MAX_AHEAD),
            Err(Problem::Time)
        );
        assert_eq!(
            moment_within("2026-03-29", "25:00", &zone, seconds, MAX_AHEAD),
            Err(Problem::Time)
        );
        assert_eq!(
            moment_within("2026-03-29", "9:3", &zone, seconds, MAX_AHEAD),
            Err(Problem::Time)
        );
        assert_eq!(
            moment_within("2026-03-28", "22:15", &zone, seconds, MAX_AHEAD),
            Err(Problem::Past)
        );
        assert_eq!(
            moment_within("2026-12-31", "09:00", &zone, seconds, MAX_AHEAD),
            Err(Problem::TooFar)
        );
        // A reminder may be set that far ahead, but not past five years.
        let reminder = super::super::remind::MAX_AHEAD;
        assert!(moment_within("2026-12-31", "09:00", &zone, seconds, reminder).is_ok());
        assert_eq!(
            moment_within("2032-12-31", "09:00", &zone, seconds, reminder),
            Err(Problem::TooFarToRemind)
        );
    }

    #[test]
    fn times_with_other_characters_are_refused_not_a_crash() {
        for text in ["1€", "1２", "€€", "１２３４", "12€"] {
            assert_eq!(parse_time(text), None, "{text}");
        }
    }

    #[test]
    fn the_dialog_starts_at_the_time_given_or_tomorrow_morning() {
        let now = amsterdam("2026-03-28T22:15");
        let target = Target::Draft {
            key: "T1/C1".into(),
            channel: "C1".into(),
            thread: None,
        };
        let fresh = Dialog::new(target.clone(), String::new(), None, &now);
        assert_eq!(
            (fresh.date.as_str(), fresh.time.as_str()),
            ("2026-03-29", "09:00")
        );
        let at = amsterdam("2026-04-02T16:45").timestamp().as_second();
        let given = Dialog::new(target, String::new(), Some(at), &now);
        assert_eq!(
            (given.date.as_str(), given.time.as_str()),
            ("2026-04-02", "16:45")
        );
    }
}
