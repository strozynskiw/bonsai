//! Pure layout math for the `/usage` activity heatmap: a GitHub-style grid of
//! 7 weekday rows × N week columns, sized to the modal's real inner width at
//! render time (the modal width is a percentage of the terminal, never a fixed
//! column count). No I/O and no styling here — unit-testable geometry only.

use crate::storage::{DailyUsage, LocalToday};

/// Weekday row labels, index 0 = Sunday to match `strftime('%w')`.
pub(super) const DAY_LABELS: [&str; 7] = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];

const MONTH_LABELS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Columns taken by the weekday gutter (`"Su "`).
pub(super) const GUTTER_WIDTH: usize = 3;
/// A full year of weeks, the widest the grid ever grows.
const MAX_WEEKS: usize = 53;

/// One heatmap cell: absent (future), inactive, or active at a quantile level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeatCell {
    /// After today — rendered blank.
    Future,
    /// No recorded activity.
    Empty,
    /// Active day with intensity level 1..=4 (quantile bucket).
    Level(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HeatmapLayout {
    /// Cell columns per week: 2 ("█ ") on wide terminals, 1 when space is tight.
    pub pitch: usize,
    /// Number of week columns, most recent week last.
    pub weeks: usize,
    /// SQLite-truncated julian day of the first column's Sunday.
    pub first_week_start: i64,
    /// `grid[week][weekday]` intensity cells.
    pub grid: Vec<[HeatCell; 7]>,
    /// `(week_column, label)` month labels, non-overlapping.
    pub month_labels: Vec<(usize, &'static str)>,
}

pub(super) fn heatmap_layout(
    days: &[DailyUsage],
    today: &LocalToday,
    inner_width: usize,
) -> HeatmapLayout {
    let pitch = if inner_width >= GUTTER_WIDTH + MAX_WEEKS * 2 {
        2
    } else {
        1
    };
    let weeks = (inner_width.saturating_sub(GUTTER_WIDTH) / pitch).clamp(1, MAX_WEEKS);

    let current_week_start = today.julian_day - i64::from(today.weekday);
    let first_week_start = current_week_start - (weeks as i64 - 1) * 7;

    let thresholds = quantile_thresholds(days, first_week_start);
    let mut grid = vec![[HeatCell::Empty; 7]; weeks];
    for (week, row) in grid.iter_mut().enumerate() {
        for (weekday, cell) in row.iter_mut().enumerate() {
            let julian = first_week_start + week as i64 * 7 + weekday as i64;
            if julian > today.julian_day {
                *cell = HeatCell::Future;
            }
        }
    }
    for day in days {
        if day.julian_day < first_week_start || day.julian_day > today.julian_day {
            continue;
        }
        let offset = (day.julian_day - first_week_start) as usize;
        let (week, weekday) = (offset / 7, offset % 7);
        let total = day.total_tokens();
        if total > 0 {
            grid[week][weekday] = HeatCell::Level(intensity_level(total, thresholds));
        }
    }

    HeatmapLayout {
        pitch,
        weeks,
        first_week_start,
        month_labels: month_labels(first_week_start, weeks),
        grid,
    }
}

/// GitHub-style quantile thresholds (p25/p50/p75) over the *visible* nonzero
/// day totals. Degenerate distributions (all days equal) collapse levels
/// harmlessly toward the top bucket.
fn quantile_thresholds(days: &[DailyUsage], first_week_start: i64) -> [i64; 3] {
    let mut totals: Vec<i64> = days
        .iter()
        .filter(|day| day.julian_day >= first_week_start)
        .map(DailyUsage::total_tokens)
        .filter(|total| *total > 0)
        .collect();
    totals.sort_unstable();
    if totals.is_empty() {
        return [0; 3];
    }
    let at = |fraction: usize| totals[(totals.len() * fraction / 4).min(totals.len() - 1)];
    [at(1), at(2), at(3)]
}

fn intensity_level(total: i64, thresholds: [i64; 3]) -> u8 {
    match total {
        t if t <= thresholds[0] => 1,
        t if t <= thresholds[1] => 2,
        t if t <= thresholds[2] => 3,
        _ => 4,
    }
}

/// A month label at each week column whose Sunday starts a new month, skipping
/// labels that would collide with the previous one (3-char labels).
fn month_labels(first_week_start: i64, weeks: usize) -> Vec<(usize, &'static str)> {
    let mut labels = Vec::new();
    let mut previous_month = civil_month_from_sqlite_julian(first_week_start - 7);
    let mut last_label_column: Option<usize> = None;
    for week in 0..weeks {
        let month = civil_month_from_sqlite_julian(first_week_start + week as i64 * 7);
        if month != previous_month {
            let collides = last_label_column
                .is_some_and(|column| week.saturating_sub(column) * label_pitch(weeks) < 4);
            if !collides {
                labels.push((week, MONTH_LABELS[(month - 1) as usize]));
                last_label_column = Some(week);
            }
        }
        previous_month = month;
    }
    labels
}

/// Cell pitch assumed for label collision checks: 1 keeps it conservative for
/// narrow layouts, which is also correct for wide (pitch-2) ones.
const fn label_pitch(_weeks: usize) -> usize {
    1
}

/// Month (1–12) of a SQLite `CAST(julianday(date) AS INTEGER)` day number.
///
/// SQLite's `julianday` of a date string is the *midnight-UTC* Julian date
/// (`JDN − 0.5`), so the CAST truncates to `JDN − 1`; add the 1 back before
/// running Fliegel–Van Flandern. The storage integration test round-trips this
/// against SQLite's own `date()` strings.
fn civil_month_from_sqlite_julian(sqlite_julian: i64) -> i64 {
    civil_from_julian_day_number(sqlite_julian + 1).1
}

/// Fliegel–Van Flandern: Julian Day Number → (year, month, day) in the
/// proleptic Gregorian calendar.
pub(super) fn civil_from_julian_day_number(jdn: i64) -> (i64, i64, i64) {
    let l = jdn + 68_569;
    let n = 4 * l / 146_097;
    let l = l - (146_097 * n + 3) / 4;
    let i = 4_000 * (l + 1) / 1_461_001;
    let l = l - 1_461 * i / 4 + 31;
    let j = 80 * l / 2_447;
    let day = l - 2_447 * j / 80;
    let l = j / 11;
    let month = j + 2 - 12 * l;
    let year = 100 * (n - 49) + i + l;
    (year, month, day)
}

/// `YYYY-MM-DD` for a SQLite-truncated julian day; must match SQLite's own
/// `date()` output for the same day.
#[cfg(test)]
fn day_string_from_sqlite_julian(sqlite_julian: i64) -> String {
    let (year, month, day) = civil_from_julian_day_number(sqlite_julian + 1);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(julian_day: i64, weekday: u8, tokens: i64) -> DailyUsage {
        DailyUsage {
            day: String::new(),
            julian_day,
            weekday,
            turns: 1,
            sessions: 1,
            input_tokens: tokens,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_measured_tokens: 0,
            cost_micros: 0,
            savings_micros: 0,
            cold_turns: 0,
            warm_eligible_turns: 0,
        }
    }

    fn today(julian_day: i64, weekday: u8) -> LocalToday {
        LocalToday {
            day: String::new(),
            julian_day,
            weekday,
        }
    }

    /// 2000-01-01 is JDN 2451545; SQLite's CAST(julianday(...)) yields 2451544.
    const SQLITE_Y2K: i64 = 2_451_544;

    #[test]
    fn civil_conversion_hits_known_dates() {
        assert_eq!(civil_from_julian_day_number(SQLITE_Y2K + 1), (2000, 1, 1));
        assert_eq!(day_string_from_sqlite_julian(SQLITE_Y2K), "2000-01-01");
        // 2000-02-29: the leap day of a century leap year, 59 days later.
        assert_eq!(day_string_from_sqlite_julian(SQLITE_Y2K + 59), "2000-02-29");
    }

    #[test]
    fn layout_narrow_width_shrinks_weeks_and_pitch() {
        let layout = heatmap_layout(&[], &today(1_000, 3), 30);
        assert_eq!(layout.pitch, 1);
        assert_eq!(layout.weeks, 27);
    }

    #[test]
    fn layout_wide_width_uses_two_column_pitch_and_caps_weeks() {
        let layout = heatmap_layout(&[], &today(1_000, 3), 200);
        assert_eq!(layout.pitch, 2);
        assert_eq!(layout.weeks, 53);
    }

    #[test]
    fn grid_places_days_by_week_and_weekday() {
        // Today is a Wednesday (weekday 3) at julian 1_003; its week starts
        // at 1_000. Activity today and the prior Sunday.
        let days = [day(1_003, 3, 10), day(996, 3, 20)];
        let layout = heatmap_layout(&days, &today(1_003, 3), 200);
        let last_week = layout.weeks - 1;
        assert_eq!(layout.grid[last_week][3], HeatCell::Level(1));
        assert_eq!(layout.grid[last_week - 1][3], HeatCell::Level(2));
        // Thursday..Saturday of the current week are in the future.
        assert_eq!(layout.grid[last_week][4], HeatCell::Future);
        assert_eq!(layout.grid[last_week][6], HeatCell::Future);
        // An untouched past day is empty.
        assert_eq!(layout.grid[last_week][0], HeatCell::Empty);
    }

    #[test]
    fn gaps_between_active_days_stay_empty() {
        let days = [day(900, 3, 5), day(1_003, 3, 5)];
        let layout = heatmap_layout(&days, &today(1_003, 3), 200);
        let offset = (900 - layout.first_week_start) as usize;
        // Equal totals collapse the quantiles into the bottom bucket.
        assert_eq!(layout.grid[offset / 7][offset % 7], HeatCell::Level(1));
        let gap = (950 - layout.first_week_start) as usize;
        assert_eq!(layout.grid[gap / 7][gap % 7], HeatCell::Empty);
    }

    #[test]
    fn intensity_levels_follow_quantiles() {
        let days: Vec<DailyUsage> = (0..8)
            .map(|i| day(1_000 - i, ((1_000 - i) % 7) as u8, (i + 1) * 100))
            .collect();
        let layout = heatmap_layout(&days, &today(1_000, (1_000 % 7) as u8), 200);
        let cell = |julian: i64| {
            let offset = (julian - layout.first_week_start) as usize;
            layout.grid[offset / 7][offset % 7]
        };
        // Smallest (100 tokens) sits in the bottom bucket, largest (800) at
        // the top, with the second-largest (700) one bucket below.
        assert_eq!(cell(1_000), HeatCell::Level(1));
        assert_eq!(cell(993), HeatCell::Level(4));
        assert_eq!(cell(994), HeatCell::Level(3));
    }

    #[test]
    fn month_labels_mark_month_starts_without_overlap() {
        // 53 weeks ending at SQLITE_Y2K (a Saturday, weekday 6).
        let layout = heatmap_layout(&[], &today(SQLITE_Y2K, 6), 200);
        assert!(layout.month_labels.len() >= 11);
        let mut previous = None;
        for (column, label) in &layout.month_labels {
            assert!(!label.is_empty());
            if let Some(previous) = previous {
                assert!(*column > previous);
            }
            previous = Some(*column);
        }
    }
}
