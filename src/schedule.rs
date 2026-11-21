//! When is the next daily run?

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// The next moment strictly after `now` when the local clock in `tz` shows
/// `at`. If that local time does not exist on a day (DST gap), the run happens
/// one hour later; if it exists twice (DST fold), the first one is used.
pub fn next_run(now: DateTime<Utc>, at: NaiveTime, tz: Tz) -> DateTime<Utc> {
    let today = now.with_timezone(&tz).date_naive();
    for offset in 0..=2 {
        let day = today + Duration::days(offset);
        let local = day.and_time(at);
        let candidate = tz
            .from_local_datetime(&local)
            .earliest()
            .or_else(|| {
                tz.from_local_datetime(&(local + Duration::hours(1)))
                    .earliest()
            })
            .map(|d| d.with_timezone(&Utc));
        if let Some(c) = candidate
            && c > now
        {
            return c;
        }
    }
    unreachable!("a daily time always occurs within two days")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Madrid;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn later_today() {
        // 06:00 Madrid (CET, UTC+1) → 08:40 Madrid same day = 07:40 UTC
        assert_eq!(
            next_run(utc("2026-11-21T05:00:00Z"), t(8, 40), Madrid),
            utc("2026-11-21T07:40:00Z")
        );
    }

    #[test]
    fn tomorrow_when_already_passed_or_exact() {
        assert_eq!(
            next_run(utc("2026-11-21T07:40:00Z"), t(8, 40), Madrid),
            utc("2026-11-22T07:40:00Z")
        );
        assert_eq!(
            next_run(utc("2026-11-21T20:00:00Z"), t(8, 40), Madrid),
            utc("2026-11-22T07:40:00Z")
        );
    }

    #[test]
    fn summer_time_offset() {
        // CEST is UTC+2
        assert_eq!(
            next_run(utc("2026-07-01T00:00:00Z"), t(8, 40), Madrid),
            utc("2026-07-01T06:40:00Z")
        );
    }

    #[test]
    fn across_spring_forward() {
        // 2026-03-29: clocks jump 02:00 → 03:00 in Madrid.
        assert_eq!(
            next_run(utc("2026-03-28T08:00:00Z"), t(8, 40), Madrid),
            utc("2026-03-29T06:40:00Z")
        );
        // 02:30 does not exist that day → 03:30 CEST = 01:30 UTC
        assert_eq!(
            next_run(utc("2026-03-28T23:00:00Z"), t(2, 30), Madrid),
            utc("2026-03-29T01:30:00Z")
        );
    }

    #[test]
    fn across_fall_back() {
        // 2026-10-25: 03:00 → 02:00; 02:30 happens twice, take the first (CEST).
        assert_eq!(
            next_run(utc("2026-10-24T23:00:00Z"), t(2, 30), Madrid),
            utc("2026-10-25T00:30:00Z")
        );
    }
}
