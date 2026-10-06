//! "Remind me about this message": the times the message menu offers, and
//! what Slack's `reminders.add` is sent.
//!
//! `reminders.add` takes only a text and a time; nothing ties a reminder to
//! a message. So the reminder's text carries the start of the message and
//! its permalink, which Slack shows as a link when the reminder fires.

use jiff::Zoned;
use jiff::civil::Weekday;

use crate::i18n::t;

/// How far ahead Slack sets a reminder: five years.
pub const MAX_AHEAD: i64 = 5 * 366 * 86_400;
/// How many characters of the message the reminder's text quotes.
const EXCERPT: usize = 80;

/// When the "Remind me" menu reminds you.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemindIn {
    Minutes20,
    Hour,
    Hours3,
    /// Nine in the morning, local time, on the next day.
    Tomorrow,
    /// Nine in the morning, local time, on the next Monday: a week ahead
    /// when it is Monday already.
    NextWeek,
}

impl RemindIn {
    /// Every choice, in menu order.
    pub const ALL: [Self; 5] = [
        Self::Minutes20,
        Self::Hour,
        Self::Hours3,
        Self::Tomorrow,
        Self::NextWeek,
    ];

    /// What the menu calls it.
    pub fn label(self) -> String {
        match self {
            Self::Minutes20 => t("In 20 minutes"),
            Self::Hour => t("In 1 hour"),
            Self::Hours3 => t("In 3 hours"),
            Self::Tomorrow => t("Tomorrow at 9:00"),
            Self::NextWeek => t("Next Monday at 9:00"),
        }
        .into_owned()
    }

    /// The moment, in seconds since the epoch, as seen from `now` in its
    /// time zone. The "in …" choices count real seconds, so a clock change
    /// does not move them; the morning ones are nine on the wall clock,
    /// whatever the offset is that day.
    pub fn at(self, now: &Zoned) -> Option<i64> {
        let seconds = now.timestamp().as_second();
        let morning = |date: jiff::civil::Date| {
            date.at(9, 0, 0, 0)
                .to_zoned(now.time_zone().clone())
                .ok()
                .map(|z| z.timestamp().as_second())
        };
        match self {
            Self::Minutes20 => Some(seconds + 20 * 60),
            Self::Hour => Some(seconds + 60 * 60),
            Self::Hours3 => Some(seconds + 3 * 60 * 60),
            Self::Tomorrow => morning(now.date().tomorrow().ok()?),
            // The first Monday after today, never today.
            Self::NextWeek => morning(now.date().nth_weekday(1, Weekday::Monday).ok()?),
        }
    }
}

/// The reminder's text: the start of the message's words (`plain`, as
/// shown) in quotes, then its `permalink`. A message with no words is
/// just the link.
pub fn reminder_text(plain: &str, permalink: &str) -> String {
    let line = plain
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.is_empty() {
        return permalink.to_owned();
    }
    let mut excerpt: String = line.chars().take(EXCERPT).collect();
    if line.chars().count() > EXCERPT || plain.trim() != line {
        excerpt = excerpt.trim_end().to_owned();
        excerpt.push('…');
    }
    format!("“{excerpt}” {permalink}")
}

/// What `reminders.add` is sent: the text, and the time in seconds since
/// the epoch. Leaving out `user` sets it for you, the only one Slack still
/// allows.
pub fn params(text: &str, time: i64) -> Vec<(&'static str, String)> {
    vec![("text", text.to_owned()), ("time", time.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz::TimeZone;

    fn at(zone: &str, when: &str) -> Zoned {
        let zone = TimeZone::get(zone).unwrap_or(TimeZone::UTC);
        when.parse::<jiff::civil::DateTime>()
            .expect("a date and time")
            .to_zoned(zone)
            .expect("in the zone")
    }

    fn local(seconds: i64, like: &Zoned) -> String {
        jiff::Timestamp::from_second(seconds)
            .expect("a timestamp")
            .to_zoned(like.time_zone().clone())
            .strftime("%a %Y-%m-%d %H:%M %:z")
            .to_string()
    }

    #[test]
    fn reminders_in_a_while_count_real_time_across_a_clock_change() {
        // Amsterdam's clocks go forward at 02:00 on 2026-03-29.
        let now = at("Europe/Amsterdam", "2026-03-29T01:30");
        let hour = RemindIn::Hour.at(&now).expect("a time");
        assert_eq!(hour - now.timestamp().as_second(), 3600);
        assert_eq!(local(hour, &now), "Sun 2026-03-29 03:30 +02:00");
        let three = RemindIn::Hours3.at(&now).expect("a time");
        assert_eq!(three - now.timestamp().as_second(), 3 * 3600);
        let soon = RemindIn::Minutes20.at(&now).expect("a time");
        assert_eq!(soon - now.timestamp().as_second(), 1200);
    }

    #[test]
    fn tomorrow_morning_is_nine_on_the_wall_clock_after_a_clock_change() {
        let now = at("Europe/Amsterdam", "2026-03-28T22:00");
        let tomorrow = RemindIn::Tomorrow.at(&now).expect("a time");
        assert_eq!(local(tomorrow, &now), "Sun 2026-03-29 09:00 +02:00");
        // Ten hours on the wall clock, but one of them was skipped.
        assert_eq!(tomorrow - now.timestamp().as_second(), 10 * 3600);
    }

    #[test]
    fn tomorrow_late_at_night_is_the_next_calendar_day() {
        let late = at("Europe/Amsterdam", "2026-10-06T23:55");
        let tomorrow = RemindIn::Tomorrow.at(&late).expect("a time");
        assert_eq!(local(tomorrow, &late), "Wed 2026-10-07 09:00 +02:00");
        // Just past midnight, "tomorrow" is the day after, as in Slack.
        let early = at("Europe/Amsterdam", "2026-10-07T00:10");
        let tomorrow = RemindIn::Tomorrow.at(&early).expect("a time");
        assert_eq!(local(tomorrow, &early), "Thu 2026-10-08 09:00 +02:00");
    }

    #[test]
    fn next_week_is_the_next_monday_and_never_today() {
        // 2026-10-05 is a Monday.
        let monday = at("America/New_York", "2026-10-05T08:00");
        let next = RemindIn::NextWeek.at(&monday).expect("a time");
        assert_eq!(local(next, &monday), "Mon 2026-10-12 09:00 -04:00");
        let sunday = at("America/New_York", "2026-10-04T20:00");
        let next = RemindIn::NextWeek.at(&sunday).expect("a time");
        assert_eq!(local(next, &sunday), "Mon 2026-10-05 09:00 -04:00");
        // Across New York's clock change on 2026-11-01.
        let friday = at("America/New_York", "2026-10-30T12:00");
        let next = RemindIn::NextWeek.at(&friday).expect("a time");
        assert_eq!(local(next, &friday), "Mon 2026-11-02 09:00 -05:00");
    }

    #[test]
    fn the_reminder_quotes_the_message_and_links_to_it() {
        let link = "https://acme.slack.com/archives/C1/p1700000000000100";
        assert_eq!(
            reminder_text("Ship it on Friday", link),
            format!("“Ship it on Friday” {link}")
        );
        assert_eq!(
            reminder_text("\n  first line \nsecond", link),
            format!("“first line…” {link}")
        );
        let long = "é".repeat(100);
        let text = reminder_text(&long, link);
        assert!(
            text.starts_with(&format!("“{}…”", "é".repeat(80))),
            "{text}"
        );
        assert_eq!(reminder_text("  ", link), link);
    }

    #[test]
    fn reminders_add_is_sent_the_text_and_the_time() {
        assert_eq!(
            params(
                "“Ship it” https://x.slack.com/archives/C1/p1",
                1_790_000_000
            ),
            vec![
                (
                    "text",
                    "“Ship it” https://x.slack.com/archives/C1/p1".to_owned()
                ),
                ("time", "1790000000".to_owned()),
            ]
        );
    }
}
