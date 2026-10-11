//! Video in the UI: what the grid and the panels say about a video.

/// A length in seconds as a clock: `0:07`, `12:34`, `1:02:03`.
pub fn clock(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 { seconds.round().min(359_999.0) as u64 } else { 0 };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::clock;

    #[test]
    fn lengths_read_as_clocks() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(0.4), "0:00");
        assert_eq!(clock(6.6), "0:07");
        assert_eq!(clock(754.0), "12:34");
        assert_eq!(clock(3723.0), "1:02:03");
        // nonsense from a damaged file is a zero, never a panic or a wall of digits
        for odd in [-3.0, f64::NAN, f64::INFINITY, 1e300] {
            assert!(clock(odd).len() <= 8, "{odd}: {}", clock(odd));
        }
        assert_eq!(clock(f64::NAN), "0:00");
    }
}
