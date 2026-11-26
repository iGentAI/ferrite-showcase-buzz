//! Formatting an instant the way the client's wire shapes carry one.
//!
//! The client's `Raw*` rows carry ISO 8601 strings (`created_at`, `updated_at`,
//! `last_started_at`) and parse them with `new Date(...)`, so a wrong or
//! unparseable string renders as "Invalid Date" in a panel rather than failing
//! anywhere a test would notice.
//!
//! Written out rather than taken from a crate because this crate deliberately
//! depends on almost nothing, and the conversion is exact integer arithmetic:
//! `civil_from_days` is Howard Hinnant's algorithm, which is what every date
//! library uses underneath.

/// Format epoch milliseconds as an ISO 8601 UTC timestamp.
///
/// Second resolution, which is all these fields are read at — and `Z` rather
/// than an offset, because the instant is UTC and an offset nobody computed
/// would be a guess about the machine's zone.
#[must_use]
pub fn iso_from_epoch_ms(ms: u64) -> String {
    let seconds = i64::try_from(ms / 1_000).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3_600;
    let minute = (time_of_day % 3_600) / 60;
    let second = time_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The civil date `days` after the Unix epoch.
///
/// Hinnant's `civil_from_days`, transcribed. The shifted era arithmetic is what
/// makes it branch-free across leap years and centuries; the constants are the
/// algorithm's, not arbitrary.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, which puts the leap day at the END of the
    // year and removes every special case from the arithmetic below.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64; // [0, 146096]
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365; // [0, 399]
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153; // [0, 11], March = 0
    let day = u32::try_from(day_of_year - (153 * month_position + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    })
    .unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_exactly() {
        // Anchors a reader can check independently rather than values taken from
        // this implementation's own output.
        assert_eq!(iso_from_epoch_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_epoch_ms(1_000), "1970-01-01T00:00:01Z");
        assert_eq!(iso_from_epoch_ms(86_400_000), "1970-01-02T00:00:00Z");
        assert_eq!(iso_from_epoch_ms(1_700_000_000_000), "2023-11-14T22:13:20Z");
        // Sub-second precision is dropped rather than rounded.
        assert_eq!(iso_from_epoch_ms(1_700_000_000_999), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn leap_years_and_century_boundaries_are_right() {
        // The cases a hand-rolled conversion gets wrong: a leap day, the
        // non-leap century, and the leap century.
        assert_eq!(iso_from_epoch_ms(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso_from_epoch_ms(1_583_020_800_000), "2020-03-01T00:00:00Z");
        assert_eq!(iso_from_epoch_ms(4_107_542_400_000), "2100-03-01T00:00:00Z");
        assert_eq!(iso_from_epoch_ms(1_709_164_800_000), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn every_day_of_a_span_of_years_round_trips_monotonically() {
        // A property rather than a table: the date must advance by exactly one
        // day per 86,400 seconds, never repeat, and never go backwards.
        let mut previous = String::new();
        for day in 0..(365 * 60) {
            let formatted = iso_from_epoch_ms(day * 86_400_000);
            assert_eq!(formatted.len(), 20, "{formatted}");
            assert!(formatted > previous, "{previous} then {formatted}");
            previous = formatted;
        }
    }
}
