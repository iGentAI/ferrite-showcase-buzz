//! A five-field cron matcher, in UTC, with no dependencies.
//!
//! [`crate::AgentBuilder::schedule`] takes a cron expression, so the SDK needs
//! to answer one question: *does this minute match?* That is a pure function of
//! the expression and a Unix timestamp, so it lives here as one — testable
//! without a clock, a runtime, or a network.
//!
//! ## Which tier this is for
//!
//! This matcher is the scheduling path for an **out-of-process, node-tier**
//! agent — its own long-lived process, ticking its own run loop. It is *not* the
//! path for a **constrained persona** hosted by the relay: that
//! tier does not own an ambient clock or a background loop, and its schedules are
//! declared in the AgentBundle and lowered onto `ferrite-workflows`, which fires
//! them with generation-checked idempotency. The platform owns
//! durable scheduling; this is the client-library convenience for the tier that
//! cannot reach it. The two do not overlap, and neither is a reduced copy of the
//! other — a missed-while-down schedule here is explicitly *not* backfilled,
//! whereas `ferrite-workflows` has an explicit catch-up policy.
//!
//! ## What it supports, exactly
//!
//! `minute hour day-of-month month day-of-week`, where every field is a
//! comma-separated list of `*`, `N`, `A-B`, or any of those with a `/step`
//! suffix. Day-of-week takes `0`–`7` with both `0` and `7` meaning Sunday.
//!
//! ## What it does not support, deliberately
//!
//! * **No seconds field and no year field.** Five fields, POSIX order.
//! * **No month or weekday names** (`JAN`, `MON`) — numbers only.
//! * **No `@daily`/`@hourly` macros**, no `L`, `W`, `#`, or `?` extensions.
//! * **No timezones.** Matching is against UTC. A workspace spans timezones and
//!   a schedule that silently drifts twice a year is worse than one that never
//!   does; a caller who wants local time should offset the expression.
//!
//! ## The day-of-month / day-of-week rule
//!
//! When *both* day fields are restricted, a day matches if **either** does —
//! Vixie cron's behavior, which `0 9 13 * 5` (the 13th, and every Friday) relies
//! on. When one is `*`, only the other constrains. This is the one place cron
//! semantics are surprising, so it is implemented explicitly rather than as a
//! plain conjunction.

use crate::error::{Result, SdkError};

const DAY_SECS: u64 = 86_400;

/// A parsed cron expression.
///
/// Each field is a bitmask, so [`Cron::matches`] is five shifts and five `AND`s
/// — cheap enough to call once a second in an agent's run loop without thinking
/// about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    day_of_month: u64,
    month: u64,
    day_of_week: u64,
    /// Whether the day-of-month field was `*`. Needed for the OR rule above:
    /// a mask alone cannot distinguish "every day" from "all 31 days listed".
    dom_unrestricted: bool,
    /// Whether the day-of-week field was `*`.
    dow_unrestricted: bool,
}

impl Cron {
    /// Parse a five-field expression.
    ///
    /// # Errors
    ///
    /// [`SdkError::Config`] naming the field and the offending token: a bad
    /// schedule is a startup mistake, and the message is what a developer sees
    /// when their agent refuses to start.
    pub fn parse(expression: &str) -> Result<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, dom, month, dow] = fields.as_slice() else {
            return Err(SdkError::config(format!(
                "cron expression needs exactly 5 fields (minute hour day-of-month month day-of-week), got {}: {expression:?}",
                fields.len()
            )));
        };
        Ok(Self {
            minute: field(minute, 0, 59, "minute")?,
            hour: field(hour, 0, 23, "hour")?,
            day_of_month: field(dom, 1, 31, "day-of-month")?,
            month: field(month, 1, 12, "month")?,
            day_of_week: weekday_field(dow)?,
            dom_unrestricted: *dom == "*",
            dow_unrestricted: *dow == "*",
        })
    }

    /// Does the UTC minute containing `unix_secs` match?
    ///
    /// Minute granularity: every second inside a matching minute matches, so a
    /// caller must remember which minute it last fired for (see
    /// [`Cron::minute_of`]) or it will fire up to sixty times.
    #[must_use]
    pub fn matches(&self, unix_secs: u64) -> bool {
        let t = UtcTime::from_unix(unix_secs);
        if !bit(self.minute, u64::from(t.minute)) || !bit(self.hour, u64::from(t.hour)) {
            return false;
        }
        if !bit(self.month, u64::from(t.month)) {
            return false;
        }
        let dom_hit = bit(self.day_of_month, u64::from(t.day));
        let dow_hit = bit(self.day_of_week, u64::from(t.weekday));
        match (self.dom_unrestricted, self.dow_unrestricted) {
            // Both `*`: every day.
            (true, true) => true,
            // One restricted: that one decides.
            (true, false) => dow_hit,
            (false, true) => dom_hit,
            // Both restricted: either satisfies the schedule (Vixie cron).
            (false, false) => dom_hit || dow_hit,
        }
    }

    /// The minute number a timestamp falls in — the natural fire-once key.
    ///
    /// A scheduler stores this after firing and refuses to fire again while the
    /// value is unchanged, which makes "at most once per matching minute" a
    /// property of the data rather than of the polling interval.
    #[must_use]
    pub const fn minute_of(unix_secs: u64) -> u64 {
        unix_secs / 60
    }
}

/// Whether bit `n` is set. `n` above 63 is never set, which keeps every caller
/// from having to bounds-check a field value.
const fn bit(mask: u64, n: u64) -> bool {
    n < 64 && (mask & (1u64 << n)) != 0
}

fn field(spec: &str, min: u64, max: u64, name: &str) -> Result<u64> {
    let mut mask = 0u64;
    if spec.is_empty() {
        return Err(SdkError::config(format!("cron {name} field is empty")));
    }
    for part in spec.split(',') {
        mask |= part_mask(part, min, max, name)?;
    }
    if mask == 0 {
        return Err(SdkError::config(format!(
            "cron {name} field {spec:?} matches nothing"
        )));
    }
    Ok(mask)
}

/// Parse one comma-separated element: `*`, `N`, `A-B`, or any with `/step`.
fn part_mask(part: &str, min: u64, max: u64, name: &str) -> Result<u64> {
    let (range, step) = match part.split_once('/') {
        Some((range, step)) => {
            let step: u64 = step.parse().map_err(|_| {
                SdkError::config(format!("cron {name} step {step:?} is not a number"))
            })?;
            if step == 0 {
                return Err(SdkError::config(format!("cron {name} step must not be 0")));
            }
            (range, step)
        }
        None => (part, 1),
    };

    let (start, end) = if range == "*" {
        (min, max)
    } else if let Some((a, b)) = range.split_once('-') {
        (number(a, min, max, name)?, number(b, min, max, name)?)
    } else {
        let n = number(range, min, max, name)?;
        // A bare number with a step means "from n to the end of the field",
        // which is what `*/15` and `5/15` both mean in cron.
        if step > 1 {
            (n, max)
        } else {
            (n, n)
        }
    };
    if start > end {
        return Err(SdkError::config(format!(
            "cron {name} range {range:?} runs backwards"
        )));
    }

    let mut mask = 0u64;
    let mut value = start;
    while value <= end {
        mask |= 1u64 << value;
        value = value.saturating_add(step);
    }
    Ok(mask)
}

fn number(text: &str, min: u64, max: u64, name: &str) -> Result<u64> {
    let n: u64 = text
        .trim()
        .parse()
        .map_err(|_| SdkError::config(format!("cron {name} value {text:?} is not a number")))?;
    if n < min || n > max {
        return Err(SdkError::config(format!(
            "cron {name} value {n} is outside {min}..={max}"
        )));
    }
    Ok(n)
}

/// Day-of-week, where `7` is a second spelling of Sunday.
fn weekday_field(spec: &str) -> Result<u64> {
    let raw = field(spec, 0, 7, "day-of-week")?;
    // Fold bit 7 onto bit 0 so `matches` only ever tests 0..=6.
    let sunday_high = raw & (1u64 << 7);
    Ok((raw & !(1u64 << 7)) | if sunday_high != 0 { 1 } else { 0 })
}

/// A UTC calendar instant, to minute resolution.
///
/// Derived arithmetically from the Unix timestamp — no `chrono`, no `time`, no
/// leap-second table (Unix time has none to account for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcTime {
    /// Proleptic Gregorian year.
    pub year: u64,
    /// Month, 1–12.
    pub month: u32,
    /// Day of month, 1–31.
    pub day: u32,
    /// Hour, 0–23.
    pub hour: u32,
    /// Minute, 0–59.
    pub minute: u32,
    /// Second, 0–59.
    pub second: u32,
    /// Day of week, 0 = Sunday.
    pub weekday: u32,
}

impl UtcTime {
    /// Split a Unix timestamp into UTC calendar fields.
    ///
    /// The date half is Howard Hinnant's `civil_from_days`, which is exact for
    /// every day the Gregorian calendar defines and needs no lookup tables.
    #[must_use]
    pub const fn from_unix(secs: u64) -> Self {
        let days = secs / DAY_SECS;
        let time_of_day = secs % DAY_SECS;

        // civil_from_days, era-based. Shift the epoch to 0000-03-01 so leap days
        // land at the end of the year and the month arithmetic is linear.
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };

        Self {
            year,
            month,
            day,
            hour: (time_of_day / 3_600) as u32,
            minute: ((time_of_day % 3_600) / 60) as u32,
            second: (time_of_day % 60) as u32,
            // 1970-01-01 was a Thursday, and 0 is Sunday here.
            weekday: ((days + 4) % 7) as u32,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Convert a UTC calendar instant to a timestamp, for readable tests.
    fn ts(year: u64, month: u64, day: u64, hour: u64, minute: u64) -> u64 {
        // days_from_civil, the inverse of the function under test. Written
        // independently so a bug in one does not hide a bug in the other.
        let y = if month <= 2 { year - 1 } else { year };
        let era = y / 400;
        let yoe = y - era * 400;
        let mp = if month > 2 { month - 3 } else { month + 9 };
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        days * DAY_SECS + hour * 3_600 + minute * 60
    }

    #[test]
    fn calendar_conversion_matches_known_instants() {
        let epoch = UtcTime::from_unix(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        assert_eq!(epoch.weekday, 4, "1970-01-01 was a Thursday");

        // 2026-07-27T14:35:09Z, a Monday.
        let t = UtcTime::from_unix(ts(2026, 7, 27, 14, 35) + 9);
        assert_eq!((t.year, t.month, t.day), (2026, 7, 27));
        assert_eq!((t.hour, t.minute, t.second), (14, 35, 9));
        assert_eq!(t.weekday, 1, "2026-07-27 was a Monday");

        // Leap day.
        let leap = UtcTime::from_unix(ts(2024, 2, 29, 0, 0));
        assert_eq!((leap.year, leap.month, leap.day), (2024, 2, 29));

        // 1900 is not a leap year, 2000 is; the era arithmetic must get both.
        assert_eq!(UtcTime::from_unix(ts(2000, 2, 29, 12, 0)).day, 29);
    }

    #[test]
    fn the_readme_example_fires_on_weekday_mornings_only() {
        // "0 9 * * 1-5" — 09:00 UTC, Monday through Friday.
        let cron = Cron::parse("0 9 * * 1-5").unwrap();
        assert!(cron.matches(ts(2026, 7, 27, 9, 0)), "Monday 09:00");
        assert!(cron.matches(ts(2026, 7, 31, 9, 0)), "Friday 09:00");
        assert!(!cron.matches(ts(2026, 8, 1, 9, 0)), "Saturday");
        assert!(!cron.matches(ts(2026, 8, 2, 9, 0)), "Sunday");
        assert!(
            !cron.matches(ts(2026, 7, 27, 9, 1)),
            "09:01 is a different minute"
        );
        assert!(!cron.matches(ts(2026, 7, 27, 10, 0)), "10:00");
    }

    #[test]
    fn steps_lists_and_ranges_all_parse() {
        let quarter = Cron::parse("*/15 * * * *").unwrap();
        for minute in 0..60 {
            assert_eq!(
                quarter.matches(ts(2026, 7, 27, 0, minute)),
                minute % 15 == 0,
                "minute {minute}"
            );
        }

        let listed = Cron::parse("5,10,55 0 1 1 *").unwrap();
        assert!(listed.matches(ts(2027, 1, 1, 0, 55)));
        assert!(!listed.matches(ts(2027, 1, 1, 0, 56)));
        assert!(!listed.matches(ts(2027, 1, 2, 0, 55)), "wrong day of month");

        let stepped_range = Cron::parse("0 8-18/2 * * *").unwrap();
        assert!(stepped_range.matches(ts(2026, 7, 27, 8, 0)));
        assert!(stepped_range.matches(ts(2026, 7, 27, 18, 0)));
        assert!(!stepped_range.matches(ts(2026, 7, 27, 9, 0)));

        let from_offset = Cron::parse("7/20 * * * *").unwrap();
        assert!(from_offset.matches(ts(2026, 7, 27, 0, 7)));
        assert!(from_offset.matches(ts(2026, 7, 27, 0, 27)));
        assert!(from_offset.matches(ts(2026, 7, 27, 0, 47)));
        assert!(!from_offset.matches(ts(2026, 7, 27, 0, 8)));
    }

    #[test]
    fn sunday_is_both_zero_and_seven() {
        let zero = Cron::parse("30 3 * * 0").unwrap();
        let seven = Cron::parse("30 3 * * 7").unwrap();
        let sunday = ts(2026, 8, 2, 3, 30);
        assert!(zero.matches(sunday));
        assert!(seven.matches(sunday));
        assert!(!zero.matches(ts(2026, 8, 3, 3, 30)), "Monday");
    }

    /// The one genuinely surprising cron rule, pinned so a refactor cannot
    /// quietly turn it into an AND.
    #[test]
    fn both_day_fields_restricted_means_either_may_match() {
        // The 13th of any month, and every Friday.
        let cron = Cron::parse("0 0 13 * 5").unwrap();
        assert!(cron.matches(ts(2026, 8, 13, 0, 0)), "the 13th (a Thursday)");
        assert!(cron.matches(ts(2026, 8, 14, 0, 0)), "a Friday");
        assert!(!cron.matches(ts(2026, 8, 12, 0, 0)), "neither");

        // With one field unrestricted, only the other constrains.
        let dom_only = Cron::parse("0 0 13 * *").unwrap();
        assert!(dom_only.matches(ts(2026, 8, 13, 0, 0)));
        assert!(!dom_only.matches(ts(2026, 8, 14, 0, 0)));
    }

    #[test]
    fn malformed_expressions_are_refused_with_a_useful_message() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "10-5 * * * *",
            "abc * * * *",
            "*/x * * * *",
            ", * * * *",
        ] {
            let err = Cron::parse(bad).unwrap_err();
            assert!(
                matches!(err, SdkError::Config(_)),
                "{bad:?} produced {err:?}"
            );
        }
        let named = Cron::parse("0 9 * * MON").unwrap_err();
        assert!(
            named.to_string().contains("day-of-week"),
            "names are unsupported and the error should say which field: {named}"
        );
    }

    #[test]
    fn the_fire_once_key_is_the_minute() {
        let base = ts(2026, 7, 27, 9, 0);
        assert_eq!(Cron::minute_of(base), Cron::minute_of(base + 59));
        assert_ne!(Cron::minute_of(base), Cron::minute_of(base + 60));
    }
}
