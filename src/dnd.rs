//! Do Not Disturb: Slack's schedule of quiet hours and the snooze you set
//! by hand, and whether notifications are held right now.
//!
//! Everything here takes the time as an argument, so it can be tested
//! without a clock.

/// One workspace's Do Not Disturb state, as Slack reported it (or as you
/// set it here while Slack had not answered, or would not).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dnd {
    /// The next (or current) stretch of your Do Not Disturb schedule, as
    /// Unix seconds, when the schedule is on.
    pub schedule: Option<(i64, i64)>,
    /// Notifications are snoozed until then, in Unix seconds.
    pub snooze_until: Option<i64>,
}

impl Dnd {
    /// Until when a snooze holds notifications, if one does at `now`.
    pub fn snoozed(&self, now: i64) -> Option<i64> {
        self.snooze_until.filter(|until| *until > now)
    }

    /// Until when the schedule holds notifications, if it does at `now`.
    pub fn scheduled(&self, now: i64) -> Option<i64> {
        self.schedule
            .filter(|(start, end)| (*start..*end).contains(&now))
            .map(|(_, end)| end)
    }

    /// Until when notifications are held at `now`, by either: the later
    /// end when both hold them.
    pub fn quiet_until(&self, now: i64) -> Option<i64> {
        match (self.snoozed(now), self.scheduled(now)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether notifications are held at `now`.
    pub fn quiet(&self, now: i64) -> bool {
        self.quiet_until(now).is_some()
    }

    /// Whether the schedule's stretch is over at `now`, so Slack should be
    /// asked for the next one.
    pub fn schedule_passed(&self, now: i64) -> bool {
        self.schedule.is_some_and(|(_, end)| end <= now)
    }
}

/// How long to snooze notifications for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Snooze {
    Minutes20,
    Hour1,
    Hours2,
    /// Until nine in the morning, local time, tomorrow.
    Tomorrow,
}

impl Snooze {
    /// Every choice, in menu order.
    pub const ALL: [Snooze; 4] = [
        Snooze::Minutes20,
        Snooze::Hour1,
        Snooze::Hours2,
        Snooze::Tomorrow,
    ];

    /// What the menu calls the choice.
    pub fn label(self) -> String {
        use crate::i18n::t;
        match self {
            Snooze::Minutes20 => t("For 20 minutes"),
            Snooze::Hour1 => t("For 1 hour"),
            Snooze::Hours2 => t("For 2 hours"),
            Snooze::Tomorrow => t("Until tomorrow"),
        }
        .into_owned()
    }

    /// The minutes the snooze lasts from `now`, which Slack's
    /// `dnd.setSnooze` takes. "Tomorrow" ends at 09:00 local time the
    /// next day; a time zone that skips that hour moves it on.
    pub fn minutes(self, now: &jiff::Zoned) -> u32 {
        match self {
            Snooze::Minutes20 => 20,
            Snooze::Hour1 => 60,
            Snooze::Hours2 => 120,
            Snooze::Tomorrow => {
                let morning = now
                    .date()
                    .tomorrow()
                    .ok()
                    .and_then(|day| day.at(9, 0, 0, 0).to_zoned(now.time_zone().clone()).ok());
                let seconds = morning.map_or(12 * 3600, |end| {
                    end.timestamp().as_second() - now.timestamp().as_second()
                });
                // Round up, so the snooze never ends a minute early.
                u32::try_from((seconds + 59) / 60)
                    .unwrap_or(u32::MAX)
                    .max(1)
            }
        }
    }
}

/// When notifications come back, as the menu says it: "14:20" today,
/// "tomorrow at 09:00", or a date further out. `today` is the local date
/// now.
pub fn until_label(until: i64, today: jiff::civil::Date, tz: &jiff::tz::TimeZone) -> String {
    use crate::i18n::tf;
    let Ok(at) = jiff::Timestamp::from_second(until) else {
        return String::new();
    };
    let zoned = at.to_zoned(tz.clone());
    let time = zoned.strftime("%H:%M").to_string();
    if zoned.date() == today {
        tf("until {time}", &[("time", &time)])
    } else if today.tomorrow().ok() == Some(zoned.date()) {
        tf("until tomorrow at {time}", &[("time", &time)])
    } else {
        let date = zoned.strftime("%Y-%m-%d").to_string();
        tf(
            "until {date} at {time}",
            &[("date", &date), ("time", &time)],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snooze_holds_until_it_ends() {
        let dnd = Dnd {
            schedule: None,
            snooze_until: Some(100),
        };
        assert!(dnd.quiet(99));
        assert!(!dnd.quiet(100));
        assert_eq!(dnd.quiet_until(50), Some(100));
    }

    #[test]
    fn the_schedule_holds_only_inside_its_stretch() {
        let dnd = Dnd {
            schedule: Some((100, 200)),
            snooze_until: None,
        };
        assert!(!dnd.quiet(99));
        assert!(dnd.quiet(100));
        assert!(!dnd.quiet(200));
        assert!(!dnd.schedule_passed(150));
        assert!(dnd.schedule_passed(200));
        let both = Dnd {
            snooze_until: Some(250),
            ..dnd
        };
        assert_eq!(both.quiet_until(150), Some(250));
    }

    #[test]
    fn tomorrow_means_nine_in_the_morning() {
        let tz = jiff::tz::TimeZone::fixed(jiff::tz::offset(2));
        let now = jiff::civil::date(2026, 10, 2)
            .at(22, 30, 0, 0)
            .to_zoned(tz)
            .expect("valid");
        assert_eq!(Snooze::Tomorrow.minutes(&now), 10 * 60 + 30);
        assert_eq!(Snooze::Hour1.minutes(&now), 60);
        let early = jiff::civil::date(2026, 10, 2)
            .at(8, 59, 30, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .expect("valid");
        assert_eq!(Snooze::Tomorrow.minutes(&early), 24 * 60 + 1);
    }

    #[test]
    fn the_end_reads_relative_to_today() {
        let tz = jiff::tz::TimeZone::UTC;
        let today = jiff::civil::date(2026, 10, 2);
        let at = |d: i8, h: i8| {
            jiff::civil::date(2026, 10, d)
                .at(h, 0, 0, 0)
                .to_zoned(tz.clone())
                .expect("valid")
                .timestamp()
                .as_second()
        };
        assert_eq!(until_label(at(2, 14), today, &tz), "until 14:00");
        assert_eq!(until_label(at(3, 9), today, &tz), "until tomorrow at 09:00");
        assert_eq!(
            until_label(at(5, 9), today, &tz),
            "until 2026-10-05 at 09:00"
        );
    }
}
