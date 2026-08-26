//! Formatting shared by the screens that show build timing.

/// Build timestamps are Unix **seconds**, not milliseconds — `stats.rs`
/// compares them against `strftime('%s', 'now')`.
pub fn now_secs() -> i64 {
    (js_sys::Date::now() / 1000.0) as i64
}

/// How long a build took, from its two timestamps.
///
/// `None` for either end means the build has not finished (or never started),
/// which is not a zero-length build — so it reads as unknown rather than `0s`.
pub fn format_duration(start: Option<i64>, end: Option<i64>) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return "—".to_string();
    };
    let secs = end - start;
    if secs < 0 {
        // A worker with a skewed clock can report an end before the start.
        // Showing a negative duration is worse than admitting it is unknown.
        return "—".to_string();
    }
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// How long ago something happened, relative to `now`.
///
/// Relative rather than absolute because it avoids a timezone and locale
/// story, and "3h ago" is what you actually want when scanning a build list.
pub fn format_age(ts: Option<i64>, now: i64) -> String {
    let Some(ts) = ts else {
        return "—".to_string();
    };
    let secs = now - ts;
    if secs < 0 {
        // Clock skew again: a build stamped in the future.
        return "just now".to_string();
    }
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_in_the_largest_useful_unit() {
        assert_eq!(format_duration(Some(0), Some(43)), "43s");
        assert_eq!(format_duration(Some(0), Some(123)), "2m 3s");
        assert_eq!(format_duration(Some(100), Some(100)), "0s");
        assert_eq!(format_duration(Some(0), Some(3600)), "1h 0m");
        assert_eq!(format_duration(Some(0), Some(7860)), "2h 11m");
    }

    /// A running build has a start but no end. That is unknown, not zero — a
    /// build in progress must not read as having taken no time.
    #[test]
    fn an_unfinished_build_has_no_duration() {
        assert_eq!(format_duration(Some(100), None), "—");
        assert_eq!(format_duration(None, None), "—");
        assert_eq!(format_duration(None, Some(100)), "—");
    }

    /// Builds are timed by whichever worker ran them, so the two ends can come
    /// from different clocks. A negative duration is nonsense to display.
    #[test]
    fn a_backwards_timestamp_pair_is_not_shown_as_negative() {
        assert_eq!(format_duration(Some(500), Some(100)), "—");
    }

    #[test]
    fn ages_read_in_the_largest_useful_unit() {
        let now = 1_000_000;
        assert_eq!(format_age(Some(now), now), "just now");
        assert_eq!(format_age(Some(now - 59), now), "just now");
        assert_eq!(format_age(Some(now - 60), now), "1m ago");
        assert_eq!(format_age(Some(now - 3599), now), "59m ago");
        assert_eq!(format_age(Some(now - 3600), now), "1h ago");
        assert_eq!(format_age(Some(now - 86400), now), "1d ago");
        assert_eq!(format_age(Some(now - 86400 * 30), now), "30d ago");
    }

    #[test]
    fn a_build_with_no_start_time_has_no_age() {
        assert_eq!(format_age(None, 1_000_000), "—");
    }

    /// A worker whose clock runs ahead stamps a build in the future. "in -3m"
    /// would be worse than rounding to the present.
    #[test]
    fn a_future_timestamp_reads_as_just_now() {
        assert_eq!(format_age(Some(2_000_000), 1_000_000), "just now");
    }
}
