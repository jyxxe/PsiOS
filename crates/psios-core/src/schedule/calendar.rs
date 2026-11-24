use super::{
    Window,
    repeat::{Missed, Overlap},
};
use crate::{Result, err, require};
use chrono::{
    DateTime, Days, LocalResult, NaiveDate, NaiveTime, TimeDelta, TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
enum Fold {
    Earlier,
    Later,
    Both,
}

#[derive(Deserialize, Serialize)]
enum Gap {
    Skip,
    Reject,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Calendar {
    pub timezone: String,
    start_date: String,
    local_time: String,
    days: u32,
    window_seconds: u32,
    fold: Fold,
    gap: Gap,
    pub missed: Missed,
    pub overlap: Overlap,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CalendarPlan {
    timezone_database: String,
    pub(super) windows: Vec<Window>,
    skipped_dates: Vec<String>,
}

impl Calendar {
    pub(super) fn plan(&self) -> Result<CalendarPlan> {
        require(
            (1..=30).contains(&self.days) && (1..=86400).contains(&self.window_seconds),
            "InvalidArgument",
            "calendar requires 1 to 30 local dates and a 1 to 86400 second execution window",
        )?;
        let timezone: Tz = self
            .timezone
            .parse()
            .map_err(|_| err("InvalidArgument", "unknown IANA time zone"))?;
        let start = NaiveDate::parse_from_str(&self.start_date, "%Y-%m-%d")
            .map_err(|_| err("InvalidArgument", "calendar date requires YYYY-MM-DD"))?;
        let time = NaiveTime::parse_from_str(&self.local_time, "%H:%M:%S")
            .map_err(|_| err("InvalidArgument", "calendar time requires HH:MM:SS"))?;
        require(
            self.start_date.len() == 10
                && start.format("%Y-%m-%d").to_string() == self.start_date
                && time.format("%H:%M:%S").to_string() == self.local_time
                && time.nanosecond() == 0,
            "InvalidArgument",
            "calendar requires a canonical local date and second",
        )?;
        let mut instants: Vec<DateTime<Utc>> = Vec::new();
        let mut skipped_dates = Vec::new();
        for offset in 0..self.days {
            let date = start
                .checked_add_days(Days::new(u64::from(offset)))
                .ok_or_else(|| err("InvalidArgument", "calendar date range overflow"))?;
            match timezone.from_local_datetime(&date.and_time(time)) {
                LocalResult::Single(at) => instants.push(at.to_utc()),
                LocalResult::Ambiguous(earlier, later) => {
                    if matches!(self.fold, Fold::Earlier | Fold::Both) {
                        instants.push(earlier.to_utc());
                    }
                    if matches!(self.fold, Fold::Later | Fold::Both) {
                        instants.push(later.to_utc());
                    }
                }
                LocalResult::None if matches!(self.gap, Gap::Skip) => {
                    skipped_dates.push(date.to_string())
                }
                LocalResult::None => {
                    return Err(err(
                        "InvalidArgument",
                        format!("local time is missing on {date} in {timezone}"),
                    ));
                }
            }
        }
        instants.sort_unstable();
        require(
            (1..=32).contains(&instants.len())
                && instants.windows(2).all(|pair| pair[0] < pair[1])
                && self.missed.valid(instants.len() as u32),
            "InvalidArgument",
            "calendar requires 1 to 32 distinct occurrences and a valid catch-up limit",
        )?;
        let allowance = self.allowance();
        let windows = instants
            .into_iter()
            .map(|run| Window {
                run_at: run,
                deadline_at: run + allowance,
            })
            .collect();
        Ok(CalendarPlan {
            timezone_database: chrono_tz::IANA_TZDB_VERSION.to_owned(),
            windows,
            skipped_dates,
        })
    }

    fn allowance(&self) -> TimeDelta {
        TimeDelta::seconds(i64::from(self.window_seconds))
    }

    pub(super) fn validate_plan(&self, plan: &CalendarPlan) -> Result<()> {
        require(
            (1..=30).contains(&self.days)
                && (1..=86400).contains(&self.window_seconds)
                && (1..=32).contains(&plan.windows.len())
                && self.missed.valid(plan.windows.len() as u32)
                && !plan.timezone_database.is_empty()
                && plan
                    .windows
                    .iter()
                    .all(|window| window.deadline_at - window.run_at == self.allowance())
                && plan
                    .windows
                    .windows(2)
                    .all(|pair| pair[0].run_at < pair[1].run_at),
            "RecoveryRequired",
            "invalid pinned calendar schedule",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::timestamp;

    fn ny(date: &str, time: &str, days: u32, fold: Fold, gap: Gap) -> Calendar {
        Calendar {
            timezone: "America/New_York".into(),
            start_date: date.into(),
            local_time: time.into(),
            days,
            window_seconds: 120,
            fold,
            gap,
            missed: Missed::Skip,
            overlap: Overlap::Allow,
        }
    }

    #[test]
    fn skips_or_rejects_a_missing_local_time_in_a_real_zone() {
        let planned = ny("2026-03-07", "02:30:00", 3, Fold::Earlier, Gap::Skip)
            .plan()
            .unwrap();
        assert_eq!(planned.skipped_dates, ["2026-03-08"]);
        assert_eq!(timestamp(planned.windows[0].run_at), "2026-03-07T07:30:00Z");
        assert_eq!(timestamp(planned.windows[1].run_at), "2026-03-09T06:30:00Z");
        assert!(
            ny("2026-03-07", "02:30:00", 3, Fold::Earlier, Gap::Reject)
                .plan()
                .is_err()
        );
    }

    #[test]
    fn distinguishes_the_two_instants_in_a_local_clock_fold() {
        let date = "2026-11-01";
        let time = "01:30:00";
        let both = ny(date, time, 1, Fold::Both, Gap::Reject).plan().unwrap();
        assert_eq!(both.windows.len(), 2);
        assert_eq!(timestamp(both.windows[0].run_at), "2026-11-01T05:30:00Z");
        assert_eq!(timestamp(both.windows[1].run_at), "2026-11-01T06:30:00Z");
        assert_eq!(
            timestamp(both.windows[1].deadline_at),
            "2026-11-01T06:32:00Z"
        );
        let earlier = ny(date, time, 1, Fold::Earlier, Gap::Skip).plan().unwrap();
        let later = ny(date, time, 1, Fold::Later, Gap::Skip).plan().unwrap();
        assert_eq!(earlier.windows[0].run_at, both.windows[0].run_at);
        assert_eq!(later.windows[0].run_at, both.windows[1].run_at);
    }
}
