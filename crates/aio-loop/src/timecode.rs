//! Parsing and formatting times like `75`, `1:15.5` or `0:01:15.250`.

/// Seconds from `ss`, `m:ss` or `h:mm:ss`, each with optional decimals.
pub fn parse(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.trim().is_empty()) {
        return None;
    }
    let mut seconds = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let value: f64 = part.trim().parse().ok()?;
        let last = i == parts.len() - 1;
        if value < 0.0 || (!last && value.fract() != 0.0) || (i > 0 && value >= 60.0) {
            return None;
        }
        seconds = seconds * 60.0 + value;
    }
    Some(seconds)
}

/// `m:ss.mmm`, or `h:mm:ss.mmm` from one hour.
pub fn format(seconds: f64) -> String {
    let ms_total = (seconds.max(0.0) * 1000.0).round() as u64;
    let (h, rest) = (ms_total / 3_600_000, ms_total % 3_600_000);
    let (m, rest) = (rest / 60_000, rest % 60_000);
    let (s, ms) = (rest / 1000, rest % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{ms:03}") } else { format!("{m}:{s:02}.{ms:03}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(parse("75"), Some(75.0));
        assert_eq!(parse("75.5"), Some(75.5));
        assert_eq!(parse("1:15"), Some(75.0));
        assert_eq!(parse(" 1:15.25 "), Some(75.25));
        assert_eq!(parse("0:01:15.250"), Some(75.25));
        assert_eq!(parse("2:00:00"), Some(7200.0));
    }

    #[test]
    fn rejects_nonsense() {
        for bad in ["", "abc", "1:60", "1::2", "-3", "1.5:00", "1:2:3:4"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn formats_and_roundtrips() {
        assert_eq!(format(75.25), "1:15.250");
        assert_eq!(format(3725.5), "1:02:05.500");
        assert_eq!(parse(&format(130.042)), Some(130.042));
    }
}
