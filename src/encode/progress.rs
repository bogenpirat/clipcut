//! Parses ffmpeg's `-progress pipe:1` output.
//!
//! ffmpeg emits `key=value` lines and terminates each block with
//! `progress=continue` (or `progress=end` for the final one):
//!
//! ```text
//! frame=250
//! fps=62.5
//! total_size=1048576
//! out_time_us=10000000
//! out_time=00:00:10.000000
//! speed=2.08x
//! progress=continue
//! ```
//!
//! Values may be `N/A` before the first frame is encoded, and reads from the
//! pipe can split a line anywhere, so the parser carries a partial-line buffer.

use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    /// Position within the *output* clip, not the source file.
    pub out_time: Duration,
    pub frame: u64,
    pub fps: f64,
    /// Encoding rate relative to realtime; 2.0 means twice as fast as playback.
    pub speed: f64,
    pub total_size: u64,
    /// True for the block ending in `progress=end`.
    pub finished: bool,
}

impl Progress {
    /// Completed fraction in `0.0..=1.0`, given the expected clip length.
    ///
    /// Returns 1.0 once finished, so a clip whose final timestamp undershoots
    /// still reports as complete.
    pub fn fraction(&self, clip_duration_secs: f64) -> f64 {
        if self.finished {
            return 1.0;
        }
        if clip_duration_secs <= 0.0 {
            return 0.0;
        }
        (self.out_time.as_secs_f64() / clip_duration_secs).clamp(0.0, 1.0)
    }

    /// Estimated seconds remaining, or `None` while speed is still unknown.
    pub fn eta_secs(&self, clip_duration_secs: f64) -> Option<f64> {
        if self.finished || self.speed <= 0.0 || clip_duration_secs <= 0.0 {
            return None;
        }
        let remaining = clip_duration_secs - self.out_time.as_secs_f64();
        (remaining > 0.0).then(|| remaining / self.speed)
    }
}

#[derive(Debug, Default)]
pub struct ProgressParser {
    partial: String,
    current: Progress,
}

impl ProgressParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of stdout. Returns one [`Progress`] per completed block.
    ///
    /// Chunks may split lines arbitrarily; the remainder is carried over.
    pub fn push_str(&mut self, chunk: &str) -> Vec<Progress> {
        self.partial.push_str(chunk);
        let mut out = Vec::new();

        while let Some(nl) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=nl).collect();
            if let Some(p) = self.push_line(line.trim_end_matches(['\r', '\n'])) {
                out.push(p);
            }
        }
        out
    }

    /// Returns a snapshot when the line terminates a block.
    fn push_line(&mut self, line: &str) -> Option<Progress> {
        let (key, value) = line.split_once('=')?;
        let key = key.trim();
        let value = value.trim();

        // ffmpeg writes N/A until the corresponding statistic exists.
        if value.eq_ignore_ascii_case("N/A") {
            return None;
        }

        match key {
            // out_time_ms is microseconds too, despite the name. Prefer _us.
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = value.parse::<u64>() {
                    self.current.out_time = Duration::from_micros(us);
                }
            }
            "out_time" => {
                if self.current.out_time.is_zero()
                    && let Some(d) = parse_hhmmss(value)
                {
                    self.current.out_time = d;
                }
            }
            "frame" => self.current.frame = value.parse().unwrap_or(self.current.frame),
            "fps" => self.current.fps = value.parse().unwrap_or(self.current.fps),
            "total_size" => {
                self.current.total_size = value.parse().unwrap_or(self.current.total_size)
            }
            "speed" => {
                if let Ok(s) = value.trim_end_matches('x').parse::<f64>() {
                    self.current.speed = s;
                }
            }
            "progress" => {
                self.current.finished = value == "end";
                return Some(self.current.clone());
            }
            _ => {}
        }
        None
    }
}

/// Parse ffmpeg's `HH:MM:SS.ffffff` timestamps.
fn parse_hhmmss(s: &str) -> Option<Duration> {
    let mut parts = s.split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    Some(Duration::from_secs_f64(
        h as f64 * 3600.0 + m as f64 * 60.0 + sec,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "frame=250\nfps=62.5\nstream_0_0_q=28.0\nbitrate=1234.5kbits/s\n\
total_size=1048576\nout_time_us=10000000\nout_time_ms=10000000\n\
out_time=00:00:10.000000\ndup_frames=0\ndrop_frames=0\nspeed=2.08x\nprogress=continue\n";

    #[test]
    fn parses_a_complete_block() {
        let mut p = ProgressParser::new();
        let got = p.push_str(BLOCK);
        assert_eq!(got.len(), 1);
        let g = &got[0];
        assert_eq!(g.frame, 250);
        assert_eq!(g.fps, 62.5);
        assert_eq!(g.speed, 2.08);
        assert_eq!(g.total_size, 1_048_576);
        assert_eq!(g.out_time, Duration::from_secs(10));
        assert!(!g.finished);
    }

    /// Reads from a pipe land on arbitrary boundaries, not line boundaries.
    #[test]
    fn survives_lines_split_across_reads() {
        let mut whole = ProgressParser::new();
        let expected = whole.push_str(BLOCK).pop().unwrap();

        for split in 1..BLOCK.len() {
            let mut p = ProgressParser::new();
            let mut got = p.push_str(&BLOCK[..split]);
            got.extend(p.push_str(&BLOCK[split..]));
            assert_eq!(
                got.len(),
                1,
                "split at {split} produced {} blocks",
                got.len()
            );
            assert_eq!(got[0], expected, "split at {split} changed the result");
        }
    }

    #[test]
    fn detects_the_final_block() {
        let mut p = ProgressParser::new();
        let got = p.push_str("out_time_us=5000000\nprogress=end\n");
        assert!(got[0].finished);
        assert_eq!(got[0].fraction(10.0), 1.0, "finished always reports 100%");
    }

    #[test]
    fn ignores_not_available_values() {
        let mut p = ProgressParser::new();
        let got = p.push_str("frame=N/A\nspeed=N/A\nout_time_us=1000000\nprogress=continue\n");
        assert_eq!(got[0].frame, 0);
        assert_eq!(got[0].speed, 0.0);
        assert_eq!(got[0].out_time, Duration::from_secs(1));
    }

    #[test]
    fn falls_back_to_the_formatted_timestamp() {
        let mut p = ProgressParser::new();
        let got = p.push_str("out_time=00:01:02.500000\nprogress=continue\n");
        assert_eq!(got[0].out_time, Duration::from_millis(62_500));
    }

    #[test]
    fn fraction_is_clamped() {
        let mut p = ProgressParser::new();
        let got = p.push_str("out_time_us=20000000\nprogress=continue\n");
        assert_eq!(got[0].fraction(10.0), 1.0);
        assert_eq!(
            got[0].fraction(0.0),
            0.0,
            "unknown duration must not divide by zero"
        );
    }

    #[test]
    fn eta_uses_encoding_speed() {
        let mut p = ProgressParser::new();
        let got = p.push_str("out_time_us=5000000\nspeed=2.0x\nprogress=continue\n");
        // 5s of clip left at 2x realtime = 2.5s.
        assert_eq!(got[0].eta_secs(10.0), Some(2.5));
    }

    #[test]
    fn multiple_blocks_in_one_chunk() {
        let mut p = ProgressParser::new();
        let two = format!("{BLOCK}{BLOCK}");
        assert_eq!(p.push_str(&two).len(), 2);
    }
}
