//! Timecode formatting and parsing.
//!
//! Two audiences with different needs:
//!
//! - **ffmpeg** wants `HH:MM:SS.mmm`, always fully qualified.
//! - **The UI** wants the shortest unambiguous form, so a 40-second clip reads
//!   `0:40` rather than `00:00:40.000`.
//!
//! Everything here is locale-independent. Formatting via a decimal float would
//! be a latent bug on systems where the separator is a comma.

/// `HH:MM:SS.mmm` — the form ffmpeg accepts unambiguously.
pub fn ffmpeg_timestamp(seconds: f64) -> String {
    let (h, m, s, ms) = split(seconds);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Shortest readable clock: `M:SS` under an hour, `H:MM:SS` beyond.
pub fn clock(seconds: f64) -> String {
    let (h, m, s, _) = split(seconds);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Clock with centiseconds, for mark positions where sub-second precision matters.
pub fn clock_precise(seconds: f64) -> String {
    let (h, m, s, ms) = split(seconds);
    let cs = ms / 10;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{cs:02}")
    } else {
        format!("{m}:{s:02}.{cs:02}")
    }
}

/// Signed offset, e.g. `-4.7s`, for showing how far a keyframe snap moved a mark.
pub fn signed_offset(seconds: f64) -> String {
    if seconds.abs() < 0.05 {
        "exact".to_string()
    } else {
        format!("{seconds:+.1}s")
    }
}

/// Parse `SS`, `M:SS`, or `H:MM:SS`, each optionally with a fractional part.
///
/// Returns `None` rather than a silently wrong value, so a typo in the UI cannot
/// move a mark somewhere unexpected.
pub fn parse(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };

    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 {
        return None;
    }

    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return None;
        }
        // Only the last component may carry a fraction.
        let is_last = i + 1 == parts.len();
        let value: f64 = part.parse().ok()?;
        if !is_last && part.contains('.') {
            return None;
        }
        if value < 0.0 {
            return None;
        }
        // Minutes and seconds must be two-digit fields when they follow another.
        if i > 0 && value >= 60.0 {
            return None;
        }
        total = total * 60.0 + value;
    }
    Some(sign * total)
}

/// Split into hours, minutes, seconds, milliseconds, clamping negatives to zero.
fn split(seconds: f64) -> (u64, u64, u64, u64) {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let total_ms = (seconds * 1000.0).round() as u64;
    let total_s = total_ms / 1000;
    (
        total_s / 3600,
        (total_s / 60) % 60,
        total_s % 60,
        total_ms % 1000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_form_is_fully_qualified() {
        assert_eq!(ffmpeg_timestamp(0.0), "00:00:00.000");
        assert_eq!(ffmpeg_timestamp(62.5), "00:01:02.500");
        assert_eq!(ffmpeg_timestamp(3661.25), "01:01:01.250");
    }

    #[test]
    fn negative_and_nonfinite_input_clamps() {
        assert_eq!(ffmpeg_timestamp(-5.0), "00:00:00.000");
        assert_eq!(ffmpeg_timestamp(f64::NAN), "00:00:00.000");
        assert_eq!(ffmpeg_timestamp(f64::INFINITY), "00:00:00.000");
    }

    #[test]
    fn clock_drops_the_hour_when_unused() {
        assert_eq!(clock(40.0), "0:40");
        assert_eq!(clock(62.0), "1:02");
        assert_eq!(clock(3661.0), "1:01:01");
    }

    #[test]
    fn precise_clock_keeps_centiseconds() {
        assert_eq!(clock_precise(62.53), "1:02.53");
        assert_eq!(clock_precise(3661.5), "1:01:01.50");
    }

    #[test]
    fn offsets_read_as_deltas() {
        assert_eq!(signed_offset(-4.72), "-4.7s");
        assert_eq!(signed_offset(1.28), "+1.3s");
        assert_eq!(signed_offset(0.0), "exact");
        // Snaps under half a frame are not worth reporting as an offset.
        assert_eq!(signed_offset(0.01), "exact");
    }

    #[test]
    fn parses_every_accepted_shape() {
        assert_eq!(parse("40"), Some(40.0));
        assert_eq!(parse("1:02"), Some(62.0));
        assert_eq!(parse("1:01:01"), Some(3661.0));
        assert_eq!(parse("1:02.5"), Some(62.5));
        assert_eq!(parse("  1:02  "), Some(62.0));
    }

    #[test]
    fn rejects_input_rather_than_guessing() {
        // A wrong guess here would silently move a cut mark.
        assert_eq!(parse(""), None);
        assert_eq!(parse("abc"), None);
        assert_eq!(parse("1:2:3:4"), None);
        assert_eq!(parse("1:75"), None, "75 seconds is not a valid field");
        assert_eq!(
            parse("1.5:00"),
            None,
            "only the last field may be fractional"
        );
        assert_eq!(parse("1::2"), None);
    }

    #[test]
    fn parse_round_trips_through_format() {
        for secs in [0.0, 7.0, 62.5, 599.25, 3661.75] {
            let parsed = parse(&clock_precise(secs)).expect("formatted value must parse");
            assert!(
                (parsed - secs).abs() < 0.01,
                "{secs} round-tripped to {parsed}"
            );
        }
    }
}
